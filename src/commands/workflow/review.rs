//! Compact independent-review packages and inspectable finding disposition.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use clap::{Args, Subcommand, ValueEnum};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::classify::RiskBand;
use super::engine::{self, WorkflowState, WorkflowStatus};
use super::verification::{self, VerificationReport};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, now_secs};

const MAX_REVIEW_DIFF_BYTES: usize = 96 * 1024;
const MAX_REVIEW_EVIDENCE: usize = 16;
const MAX_REVIEW_FINDINGS: usize = 256;
const MAX_FINDINGS_PER_RUN: usize = 64;
const MAX_FINDING_SUMMARY_BYTES: usize = 4 * 1024;
const MAX_FINDING_PATH_BYTES: usize = 4 * 1024;
const MAX_REVIEW_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_FIX_REVIEW_ROUNDS: u8 = 3;
const REVIEW_RESULT_PREFIX: &str = "ZIRV_REVIEW_RESULT ";

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
    #[serde(default)]
    pub recommended_disposition: Option<FindingDisposition>,
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

pub fn required_independent_reviews_for(state: &WorkflowState) -> usize {
    let baseline = required_independent_reviews(state.classification.risk);
    if baseline > 0 && has_repeated_meaningful_finding(&state.review_findings) {
        baseline.max(2)
    } else {
        baseline
    }
}

fn finding_key(finding: &ReviewFinding) -> String {
    if let Some(path) = &finding.path {
        format!(
            "{}:{}",
            path.to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase(),
            finding.line.unwrap_or(0)
        )
    } else {
        finding
            .summary
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    }
}

fn has_repeated_meaningful_finding(findings: &[ReviewFinding]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.severity,
                FindingSeverity::Major | FindingSeverity::Critical
            ) && finding.disposition != FindingDisposition::Dismissed
        })
        .map(finding_key)
        .any(|key| !seen.insert(key))
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
    pub required_independent_reviews: usize,
    pub escalation_reason: Option<String>,
    pub base_sha: String,
    pub head_sha: String,
    pub change_fingerprint: u64,
    pub changed_paths: Vec<PathBuf>,
    pub diff: String,
    pub diff_truncated: bool,
    pub verification: Option<VerificationEvidence>,
    pub existing_findings: Vec<ReviewFinding>,
    pub review_round: u8,
    pub max_review_rounds: u8,
}

fn review_round(state: &WorkflowState, current_fingerprint: u64) -> u8 {
    let latest = state
        .review_evidence
        .iter()
        .map(|evidence| evidence.review_round)
        .max()
        .unwrap_or(0);
    let current = state
        .review_evidence
        .iter()
        .filter(|evidence| evidence.change_fingerprint == current_fingerprint)
        .map(|evidence| evidence.review_round)
        .max();
    let evidence_round = current.unwrap_or_else(|| latest.saturating_add(1).max(1));
    let attempt_round = state
        .current()
        .and_then(|step| state.attempts.get(&step.id))
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    evidence_round.max(attempt_round)
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
    const PREFIXES: &[&str] = &[
        ".env",
        // SSH private keys, the single most common untracked secret in a
        // working checkout after `.env`. The public halves (`.pub`) are caught
        // by the same prefix, which costs nothing.
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        ".netrc",
        ".pgpass",
        "kubeconfig",
    ];
    const SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore"];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || name.contains("credential")
        || name.contains("secret")
}

/// Second, content-based gate behind the filename denylist above. A file
/// named `token.txt`/`api_key.txt`/`notes.md` matches no filename pattern but
/// can still hold a pasted credential -- and since the whole point of a
/// review package is to hand a diff to an external model, a false negative
/// here is expensive and unrecoverable. Deterministic and dependency-free of
/// any network/service call (the `regex` crate is already a workspace
/// dependency, used the same way by `frontend_detector.rs`): known
/// credential shapes first, then a conservative entropy check.
///
/// One pattern per high-confidence, low-false-positive family. Each is
/// anchored so it cannot fire in the middle of an ordinary word (`risk-`,
/// `desk-check`, ...): the character immediately before the marker must not
/// itself be alphanumeric.
static TOKEN_SHAPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:^|[^A-Za-z0-9])(?P<openai>sk-[A-Za-z0-9_-]{20,})",
        r"|(?:^|[^A-Za-z0-9])(?P<ghp>ghp_[A-Za-z0-9]{20,})",
        r"|(?:^|[^A-Za-z0-9])(?P<gho>gho_[A-Za-z0-9]{20,})",
        r"|(?:^|[^A-Za-z0-9])(?P<ghpat>github_pat_[A-Za-z0-9_]{20,})",
        r"|(?:^|[^A-Za-z0-9])(?P<slack>xox[baprs]-[A-Za-z0-9-]{10,})",
        r"|(?:^|[^A-Za-z0-9])(?P<aws>A[SK]IA[0-9A-Z]{16})",
        r"|(?P<pem>-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----)",
        r"|(?:^|[^A-Za-z0-9])(?P<jwt>eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})",
    ))
    .expect("valid secret token-shape regex")
});

