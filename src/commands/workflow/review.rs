//! Compact independent-review packages and inspectable finding disposition.

use std::io::Write;
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
    pub fingerprint: u64,
    pub checks: Vec<(String, super::verification::CheckStatus, u64)>,
}

impl From<VerificationReport> for VerificationEvidence {
    fn from(report: VerificationReport) -> Self {
        Self {
            report_id: report.id,
            mode: report.mode,
            passed: report.passed(),
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
    pub changed_paths: Vec<PathBuf>,
    pub diff: String,
    pub diff_truncated: bool,
    pub verification: Option<VerificationEvidence>,
    pub existing_findings: Vec<ReviewFinding>,
    pub review_round: u8,
}

fn git(repo: &Path, args: &[&str]) -> CtxResult<String> {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output()?;
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

pub fn package(
    state_dir: &StateDir,
    state: &WorkflowState,
    base: Option<&str>,
) -> CtxResult<ReviewPackage> {
    let base_sha = match base {
        Some(base) => git(&state.repo, &["rev-parse", base])?,
        None => default_base(&state.repo)?,
    };
    let head_sha = git(&state.repo, &["rev-parse", "HEAD"])?;
    // `git diff <base>` includes committed branch changes plus current staged
    // and unstaged edits, so the reviewer sees the actual final surface.
    let raw_diff = git(&state.repo, &["diff", "--no-ext-diff", "--unified=3", &base_sha])?;
    let diff = crate::utils::truncate_bytes(raw_diff.clone(), Some(MAX_REVIEW_DIFF_BYTES));
    let diff_truncated = diff.len() < raw_diff.len();
    let changed_paths = git(&state.repo, &["diff", "--name-only", &base_sha])?
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    let verification = verification::load_latest(state_dir, &state.repo)?.map(Into::into);
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

fn launch_reviewer(agent: &str, package: &ReviewPackage) -> CtxResult<i32> {
    if !agent
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
                writeln!(writer, "review package {}..{}", package.base_sha, package.head_sha)?;
                writeln!(writer, "depth: {:?}", package.review_depth)?;
                writeln!(writer, "changed paths: {}", package.changed_paths.len())?;
                writeln!(writer, "diff bytes: {}{}", package.diff.len(), if package.diff_truncated { " (truncated)" } else { "" })?;
                if let Some(evidence) = &package.verification {
                    writeln!(writer, "verification: {} passed={}", evidence.report_id, evidence.passed)?;
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
            let package = package(&state_dir, &state, args.base.as_deref())?;
            let started = std::time::Instant::now();
            let code = launch_reviewer(&args.agent, &package)?;
            let (total, meaningful, dismissed) =
                super::telemetry::finding_counts(&state.review_findings);
            let mut event = super::telemetry::TelemetryEvent::new(
                super::telemetry::TelemetryKind::ReviewRun,
            );
            event.workflow_id = Some(state.id.clone());
            event.phase = Some(super::skill::WorkflowPhase::Review);
            event.intent = Some(state.classification.intent);
            event.complexity = Some(state.classification.complexity);
            event.risk = Some(state.classification.risk);
            event.duration_ms = Some(
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            event.adapter = Some(args.agent.clone());
            event.succeeded = Some(code == 0);
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
            return Ok(code);
        }
        ReviewCommand::Add(args) => {
            let (state_dir, mut state) = state_and_repo(args.repo.as_deref(), &args.workflow_id)?;
            let finding = ReviewFinding {
                id: uuid::Uuid::new_v4().to_string(),
                severity: args.severity,
                summary: args.summary.trim().to_string(),
                path: args.path.clone(),
                line: args.line,
                disposition: FindingDisposition::Open,
                created_at: now_secs(),
            };
            if finding.summary.is_empty() {
                return Err("finding summary must not be empty".into());
            }
            state.review_findings.push(finding.clone());
            state.updated_at = now_secs();
            save_state(&state_dir, &state)?;
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
    }
}
