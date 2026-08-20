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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum FindingDisposition {
    Open,
    Accepted,
    Dismissed,
    Fixed,
    Residual,
}

impl Default for FindingDisposition {
    fn default() -> Self {
        Self::Open
    }
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

fn default_base(repo: &Path) -> CtxResult<String> {
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
            Ok(0) | Err(_) => break,
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
        let remaining = MAX_REVIEW_DIFF_BYTES.saturating_sub(diff.len());
        let mut bytes = Vec::new();
        std::fs::File::open(&absolute)?
            .take(u64::try_from(remaining.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.len() > remaining {
            bytes.truncate(remaining);
            *truncated = true;
        }
        let body = String::from_utf8_lossy(&bytes);
        append_capped(diff, &body, MAX_REVIEW_DIFF_BYTES, truncated);
    }
    Ok(())
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
            || finding.path.as_ref().is_some_and(|path| {
                path.to_string_lossy().len() > MAX_FINDING_PATH_BYTES
            })
    }) {
        return Err("workflow contains an oversized review finding".into());
    }
    let base_sha = match base {
        Some(base) => git(&state.repo, &["rev-parse", base])?,
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
    let review_round = state
        .attempts
        .get("review")
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
        &super::telemetry::TelemetryConfig::from_env(),
    );
}

fn launch_reviewer(agent: &str, package: &ReviewPackage) -> CtxResult<i32> {
    if agent.is_empty()
        || agent.len() > 64
        || !agent
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!("invalid adapter name '{agent}'").into());
    }
    let prompt = format!(
        "Review the following compact Zirv review package. Return only confirmed concrete findings with severity, file/location, reasoning, and disposition recommendation. Do not modify files.\n\n{}",
        serde_json::to_string(package)?
    );
    let mut child = Command::new(std::env::current_exe()?)
        .args(["agent", agent, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes())?;
    }
    Ok(child.wait()?.code().unwrap_or(1))
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
            let (state_dir, mut state) = state_and_repo(args.repo.as_deref(), &args.id)?;
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
            let code = launch_reviewer(&args.agent, &package)?;
            let fingerprint_unchanged =
                verification::change_fingerprint(&state.repo)? == package.change_fingerprint;
            if code == 0 && fingerprint_unchanged {
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
            event.succeeded = Some(code == 0 && fingerprint_unchanged);
            event.findings_total = total;
            event.findings_meaningful = meaningful;
            event.findings_dismissed = dismissed;
            event.fix_round = package.review_round.saturating_sub(1);
            event.worker_count = 1;
            let _ = super::telemetry::record(
                &state_dir,
                &state.repo,
                &event,
                &super::telemetry::TelemetryConfig::from_env(),
            );
            if code == 0 && !fingerprint_unchanged {
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
                return Err(format!(
                    "finding summary exceeds {MAX_FINDING_SUMMARY_BYTES} bytes"
                )
                .into());
            }
            if args.path.as_ref().is_some_and(|path| {
                path.to_string_lossy().len() > MAX_FINDING_PATH_BYTES
            }) {
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