const TOKEN_SHAPE_FAMILIES: &[(&str, &str)] = &[
    ("openai", "OpenAI-style secret key (sk-...)"),
    ("ghp", "GitHub personal access token (ghp_...)"),
    ("gho", "GitHub OAuth token (gho_...)"),
    (
        "ghpat",
        "GitHub fine-grained personal access token (github_pat_...)",
    ),
    ("slack", "Slack token (xox[baprs]-...)"),
    ("aws", "AWS access key id (AKIA/ASIA...)"),
    ("pem", "PEM private key block"),
    ("jwt", "JSON Web Token"),
];

fn detect_token_shape(text: &str) -> Option<&'static str> {
    let caps = TOKEN_SHAPE_RE.captures(text)?;
    TOKEN_SHAPE_FAMILIES
        .iter()
        .find(|(name, _)| caps.name(name).is_some())
        .map(|(_, label)| *label)
}

/// Conservative Shannon-entropy check on long unbroken base64/hex-ish runs,
/// tuned so ordinary source identifiers, prose, minified bundle content, and
/// hex lockfile hashes do not trip it. Two independent guards keep this
/// narrow: a 16-symbol hex alphabet caps out at 4.0 bits/char, so a run that
/// is pure hex is excluded outright regardless of length (lockfile/commit
/// hashes); and `_` is deliberately not a run character here, so a long
/// `snake_case_identifier` breaks into its component words at each
/// underscore rather than reading as one long candidate run. The threshold
/// and minimum length are both set well above what a real credential's
/// entropy floor requires (a random base64-ish secret of this length sits
/// close to that alphabet's ~6 bit/char ceiling) and above what natural
/// language or identifier text realistically reaches.
const ENTROPY_MIN_RUN: usize = 40;
const ENTROPY_THRESHOLD: f64 = 4.5;

fn shannon_entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0u32; 256];
    for &byte in bytes {
        counts[byte as usize] += 1;
    }
    let len = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = f64::from(count) / len;
            -p * p.log2()
        })
        .sum()
}

fn is_run_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-')
}

fn is_hex_run(run: &[u8]) -> bool {
    run.iter().all(u8::is_ascii_hexdigit)
}

fn has_digit_and_letter(run: &[u8]) -> bool {
    run.iter().any(u8::is_ascii_digit) && run.iter().any(u8::is_ascii_alphabetic)
}

fn detect_high_entropy_run(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !is_run_char(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_run_char(bytes[index]) {
            index += 1;
        }
        let run = &bytes[start..index];
        if run.len() >= ENTROPY_MIN_RUN && !is_hex_run(run) && has_digit_and_letter(run) {
            let entropy = shannon_entropy(run);
            if entropy >= ENTROPY_THRESHOLD {
                return Some(format!(
                    "high-entropy token ({} chars, {entropy:.2} bits/char)",
                    run.len()
                ));
            }
        }
    }
    None
}

