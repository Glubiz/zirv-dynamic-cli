//! Compact independent-review packages and inspectable finding disposition.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::classify::RiskBand;
use super::engine::{self, WorkflowState, WorkflowStatus};
use super::verification::{self, VerificationReport};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, now_secs};

const MAX_REVIEW_DIFF_BYTES: usize = 96 * 1024;
const MAX_REVIEW_EVIDENCE: usize = 16;
const MAX_REVIEW_FINDINGS: usize = 256;
const MAX_FINDING_SUMMARY_BYTES: usize = 4 * 1024;
const MAX_FINDING_PATH_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum FindingSeverity {
    Note,
    Minor,
    Major,
    Critical,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum FindingDisposition {
    #[default]
    Open,
    Accepted,
    Dismissed,
    Fixed,
    Residual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub severity: FindingSeverity,
    pub summary: String,
    pub path: Option<PathBuf>,
    pub line: Option<u32>,
    #[serde(default)]
    pub disposition: FindingDisposition,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDepth {
    SelfVerification,
    OneIndependentReviewer,
    StrongIndependentReview,
}

pub fn required_independent_reviews(risk: RiskBand) -> usize {
    match depth_for_risk(risk) {
        ReviewDepth::SelfVerification => 0,
        ReviewDepth::OneIndependentReviewer => 1,
        ReviewDepth::StrongIndependentReview => 2,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRunEvidence {
    pub id: String,
    pub change_fingerprint: u64,
    pub adapter: String,
    pub review_round: u8,
    pub completed_at: u64,
}

pub fn depth_for_risk(risk: RiskBand) -> ReviewDepth {
    match risk {
        RiskBand::Low => ReviewDepth::SelfVerification,
        RiskBand::Medium | RiskBand::High => ReviewDepth::OneIndependentReviewer,
        RiskBand::Critical => ReviewDepth::StrongIndependentReview,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationEvidence {
    pub report_id: String,
    pub mode: super::verification::VerificationMode,
    pub passed: bool,
    pub fresh: bool,
    pub fingerprint: u64,
    pub checks: Vec<(String, super::verification::CheckStatus, u64)>,
}

impl VerificationEvidence {
    fn from_report(report: VerificationReport, current_fingerprint: u64) -> Self {
        let passed = report.passed();
        let fresh = report.change_fingerprint == current_fingerprint;
        Self {
            report_id: report.id,
            mode: report.mode,
            passed,
            fresh,
            fingerprint: report.change_fingerprint,
            checks: report
                .checks
                .into_iter()
                .map(|check| (check.id, check.status, check.duration_ms))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewPackage {
    pub schema_version: u32,
    pub workflow_id: String,
    pub task: String,
    pub classification: super::classify::Classification,
    pub review_depth: ReviewDepth,
    pub base_sha: String,
    pub head_sha: String,
    pub change_fingerprint: u64,
    pub changed_paths: Vec<PathBuf>,
    pub diff: String,
    pub diff_truncated: bool,
    pub verification: Option<VerificationEvidence>,
    pub existing_findings: Vec<ReviewFinding>,
    pub review_round: u8,
}

fn git(repo: &Path, args: &[&str]) -> CtxResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The diff base both this module and `classify` measure against, so the two
/// subsystems always mean the same thing by "the change".
pub fn default_base(repo: &Path) -> CtxResult<String> {
    for candidate in ["origin/main", "main", "HEAD^"] {
        if let Ok(base) = git(repo, &["merge-base", "HEAD", candidate])
            && !base.is_empty()
        {
            return Ok(base);
        }
    }
    git(repo, &["rev-parse", "HEAD"])
}

fn read_capped_head(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::with_capacity(cap);
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            // A read error is not a clean EOF: what follows is missing, and a
            // reviewer told `truncated: false` believes it has the whole diff.
            Err(_) => {
                truncated = true;
                break;
            }
            Ok(count) => count,
        };
        let remaining = cap.saturating_sub(kept.len());
        let take = count.min(remaining);
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < count;
    }
    (kept, truncated)
}

fn git_diff_capped(repo: &Path, base_sha: &str) -> CtxResult<(String, bool)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--no-ext-diff", "--unified=3", base_sha])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or("git diff stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("git diff stderr was not captured")?;
    let stderr_thread = std::thread::spawn(move || read_capped_head(stderr, 16 * 1024).0);
    let (stdout, truncated) = read_capped_head(stdout, MAX_REVIEW_DIFF_BYTES);
    let status = child.wait()?;
    let stderr = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        )
        .into());
    }
    Ok((String::from_utf8_lossy(&stdout).into_owned(), truncated))
}

fn append_capped(target: &mut String, text: &str, cap: usize, truncated: &mut bool) {
    let remaining = cap.saturating_sub(target.len());
    if text.len() <= remaining {
        target.push_str(text);
    } else {
        target.push_str(&crate::utils::truncate_bytes(
            text.to_string(),
            Some(remaining),
        ));
        *truncated = true;
    }
}

/// Cap on one untracked file's body. Untracked files are whatever happens to
/// be sitting in the working tree, so a generous per-file budget mostly buys a
/// way to fill the review package with one file.
const MAX_UNTRACKED_FILE_BYTES: usize = 16 * 1024;
/// How much of a file is examined for NUL bytes before its body is included.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Name patterns whose *contents* never go into a review package, however
/// small or textual the file is. An untracked `.env` or `credentials.json` is
/// the normal state of a working checkout, and the package is handed to a
/// separate agent process.
fn is_sensitive_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.starts_with(".env")
        || name.contains("credential")
        || name.contains("secret")
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

/// Untracked files contribute their path always, their body only when it is
/// safe: text (no NUL in the first [`BINARY_SNIFF_BYTES`]), small, and not
/// matching a sensitive name. Exclusions are stated in the package so a
/// reviewer knows a file exists and why its body is absent.
fn append_untracked(
    diff: &mut String,
    truncated: &mut bool,
    repo: &Path,
    paths: &[PathBuf],
) -> CtxResult<()> {
    for path in paths {
        let header = format!(
            "\n\ndiff --zirv-untracked a/{0} b/{0}\n--- /dev/null\n+++ b/{0}\n",
            path.display()
        );
        append_capped(diff, &header, MAX_REVIEW_DIFF_BYTES, truncated);
        if diff.len() == MAX_REVIEW_DIFF_BYTES {
            *truncated = true;
            break;
        }
        let absolute = repo.join(path);
        let metadata = std::fs::symlink_metadata(&absolute)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            append_capped(
                diff,
                "[untracked non-regular file omitted]\n",
                MAX_REVIEW_DIFF_BYTES,
                truncated,
            );
            continue;
        }
        if let Some(reason) = untracked_exclusion(path, &metadata) {
            append_capped(
                diff,
                &format!("[untracked file body omitted: {reason}]\n"),
                MAX_REVIEW_DIFF_BYTES,
                truncated,
            );
            continue;
        }
        let remaining = MAX_REVIEW_DIFF_BYTES
            .saturating_sub(diff.len())
            .min(MAX_UNTRACKED_FILE_BYTES);
        let mut bytes = Vec::new();
        std::fs::File::open(&absolute)?
            .take(u64::try_from(remaining.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
            append_capped(
                diff,
                "[untracked file body omitted: binary]\n",
                MAX_REVIEW_DIFF_BYTES,
                truncated,
            );
            continue;
        }
        if bytes.len() > remaining {
            bytes.truncate(remaining);
            *truncated = true;
        }
        let body = String::from_utf8_lossy(&bytes);
        append_capped(diff, &body, MAX_REVIEW_DIFF_BYTES, truncated);
    }
    Ok(())
}

fn untracked_exclusion(path: &Path, metadata: &std::fs::Metadata) -> Option<String> {
    if is_sensitive_name(path) {
        return Some("sensitive filename".to_string());
    }
    if metadata.len() > MAX_UNTRACKED_FILE_BYTES as u64 {
        return Some(format!(
            "{} bytes, over the {MAX_UNTRACKED_FILE_BYTES} byte untracked-file limit",
            metadata.len()
        ));
    }
    None
}

pub fn package(
    state_dir: &StateDir,
    state: &WorkflowState,
    base: Option<&str>,
) -> CtxResult<ReviewPackage> {
    if state.review_findings.len() > MAX_REVIEW_FINDINGS {
        return Err(format!(
            "workflow has more than {MAX_REVIEW_FINDINGS} review findings; dispose or consolidate findings before packaging"
        )
        .into());
    }
    if state.review_findings.iter().any(|finding| {
        finding.summary.len() > MAX_FINDING_SUMMARY_BYTES
            || finding
                .path
                .as_ref()
                .is_some_and(|path| path.to_string_lossy().len() > MAX_FINDING_PATH_BYTES)
    }) {
        return Err("workflow contains an oversized review finding".into());
    }
    let base_sha = match base {
        // `--verify --end-of-options` so a revision starting with `-` is read
        // as a revision and never as a flag to git itself. Verified against
        // git 2.50: bare `--end-of-options` echoes itself into stdout, and a
        // trailing `--` makes rev-parse treat the value as a path instead.
        Some(base) => git(
            &state.repo,
            &["rev-parse", "--verify", "--end-of-options", base],
        )?,
        None => default_base(&state.repo)?,
    };
    let head_sha = git(&state.repo, &["rev-parse", "HEAD"])?;
    // `git diff <base>` includes committed branch changes plus current staged
    // and unstaged edits. Git omits untracked files, so include bounded file
    // bodies for those explicitly and union them into the changed path list.
    let (mut diff, mut diff_truncated) = git_diff_capped(&state.repo, &base_sha)?;
    let untracked: Vec<PathBuf> =
        git(&state.repo, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
    append_untracked(&mut diff, &mut diff_truncated, &state.repo, &untracked)?;
    let mut changed_paths: BTreeSet<PathBuf> =
        git(&state.repo, &["diff", "--name-only", &base_sha])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
    changed_paths.extend(untracked);
    let changed_paths = changed_paths.into_iter().collect();
    let current_fingerprint = verification::change_fingerprint(&state.repo)?;
    let verification = verification::load_latest(state_dir, &state.repo)?
        .map(|report| VerificationEvidence::from_report(report, current_fingerprint));
    // The current step's own id, not the literal "review": a workflow whose
    // review step is named anything else reported fix_round 0 forever.
    let review_step = state
        .current()
        .map(|step| step.id.as_str())
        .unwrap_or("review");
    let review_round = state
        .attempts
        .get(review_step)
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    Ok(ReviewPackage {
        schema_version: 1,
        workflow_id: state.id.clone(),
        task: state.task.clone(),
        classification: state.classification.clone(),
        review_depth: depth_for_risk(state.classification.risk),
        base_sha,
        head_sha,
        change_fingerprint: current_fingerprint,
        changed_paths,
        diff,
        diff_truncated,
        verification,
        existing_findings: state.review_findings.clone(),
        review_round,
    })
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    #[command(subcommand)]
    pub command: ReviewCommand,
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// Emit a compact reproducible review package.
    Package(PackageArgs),
    /// Launch one isolated reviewer through Zirv supervision.
    Run(RunReviewArgs),
    /// Record a concrete review finding.
    Add(AddFindingArgs),
    /// Update a finding's final disposition.
    Dispose(DisposeFindingArgs),
    /// List findings and their dispositions.
    List(ReviewStateArgs),
}

#[derive(Debug, Args)]
pub struct ReviewStateArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PackageArgs {
    #[command(flatten)]
    pub state: ReviewStateArgs,
    #[arg(long)]
    pub base: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunReviewArgs {
    pub id: String,
    /// Enabled adapter name used by `zirv agent`.
    #[arg(long)]
    pub agent: String,
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AddFindingArgs {
    pub workflow_id: String,
    #[arg(long, value_enum)]
    pub severity: FindingSeverity,
    #[arg(long)]
    pub summary: String,
    #[arg(long)]
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub line: Option<u32>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DisposeFindingArgs {
    pub workflow_id: String,
    pub finding_id: String,
    #[arg(long, value_enum)]
    pub disposition: FindingDisposition,
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

fn state_and_repo(repo: Option<&Path>, id: &str) -> CtxResult<(StateDir, WorkflowState)> {
    let repo = match repo {
        Some(repo) => repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()),
        None => std::env::current_dir()?,
    };
    let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let state = engine::load(&state_dir, &repo, id)?;
    Ok((state_dir, state))
}

fn save_state(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    engine::save(state_dir, state, active)
}

fn record_finding_update(state_dir: &StateDir, state: &WorkflowState) {
    let (total, meaningful, dismissed) = super::telemetry::finding_counts(&state.review_findings);
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::FindingUpdated);
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(super::skill::WorkflowPhase::Review);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.findings_total = total;
    event.findings_meaningful = meaningful;
    event.findings_dismissed = dismissed;
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
}

/// A completed reviewer run, or the dashboard's acknowledgement that it
/// spawned a *pane* to do the review later.
struct ReviewerRun {
    code: i32,
    /// True when the child reported that the dashboard took the request
    /// (`agent.rs`'s [`crate::commands::ctx::agent::DASH_SPAWN_ACK_PREFIX`]
    /// line, exit 0). The review has not happened yet, so this exit 0 is not
    /// evidence of anything.
    dashboard_spawn: bool,
}

/// One delegated run's stdout line, read as "the dashboard took this request".
fn is_dashboard_ack(line: &str) -> bool {
    line.trim_start()
        .starts_with(crate::commands::ctx::agent::DASH_SPAWN_ACK_PREFIX)
}

/// Whether a delegated run counts as a completed independent review. A
/// dashboard spawn-ack exits 0 for a review that has not started yet, so exit
/// status alone is not the answer.
fn records_evidence(run: &ReviewerRun, fingerprint_unchanged: bool) -> bool {
    !run.dashboard_spawn && run.code == 0 && fingerprint_unchanged
}

fn launch_reviewer(agent: &str, package: &ReviewPackage) -> CtxResult<ReviewerRun> {
    if agent.is_empty()
        || agent.len() > 64
        || !agent
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!("invalid adapter name '{agent}'").into());
    }
    // The reviewer's prompt embeds an untrusted repository diff, exactly like
    // the distiller embeds untrusted CLAUDE.md text, so it gets the same
    // adapter-owned read-only pin rather than full tool access. An adapter
    // with no registered pin is refused rather than launched unrestricted.
    let read_only = crate::commands::ctx::adapters::read_only_args_for_agent_name(agent)
        .ok_or_else(|| format!("unknown adapter '{agent}'; cannot pin the reviewer read-only"))?;
    let prompt = format!(
        "Review the following compact Zirv review package. Return only confirmed concrete findings with severity, file/location, reasoning, and disposition recommendation. Do not modify files.\n\n{}",
        serde_json::to_string(package)?
    );
    let mut child = Command::new(std::env::current_exe()?)
        .args(["agent", agent, "-", "--"])
        .args(&read_only)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(prompt.as_bytes());
        }
        drop(stdin);
    });
    let mut dashboard_spawn = false;
    if let Some(stdout) = child.stdout.take() {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            dashboard_spawn |= is_dashboard_ack(&line);
            println!("{line}");
        }
    }
    let code = child.wait()?.code().unwrap_or(1);
    let _ = writer.join();
    Ok(ReviewerRun {
        code,
        dashboard_spawn,
    })
}