/// The content-based gate itself: a known credential shape first (cheap,
/// specific), then the entropy fallback for an unlabeled high-entropy secret.
fn detect_content_secret(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    if let Some(label) = detect_token_shape(&text) {
        return Some(format!("content matches {label}"));
    }
    detect_high_entropy_run(&text)
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
        if let Some(reason) = detect_content_secret(&bytes) {
            append_capped(
                diff,
                &format!("[untracked file body omitted: {reason}]\n"),
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
    let review_round = review_round(state, current_fingerprint);
    if review_round > MAX_FIX_REVIEW_ROUNDS {
        return Err(format!(
            "review/fix loop reached the bounded limit of {MAX_FIX_REVIEW_ROUNDS} rounds; record residual dispositions or start a new workflow"
        )
        .into());
    }
    let required_reviews = required_independent_reviews_for(state);
    let escalated = required_reviews > required_independent_reviews(state.classification.risk);
    Ok(ReviewPackage {
        schema_version: 1,
        workflow_id: state.id.clone(),
        task: state.task.clone(),
        classification: state.classification.clone(),
        review_depth: if required_reviews >= 2 {
            ReviewDepth::StrongIndependentReview
        } else {
            depth_for_risk(state.classification.risk)
        },
        required_independent_reviews: required_reviews,
        escalation_reason: escalated.then(|| {
            "a major/critical finding recurred; require a second independent review".into()
        }),
        base_sha,
        head_sha,
        change_fingerprint: current_fingerprint,
        changed_paths,
        diff,
        diff_truncated,
        verification,
        existing_findings: state.review_findings.clone(),
        review_round,
        max_review_rounds: MAX_FIX_REVIEW_ROUNDS,
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
    event.work_domain = Some(state.classification.work_domain.domain);
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
    /// `Some` only for the real harness launch. Injected tests and legacy
    /// launch shims use `None`; real output must satisfy the structured
    /// result contract before it can become review evidence.
    output: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewerResponse {
    findings: Vec<ReviewerFinding>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewerFinding {
    severity: FindingSeverity,
    summary: String,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    recommended_disposition: Option<FindingDisposition>,
}

fn parse_reviewer_output(output: &str) -> CtxResult<Vec<ReviewerFinding>> {
    if output.len() > MAX_REVIEW_OUTPUT_BYTES {
        return Err(format!("reviewer output exceeds {MAX_REVIEW_OUTPUT_BYTES} bytes").into());
    }
    let mut result = None;
    for line in output.lines() {
        if let Some(json) = line.trim().strip_prefix(REVIEW_RESULT_PREFIX) {
            if result.is_some() {
                return Err("reviewer emitted more than one structured result".into());
            }
            result = Some(serde_json::from_str::<ReviewerResponse>(json)?);
        }
    }
    let response = result.ok_or("reviewer did not emit a structured Zirv review result")?;
    if response.findings.len() > MAX_FINDINGS_PER_RUN {
        return Err(format!(
            "reviewer returned more than {MAX_FINDINGS_PER_RUN} findings in one run"
        )
        .into());
    }
    for finding in &response.findings {
        let summary = finding.summary.trim();
        if summary.is_empty() || summary.len() > MAX_FINDING_SUMMARY_BYTES {
            return Err("reviewer returned an empty or oversized finding summary".into());
        }
        if finding
            .path
            .as_ref()
            .is_some_and(|path| path.to_string_lossy().len() > MAX_FINDING_PATH_BYTES)
        {
            return Err("reviewer returned an oversized finding path".into());
        }
    }
    Ok(response.findings)
}

fn append_reviewer_findings(
    state: &mut WorkflowState,
    findings: Vec<ReviewerFinding>,
) -> CtxResult<()> {
    if state.review_findings.len().saturating_add(findings.len()) > MAX_REVIEW_FINDINGS {
        return Err(format!(
            "review results would exceed the workflow limit of {MAX_REVIEW_FINDINGS} findings"
        )
        .into());
    }
    let created_at = now_secs();
    state
        .review_findings
        .extend(findings.into_iter().map(|finding| ReviewFinding {
            id: uuid::Uuid::new_v4().to_string(),
            severity: finding.severity,
            summary: finding.summary.trim().to_string(),
            path: finding.path,
            line: finding.line,
            disposition: FindingDisposition::Open,
            recommended_disposition: finding.recommended_disposition,
            created_at,
        }));
    Ok(())
}

/// One delegated run's stdout line, read as "the dashboard took this request".
///
/// Only consulted when a dashboard spawn-request channel actually exists
/// (`dash_active`): the relayed lines include the reviewer's own output, which
/// quotes a repository diff, so a diff containing this very prefix would
/// otherwise suppress evidence for a real completed review. Fail-closed either
/// way, but there is no reason to read the marker where no dashboard could have
/// written it.
fn is_dashboard_ack(line: &str) -> bool {
    line.trim_start()
        .starts_with(crate::commands::ctx::agent::DASH_SPAWN_ACK_PREFIX)
}

// T8: `env` used to be a hard-coded `std::env::var` read -- the one process-
// global lookup in this file with no injectable seam at all, unlike
// `sessions::nested_session_evidence`'s own `EnvLookup`-based read of the
// same variable. Nothing here manipulates the real `ZIRV_CTX_DASH_REQUESTS`
// today, so it was not observed to leak between tests, but a hard-coded
// real-env read is exactly the shape that does once something does -- the
// call site below still passes the real environment, so production behavior
// is unchanged.
fn dash_channel_active(env: crate::commands::ctx::config::EnvLookup<'_>) -> bool {
    env(crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV).is_some()
}

/// The argv a reviewer is launched with, after the program itself. The
/// adapter's read-only pin travels as trailing `-- flags`, which `zirv agent`
/// passes through to the harness's own CLI.
pub(crate) fn reviewer_argv(agent: &str) -> CtxResult<Vec<String>> {
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
    let mut argv = vec![
        "agent".to_string(),
        agent.to_string(),
        "-".to_string(),
        "--".to_string(),
    ];
    argv.extend(read_only);
    Ok(argv)
}

/// Relays the child's stdout to this process's stdout line by line, lossily:
/// a reviewer that emits a non-UTF-8 byte used to end the relay early (a
/// `lines()` error was read as end-of-stream), which dropped the read end,
/// which handed the reviewer a SIGPIPE mid-review.
fn relay_lines(mut stdout: impl Read, mut on_line: impl FnMut(&str)) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(count) = stdout.read(&mut chunk) {
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..count]);
        while let Some(at) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=at).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            on_line(text.trim_end_matches('\r'));
        }
    }
    if !pending.is_empty() {
        on_line(&String::from_utf8_lossy(&pending));
    }
}

/// Whether a delegated run counts as a completed independent review. A
/// dashboard spawn-ack exits 0 for a review that has not started yet, so exit
/// status alone is not the answer.
fn records_evidence(run: &ReviewerRun, fingerprint_unchanged: bool) -> bool {
    !run.dashboard_spawn && run.code == 0 && fingerprint_unchanged
}

fn launch_reviewer(agent: &str, package: &ReviewPackage) -> CtxResult<ReviewerRun> {
    let argv = reviewer_argv(agent)?;
    let prompt = format!(
        "Review the following compact Zirv review package. Do not modify files. Return exactly one single-line result prefixed `{REVIEW_RESULT_PREFIX}` followed by JSON shaped as {{\"findings\":[{{\"severity\":\"major\",\"summary\":\"concrete reasoning\",\"path\":\"src/file.rs\",\"line\":12,\"recommended_disposition\":\"accepted\"}}]}}. Use an empty findings array when no concrete issue exists. Do not emit another result line.\n\n{}",
        serde_json::to_string(package)?
    );
    let mut child = Command::new(std::env::current_exe()?)
        .args(&argv)
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
    let dash_active = dash_channel_active(&|k| std::env::var(k).ok());
    let mut dashboard_spawn = false;
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        relay_lines(stdout, |line| {
            dashboard_spawn |= dash_active && is_dashboard_ack(line);
            if output.len() < MAX_REVIEW_OUTPUT_BYTES.saturating_add(1) {
                let remaining = MAX_REVIEW_OUTPUT_BYTES
                    .saturating_add(1)
                    .saturating_sub(output.len());
                let bytes = line.as_bytes();
                output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                if output.len() < MAX_REVIEW_OUTPUT_BYTES.saturating_add(1) {
                    output.push(b'\n');
                }
            }
            println!("{line}");
        });
    }
    let code = child.wait()?.code().unwrap_or(1);
    let _ = writer.join();
    Ok(ReviewerRun {
        code,
        dashboard_spawn,
        output: Some(String::from_utf8_lossy(&output).into_owned()),
    })
}

/// `zirv workflow review run`, with the reviewer launch injected.
///
/// `launch` is a parameter so a test can drive this whole path -- including the
/// state reload below -- against a stand-in reviewer that writes to the same
/// state file, which is exactly what a real reviewer does through `zirv
/// workflow review add` while this function waits.
fn run_independent_review(
    args: &RunReviewArgs,
    writer: &mut impl Write,
    launch: &dyn Fn(&str, &ReviewPackage) -> CtxResult<ReviewerRun>,
) -> CtxResult<i32> {
    let (state_dir, state) = state_and_repo(args.repo.as_deref(), &args.id)?;
    if depth_for_risk(state.classification.risk) == ReviewDepth::SelfVerification {
        return Err(
            "risk policy selects self-verification; an independent reviewer is not required".into(),
        );
    }
    if state.status != WorkflowStatus::Running
        || state.current().map(|step| step.phase) != Some(super::skill::WorkflowPhase::Review)
    {
        return Err("independent review can only run during an active review step".into());
    }
    let package = package(&state_dir, &state, args.base.as_deref())?;
    let started = std::time::Instant::now();
    let run = launch(&args.agent, &package)?;
    let code = run.code;
    let fingerprint_unchanged =
        verification::change_fingerprint(&state.repo)? == package.change_fingerprint;
    // The reviewer runs `zirv workflow review add` against the same state file
    // while this process waits. The snapshot loaded before the spawn is stale
    // by definition, so the evidence is appended to freshly loaded state --
    // writing the old snapshot back used to erase every finding the reviewer
    // had just recorded.
    let parsed_findings = if records_evidence(&run, fingerprint_unchanged) {
        run.output
            .as_deref()
            .map(parse_reviewer_output)
            .transpose()?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut state = engine::load(&state_dir, &state.repo, &args.id)?;
    if run.dashboard_spawn {
        writeln!(
            writer,
            "the review was spawned as a dashboard pane; review evidence requires a completed \
             run, so none was recorded"
        )?;
    } else if records_evidence(&run, fingerprint_unchanged) {
        append_reviewer_findings(&mut state, parsed_findings)?;
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
    let (total, meaningful, dismissed) = super::telemetry::finding_counts(&state.review_findings);
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::ReviewRun);
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(super::skill::WorkflowPhase::Review);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
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
    Ok(code)
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
            return run_independent_review(args, writer, &launch_reviewer);
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
                recommended_disposition: None,
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

    /// A repository with one commit, so `package` has a real base, diff and
    /// fingerprint to read.
    fn git_repo() -> tempfile::TempDir {
        let repo = tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        repo
    }

    fn review_workflow(repo: &Path, state_dir: &StateDir) -> WorkflowState {
        let classification = super::super::classify::Classification {
            intent: super::super::classify::Intent::Review,
            complexity: super::super::classify::Complexity::Trivial,
            risk: RiskBand::Medium,
            risk_score: 25,
            changed_files: 1,
            changed_lines: 10,
            declared_scope: false,
            work_domain: Default::default(),
            risk_measurement: super::super::classify::RiskMeasurement::Measured,
            reasons: vec![],
        };
        let state = WorkflowState::start(
            repo.to_path_buf(),
            "review change".into(),
            super::super::engine::WorkflowKind::Review,
            None,
            true,
            classification,
        );
        engine::save(state_dir, &state, true).unwrap();
        state
    }

    /// C3: a real reviewer records findings through `zirv workflow review add`
    /// against the same state file while `review run` waits on it, so the
    /// snapshot taken before the spawn is stale by the time the run finishes.
    /// The injected launch below does exactly that — writes a finding to the
    /// state file mid-run — so restoring the old "serialize the pre-spawn
    /// snapshot" behavior makes this test fail on the finding count.
    #[test]
    fn a_finding_recorded_while_the_reviewer_ran_survives_the_evidence_write() {
        let repo = git_repo();
        let root = tempdir().unwrap();
        // SAFETY: this suite runs single-threaded (`--test-threads=1`).
        unsafe {
            std::env::set_var(crate::commands::ctx::state::STATE_ENV, root.path());
        }
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = review_workflow(repo.path(), &state_dir);

        let id = state.id.clone();
        let repo_path = repo.path().to_path_buf();
        let state_root = root.path().to_path_buf();
        let reviewer = move |_agent: &str, _package: &ReviewPackage| -> CtxResult<ReviewerRun> {
            // What the reviewer process does while the parent waits.
            let state_dir = StateDir::from_root(state_root.clone());
            let mut theirs = engine::load(&state_dir, &repo_path, &id)?;
            theirs.review_findings.push(ReviewFinding {
                id: "finding-from-reviewer".into(),
                severity: FindingSeverity::Major,
                summary: "real defect".into(),
                path: None,
                line: None,
                disposition: FindingDisposition::Open,
                recommended_disposition: None,
                created_at: now_secs(),
            });
            engine::save(&state_dir, &theirs, true)?;
            Ok(ReviewerRun {
                code: 0,
                dashboard_spawn: false,
                output: None,
            })
        };

        let args = RunReviewArgs {
            id: state.id.clone(),
            agent: "claude".into(),
            base: None,
            repo: Some(repo.path().to_path_buf()),
        };
        let mut out = Vec::new();
        let code = run_independent_review(&args, &mut out, &reviewer);
        unsafe {
            std::env::remove_var(crate::commands::ctx::state::STATE_ENV);
        }
        assert_eq!(code.expect("the review runs"), 0);

        let stored = engine::load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(
            stored.review_evidence.len(),
            1,
            "a completed review records evidence"
        );
        assert_eq!(
            stored.review_findings.len(),
            1,
            "the finding recorded during the run must survive the evidence write"
        );
    }

    /// The same path, but the delegation only reported that a dashboard pane
    /// was spawned: nothing has been reviewed yet, so nothing is recorded.
    #[test]
    fn a_dashboard_spawn_records_no_evidence_through_the_real_run_path() {
        let repo = git_repo();
        let root = tempdir().unwrap();
        // SAFETY: single-threaded suite.
        unsafe {
            std::env::set_var(crate::commands::ctx::state::STATE_ENV, root.path());
        }
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = review_workflow(repo.path(), &state_dir);
        let args = RunReviewArgs {
            id: state.id.clone(),
            agent: "claude".into(),
            base: None,
            repo: Some(repo.path().to_path_buf()),
        };
        let mut out = Vec::new();
        let code = run_independent_review(&args, &mut out, &|_, _| {
            Ok(ReviewerRun {
                code: 0,
                dashboard_spawn: true,
                output: None,
            })
        });
        unsafe {
            std::env::remove_var(crate::commands::ctx::state::STATE_ENV);
        }
        assert_eq!(code.expect("the run reports the spawn"), 0);
        assert!(
            engine::load(&state_dir, repo.path(), &state.id)
                .unwrap()
                .review_evidence
                .is_empty()
        );
        assert!(
            String::from_utf8(out).unwrap().contains("dashboard pane"),
            "the operator is told why no evidence was recorded"
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

    /// T8: `dash_channel_active` reads whichever channel its `env` lookup
    /// hands back, never the real process environment directly -- so this
    /// exercises both branches without ever touching (or leaking) the real
    /// `ZIRV_CTX_DASH_REQUESTS`.
    #[test]
    fn dash_channel_active_reads_only_its_injected_env() {
        let set = std::collections::HashMap::from([(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            "/tmp/some-requests-dir".to_string(),
        )]);
        assert!(dash_channel_active(&|k| set.get(k).cloned()));
        assert!(!dash_channel_active(&|_| None));
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
                dashboard_spawn: true,
                output: None,
            },
            true
        ));
        assert!(records_evidence(
            &ReviewerRun {
                code: 0,
                dashboard_spawn: false,
                output: None,
            },
            true
        ));
        assert!(!records_evidence(
            &ReviewerRun {
                code: 0,
                dashboard_spawn: false,
                output: None,
            },
            false
        ));
    }

    /// The pin has to reach the argv the reviewer is actually launched with,
    /// after a `--` so `zirv agent` passes it through to the harness's own CLI
    /// -- a correct lookup table that never made it onto the command line
    /// would restrict nothing.
    #[test]
    fn a_reviewer_is_always_pinned_read_only_or_refused() {
        assert_eq!(
            reviewer_argv("claude").unwrap(),
            [
                "agent",
                "claude",
                "-",
                "--",
                "--disallowedTools=Write,Edit,Bash,NotebookEdit"
            ]
        );
        // Issue #89: the two `--ignore-*` flags ride along only when the
        // codex binary actually installed on this machine advertises them
        // (CI has no codex at all, a developer box may have 0.147.0), so
        // assert the invariant -- the read-only pin is always present and
        // the optional flags can only ever trail it -- not one machine's
        // exact argv.
        let codex = reviewer_argv("codex").unwrap();
        assert_eq!(
            &codex[..6],
            ["agent", "codex", "-", "--", "--sandbox", "read-only"]
        );
        let trailing = &codex[6..];
        assert!(
            trailing.is_empty() || trailing == ["--ignore-rules", "--ignore-user-config"],
            "unexpected trailing reviewer flags: {trailing:?}"
        );
        let error = reviewer_argv("nope").unwrap_err().to_string();
        assert!(
            error.contains("cannot pin the reviewer read-only"),
            "{error}"
        );
        assert!(
            reviewer_argv("Claude").is_err(),
            "the name is validated too"
        );
    }

    /// A reviewer that emits a non-UTF-8 byte used to end the relay early (a
    /// `lines()` error read as end-of-stream), dropping the read end and
    /// handing the reviewer a SIGPIPE mid-review.
    #[test]
    fn non_utf8_reviewer_output_does_not_end_the_relay() {
        let mut input: Vec<u8> = b"first line\n".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);
        input.extend_from_slice(b" second line\nthird line\n");
        input.extend_from_slice(b"no trailing newline");
        let mut lines = Vec::new();
        relay_lines(std::io::Cursor::new(input), |line| {
            lines.push(line.to_string())
        });
        assert_eq!(lines.len(), 4, "got {lines:?}");
        assert_eq!(lines[0], "first line");
        assert!(lines[1].ends_with(" second line"));
        assert_eq!(lines[2], "third line");
        assert_eq!(lines[3], "no trailing newline");
    }

    #[test]
    fn common_untracked_secrets_are_all_recognised() {
        for name in [
            ".env",
            ".env.local",
            "credentials.json",
            "my-secrets.yaml",
            "server.pem",
            "tls.key",
            "id_rsa",
            "id_rsa.pub",
            "id_ed25519",
            "id_ecdsa",
            "id_dsa",
            ".netrc",
            ".pgpass",
            "kubeconfig",
            "kubeconfig.yaml",
            "bundle.p12",
            "cert.pfx",
            "release.keystore",
            "ID_RSA",
        ] {
            assert!(
                is_sensitive_name(Path::new(name)),
                "{name} should be treated as sensitive"
            );
        }
        for name in ["main.rs", "README.md", "keyboard.ts", "environment.yml"] {
            assert!(!is_sensitive_name(Path::new(name)), "{name} is ordinary");
        }
    }

    /// Deterministic token-shape fixture builder. GitHub push protection scans
    /// committed *content* for exactly the secret shapes `detect_content_secret`
    /// is built to catch, so the test fixtures below must never contain an
    /// assembled secret-shaped literal in source: each is built at test-run time
    /// from small, individually-innocuous pieces (a short prefix plus a body
    /// generated from a fixed alphabet, no randomness -- fully reproducible).
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const UPPER_DIGIT: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    fn fixture(prefix: &str, body_len: usize, alphabet: &[u8]) -> String {
        let mut out = String::with_capacity(prefix.len() + body_len);
        out.push_str(prefix);
        for i in 0..body_len {
            out.push(alphabet[i % alphabet.len()] as char);
        }
        out
    }

    fn pem_fixture(kind: &str) -> String {
        format!(
            "-----BEGIN {kind} PRIVATE KEY-----\nMIIEowIBAAKCAQEA{}\n-----END {kind} PRIVATE KEY-----\n",
            fixture("", 24, ALNUM)
        )
    }

    fn jwt_fixture() -> String {
        let header = fixture("eyJ", 20, ALNUM);
        let payload = fixture("eyJ", 30, ALNUM);
        let signature = fixture("", 40, ALNUM);
        format!("{header}.{payload}.{signature}\n")
    }

    /// #90: the content-based gate applied to a file whose *name* matches
    /// nothing on the filename denylist -- a realistic key pasted into
    /// `token.txt` must still be excluded, with a reason distinct from the
    /// filename-based exclusions.
    #[test]
    fn a_plain_token_txt_is_excluded_by_content_not_name() {
        let repo = tempdir().unwrap();
        assert!(!is_sensitive_name(Path::new("token.txt")));
        let secret = fixture("sk-", 46, ALNUM);
        std::fs::write(
            repo.path().join("token.txt"),
            format!("OPENAI_KEY={secret}\n"),
        )
        .unwrap();

        let mut diff = String::new();
        let mut truncated = false;
        append_untracked(
            &mut diff,
            &mut truncated,
            repo.path(),
            &[PathBuf::from("token.txt")],
        )
        .unwrap();

        assert!(!diff.contains(&secret));
        assert!(diff.contains("token.txt"), "the path itself stays visible");
        assert!(
            diff.contains("content matches OpenAI-style secret key"),
            "got {diff}"
        );
    }

    /// #90: table test -- one positive sample per detected family, plus a
    /// negative set (ordinary Rust source, a README, a lockfile with long hex
    /// hashes, and a minified bundle) that must produce zero false positives.
    #[test]
    fn content_based_secret_detection_covers_every_family_with_no_false_positives() {
        let positives: Vec<(&str, String)> = vec![
            (
                "openai",
                format!("OPENAI_KEY={}\n", fixture("sk-", 46, ALNUM)),
            ),
            (
                "github ghp_",
                format!("export GITHUB_TOKEN={}\n", fixture("ghp_", 36, ALNUM)),
            ),
            (
                "github gho_",
                format!("export GITHUB_OAUTH={}\n", fixture("gho_", 36, ALNUM)),
            ),
            (
                "github fine-grained pat",
                format!("{}\n", fixture("github_pat_", 54, ALNUM)),
            ),
            (
                "slack",
                format!("SLACK_BOT_TOKEN={}\n", fixture("xoxb-", 47, ALNUM)),
            ),
            (
                "aws",
                format!("AWS_ACCESS_KEY_ID={}\n", fixture("AKIA", 16, UPPER_DIGIT)),
            ),
            ("pem", pem_fixture("RSA")),
            ("jwt", jwt_fixture()),
        ];
        for (family, sample) in &positives {
            assert!(
                detect_content_secret(sample.as_bytes()).is_some(),
                "{family}: expected a hit for {sample:?}"
            );
        }

        let negatives: &[(&str, &str)] = &[
            (
                "rust source",
                r#"
pub fn resolved_repo(path: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match path {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

const MAX_CONFIGURED_RETENTION_DAYS: u64 = 3650;
const DEFAULT_MAX_EVENTS: usize = 1000;
"#,
            ),
            (
                "readme",
                r#"
# Zirv Dynamic CLI

Cross-platform CLI for executing developer-defined YAML/JSON/TOML scripts.
Run `cargo build` then `cargo test --verbose -- --test-threads=1` before
opening a pull request. See docs/obsidian/_system-context.md for the full
module map and architecture overview.
"#,
            ),
            (
                "lockfile hashes",
                r#"
[[package]]
name = "example"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "c9cee0ac6d301d0f6c3e1b0a3b3d5e6f4a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d"

[[package]]
name = "other"
version = "0.4.1"
checksum = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f80"
"#,
            ),
            (
                "minified bundle",
                "!function(e,t){\"object\"==typeof exports?module.exports=t():\"function\"==typeof define&&define.amd?define(t):e.myLib=t()}(this,function(){function a(b,c){return b+c}function d(e){return e*2}var f=a(1,2),g=d(f);return{sum:a,double:d,run:function(){return g}}});\n",
            ),
        ];
        for (name, sample) in negatives {
            assert!(
                detect_content_secret(sample.as_bytes()).is_none(),
                "{name}: expected no false positive, got {:?}",
                detect_content_secret(sample.as_bytes())
            );
        }
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

    #[test]
    fn structured_reviewer_results_are_validated_and_persistable() {
        let output = format!(
            "progress\n{REVIEW_RESULT_PREFIX}{{\"findings\":[{{\"severity\":\"major\",\"summary\":\"missing bounds check\",\"path\":\"src/main.rs\",\"line\":7,\"recommended_disposition\":\"accepted\"}}]}}\n"
        );
        let findings = parse_reviewer_output(&output).expect("structured result");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Major);
        assert_eq!(findings[0].path.as_deref(), Some(Path::new("src/main.rs")));
        assert_eq!(findings[0].line, Some(7));
        assert_eq!(
            findings[0].recommended_disposition,
            Some(FindingDisposition::Accepted)
        );
        assert!(parse_reviewer_output("review complete").is_err());
        assert!(parse_reviewer_output(&format!(
            "{REVIEW_RESULT_PREFIX}{{\"findings\":[]}}\n{REVIEW_RESULT_PREFIX}{{\"findings\":[]}}"
        ))
        .is_err());
    }

    #[test]
    fn repeated_major_findings_escalate_but_dismissals_do_not() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = review_workflow(repo.path(), &state_dir);
        let finding = |id: &str, disposition| ReviewFinding {
            id: id.into(),
            severity: FindingSeverity::Major,
            summary: "same defect".into(),
            path: Some(PathBuf::from("src/lib.rs")),
            line: Some(12),
            disposition,
            recommended_disposition: None,
            created_at: now_secs(),
        };
        state
            .review_findings
            .push(finding("one", FindingDisposition::Fixed));
        state
            .review_findings
            .push(finding("two", FindingDisposition::Open));
        assert_eq!(required_independent_reviews_for(&state), 2);
        state.review_findings[1].disposition = FindingDisposition::Dismissed;
        assert_eq!(required_independent_reviews_for(&state), 1);
    }

    #[test]
    fn fix_review_rounds_advance_only_for_a_changed_fingerprint() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = review_workflow(repo.path(), &state_dir);
        assert_eq!(review_round(&state, 10), 1);
        state.review_evidence.push(ReviewRunEvidence {
            id: "first".into(),
            change_fingerprint: 10,
            adapter: "claude".into(),
            review_round: 1,
            completed_at: now_secs(),
        });
        assert_eq!(review_round(&state, 10), 1);
        assert_eq!(review_round(&state, 11), 2);
        state.review_evidence.push(ReviewRunEvidence {
            id: "second".into(),
            change_fingerprint: 11,
            adapter: "codex".into(),
            review_round: 2,
            completed_at: now_secs(),
        });
        assert_eq!(review_round(&state, 12), 3);
    }
}