pub fn run(args: &ReviewArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        ReviewCommand::Package(args) => {
            let (state_dir, state) = state_and_repo(args.state.repo.as_deref(), &args.state.id)?;
            let package = package(&state_dir, &state, args.base.as_deref())?;
            if args.state.json {
                serde_json::to_writer_pretty(&mut *writer, &package)?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "review package {}..{}",
                    package.base_sha, package.head_sha
                )?;
                writeln!(writer, "depth: {:?}", package.review_depth)?;
                writeln!(writer, "changed paths: {}", package.changed_paths.len())?;
                writeln!(
                    writer,
                    "diff bytes: {}{}",
                    package.diff.len(),
                    if package.diff_truncated {
                        " (truncated)"
                    } else {
                        ""
                    }
                )?;
                if let Some(evidence) = &package.verification {
                    writeln!(
                        writer,
                        "verification: {} passed={}",
                        evidence.report_id, evidence.passed
                    )?;
                } else {
                    writeln!(writer, "verification: none")?;
                }
            }
        }
        ReviewCommand::Run(args) => {
            let (state_dir, state) = state_and_repo(args.repo.as_deref(), &args.id)?;
            if depth_for_risk(state.classification.risk) == ReviewDepth::SelfVerification {
                return Err("risk policy selects self-verification; an independent reviewer is not required".into());
            }
            if state.status != WorkflowStatus::Running
                || state.current().map(|step| step.phase)
                    != Some(super::skill::WorkflowPhase::Review)
            {
                return Err("independent review can only run during an active review step".into());
            }
            let package = package(&state_dir, &state, args.base.as_deref())?;
            let started = std::time::Instant::now();
            let run = launch_reviewer(&args.agent, &package)?;
            let code = run.code;
            let fingerprint_unchanged =
                verification::change_fingerprint(&state.repo)? == package.change_fingerprint;
            // The reviewer runs `zirv workflow review add` against the same
            // state file while this process waits. The snapshot loaded before
            // the spawn is stale by definition, so the evidence is appended to
            // freshly loaded state -- writing the old snapshot back used to
            // erase every finding the reviewer had just recorded.
            let mut state = engine::load(&state_dir, &state.repo, &args.id)?;
            if run.dashboard_spawn {
                writeln!(
                    writer,
                    "the review was spawned as a dashboard pane; review evidence requires a \
                     completed run, so none was recorded"
                )?;
            } else if records_evidence(&run, fingerprint_unchanged) {
                state.review_evidence.push(ReviewRunEvidence {
                    id: uuid::Uuid::new_v4().to_string(),
                    change_fingerprint: package.change_fingerprint,
                    adapter: args.agent.clone(),
                    review_round: package.review_round,
                    completed_at: now_secs(),
                });
                let overflow = state
                    .review_evidence
                    .len()
                    .saturating_sub(MAX_REVIEW_EVIDENCE);
                if overflow > 0 {
                    state.review_evidence.drain(..overflow);
                }
                state.updated_at = now_secs();
                save_state(&state_dir, &state)?;
            }
            let (total, meaningful, dismissed) =
                super::telemetry::finding_counts(&state.review_findings);
            let mut event =
                super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::ReviewRun);
            event.workflow_id = Some(state.id.clone());
            event.phase = Some(super::skill::WorkflowPhase::Review);
            event.intent = Some(state.classification.intent);
            event.complexity = Some(state.classification.complexity);
            event.risk = Some(state.classification.risk);
            event.duration_ms =
                Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            event.adapter = Some(args.agent.clone());
            event.succeeded = Some(records_evidence(&run, fingerprint_unchanged));
            event.findings_total = total;
            event.findings_meaningful = meaningful;
            event.findings_dismissed = dismissed;
            event.fix_round = package.review_round.saturating_sub(1);
            event.worker_count = 1;
            let _ = super::telemetry::record(
                &state_dir,
                &state.repo,
                &event,
                &super::telemetry::TelemetryConfig::for_repo(&state.repo),
            );
            if code == 0 && !fingerprint_unchanged && !run.dashboard_spawn {
                return Err(
                    "the change set changed during review; review evidence was not recorded".into(),
                );
            }
            return Ok(code);
        }
        ReviewCommand::Add(args) => {
            let (state_dir, mut state) = state_and_repo(args.repo.as_deref(), &args.workflow_id)?;
            if state.review_findings.len() >= MAX_REVIEW_FINDINGS {
                return Err(format!(
                    "workflow already has the maximum of {MAX_REVIEW_FINDINGS} review findings"
                )
                .into());
            }
            let summary = args.summary.trim();
            if summary.is_empty() {
                return Err("finding summary must not be empty".into());
            }
            if summary.len() > MAX_FINDING_SUMMARY_BYTES {
                return Err(
                    format!("finding summary exceeds {MAX_FINDING_SUMMARY_BYTES} bytes").into(),
                );
            }
            if args
                .path
                .as_ref()
                .is_some_and(|path| path.to_string_lossy().len() > MAX_FINDING_PATH_BYTES)
            {
                return Err(format!("finding path exceeds {MAX_FINDING_PATH_BYTES} bytes").into());
            }
            let finding = ReviewFinding {
                id: uuid::Uuid::new_v4().to_string(),
                severity: args.severity,
                summary: summary.to_string(),
                path: args.path.clone(),
                line: args.line,
                disposition: FindingDisposition::Open,
                created_at: now_secs(),
            };
            state.review_findings.push(finding.clone());
            state.updated_at = now_secs();
            save_state(&state_dir, &state)?;
            record_finding_update(&state_dir, &state);
            writeln!(writer, "{}", finding.id)?;
        }
        ReviewCommand::Dispose(args) => {
            let (state_dir, mut state) = state_and_repo(args.repo.as_deref(), &args.workflow_id)?;
            let finding = state
                .review_findings
                .iter_mut()
                .find(|finding| finding.id == args.finding_id)
                .ok_or("review finding not found")?;
            finding.disposition = args.disposition;
            state.updated_at = now_secs();
            save_state(&state_dir, &state)?;
            record_finding_update(&state_dir, &state);
            writeln!(writer, "{}: {:?}", args.finding_id, args.disposition)?;
        }
        ReviewCommand::List(args) => {
            let (_, state) = state_and_repo(args.repo.as_deref(), &args.id)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &state.review_findings)?;
                writeln!(writer)?;
            } else if state.review_findings.is_empty() {
                writeln!(writer, "no review findings")?;
            } else {
                for finding in state.review_findings {
                    writeln!(
                        writer,
                        "{}\t{:?}\t{:?}\t{}",
                        finding.id, finding.severity, finding.disposition, finding.summary
                    )?;
                }
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;
    use tempfile::tempdir;

    /// C3: the reviewer records findings through `zirv workflow review add`
    /// while `review run` waits, so the snapshot `run` loaded before the spawn
    /// is stale. Writing it back destroyed the reviewer's work; the fix is to
    /// re-load and append to the fresh state.
    #[test]
    fn evidence_is_appended_to_state_the_reviewer_may_have_changed() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let classification = super::super::classify::Classification {
            intent: super::super::classify::Intent::Review,
            complexity: super::super::classify::Complexity::Trivial,
            risk: RiskBand::Medium,
            risk_score: 25,
            changed_files: 1,
            changed_lines: 10,
            declared_scope: false,
            reasons: vec![],
        };
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review change".into(),
            super::super::engine::WorkflowKind::Review,
            None,
            true,
            classification,
        );
        engine::save(&state_dir, &state, true).unwrap();

        // The reviewer's own process: a finding added against the same file.
        let mut reviewer_view = engine::load(&state_dir, repo.path(), &state.id).unwrap();
        reviewer_view.review_findings.push(ReviewFinding {
            id: "finding-from-reviewer".into(),
            severity: FindingSeverity::Major,
            summary: "real defect".into(),
            path: None,
            line: None,
            disposition: FindingDisposition::Open,
            created_at: now_secs(),
        });
        engine::save(&state_dir, &reviewer_view, true).unwrap();

        // The parent, holding its pre-spawn snapshot, records evidence.
        let fresh = engine::load(&state_dir, repo.path(), &state.id).unwrap();
        state = fresh;
        state.review_evidence.push(ReviewRunEvidence {
            id: "evidence".into(),
            change_fingerprint: 7,
            adapter: "claude".into(),
            review_round: 1,
            completed_at: now_secs(),
        });
        save_state(&state_dir, &state).unwrap();

        let stored = engine::load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(stored.review_evidence.len(), 1);
        assert_eq!(
            stored.review_findings.len(),
            1,
            "the reviewer's finding must survive the parent's own write"
        );
    }

    #[test]
    fn untracked_secrets_contribute_a_path_but_never_a_body() {
        let repo = tempdir().unwrap();
        std::fs::write(repo.path().join(".env"), "TOKEN=super-secret\n").unwrap();
        std::fs::write(
            repo.path().join("credentials.json"),
            "{\"credential\":\"x\"}",
        )
        .unwrap();
        std::fs::write(repo.path().join("key.pem"), "-----BEGIN KEY-----\n").unwrap();
        std::fs::write(repo.path().join("notes.txt"), "ordinary text\n").unwrap();
        std::fs::write(repo.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();

        let mut diff = String::new();
        let mut truncated = false;
        append_untracked(
            &mut diff,
            &mut truncated,
            repo.path(),
            &[
                PathBuf::from(".env"),
                PathBuf::from("credentials.json"),
                PathBuf::from("key.pem"),
                PathBuf::from("notes.txt"),
                PathBuf::from("blob.bin"),
            ],
        )
        .unwrap();

        assert!(!diff.contains("super-secret"));
        assert!(!diff.contains("BEGIN KEY"));
        assert!(diff.contains(".env"), "the path itself stays visible");
        assert_eq!(diff.matches("sensitive filename").count(), 3);
        assert!(diff.contains("omitted: binary"));
        assert!(diff.contains("ordinary text"));
    }

    /// C4: under `ZIRV_CTX_DASH_REQUESTS` a delegation exits 0 as soon as the
    /// dashboard *accepts* the request. Recording review evidence off that
    /// exit code credited a review that had not run.
    #[test]
    fn a_dashboard_spawn_ack_is_not_a_completed_review() {
        let ack = format!(
            "{}abcd1234",
            crate::commands::ctx::agent::DASH_SPAWN_ACK_PREFIX
        );
        assert!(is_dashboard_ack(&ack));
        assert!(!is_dashboard_ack("Findings: 1 major issue"));
        assert!(!records_evidence(
            &ReviewerRun {
                code: 0,
                dashboard_spawn: true
            },
            true
        ));
        assert!(records_evidence(
            &ReviewerRun {
                code: 0,
                dashboard_spawn: false
            },
            true
        ));
        assert!(!records_evidence(
            &ReviewerRun {
                code: 0,
                dashboard_spawn: false
            },
            false
        ));
    }

    #[test]
    fn a_reviewer_is_always_pinned_read_only_or_refused() {
        assert_eq!(
            crate::commands::ctx::adapters::read_only_args_for_agent_name("claude"),
            Some(vec![
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string()
            ])
        );
        assert_eq!(
            crate::commands::ctx::adapters::read_only_args_for_agent_name("codex"),
            Some(vec!["--sandbox".to_string(), "read-only".to_string()])
        );
        assert_eq!(
            crate::commands::ctx::adapters::read_only_args_for_agent_name("nope"),
            None
        );
    }

    #[test]
    fn review_depth_is_explicit_and_risk_based() {
        assert_eq!(depth_for_risk(RiskBand::Low), ReviewDepth::SelfVerification);
        assert_eq!(
            depth_for_risk(RiskBand::Medium),
            ReviewDepth::OneIndependentReviewer
        );
        assert_eq!(
            depth_for_risk(RiskBand::Critical),
            ReviewDepth::StrongIndependentReview
        );
        assert_eq!(required_independent_reviews(RiskBand::Low), 0);
        assert_eq!(required_independent_reviews(RiskBand::Medium), 1);
        assert_eq!(required_independent_reviews(RiskBand::High), 1);
        assert_eq!(required_independent_reviews(RiskBand::Critical), 2);
    }
}
