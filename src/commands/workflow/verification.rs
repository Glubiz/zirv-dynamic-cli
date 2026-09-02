//! Repository-aware targeted and final verification.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::event::input_hash;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

/// `.zirv/verify.toml`'s own schema. Repository-facing: bumping it asks every
/// repository to rewrite its config, so it moves only for a real config-format
/// change.
const VERIFY_CONFIG_SCHEMA_VERSION: u32 = 1;
/// The stored report's schema, which only zirv writes and reads. Bumped to 2
/// when `narrowed_to` was added: the field is `#[serde(default)]`, so a
/// narrowed report written by an earlier build would otherwise deserialize as
/// un-narrowed and satisfy the freshness gate it was supposed to fail.
pub(crate) const VERIFY_REPORT_SCHEMA_VERSION: u32 = 2;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_FAILURE_OUTPUT_BYTES: usize = 16 * 1024;
/// Hard ceiling on a repository-supplied check's own timeout, independent of
/// `workflow.repo_checks_enabled`. `CheckSpec::validate` allows up to a day,
/// which for a checkout-authored command is a wall-clock denial of service
/// dressed as a test suite. Clamped rather than refused, with a report note:
/// the check still runs, it just cannot hold the session for hours.
const MAX_REPO_TIMEOUT_SECS: u64 = 900;
/// Hard ceiling on how many repository-supplied checks one run will consider.
const MAX_REPO_CHECKS: usize = 32;
/// Shared wall-clock budget across every repository-supplied check in one run.
/// The per-check clamp alone still allows 32 x 900s; this is the ceiling on the
/// whole set.
const MAX_REPO_TOTAL_TIME: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    Format,
    Lint,
    Unit,
    Integration,
    Typecheck,
    Build,
    Custom,
}

fn default_timeout() -> u64 {
    600
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    pub id: String,
    pub kind: CheckKind,
    pub command: String,
    #[serde(default)]
    pub paths: Vec<String>,
    /// Eligible during changed-scope implementation checks.
    #[serde(default = "default_true")]
    pub changed: bool,
    /// Required by `zirv verify` final verification.
    #[serde(default = "default_true")]
    pub final_check: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl CheckSpec {
    fn validate(&self) -> CtxResult<()> {
        if self.id.is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
        {
            return Err(format!("invalid verification check id '{}'", self.id).into());
        }
        if self.command.trim().is_empty() {
            return Err(format!("verification check '{}': command is empty", self.id).into());
        }
        if self.timeout_secs == 0 || self.timeout_secs > 86_400 {
            return Err(format!(
                "verification check '{}': timeout_secs must be in 1..=86400",
                self.id
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationConfig {
    pub schema_version: u32,
    #[serde(default)]
    pub checks: Vec<CheckSpec>,
}

impl VerificationConfig {
    fn validate(&self) -> CtxResult<()> {
        if self.schema_version != VERIFY_CONFIG_SCHEMA_VERSION {
            return Err(format!(
                "unsupported verification schema_version {}; supported version is {}",
                self.schema_version, VERIFY_CONFIG_SCHEMA_VERSION
            )
            .into());
        }
        // Issue #268: an explicitly empty `checks = []` used to hard-error
        // here, which meant `zirv verify` on a checked-in but empty
        // `.zirv/verify.toml` bricked the command outright instead of
        // producing a report an operator could see. `run_mode` now turns
        // zero resolved checks into an `Inconclusive` report (or a `Passed`
        // one under the `workflow.allow_empty_verify` override) -- see its
        // own doc comment.
        let mut ids = std::collections::BTreeSet::new();
        for check in &self.checks {
            check.validate()?;
            if !ids.insert(&check.id) {
                return Err(format!("duplicate verification check id '{}'", check.id).into());
            }
        }
        Ok(())
    }
}

/// Where one check's command text came from. A report records this per check
/// so a vacuous `command = "true"` gate is visible as repo-authored rather
/// than looking like a real toolchain check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckSource {
    /// A command written in the repository's own `.zirv/verify.toml`.
    RepoConfig,
    /// `npm run <script>`, whose body is written in the repository's own
    /// `package.json`. Discovered by zirv, authored by the checkout.
    DiscoveredScript,
    /// A command zirv itself owns (the Cargo checks), discovered from the
    /// presence of a manifest but never taken from repository text.
    DiscoveredToolchain,
}

impl CheckSource {
    /// Whether the command text this check runs was authored by the checkout.
    fn repo_supplied(self) -> bool {
        !matches!(self, Self::DiscoveredToolchain)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCheck {
    pub spec: CheckSpec,
    pub source: CheckSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedChecks {
    pub checks: Vec<ResolvedCheck>,
    pub origin: &'static str,
    pub notes: Vec<String>,
}

/// Caps that hold whether or not `workflow.repo_checks_enabled` is on: a
/// checkout must not be able to choose its own wall-clock budget or flood a
/// run with hundreds of commands.
fn apply_repo_caps(checks: &mut Vec<ResolvedCheck>, notes: &mut Vec<String>) {
    for check in checks.iter_mut() {
        if check.source.repo_supplied() && check.spec.timeout_secs > MAX_REPO_TIMEOUT_SECS {
            notes.push(format!(
                "check '{}': repo-supplied timeout {}s clamped to {MAX_REPO_TIMEOUT_SECS}s",
                check.spec.id, check.spec.timeout_secs
            ));
            check.spec.timeout_secs = MAX_REPO_TIMEOUT_SECS;
        }
    }
    let repo_supplied = checks
        .iter()
        .filter(|check| check.source.repo_supplied())
        .count();
    if repo_supplied > MAX_REPO_CHECKS {
        let mut kept = 0usize;
        checks.retain(|check| {
            if !check.source.repo_supplied() {
                return true;
            }
            kept += 1;
            kept <= MAX_REPO_CHECKS
        });
        notes.push(format!(
            "{repo_supplied} repo-supplied checks truncated to the first {MAX_REPO_CHECKS}"
        ));
    }
}

pub fn load_or_discover(repo: &Path) -> CtxResult<ResolvedChecks> {
    let (config, origin, source) = load_or_discover_raw(repo)?;
    let mut checks: Vec<ResolvedCheck> = config
        .checks
        .into_iter()
        .map(|spec| ResolvedCheck {
            source: source(&spec),
            spec,
        })
        .collect();
    let mut notes = Vec::new();
    apply_repo_caps(&mut checks, &mut notes);
    Ok(ResolvedChecks {
        checks,
        origin,
        notes,
    })
}

type SourceFor = fn(&CheckSpec) -> CheckSource;

fn load_or_discover_raw(repo: &Path) -> CtxResult<(VerificationConfig, &'static str, SourceFor)> {
    let path = repo.join(".zirv").join("verify.toml");
    if path.exists() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "verification config '{}' must be a regular file",
                path.display()
            )
            .into());
        }
        if usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_CONFIG_BYTES {
            return Err(format!(
                "verification config '{}' exceeds {MAX_CONFIG_BYTES} bytes",
                path.display()
            )
            .into());
        }
        let config: VerificationConfig = toml::from_str(&std::fs::read_to_string(&path)?)?;
        config.validate()?;
        return Ok((config, "configured", |_| CheckSource::RepoConfig));
    }

    let mut checks = Vec::new();
    if repo.join("Cargo.toml").is_file() {
        let rust_paths = vec![
            "src/".to_string(),
            "tests/".to_string(),
            "examples/".to_string(),
            "Cargo.toml".to_string(),
            "Cargo.lock".to_string(),
            "*.rs".to_string(),
        ];
        checks.extend([
            CheckSpec {
                id: "format".into(),
                kind: CheckKind::Format,
                command: "cargo fmt -- --check".into(),
                paths: rust_paths.clone(),
                changed: true,
                final_check: true,
                timeout_secs: 120,
            },
            CheckSpec {
                id: "clippy".into(),
                kind: CheckKind::Lint,
                command: "cargo clippy --all-targets -- -D warnings".into(),
                paths: rust_paths.clone(),
                changed: true,
                final_check: true,
                timeout_secs: 900,
            },
            CheckSpec {
                id: "test".into(),
                kind: CheckKind::Unit,
                command: "cargo test --verbose -- --test-threads=1".into(),
                paths: rust_paths,
                changed: true,
                final_check: true,
                timeout_secs: 1800,
            },
        ]);
    }
    if repo.join("package.json").is_file() {
        let package: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(repo.join("package.json"))?)?;
        let scripts = package.get("scripts").and_then(|value| value.as_object());
        for (id, kind) in [
            ("lint", CheckKind::Lint),
            ("typecheck", CheckKind::Typecheck),
            ("test", CheckKind::Unit),
            ("build", CheckKind::Build),
        ] {
            if scripts.is_some_and(|scripts| scripts.contains_key(id))
                && !checks.iter().any(|check| check.id == id)
            {
                checks.push(CheckSpec {
                    id: id.to_string(),
                    kind,
                    command: format!("npm run {id}"),
                    paths: vec![
                        "src/".into(),
                        "test/".into(),
                        "tests/".into(),
                        "package.json".into(),
                        "package-lock.json".into(),
                        "*.js".into(),
                        "*.jsx".into(),
                        "*.ts".into(),
                        "*.tsx".into(),
                    ],
                    changed: true,
                    final_check: true,
                    timeout_secs: 900,
                });
            }
        }
    }
    let config = VerificationConfig {
        schema_version: VERIFY_CONFIG_SCHEMA_VERSION,
        checks,
    };
    // Zero checks (neither a Cargo nor a recognized package.json project) is
    // no longer a hard error here -- see issue #268 and `run_mode`'s own
    // handling of `resolved.checks.is_empty()`.
    config.validate()?;
    // `npm run <id>` executes a command body written in the repository's own
    // package.json: discovered by zirv, authored by the checkout, and gated
    // as such. The Cargo commands are zirv's own text.
    Ok((config, "discovered", |spec| {
        if spec.command.starts_with("npm run ") {
            CheckSource::DiscoveredScript
        } else {
            CheckSource::DiscoveredToolchain
        }
    }))
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    if pattern.ends_with('/') {
        return path.starts_with(&pattern);
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return path == pattern || path.starts_with(&format!("{pattern}/"));
    }
    wildcard_match(pattern.as_bytes(), path.as_bytes())
}

fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    let (mut p, mut v, mut star, mut retry) = (0usize, 0usize, None, 0usize);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            retry += 1;
            v = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// The worktree root, so every path this module handles is root-relative.
/// `git diff --name-only` already reports root-relative paths while `git
/// ls-files --others` reports them relative to the process's directory, so
/// running both from a subdirectory (`--repo <subdir>`) mixed two path bases
/// in one list and content edits then matched nothing.
fn git_root(repo: &Path) -> PathBuf {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                repo.to_path_buf()
            } else {
                PathBuf::from(text)
            }
        }
        _ => repo.to_path_buf(),
    }
}

/// `-c core.quotePath=false`: with the default on, git escapes any non-ASCII
/// path into `"\303\244..."`, which no later `hash-object`/pattern match can
/// resolve back to the real file.
fn git_at(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.quotePath=false")
        .arg("-C")
        .arg(repo);
    command
}

pub fn changed_paths(repo: &Path) -> CtxResult<Vec<PathBuf>> {
    let root = git_root(repo);
    let mut paths = Vec::new();
    for args in [
        &["diff", "--name-only", "HEAD"][..],
        &["ls-files", "--others", "--exclude-standard", "--full-name"][..],
    ] {
        let output = git_at(&root).args(args).output()?;
        if !output.status.success() {
            return Err(format!(
                "cannot inspect changed paths: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        paths.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(PathBuf::from)
                // #229/#232: the workflow's own `.zirv/work/<id>/*` artifacts
                // (plans, execute-plan pages, raw review salvage) are not the
                // operator's change surface. Left in, an edit to one of them
                // -- ticking a plan checkbox, a concurrent dashboard write --
                // shifts `change_fingerprint` out from under an in-flight
                // review and any other `changed_paths` consumer.
                .filter(|path: &PathBuf| !super::classify::is_workflow_work_path(path)),
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The operator's actual change surface since the workflow's own diff base
/// (`review::default_base`: merge-base against origin/main, then main, then
/// HEAD^, then HEAD) -- unlike `changed_paths` above, which is uncommitted-
/// only against bare HEAD and therefore goes blind to a frontend change the
/// moment an earlier workflow step commits it (#251). Union of the diff
/// against that base, the uncommitted diff against HEAD (kept for
/// resilience when `default_base` degrades to `HEAD` itself), and untracked
/// not-ignored files; paths that no longer exist on disk are dropped, since
/// nothing later can scan them.
pub fn changed_paths_since_base(repo: &Path) -> CtxResult<Vec<PathBuf>> {
    let root = git_root(repo);
    let base = super::review::default_base(repo)
        .map_err(|err| format!("cannot inspect changed paths: {err}"))?;
    let mut paths = Vec::new();
    for args in [
        &["diff", "--name-only", base.as_str()][..],
        &["diff", "--name-only", "HEAD"][..],
        &["ls-files", "--others", "--exclude-standard", "--full-name"][..],
    ] {
        let output = git_at(&root).args(args).output()?;
        if !output.status.success() {
            return Err(format!(
                "cannot inspect changed paths: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        paths.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(PathBuf::from)
                .filter(|path: &PathBuf| !super::classify::is_workflow_work_path(path)),
        );
    }
    paths.retain(|path| root.join(path).exists());
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub fn change_fingerprint(repo: &Path) -> CtxResult<u64> {
    let root = git_root(repo);
    let head = git_at(&root).args(["rev-parse", "HEAD"]).output()?;
    if !head.status.success() {
        return Err("cannot fingerprint repository HEAD".into());
    }
    let diff = git_at(&root).args(["diff", "--raw", "HEAD"]).output()?;
    if !diff.status.success() {
        return Err("cannot fingerprint repository diff".into());
    }
    let mut input = String::from_utf8_lossy(&head.stdout).into_owned();
    input.push_str(&String::from_utf8_lossy(&diff.stdout));
    for path in changed_paths(repo)? {
        input.push_str("\npath:");
        input.push_str(&path.to_string_lossy());
        if std::fs::symlink_metadata(root.join(&path)).is_ok() {
            let hashed = git_at(&root)
                .args(["hash-object", "--no-filters", "--"])
                .arg(&path)
                .output()?;
            if !hashed.status.success() {
                return Err(format!(
                    "cannot fingerprint '{}': {}",
                    path.display(),
                    String::from_utf8_lossy(&hashed.stderr).trim()
                )
                .into());
            }
            input.push_str("\nhash:");
            input.push_str(String::from_utf8_lossy(&hashed.stdout).trim());
        } else {
            input.push_str("\ndeleted");
        }
    }
    Ok(input_hash(&input))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationMode {
    Changed,
    All,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Passed,
    Failed,
    TimedOut,
    DryRun,
    /// Selected and reported, but deliberately not executed -- today only
    /// because `workflow.repo_checks_enabled` is off. Never counts as passing.
    Skipped,
    /// Issue #268: the check *proves nothing either way* -- a test runner
    /// that crashed before printing a summary, an empty selection, a command
    /// that could not be found, or (see `run_mode`) no checks at all being
    /// configured or discoverable. Distinct from `Failed`: a `Failed` check
    /// is evidence something is broken; `Inconclusive` is the absence of
    /// evidence, which must block a gate exactly as hard as `Failed` (see
    /// `VerificationReport::passed`/`evaluate_against_baseline`) but is
    /// reported and, per `run_baseline`, never eligible for baselining.
    Inconclusive,
}

/// Why one [`CheckResult`] came back `Inconclusive` (issue #268). Set only
/// when `status == CheckStatus::Inconclusive`; see `CheckResult::
/// inconclusive_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InconclusiveReason {
    /// The check's command could not be spawned, or exited in a way that
    /// looks like the shell never found it (`exit code 127`, or a
    /// "command not found"/"not recognized" message) -- see
    /// `looks_like_tool_missing`.
    ToolMissing,
    /// A `cargo test`/`cargo nextest run`-shaped `Unit` check exited
    /// non-zero without printing a single parseable summary line -- the
    /// STATUS_ACCESS_VIOLATION trap this issue exists to close: no
    /// `test result:`/`Summary [...]` line means the runner never finished
    /// reporting, not that nothing failed. See `classify_test_report`.
    RunnerCrashed,
    /// A `cargo test`/`cargo nextest run`-shaped `Unit` check's summary line
    /// declared zero tests run with a successful exit -- an empty filter or
    /// a selection that matched nothing, not evidence the change set is
    /// clean.
    NoTestsSelected,
    /// A `cargo test`/`cargo nextest run`-shaped `Unit` check exited zero
    /// but no summary line could be found at all -- reserved for a
    /// well-formed-but-empty output that is neither a crash (exit succeeded)
    /// nor a recognizable summary.
    ReportUnparseable,
    /// The check's own process-level timeout fired. Reserved for parity with
    /// the design's reason list; `CheckStatus::TimedOut` already carries
    /// this distinctly and continues to block every gate exactly like
    /// `Inconclusive` does, so nothing in this module currently emits
    /// `Inconclusive` with this reason.
    Timeout,
    /// `run_mode` found zero verification checks configured or
    /// discoverable at all (an empty/absent `verify.toml` with nothing
    /// else to fall back on) and no operator override
    /// (`workflow.allow_empty_verify`) was set.
    NoChecks,
}

impl InconclusiveReason {
    /// The kebab-case spelling this reason serializes as -- reused for the
    /// operator-facing `proves:`/`fix:` announcement (`gate_announcement`)
    /// so the two never drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolMissing => "tool-missing",
            Self::RunnerCrashed => "runner-crashed",
            Self::NoTestsSelected => "no-tests-selected",
            Self::ReportUnparseable => "report-unparseable",
            Self::Timeout => "timeout",
            Self::NoChecks => "no-checks",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub id: String,
    pub kind: CheckKind,
    pub command: String,
    #[serde(default = "default_check_source")]
    pub source: CheckSource,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_output: Option<String>,
    /// Failing test names recognized *while the check's output streamed in*,
    /// via [`FailureNameScanner`] -- independent of `failure_output`, which
    /// is only ever a capped display tail (see `MAX_FAILURE_OUTPUT_BYTES`)
    /// that a large enough amount of *later* output (a subprocess inheriting
    /// the real stdout fd, say) can evict the summary from entirely, even
    /// though the summary was seen during capture. `#[serde(default)]` so a
    /// report persisted before this field existed still deserializes, with
    /// callers falling back to parsing `failure_output` text exactly as they
    /// did before -- see `evaluate_against_baseline` and `run_baseline`.
    #[serde(default)]
    pub failure_test_names: Vec<String>,
    /// Set exactly when `status == CheckStatus::Inconclusive` (issue #268).
    /// `#[serde(default)]` so a report persisted before this field existed
    /// still deserializes -- it never had an `Inconclusive` check to begin
    /// with, so `None` is exactly right, not a lossy fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_reason: Option<InconclusiveReason>,
}

fn default_check_source() -> CheckSource {
    CheckSource::RepoConfig
}

/// The three-valued verdict issue #268 asks for, everywhere a gate is
/// evaluated: `Pass` only when every check in the report passed; `Fail` when
/// at least one check is a genuine, evidenced failure; `Inconclusive` when
/// at least one check proves nothing either way, which blocks a gate exactly
/// like `Fail` (see `VerificationReport::passed`) but is announced
/// differently (see `gate_announcement`) and is never eligible for baseline
/// waiver or recording (see `evaluate_against_baseline`, `run_baseline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Pass,
    Fail,
    Inconclusive(InconclusiveReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema_version: u32,
    pub id: String,
    pub mode: VerificationMode,
    pub source: String,
    pub repo: PathBuf,
    pub change_fingerprint: u64,
    pub changed_paths: Vec<PathBuf>,
    pub fallback_to_full: bool,
    /// The `--check` ids this run was narrowed to. A narrowed run is not
    /// completion evidence for the whole change set -- see
    /// [`latest_is_fresh_and_passing`], which used to accept a format-only run
    /// as a satisfied gate.
    #[serde(default)]
    pub narrowed_to: Vec<String>,
    /// Clamps, truncations, and skips applied to this run.
    #[serde(default)]
    pub notes: Vec<String>,
    pub started_at: u64,
    pub finished_at: u64,
    pub checks: Vec<CheckResult>,
}

impl VerificationReport {
    pub fn passed(&self) -> bool {
        !self.checks.is_empty()
            && self
                .checks
                .iter()
                .all(|check| check.status == CheckStatus::Passed)
    }

    /// The report's three-valued [`GateOutcome`] (issue #268), ranked with
    /// `Fail` above `Inconclusive` above `Pass`: a genuinely `Failed` check
    /// -- real evidence something is broken -- always wins over an
    /// `Inconclusive` one on the same run, even though both block the gate
    /// identically (see `passed`). A run that is *only* ever unreliable (no
    /// `Failed` check at all) reports the first `Inconclusive` check's
    /// reason; announcing that reason as if it were the *whole* story when
    /// a real failure sits alongside it would say "proves nothing" about a
    /// run that, in fact, proved something broke -- see `gate_announcement`,
    /// which still lists any accompanying `Inconclusive` checks by id even
    /// when the overall outcome is `Fail`.
    pub fn outcome(&self) -> GateOutcome {
        if self
            .checks
            .iter()
            .any(|check| check.status == CheckStatus::Failed)
        {
            return GateOutcome::Fail;
        }
        if let Some(check) = self
            .checks
            .iter()
            .find(|check| check.status == CheckStatus::Inconclusive)
        {
            return GateOutcome::Inconclusive(
                check
                    .inconclusive_reason
                    .unwrap_or(InconclusiveReason::ReportUnparseable),
            );
        }
        if self.passed() {
            GateOutcome::Pass
        } else {
            GateOutcome::Fail
        }
    }

    /// Whether this (already-failing) report's failures are covered by an
    /// operator-recorded [`TestBaseline`] -- see issue #215. A baseline can
    /// only ever waive a *named* test failure on a `Unit`-kind check parsed
    /// out of that check's own captured output; a `Format`/`Lint`/`Build`/
    /// `Typecheck`/`Custom` check that isn't `Passed`, a `Unit` check whose
    /// status isn't exactly `Failed` (a `TimedOut`/`Skipped`/`DryRun` check
    /// names no individual test), or a `Failed` `Unit` check whose output
    /// yields no parseable names, always blocks the gate outright: none of
    /// those describe a specific known failure the operator could have
    /// looked at and chosen to baseline. `passed()` itself is untouched by
    /// any of this -- this is a second, weaker gate the caller falls back to
    /// only once `passed()` has already said no.
    pub fn evaluate_against_baseline(&self, baseline: Option<&TestBaseline>) -> BaselineEvaluation {
        if self.passed() {
            return BaselineEvaluation {
                gate_passed: true,
                waived: Vec::new(),
                blocking: Vec::new(),
            };
        }
        if self.checks.is_empty() {
            return BaselineEvaluation {
                gate_passed: false,
                waived: Vec::new(),
                blocking: vec!["no checks were run".to_string()],
            };
        }
        let baseline_names: std::collections::BTreeSet<&str> = baseline
            .map(|baseline| baseline.failing_tests.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let mut waived = std::collections::BTreeSet::new();
        let mut blocking = Vec::new();
        for check in &self.checks {
            if check.status == CheckStatus::Passed {
                continue;
            }
            if check.status != CheckStatus::Failed || check.kind != CheckKind::Unit {
                blocking.push(format!("{} ({:?})", check.id, check.status));
                continue;
            }
            let names = failure_names_for(check);
            if names.is_empty() {
                blocking.push(format!(
                    "{} (failed, but no individual test names could be parsed from its output)",
                    check.id
                ));
                continue;
            }
            let mut new_names = Vec::new();
            for name in names {
                if baseline_names.contains(name.as_str()) {
                    waived.insert(name);
                } else {
                    new_names.push(name);
                }
            }
            if !new_names.is_empty() {
                blocking.push(format!(
                    "{}: new failing test(s) not in the recorded baseline: {}",
                    check.id,
                    new_names.join(", ")
                ));
            }
        }
        BaselineEvaluation {
            gate_passed: blocking.is_empty(),
            waived: waived.into_iter().collect(),
            blocking,
        }
    }
}

/// The result of weighing a failing [`VerificationReport`] against an
/// operator's recorded [`TestBaseline`]. `waived` is only ever non-empty when
/// `gate_passed` is true; `blocking` explains, per check, why the gate stayed
/// closed when it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaselineEvaluation {
    pub gate_passed: bool,
    pub waived: Vec<String>,
    pub blocking: Vec<String>,
}

/// Extracts the sorted, deduplicated set of failing test names from cargo
/// test output. Cargo prints a `failures:` section twice per test binary when
/// run `--verbose`: first followed by each failing test's own `----  <name>
/// stdout ----` dump, then again -- immediately before the `test result:
/// FAILED` line -- followed by nothing but the bare, indented names. Rather
/// than trying to tell those two sections apart by shape (a panic message
/// inside the first section is indented exactly like a name line in the
/// second), this walks backward from each `test result: FAILED` line to the
/// *nearest* preceding `failures:` line, which is always the plain name list.
/// A multi-binary `cargo test` run repeats this pattern once per binary, and
/// every occurrence is unioned into one set. Output is only ever a capped
/// tail (see `MAX_FAILURE_OUTPUT_BYTES`), so an early binary's failures can
/// still be missing here even when they are present in the real, uncapped
/// log -- this is best-effort against whatever text survived the cap, not a
/// guarantee every failing binary is found.
/// The failing test names for one already-`Failed` `Unit` check: the names
/// [`FailureNameScanner`] recognized while the check's own output streamed
/// in, when there are any, since those survive a display-tail eviction that
/// [`parse_cargo_test_failure_names`] cannot see past (issue #215's Windows
/// follow-up -- a real run's capped `failure_output` held no `failures:`
/// text at all, even though the failing test's own summary had been printed
/// well before the cap-evicting flood that followed it). Falls back to
/// parsing `failure_output` text for a report persisted before
/// `failure_test_names` existed, or for a `dry_run` check that was never
/// actually executed and so was never scanned.
fn failure_names_for(check: &CheckResult) -> std::collections::BTreeSet<String> {
    if !check.failure_test_names.is_empty() {
        return check.failure_test_names.iter().cloned().collect();
    }
    check
        .failure_output
        .as_deref()
        .map(parse_cargo_test_failure_names)
        .unwrap_or_default()
}

/// Whether `command` invokes one of the two test runners
/// [`classify_test_report`] knows how to read a well-formedness verdict out
/// of. Scoped deliberately narrow (issue #268): an arbitrary `Unit` check
/// (`npm run test`, say) does not print either runner's summary shape, and
/// misclassifying its ordinary output as `report-unparseable` would turn a
/// real pass into a false `Inconclusive`.
fn is_cargo_test_runner(command: &str) -> bool {
    let command = command.trim_start();
    command.starts_with("cargo test") || command.starts_with("cargo nextest")
}

/// A cheap, pure signal (issue #268's "degraded-gate ban") that a check's own
/// command could not actually be run: the shell's own "command not found"
/// convention (POSIX exit code 127) or message, or Windows `cmd.exe`'s own
/// wording. Deliberately heuristic text matching -- there is no portable way
/// to ask a shell why its child failed -- so it only ever *adds* an
/// `Inconclusive` classification on top of what would otherwise have been
/// `Failed`, never removes one.
fn looks_like_tool_missing(exit_code: Option<i32>, output: &str) -> bool {
    if exit_code == Some(127) {
        return true;
    }
    let lower = output.to_ascii_lowercase();
    lower.contains("command not found")
        || lower.contains("is not recognized as an internal or external command")
}

/// Extracts the total test count declared by a `cargo test`/`cargo nextest
/// run` summary line, if the output contains at least one well-formed one --
/// the well-formedness check [`classify_test_report`] is built on. Sums
/// across every such line found (a multi-binary `cargo test` run prints one
/// `test result:` line per binary, never one combined total), so this is
/// "how many test outcomes did the runner report finishing", not "how many
/// binaries ran". `None` means no summary line was found at all.
///
/// `#[cfg(test)]`: production code (`run_check`) gets this same total from
/// `FailureNameScanner`'s full-stream `summary_seen`/`summary_total`
/// instead, never from a piece of already-capped display text -- see
/// `classify_test_outcome`'s own doc comment for why. This function (and
/// [`classify_test_report`] below) exist only so the decision table has a
/// pure, fixture-driven entry point for tests.
#[cfg(test)]
fn extract_test_total(output: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut found = false;
    for line in output.lines() {
        if let Some(count) = parse_cargo_test_result_line(line) {
            total += count;
            found = true;
        } else if let Some(count) = parse_nextest_summary_line(line) {
            total += count;
            found = true;
        }
    }
    found.then_some(total)
}

/// Parses `test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0
/// filtered out; finished in 0.00s` (or the `FAILED` spelling) into
/// `passed + failed`, the count of tests the binary actually reported an
/// outcome for. `None` when the line is not this exact shape -- callers
/// treat that as "not this kind of summary line", not as an error.
fn parse_cargo_test_result_line(line: &str) -> Option<u64> {
    let rest = line.trim().strip_prefix("test result: ")?;
    let (_, after_dot) = rest.split_once(". ")?;
    let mut fields = after_dot.split(';');
    let passed = parse_count_field(fields.next()?, "passed")?;
    let failed = parse_count_field(fields.next()?, "failed")?;
    Some(passed + failed)
}

/// Parses one `; `-delimited field of a `cargo test` summary line, e.g.
/// `" 3 passed"` with `label = "passed"`, into its leading count. `None` when
/// the field's label does not match, which is how callers detect the line is
/// not the shape they expected rather than mis-summing an unrelated number.
fn parse_count_field(field: &str, label: &str) -> Option<u64> {
    let (count, name) = field.trim().split_once(' ')?;
    if name.trim() != label {
        return None;
    }
    count.parse().ok()
}

/// Parses a `cargo nextest run` human-readable `Summary [   0.12s] 3 tests
/// run: 3 passed, 0 skipped` line (or the zero-tests/failing spellings) into
/// the leading "N tests run" count. `None` when the line is not this shape.
fn parse_nextest_summary_line(line: &str) -> Option<u64> {
    let trimmed = line.trim();
    let after_summary = trimmed.strip_prefix("Summary ")?;
    let close = after_summary.find(']')?;
    let after_bracket = after_summary[close + 1..].trim_start();
    let mut parts = after_bracket.split_whitespace();
    let count = parts.next()?;
    let next = parts.next()?;
    if !next.starts_with("test") {
        return None;
    }
    count.parse().ok()
}

/// The pure runner-report classifier issue #268 asks for: whether a
/// `cargo test`/`cargo nextest run`-shaped `Unit` check's captured output is
/// well-formed enough to trust `exit_success` and any parsed failure names
/// at all. `None` means "well-formed, proceed with the ordinary exit-code
/// verdict"; `Some(reason)` means the check must be reported `Inconclusive`
/// instead, regardless of what its exit code said.
///
/// A summary line declaring zero tests run is `NoTestsSelected` on a
/// successful exit (an empty filter or a selection matching nothing) but
/// `RunnerCrashed` on a failing one (something died before selecting any
/// test, which a `0 tests run` line does not normally accompany). No summary
/// line at all is `RunnerCrashed` on a failing exit -- the
/// STATUS_ACCESS_VIOLATION trap this issue exists to close, where a crashed
/// runner leaves no `test result:`/`Summary [...]` line behind for
/// [`parse_cargo_test_failure_names`] to find, and an empty failure-name set
/// used to read as a clean pass -- or `ReportUnparseable` on a successful
/// one.
///
/// `#[cfg(test)]`: exists so this decision table has a pure, text-in
/// entry point fixture files can exercise directly. `run_check` calls
/// [`classify_test_outcome`] instead, with a total taken from the check's
/// complete, uncapped output -- text handed to *this* function can
/// legitimately have lost its summary line to display-buffer capping (see
/// `classify_test_outcome`'s own doc comment), which is exactly the bug a
/// caller relying on this function for production classification would
/// have reintroduced.
#[cfg(test)]
fn classify_test_report(output: &str, exit_success: bool) -> Option<InconclusiveReason> {
    classify_test_outcome(extract_test_total(output), exit_success)
}

/// The actual decision table behind [`classify_test_report`], taking the
/// already-extracted total directly rather than re-deriving it from text.
/// `run_check` calls this with a total computed from the check's *complete*
/// uncapped output (see `FailureNameScanner`'s `summary_total`/
/// `summary_seen`, fed during capture exactly like failing test names
/// already are) -- never from the capped *display* buffer, which two
/// independent 16 KiB caps (`read_capped_tail_and_scan`'s own tail cap, then
/// `run_check`'s second cap on the stdout+stderr concatenation) can leave
/// with no summary line at all even though the runner printed one. Doing
/// this classification against that doubly-capped text let a `cargo test
/// --verbose` run's compiler noise alone misreport a real pass as
/// `ReportUnparseable` or a real failure as `RunnerCrashed`, which the
/// mandated `--verbose` gate made likely rather than theoretical.
fn classify_test_outcome(total: Option<u64>, exit_success: bool) -> Option<InconclusiveReason> {
    match total {
        Some(0) if exit_success => Some(InconclusiveReason::NoTestsSelected),
        Some(0) => Some(InconclusiveReason::RunnerCrashed),
        Some(_) => None,
        None if exit_success => Some(InconclusiveReason::ReportUnparseable),
        None => Some(InconclusiveReason::RunnerCrashed),
    }
}

/// Applies both of `run_check`'s post-hoc `Inconclusive` reclassifications
/// (issue #268) to an otherwise-final `(status, exit_code)`: a tool the
/// shell could not find, then -- only for a `cargo test`/`cargo nextest
/// run`-shaped `Unit` check -- [`classify_test_outcome`], fed
/// `test_summary_total` from the check's complete, uncapped output (see that
/// function's own doc comment for why the capped display text must never be
/// used here). `output` itself is only ever consulted for the tool-missing
/// heuristic, which is fine against the capped text: a "command not
/// found"/exit-127 signal shows up early and reliably, unlike a summary line
/// a large enough later flood can push out of a capped tail. Never touches
/// `CheckStatus::DryRun`/`Skipped`/`TimedOut`: a timeout already blocks every
/// gate exactly as hard as `Inconclusive` does (see `CheckStatus::
/// Inconclusive`'s own doc comment), and dry-run/skipped checks were never
/// actually executed, so there is no output to classify.
fn classify_gate_status(
    status: CheckStatus,
    exit_code: Option<i32>,
    output: &str,
    is_test_runner_check: bool,
    test_summary_total: Option<u64>,
) -> (CheckStatus, Option<InconclusiveReason>) {
    if status == CheckStatus::Failed && looks_like_tool_missing(exit_code, output) {
        return (
            CheckStatus::Inconclusive,
            Some(InconclusiveReason::ToolMissing),
        );
    }
    if is_test_runner_check
        && matches!(status, CheckStatus::Passed | CheckStatus::Failed)
        && let Some(reason) =
            classify_test_outcome(test_summary_total, status == CheckStatus::Passed)
    {
        return (CheckStatus::Inconclusive, Some(reason));
    }
    (status, None)
}

fn parse_cargo_test_failure_names(output: &str) -> std::collections::BTreeSet<String> {
    let lines: Vec<&str> = output.lines().collect();
    let mut names = std::collections::BTreeSet::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.contains("test result: FAILED") {
            continue;
        }
        let Some(header) = lines[..index]
            .iter()
            .rposition(|line| line.trim() == "failures:")
        else {
            continue;
        };
        for candidate in &lines[header + 1..index] {
            let trimmed = candidate.trim();
            if trimmed.is_empty() || trimmed.starts_with("----") {
                continue;
            }
            names.insert(trimmed.to_string());
        }
    }
    names
}

/// The streaming counterpart to [`parse_cargo_test_failure_names`]: applies
/// the identical `failures:` -> nearest-following `test result: FAILED`
/// matching rule, but incrementally, one line at a time, as a check's output
/// arrives -- rather than against whatever text happens to survive
/// `read_capped_tail`'s cap. A capped *display* tail is fine to lose an
/// early binary's failure summary to a large enough later flood (this is
/// still `MAX_FAILURE_OUTPUT_BYTES`-bounded, not a full second uncapped
/// buffer); it is not fine for that flood to also erase the operator's only
/// way of learning the failing test's *name*, which is exactly what a real
/// Windows run of `zirv test baseline` hit: a check's own stdout fd was
/// inherited by a later-spawned subprocess whose own output dwarfed the
/// 16 KiB tail, leaving zero bytes of `failures:`/`test result: FAILED`
/// text behind for [`parse_cargo_test_failure_names`] to find.
///
/// `pending` only ever holds the lines seen since the *most recently
/// observed* `failures:` line -- reset on every new one -- so memory stays
/// bounded by the shape of one such section, never by total output size;
/// `MAX_PENDING_LINES` is a defensive backstop against a pathological
/// producer that never resets it. `pending` is only ever drained into
/// `names` on a `test result: FAILED` line that was itself preceded by a
/// `failures:` line since the last such drain/reset (`saw_failures_header`)
/// -- otherwise a `test result: FAILED` block that never printed a
/// `failures:` header (nothing captured, or a differently-shaped tool's
/// output) would wrongly promote unrelated non-failure lines it happened to
/// see into failing test names.
///
/// `partial` -- the not-yet-newline-terminated tail of the current line --
/// is bounded the same way: `MAX_PARTIAL_LINE_BYTES` is far larger than any
/// real cargo `failures:`/`test result:`/test-name line, so once it grows
/// past that a real line can never be hiding in it. The excess is discarded
/// and `partial_overflowed` marks the rest of that (poisoned) line as
/// ignorable up to its next newline, rather than buffering an unbounded
/// amount of a single newline-less stream -- which would otherwise defeat
/// the memory cap the capped *display* tail is meant to provide.
///
/// Also recognizes the `cargo test`/`cargo nextest run` summary line itself
/// (`summary_seen`/`summary_total`, fed by `parse_cargo_test_result_line`/
/// `parse_nextest_summary_line`, same as `extract_test_total`) -- for the
/// identical reason names are recovered from the full stream rather than
/// the capped display tail: `run_check` concatenates the stdout and stderr
/// tails and caps the result a *second* time, and verbose compiler noise on
/// either stream can push the entire summary line out of that second cap
/// even though both per-stream tails individually held it. Classifying off
/// that doubly-capped text let a real pass or failure misreport as
/// `Inconclusive`; `classify_gate_status` now uses `summary_seen`/
/// `summary_total` from this full-stream scan instead.
#[derive(Default)]
struct FailureNameScanner {
    names: std::collections::BTreeSet<String>,
    pending: Vec<String>,
    partial: Vec<u8>,
    partial_overflowed: bool,
    saw_failures_header: bool,
    summary_seen: bool,
    summary_total: u64,
}

impl FailureNameScanner {
    const MAX_PENDING_LINES: usize = 4096;
    /// Far longer than any real `failures:`/`test result:`/test-name line
    /// cargo (or any other supported check runner) ever emits -- once an
    /// unterminated line exceeds this, it cannot be one of those lines, so
    /// it is safe to discard rather than accumulate without bound.
    const MAX_PARTIAL_LINE_BYTES: usize = 4096;

    fn feed(&mut self, chunk: &[u8]) {
        self.partial.extend_from_slice(chunk);
        while let Some(newline) = self.partial.iter().position(|&byte| byte == b'\n') {
            let line_bytes: Vec<u8> = self.partial.drain(..=newline).collect();
            if std::mem::take(&mut self.partial_overflowed) {
                // The line that just ended was already discarded as
                // over-length; only the newline itself resynced us.
                continue;
            }
            let line = String::from_utf8_lossy(&line_bytes);
            self.observe_line(line.trim_end_matches(['\n', '\r']));
        }
        if self.partial.len() > Self::MAX_PARTIAL_LINE_BYTES {
            // No newline showed up before the buffer grew past what any
            // real line of interest could be. Drop it -- keeping only the
            // fact that a poisoned, still-unterminated line is in flight --
            // rather than let a newline-less stream grow this without
            // bound for the life of the check.
            self.partial.clear();
            self.partial_overflowed = true;
        }
    }

    fn observe_line(&mut self, line: &str) {
        // Independent of the `failures:`/pending-name state machine below:
        // a summary line is a summary line regardless of what surrounds it,
        // so this never interferes with (and is never interfered with by)
        // the existing name-recovery branches.
        if let Some(count) =
            parse_cargo_test_result_line(line).or_else(|| parse_nextest_summary_line(line))
        {
            self.summary_seen = true;
            self.summary_total = self.summary_total.saturating_add(count);
        }
        let trimmed = line.trim();
        if trimmed == "failures:" {
            self.pending.clear();
            self.saw_failures_header = true;
            return;
        }
        if line.contains("test result: FAILED") {
            if self.saw_failures_header {
                self.names.extend(self.pending.drain(..));
            } else {
                // No `failures:` header was observed since the last reset,
                // so nothing in `pending` is trustworthy as a failing test
                // name -- discard it rather than draining it into `names`.
                self.pending.clear();
            }
            self.saw_failures_header = false;
            return;
        }
        if trimmed.is_empty() || trimmed.starts_with("----") {
            return;
        }
        if self.pending.len() < Self::MAX_PENDING_LINES {
            self.pending.push(trimmed.to_string());
        }
    }

    /// Consumes the scanner, flushing any final unterminated line (a stream
    /// that ends without a trailing newline) before returning the names,
    /// plus whether a summary line was seen anywhere in the full stream and
    /// the running total it declared (summed across every such line found,
    /// exactly like `extract_test_total` -- a multi-binary `cargo test` run
    /// prints one `test result:` line per binary, never one combined total).
    fn finish(mut self) -> (std::collections::BTreeSet<String>, bool, u64) {
        if !self.partial.is_empty() && !self.partial_overflowed {
            let line = String::from_utf8_lossy(&self.partial).into_owned();
            self.observe_line(line.trim_end_matches(['\n', '\r']));
        }
        (self.names, self.summary_seen, self.summary_total)
    }
}

/// An operator-owned record of failing test names that are already known
/// about for one repository, stored under `~/.zirv/test-baseline/` -- never
/// under `<repo>/.zirv/`, which is untrusted checkout content that may only
/// narrow what a repository can do (see `CLAUDE.md`'s "Repo-owned surfaces"
/// rule and `crate::commands::ctx::config::CtxConfig::load`'s identical
/// `~/.zirv/ctx.toml`-then-repo-layer convention). A repository can never
/// read, write, or widen this file: it lives outside the checkout entirely,
/// keyed by [`repo_slug`], and is only ever written by the explicit
/// `zirv test baseline` operator action -- see [`save_baseline`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestBaseline {
    pub schema_version: u32,
    /// Sorted, deduplicated failing test names as of `recorded_at`.
    #[serde(default)]
    pub failing_tests: Vec<String>,
    pub recorded_at: u64,
    /// Issue #268's baseline hygiene: how many consecutive non-dry-run
    /// evaluations in a row each still-baselined name has gone unseen among
    /// this repository's own failures (see `update_baseline_after_run`).
    /// Reset to 0 the moment a name is seen failing again; a name reaching
    /// `PRUNE_AFTER_GREENS` becomes eligible for `zirv test baseline
    /// --prune` (`run_baseline_prune`). `#[serde(default)]` so a baseline
    /// recorded before this field existed still deserializes, with every
    /// name starting at 0 greens rather than failing to load.
    #[serde(default)]
    pub green_streaks: std::collections::BTreeMap<String, u32>,
}

const TEST_BASELINE_SCHEMA_VERSION: u32 = 1;

/// How many consecutive green evaluations a baselined name needs before
/// `zirv test baseline --prune` will drop it. Not yet operator-configurable
/// (the issue's own `test.prune_after_greens` key is left for a follow-up)
/// -- a fixed default of 3, matching the issue's own default, until that
/// lands.
const PRUNE_AFTER_GREENS: u32 = 3;

fn test_baseline_dir() -> CtxResult<PathBuf> {
    Ok(crate::utils::home_dir()?
        .join(crate::utils::SCRIPT_DIR_NAME)
        .join("test-baseline"))
}

fn test_baseline_path(repo: &Path) -> CtxResult<PathBuf> {
    Ok(test_baseline_dir()?.join(format!("{}.json", repo_slug(repo))))
}

/// Loads the operator's recorded baseline for `repo`, or `None` when nothing
/// has ever been recorded. Never `Err` on a plain "not there yet" -- only a
/// genuine I/O failure or an unreadable/future schema propagates, and even
/// then callers on the gate path (see `latest_is_fresh_and_passing`) treat
/// that the same as "no baseline" rather than letting a broken baseline file
/// brick every gate check: current strict behavior (any failure closes the
/// gate) is exactly what "no baseline" already means.
pub fn load_baseline(repo: &Path) -> CtxResult<Option<TestBaseline>> {
    let path = test_baseline_path(repo)?;
    if !path.exists() {
        return Ok(None);
    }
    let baseline: TestBaseline = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    if baseline.schema_version != TEST_BASELINE_SCHEMA_VERSION {
        return Err(format!(
            "test baseline '{}': unsupported schema_version {}",
            path.display(),
            baseline.schema_version
        )
        .into());
    }
    Ok(Some(baseline))
}

/// Records (or overwrites) the operator's baseline for `repo` with exactly
/// `failing_tests` -- always an explicit operator action (`zirv test
/// baseline`), never a side effect of an ordinary `zirv test`/`zirv verify`
/// run, so a baseline only ever reflects failures an operator has actually
/// looked at and chosen to waive.
pub fn save_baseline(
    repo: &Path,
    failing_tests: std::collections::BTreeSet<String>,
) -> CtxResult<TestBaseline> {
    // Carries forward each still-baselined name's own green streak (from
    // ordinary `zirv test changed`/`zirv verify` evaluations, see
    // `update_baseline_after_run`) rather than resetting it just because the
    // operator re-ran `zirv test baseline` -- a fresh full recompute is not
    // itself evidence the name regressed. A name absent from the previous
    // baseline (newly recorded) starts at 0, same as before this field
    // existed.
    let previous = load_baseline(repo).unwrap_or(None);
    let green_streaks = failing_tests
        .iter()
        .filter_map(|name| {
            previous
                .as_ref()
                .and_then(|baseline| baseline.green_streaks.get(name))
                .map(|streak| (name.clone(), *streak))
        })
        .collect();
    let baseline = TestBaseline {
        schema_version: TEST_BASELINE_SCHEMA_VERSION,
        failing_tests: failing_tests.into_iter().collect(),
        recorded_at: now_secs(),
        green_streaks,
    };
    write_baseline(repo, &baseline)?;
    Ok(baseline)
}

fn write_baseline(repo: &Path, baseline: &TestBaseline) -> CtxResult<()> {
    create_private_dir_all(&test_baseline_dir()?)?;
    write_private(
        &test_baseline_path(repo)?,
        &serde_json::to_string_pretty(baseline)?,
    )?;
    Ok(())
}

/// Issue #268's baseline-hygiene half: after every non-dry-run evaluation
/// (`zirv test changed`/`zirv test all`/`zirv verify`, and `zirv test
/// baseline` itself before it overwrites the file), advances each
/// still-baselined name's `green_streaks` counter when this run's evidence
/// says it is clean, or resets it to 0 the moment it is seen failing again --
/// so a name only ever earns `zirv test baseline --prune` eligibility from
/// repeated, real green evidence, never from a single lucky run or a run
/// that never even exercised it.
///
/// Does nothing (returns `None`, touches no file) when there is no baseline
/// yet, the baseline is empty, this run selected no `Unit`, non-repo-supplied
/// check at all (so it has no opinion on any baselined name), or any such
/// check came back `Inconclusive`/`TimedOut` -- an unreliable run must never
/// advance a streak on a guess. Only ever ingests failing names from the
/// same check sources `run_baseline` itself trusts (never `RepoConfig`/
/// `DiscoveredScript`), so a repository-authored check can neither manufacture
/// nor erase prune eligibility for a name it does not own.
///
/// Returns an operator-facing note naming how many entries just became
/// prune-eligible, if any -- the `zirv test changed` hint the issue's design
/// asks for.
fn update_baseline_after_run(repo: &Path, report: &VerificationReport) -> Option<String> {
    let mut baseline = load_baseline(repo).ok().flatten()?;
    if baseline.failing_tests.is_empty() {
        return None;
    }
    let unit_checks: Vec<&CheckResult> = report
        .checks
        .iter()
        .filter(|check| check.kind == CheckKind::Unit && !check.source.repo_supplied())
        .collect();
    if unit_checks.is_empty()
        || unit_checks.iter().any(|check| {
            matches!(
                check.status,
                CheckStatus::Inconclusive | CheckStatus::TimedOut
            )
        })
    {
        return None;
    }
    let observed_failing: std::collections::BTreeSet<String> = unit_checks
        .iter()
        .filter(|check| check.status == CheckStatus::Failed)
        .flat_map(|check| failure_names_for(check))
        .collect();
    let mut eligible = 0usize;
    for name in &baseline.failing_tests {
        let streak = baseline.green_streaks.entry(name.clone()).or_insert(0);
        if observed_failing.contains(name) {
            *streak = 0;
        } else {
            *streak = streak.saturating_add(1);
        }
        if *streak >= PRUNE_AFTER_GREENS {
            eligible += 1;
        }
    }
    write_baseline(repo, &baseline).ok()?;
    (eligible > 0).then(|| {
        let plural = if eligible == 1 { "y" } else { "ies" };
        format!(
            "baseline: {eligible} entr{plural} eligible for prune (run `zirv test baseline \
             --prune`)"
        )
    })
}

fn command_for_shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut value = Command::new("cmd");
        value.args(["/D", "/S", "/C", command]);
        value
    }
    #[cfg(not(windows))]
    {
        let mut value = Command::new("sh");
        value.args(["-c", command]);
        value
    }
}

/// The retained tail (whether the stream ended in a read error rather than
/// at EOF is the second element -- an error read as a clean end silently
/// turned a truncated failure log into a complete-looking one), plus what a
/// [`FailureNameScanner`] recognized in the *full*, uncapped stream as it
/// went by -- the failing test names, whether a `test result:`/`Summary
/// [...]` line was seen at all, and the total it declared -- see that
/// struct's doc comment for why this must happen during capture rather than
/// against the capped tail alone. Every check's output goes through this
/// path (`run_check`); only the retained-tail element is ever a display
/// artifact.
fn read_capped_tail_and_scan(
    mut reader: impl Read,
    cap: usize,
) -> (Vec<u8>, bool, std::collections::BTreeSet<String>, bool, u64) {
    let mut kept = Vec::with_capacity(cap);
    let mut chunk = [0u8; 8192];
    let mut errored = false;
    let mut scanner = FailureNameScanner::default();
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Err(_) => {
                errored = true;
                break;
            }
            Ok(count) => count,
        };
        scanner.feed(&chunk[..count]);
        if count >= cap {
            kept.clear();
            kept.extend_from_slice(&chunk[count - cap..count]);
            continue;
        }
        let overflow = kept.len().saturating_add(count).saturating_sub(cap);
        if overflow > 0 {
            kept.drain(..overflow);
        }
        kept.extend_from_slice(&chunk[..count]);
    }
    let (names, summary_seen, summary_total) = scanner.finish();
    (kept, errored, names, summary_seen, summary_total)
}

/// The last `cap` bytes as text, on a char boundary. `utils::truncate_bytes`
/// keeps the *head*, which throws away the failure tail this whole capped-tail
/// path exists to preserve (lossy UTF-8 replacement can inflate the byte
/// count past the cap, so the head-truncating call was reachable).
fn tail_text(bytes: &[u8], cap: usize) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// Check output is repository-controlled text printed straight to the
/// operator's terminal, where an escape sequence can clear the screen and
/// forge an "all checks passed" summary. Same treatment as `mail.rs` and
/// `wrap.rs` give relayed text, except that `\n`/`\t` stay: a failure log is
/// read as lines.
fn scrub_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' {
            out.push(ch);
            in_run = false;
            continue;
        }
        if ch.is_control() {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
            continue;
        }
        out.push(ch);
        in_run = false;
    }
    out
}

/// `scrub_output` for a field that is one line by construction (a check id, a
/// command, a note). Newlines go too: a `command` string may legally contain
/// one, and a printed newline lets repository text start a line of its own that
/// looks like zirv's.
fn scrub_line(text: &str) -> String {
    scrub_output(text).replace(['\n', '\t'], " ")
}

fn check_result(check: &ResolvedCheck, status: CheckStatus) -> CheckResult {
    CheckResult {
        id: check.spec.id.clone(),
        kind: check.spec.kind,
        command: check.spec.command.clone(),
        source: check.source,
        status,
        exit_code: None,
        duration_ms: 0,
        failure_output: None,
        failure_test_names: Vec::new(),
        inconclusive_reason: None,
    }
}

/// Environment variable names always passed from the zirv process's own
/// environment (or, on macOS only, the login session -- see
/// `launchd_getenv` below) into a verification check child, with no
/// `[workflow] check_env_passthrough` configuration at all. `run_check`
/// already spawns the check with `std::process::Command::new`, which
/// inherits the parent's full environment by default (no `env_clear`/
/// `env_remove` sits between zirv and the check child) -- so on a machine
/// where the zirv process itself has these set, the child already sees
/// them. This list exists as an explicit, tested guarantee for that path
/// rather than an implicit consequence of never having called `env_clear`,
/// and as the seam a future sandboxing change (`Command::env_clear` for
/// stricter check isolation) would have to widen instead of quietly
/// regressing (issue #233: a macOS/Linux desktop session's `ssh-agent`
/// family -- `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `SSH_ASKPASS` -- plus GPG's
/// terminal/homedir pointers -- `GPG_TTY`, `GNUPGHOME` -- so a check that
/// shells out to `ssh`/git-over-ssh/`gpg` (e.g. `gitlab-ci-local`'s
/// remote-variable fetch) passes without a per-command shell workaround).
///
/// The reported case (issue #233) was worse than "zirv's own process has
/// the value": the harness's shell child had no `SSH_AUTH_SOCK` in ITS OWN
/// environment either, and the reporter's working workaround was `export
/// SSH_AUTH_SOCK="$(launchctl getenv SSH_AUTH_SOCK)"`. `launchd_getenv`
/// below is that same command, consulted only when a name in this list is
/// absent from zirv's own process environment, and only on macOS.
const DEFAULT_CHECK_ENV_PASSTHROUGH: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_ASKPASS",
    "GPG_TTY",
    "GNUPGHOME",
];

/// Whether `a` and `b` name the same environment variable. Unix environment
/// blocks are case-sensitive; Windows's is not, so an operator-configured
/// `[workflow] check_env_passthrough` entry that only differs in case from a
/// `DEFAULT_CHECK_ENV_PASSTHROUGH` name must still be treated as the same
/// name there, matching how the OS itself resolves the child's environment.
#[cfg(windows)]
fn env_names_match(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(not(windows))]
fn env_names_match(a: &str, b: &str) -> bool {
    a == b
}

/// `DEFAULT_CHECK_ENV_PASSTHROUGH` plus `extra` (the operator's own
/// `[workflow] check_env_passthrough`, `~/.zirv/ctx.toml`/`ZIRV_CTX_*` only
/// -- REPO_FORBIDDEN, see `config.rs`), deduplicated by [`env_names_match`]
/// so the operator key can only ADD names, never narrow or replace the
/// built-in defaults.
fn resolved_check_env_passthrough(extra: &[String]) -> Vec<String> {
    let mut names: Vec<String> = DEFAULT_CHECK_ENV_PASSTHROUGH
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    for name in extra {
        if !names.iter().any(|existing| env_names_match(existing, name)) {
            names.push(name.clone());
        }
    }
    names
}

/// Where one allowlisted check-env variable's value came from. Only
/// `Launchd` earns the one-line stderr notice `run_check` prints -- a value
/// already sitting in zirv's own process environment needs no explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedCheckEnvValue {
    Process(String),
    Launchd(String),
}

/// Resolution order for one allowlisted name (issue #233): a value already
/// present in zirv's own process environment always wins; only when it is
/// ABSENT there is the macOS login-session fallback consulted, and only a
/// non-empty result from it is used -- otherwise the name stays unset on the
/// check child, exactly as it does today. `process_env`/`launchd_env` are
/// injected so this merge is unit-tested cross-platform with a fake
/// resolver; `run_check` wires the real lookups (`std::env::var` and
/// `launchd_getenv`, which is a no-op on every platform but macOS).
fn resolve_one_check_env_var(
    name: &str,
    process_env: &impl Fn(&str) -> Option<String>,
    launchd_env: &impl Fn(&str) -> Option<String>,
) -> Option<ResolvedCheckEnvValue> {
    if let Some(value) = process_env(name) {
        return Some(ResolvedCheckEnvValue::Process(value));
    }
    match launchd_env(name) {
        Some(value) if !value.trim().is_empty() => Some(ResolvedCheckEnvValue::Launchd(value)),
        _ => None,
    }
}

/// `launchctl getenv <name>`: macOS's per-login-session environment, which
/// is where a variable like `SSH_AUTH_SOCK` actually lives when the zirv
/// process itself was not launched from that session's shell -- the exact
/// gap issue #233 reported, and the exact command the reporter's own working
/// workaround ran by hand. Bounded to a few seconds, no shell, stdin/stderr
/// null; any failure (binary missing, non-zero exit, timeout, unreadable
/// stdout) is silently `None` -- this is a best-effort fallback, never a
/// reason to fail a check.
#[cfg(target_os = "macos")]
fn launchd_getenv(name: &str) -> Option<String> {
    let mut command = Command::new("launchctl");
    command
        .args(["getenv", name])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
    let mut stdout = child.stdout.take()?;
    let mut buf = String::new();
    stdout.read_to_string(&mut buf).ok()?;
    let value = buf.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Every platform but macOS: the login-session fallback does not exist, so
/// this is always `None` and `launchctl` is never invoked.
#[cfg(not(target_os = "macos"))]
fn launchd_getenv(_name: &str) -> Option<String> {
    None
}

fn run_check(
    repo: &Path,
    check: &ResolvedCheck,
    dry_run: bool,
    check_env_passthrough: &[String],
) -> CheckResult {
    if dry_run {
        return check_result(check, CheckStatus::DryRun);
    }
    let check_source = check.source;
    let check = &check.spec;
    let is_test_runner_check =
        check.kind == CheckKind::Unit && is_cargo_test_runner(&check.command);
    let started = Instant::now();
    let mut command = command_for_shell(&check.command);
    super::isolate_process_tree(&mut command);
    command
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Explicit guarantee, not a no-op: see `DEFAULT_CHECK_ENV_PASSTHROUGH`'s
    // own doc comment. Read from zirv's own process environment (then, on
    // macOS only, the login session via `launchd_getenv`) at check time,
    // exactly like every other operator-config-gated env read in this
    // codebase (never a repo-controlled value).
    for name in resolved_check_env_passthrough(check_env_passthrough) {
        let resolved =
            resolve_one_check_env_var(&name, &|key: &str| std::env::var(key).ok(), &|key: &str| {
                launchd_getenv(key)
            });
        match resolved {
            Some(ResolvedCheckEnvValue::Process(value)) => {
                command.env(&name, value);
            }
            Some(ResolvedCheckEnvValue::Launchd(value)) => {
                eprintln!("zirv \u{25b8} verify: {name} taken from the login session (launchctl)");
                command.env(&name, value);
            }
            None => {}
        }
    }
    /// One check's raw run outcome: status, exit code, combined capped
    /// output (display/storage only -- never classification, see
    /// `FailureNameScanner`'s doc comment), the failing test names a
    /// `FailureNameScanner` recognized while that output streamed by, and
    /// whether/what total a `test result:`/`Summary [...]` line declared
    /// anywhere in the *complete* stdout+stderr streams.
    type RawCheckOutcome = (
        CheckStatus,
        Option<i32>,
        Vec<u8>,
        std::collections::BTreeSet<String>,
        Option<u64>,
    );
    let result = (|| -> CtxResult<RawCheckOutcome> {
        let mut child = command.spawn()?;
        let mut job = crate::commands::ctx::supervise::JobGuard::adopt(child.id());
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || {
            stdout
                .map(|stdout| read_capped_tail_and_scan(stdout, MAX_FAILURE_OUTPUT_BYTES))
                .unwrap_or_default()
        });
        let stderr_thread = std::thread::spawn(move || {
            stderr
                .map(|stderr| read_capped_tail_and_scan(stderr, MAX_FAILURE_OUTPUT_BYTES))
                .unwrap_or_default()
        });

        let timeout = Duration::from_secs(check.timeout_secs);
        let (status, code) = loop {
            if let Some(status) = child.try_wait()? {
                super::terminate_process_tree(&mut child)?;
                job.close();
                break (
                    if status.success() {
                        CheckStatus::Passed
                    } else {
                        CheckStatus::Failed
                    },
                    status.code(),
                );
            }
            if started.elapsed() >= timeout {
                super::terminate_process_tree(&mut child)?;
                job.close();
                break (CheckStatus::TimedOut, None);
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let (mut output, mut errored, mut names, stdout_summary_seen, stdout_summary_total) =
            stdout_thread.join().unwrap_or_default();
        let (
            stderr_output,
            stderr_errored,
            stderr_names,
            stderr_summary_seen,
            stderr_summary_total,
        ) = stderr_thread.join().unwrap_or_default();
        output.extend(stderr_output);
        errored |= stderr_errored;
        names.extend(stderr_names);
        // Full-stream totals, never the capped `output` buffer below: a
        // `cargo test`/`cargo nextest run` summary line survives here even
        // when the doubly-capped display buffer (per-stream tail, then a
        // second cap on the stdout+stderr concatenation) has lost it to a
        // large enough flood of noise on either stream -- see
        // `classify_test_outcome`'s own doc comment.
        let summary_seen = stdout_summary_seen || stderr_summary_seen;
        let summary_total =
            summary_seen.then(|| stdout_summary_total.saturating_add(stderr_summary_total));
        if output.len() > MAX_FAILURE_OUTPUT_BYTES {
            output.drain(..output.len() - MAX_FAILURE_OUTPUT_BYTES);
        }
        if errored {
            output.extend_from_slice(b"\n[output stream ended in a read error]");
        }
        Ok((status, code, output, names, summary_total))
    })();

    let (status, exit_code, output, failure_test_names, test_summary_total, spawn_tool_missing) =
        match result {
            Ok((status, code, output, names, summary_total)) => {
                (status, code, output, names, summary_total, false)
            }
            Err(err) => {
                let missing = is_spawn_not_found(err.as_ref());
                (
                    CheckStatus::Failed,
                    None,
                    err.to_string().into_bytes(),
                    std::collections::BTreeSet::new(),
                    None,
                    missing,
                )
            }
        };
    let failure_output = (status != CheckStatus::Passed)
        .then(|| scrub_output(&tail_text(&output, MAX_FAILURE_OUTPUT_BYTES)));
    // Scrubbed the same way `failure_output` is: a name recognized inside
    // repository-controlled text is still repository-controlled text.
    let failure_test_names: Vec<String> = if status == CheckStatus::Passed {
        Vec::new()
    } else {
        failure_test_names
            .into_iter()
            .map(|name| scrub_line(&name))
            .collect()
    };
    // Issue #268: never trust a bare exit code alone for a test runner. A
    // literal spawn failure (the shell binary itself missing -- vanishingly
    // rare) and a shell-reported "command not found" (the command inside it
    // missing -- the realistic case for a misconfigured `verify.toml`) both
    // become `Inconclusive`, and so does a `cargo test`/`cargo nextest
    // run`-shaped `Unit` check whose own output never reached a well-formed
    // summary line.
    let (status, inconclusive_reason) = if spawn_tool_missing {
        (
            CheckStatus::Inconclusive,
            Some(InconclusiveReason::ToolMissing),
        )
    } else {
        classify_gate_status(
            status,
            exit_code,
            &String::from_utf8_lossy(&output),
            is_test_runner_check,
            test_summary_total,
        )
    };
    CheckResult {
        id: check.id.clone(),
        kind: check.kind,
        command: check.command.clone(),
        source: check_source,
        status,
        exit_code,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        failure_output,
        failure_test_names,
        inconclusive_reason,
    }
}

/// Whether `error` is (or wraps) an [`std::io::Error`] of
/// [`std::io::ErrorKind::NotFound`] -- the kind [`std::process::Command::
/// spawn`] returns when the program it was told to run does not exist.
/// Distinct from a shell finding *itself* but failing to find the command
/// named *inside* it (`looks_like_tool_missing`'s exit-127 case): this is
/// the shell (`sh`/`cmd`) itself never starting, which in practice only
/// happens in a badly broken environment.
fn is_spawn_not_found(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn report_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.verification().join(repo_slug(repo))
}

/// Default when the operator config cannot even be loaded (mirrors
/// `TelemetryConfig::for_repo`'s own fail-safe default).
const DEFAULT_VERIFICATION_RETENTION_DAYS: u64 = 30;

/// Verification's own retention shares `[workflow] telemetry_retention_days`
/// rather than adding a second `REPO_FORBIDDEN` key: that field lives in
/// `src/commands/ctx/config.rs`, which was off-limits for this change (a
/// concurrent branch was mid-edit on that file), and reusing an existing
/// operator-controlled, already-clamped value is a safer choice than either
/// hand-adding a parallel config surface later (drift risk) or a raw
/// environment read (the exact anti-pattern `telemetry_retention_days`
/// itself replaced -- see CLAUDE.md's workflow-telemetry passage). This
/// function is the seam if a truly independent `verification_retention_days`
/// key is added later: only this function's body would need to change.
/// Recorded in the Decision Log and Known Issues.
fn resolved_retention_days_from_config(cfg: &crate::commands::ctx::config::WorkflowConfig) -> u64 {
    super::telemetry::TelemetryConfig::from_config(cfg).retention_days
}

/// A configuration that will not load at all disables neither reads nor
/// writes here (verification always runs); it simply falls back to the
/// default retention rather than an operator's real, unreadable intent.
fn resolved_retention_days(repo: &Path) -> u64 {
    match crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()) {
        Ok(cfg) => resolved_retention_days_from_config(&cfg.workflow),
        Err(_) => DEFAULT_VERIFICATION_RETENTION_DAYS,
    }
}

/// Zero-padded timestamp prefix, the same shape `telemetry.rs` names its own
/// event files with, so `telemetry::prune_expired_except`'s filename-prefix
/// age rule applies unchanged to this directory too -- see `save_report`.
fn report_filename(report: &VerificationReport) -> String {
    format!("{:020}-{}.json", report.finished_at, report.id)
}

/// Persists one report and prunes reports older than the resolved retention
/// window. Reuses `telemetry::prune_expired_except` rather than adding a
/// second pruner: this directory holds one file per verification run, named
/// with the same leading-timestamp shape telemetry's own event files use, so
/// the same age rule applies unchanged. The just-written report's own
/// filename is passed as the one entry `prune_expired_except` must never
/// remove -- `latest` must survive even a stale retention window, and
/// pruning always runs relative to *this* report's own `finished_at`, so the
/// report being written can never be older than the cutoff computed from its
/// own timestamp regardless.
pub(crate) fn save_report(state: &StateDir, report: &VerificationReport) -> CtxResult<()> {
    let dir = report_dir(state, &report.repo);
    create_private_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(report)?;
    let filename = report_filename(report);
    write_private(&dir.join(&filename), &body)?;
    write_private(&dir.join("latest"), &filename)?;
    let retention_days = resolved_retention_days(&report.repo);
    super::telemetry::prune_expired_except(
        &dir,
        report.finished_at,
        retention_days,
        &[filename.as_str()],
    );
    Ok(())
}

pub fn load_latest(state: &StateDir, repo: &Path) -> CtxResult<Option<VerificationReport>> {
    let dir = report_dir(state, repo);
    let latest = dir.join("latest");
    if !latest.exists() {
        return Ok(None);
    }
    let filename = std::fs::read_to_string(&latest)?;
    let path = dir.join(filename.trim());
    let report: VerificationReport = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if report.schema_version != VERIFY_REPORT_SCHEMA_VERSION {
        return Err(format!(
            "verification report '{}': unsupported schema_version {}",
            report.id, report.schema_version
        )
        .into());
    }
    Ok(Some(report))
}

pub fn latest_is_fresh_and_passing(
    state: &StateDir,
    repo: &Path,
    final_only: bool,
) -> CtxResult<bool> {
    let Some(report) = load_latest(state, repo)? else {
        return Ok(false);
    };
    if !((!final_only || report.mode == VerificationMode::Final)
        // A `--check format` run is evidence about formatting, not about the
        // change set, so it can never satisfy a step gate.
        && report.narrowed_to.is_empty()
        && report.change_fingerprint == change_fingerprint(repo)?)
    {
        return Ok(false);
    }
    if report.passed() {
        return Ok(true);
    }
    // #215: a report with failures can still satisfy the gate if every
    // failure is a named test already covered by the operator's own
    // per-repository baseline (`zirv test baseline`) -- see
    // `VerificationReport::evaluate_against_baseline`. A baseline file that
    // fails to load (never recorded, or a genuine read error) degrades to
    // "no baseline", which reproduces today's strict any-failure-closes-the-
    // gate behavior exactly.
    let evaluation = evaluate_against_operator_baseline(&report, repo);
    if evaluation.gate_passed && !evaluation.waived.is_empty() {
        crate::output::warn(format!(
            "test gate passed only because these failing test(s) are covered by the recorded \
             baseline for this repository: {}",
            evaluation.waived.join(", ")
        ));
    }
    Ok(evaluation.gate_passed)
}

/// Weighs an already-failing `report` against the operator's recorded
/// per-repository baseline (`zirv test baseline`), sharing this one code path
/// between the step gate (`latest_is_fresh_and_passing`) and the review
/// package's verification evidence (`review::VerificationEvidence`) -- see
/// issue #238, where the review package previously had zero baseline
/// awareness and so reported a raw, waiver-blind `passed:false` even when the
/// gate itself had already passed via the baseline. A baseline file that
/// fails to load (never recorded, or a genuine read error) degrades to "no
/// baseline", reproducing the strict any-failure-closes-the-gate behavior
/// exactly, just as it does on the gate path.
pub fn evaluate_against_operator_baseline(
    report: &VerificationReport,
    repo: &Path,
) -> BaselineEvaluation {
    let baseline = load_baseline(repo).unwrap_or(None);
    report.evaluate_against_baseline(baseline.as_ref())
}

/// The `proves:`/`fix:` announcement issue #268 asks for alongside a gate
/// failure: an `Inconclusive` verdict is worded differently from an ordinary
/// `Fail`, since a crashed runner or an empty selection proves nothing about
/// the change set in either direction, whereas a real failure at least
/// proves something is broken. Meant to be appended to the plain "run `zirv
/// test changed`/`zirv verify`" message every `latest_is_fresh_and_passing`
/// call site already produces on its own -- see `engine.rs`'s step gate and
/// `deploy.rs`'s production gate.
pub fn gate_announcement(state: &StateDir, repo: &Path, final_only: bool) -> String {
    let command = if final_only {
        "zirv verify"
    } else {
        "zirv test changed"
    };
    let Ok(Some(report)) = load_latest(state, repo) else {
        return format!("proves: nothing (no verification evidence yet) · fix: run `{command}`");
    };
    match report.outcome() {
        GateOutcome::Inconclusive(reason) => format!(
            "gate: Inconclusive ({}) · proves: nothing · fix: re-run `{command}`; if it \
             repeats, investigate before treating the change as verified",
            reason.as_str()
        ),
        GateOutcome::Fail | GateOutcome::Pass => {
            // `outcome()` ranks `Fail` above `Inconclusive`, so a mixed
            // report (one check genuinely failed, another's runner
            // crashed) lands here -- the accompanying `Inconclusive`
            // check(s) are still worth naming, not silently absorbed into
            // a plain "Fail" that implies every check produced real
            // evidence.
            let inconclusive_ids: Vec<&str> = report
                .checks
                .iter()
                .filter(|check| check.status == CheckStatus::Inconclusive)
                .map(|check| check.id.as_str())
                .collect();
            let suffix = if inconclusive_ids.is_empty() {
                String::new()
            } else {
                format!(" (also inconclusive: {})", inconclusive_ids.join(", "))
            };
            format!(
                "gate: Fail · proves: the current evidence does not cover this change set, or \
                 has unwaived failures · fix: run `{command}`{suffix}"
            )
        }
    }
}

fn run_mode(
    repo: &Path,
    mode: VerificationMode,
    only: &[String],
    dry_run: bool,
) -> CtxResult<VerificationReport> {
    let resolved = load_or_discover(repo)?;
    // Not `?`: an unparseable ctx.toml closes the repo-check gate (and
    // announces itself) rather than failing the whole command, which a
    // checkout could otherwise use to brick `zirv test`/`zirv verify`.
    let repo_gates = super::repo_gates(repo);
    let repo_checks_enabled = repo_gates.checks;
    let mut notes = resolved.notes;
    // Issue #268's degraded-gate ban: zero checks configured or
    // discoverable is reported, not silently treated as a pass and not a
    // hard `Err` either (an empty/absent `verify.toml` must not brick `zirv
    // test`/`zirv verify` outright -- same reasoning as the repo-gate note
    // above). Only the operator-only `workflow.allow_empty_verify` override
    // can make this a `Passed` run instead; the override's use is recorded
    // in `notes` either way, so it is visible in the report.
    if resolved.checks.is_empty() {
        let allow_empty = repo_gates.allow_empty_verify;
        notes.push(if allow_empty {
            "no verification checks configured or discoverable; passing only because the \
             operator override workflow.allow_empty_verify is set"
                .to_string()
        } else {
            "no verification checks configured or discoverable; add .zirv/verify.toml (or set \
             the operator override workflow.allow_empty_verify)"
                .to_string()
        });
        return Ok(VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            mode,
            source: resolved.origin.to_string(),
            repo: repo.to_path_buf(),
            change_fingerprint: change_fingerprint(repo)?,
            changed_paths: Vec::new(),
            fallback_to_full: false,
            narrowed_to: only.to_vec(),
            notes,
            started_at: now_secs(),
            finished_at: now_secs(),
            checks: vec![CheckResult {
                id: "no-checks".into(),
                kind: CheckKind::Custom,
                command: String::new(),
                source: CheckSource::DiscoveredToolchain,
                status: if allow_empty {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Inconclusive
                },
                exit_code: None,
                duration_ms: 0,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: (!allow_empty).then_some(InconclusiveReason::NoChecks),
            }],
        });
    }
    let paths = changed_paths(repo)?;
    let mut fallback_to_full = false;
    let mut selected: Vec<&ResolvedCheck> = resolved
        .checks
        .iter()
        .filter(|check| match mode {
            VerificationMode::Changed => check.spec.changed,
            VerificationMode::Final => check.spec.final_check,
            VerificationMode::All => true,
        })
        .filter(|check| only.is_empty() || only.contains(&check.spec.id))
        .collect();

    if mode == VerificationMode::Changed {
        let targeted: Vec<_> = selected
            .iter()
            .copied()
            .filter(|check| {
                check.spec.paths.is_empty()
                    || paths.iter().any(|path| {
                        let path = path.to_string_lossy();
                        check
                            .spec
                            .paths
                            .iter()
                            .any(|pattern| path_matches(pattern, &path))
                    })
            })
            .collect();
        if paths.is_empty() || targeted.is_empty() {
            fallback_to_full = true;
        } else {
            selected = targeted;
        }
    }
    if selected.is_empty() {
        return Err("no verification checks matched the requested selection".into());
    }
    if !repo_checks_enabled && selected.iter().any(|check| check.source.repo_supplied()) {
        notes.push(
            "skipped: repo-supplied checks disabled (workflow.repo_checks_enabled)".to_string(),
        );
    }

    let started_at = now_secs();
    // Before the checks, not after: a fingerprint taken afterwards records
    // edits made *during* a long suite as if they had been tested.
    let change_fingerprint = change_fingerprint(repo)?;
    let mut checks = Vec::with_capacity(selected.len());
    // The per-check clamp bounds one command; 32 clamped commands still add up
    // to eight hours, so the repo-supplied set gets one shared wall-clock
    // budget as well. Whatever is left over is reported as skipped rather than
    // quietly dropped.
    let mut repo_spent = Duration::ZERO;
    let mut budget_noted = false;
    for check in selected {
        if !repo_checks_enabled && check.source.repo_supplied() {
            checks.push(check_result(check, CheckStatus::Skipped));
            continue;
        }
        if check.source.repo_supplied() && repo_spent >= MAX_REPO_TOTAL_TIME {
            if !budget_noted {
                notes.push(format!(
                    "repo-supplied checks exceeded the {}s total budget; the rest were skipped",
                    MAX_REPO_TOTAL_TIME.as_secs()
                ));
                budget_noted = true;
            }
            checks.push(check_result(check, CheckStatus::Skipped));
            continue;
        }
        let result = run_check(repo, check, dry_run, &repo_gates.check_env_passthrough);
        if check.source.repo_supplied() {
            repo_spent = repo_spent.saturating_add(Duration::from_millis(result.duration_ms));
        }
        checks.push(result);
    }
    Ok(VerificationReport {
        schema_version: VERIFY_REPORT_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        mode,
        source: resolved.origin.to_string(),
        repo: repo.to_path_buf(),
        change_fingerprint,
        changed_paths: paths,
        fallback_to_full,
        narrowed_to: only.to_vec(),
        notes,
        started_at,
        finished_at: now_secs(),
        checks,
    })
}

/// Persist the report and record telemetry. Deliberately separate from
/// `run_mode` and called *after* the results are printed: a state-directory
/// failure used to discard a whole verification run's results with a `?`.
fn persist(report: &VerificationReport, repo: &Path, mode: VerificationMode) -> CtxResult<()> {
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    save_report(&state, report)?;
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::VerificationRun);
    event.phase = Some(match mode {
        VerificationMode::Final => super::skill::WorkflowPhase::Verify,
        VerificationMode::Changed | VerificationMode::All => super::skill::WorkflowPhase::Test,
    });
    event.duration_ms = Some(
        report
            .checks
            .iter()
            .map(|check| check.duration_ms)
            .fold(0u64, u64::saturating_add),
    );
    event.succeeded = Some(report.passed());
    if let Ok(Some(workflow)) = super::engine::load_active(&state, repo) {
        event.workflow_id = Some(workflow.id);
        event.intent = Some(workflow.classification.intent);
        event.complexity = Some(workflow.classification.complexity);
        event.risk = Some(workflow.classification.risk);
        event.work_domain = Some(workflow.classification.work_domain.domain);
    }
    let _ = super::telemetry::record(
        &state,
        repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(repo),
    );
    Ok(())
}

fn is_broken_pipe(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
}

/// One run: results printed first, then persisted. A persistence failure is a
/// warning on the way out, never a reason to lose the results themselves.
/// Returns the report alongside the exit code so `run_baseline` can inspect
/// its checks without re-running anything.
fn run_and_persist(
    repo: &Path,
    mode: VerificationMode,
    args: &RunArgs,
    writer: &mut impl Write,
) -> CtxResult<(i32, VerificationReport)> {
    let mut report = run_mode(repo, mode, &args.checks, args.dry_run)?;
    if !args.dry_run
        && let Some(note) = update_baseline_after_run(repo, &report)
    {
        report.notes.push(note);
    }
    // `zirv verify | head` closes the pipe mid-report. Printing is the part
    // that failed, so the run's own results are still worth storing -- without
    // this, piping into a pager silently cost the workflow its evidence.
    if let Err(error) = write_report(writer, &report, args.json) {
        if !is_broken_pipe(error.as_ref()) {
            return Err(error);
        }
        crate::output::warn("verification output was cut short (broken pipe)");
    }
    if !args.dry_run
        && let Err(error) = persist(&report, repo, mode)
    {
        crate::output::warn(format!(
            "verification results were not persisted: {error}; the next step gate will ask for a \
             fresh run"
        ));
    }
    let code = if args.dry_run || report.passed() {
        0
    } else {
        1
    };
    Ok((code, report))
}

fn run_and_report(
    repo: &Path,
    mode: VerificationMode,
    args: &RunArgs,
    writer: &mut impl Write,
) -> CtxResult<i32> {
    run_and_persist(repo, mode, args, writer).map(|(code, _report)| code)
}

/// `zirv test baseline`: runs every check (`VerificationMode::All`, like
/// `zirv test all`), prints and persists the report exactly as any other run
/// does, then records the `Unit`-kind checks' failing test names as this
/// repository's operator-owned baseline (`save_baseline`). Deliberately the
/// only path that ever writes a baseline -- `zirv test changed`/`zirv
/// verify`/a workflow gate never do, so a baseline only ever reflects
/// failures an operator ran this command and looked at.
///
/// Only ever ingests names from a check whose `source` is *not*
/// `repo_supplied()` -- today that means `CheckSource::DiscoveredToolchain`,
/// zirv's own built-in Cargo checks, never `RepoConfig` (`<repo>/.zirv/
/// verify.toml`) or `DiscoveredScript` (`npm run <id>`, whose body is
/// `package.json` text). Both of the latter run command text the checkout
/// itself authored; a repository is UNTRUSTED and may only ever narrow what
/// an operator can do (see `CLAUDE.md`'s "Repo-owned surfaces" rule), so a
/// repo-authored check that prints forged `failures:`/`test result: FAILED`
/// text must never be able to widen the operator's baseline with names of
/// its own choosing that it plans to actually break later. Such a check
/// still fails the run and still blocks the gate -- it is simply never
/// consulted for baseline *names*, exactly like any other unrecordable
/// check.
fn run_baseline(repo: &Path, args: &BaselineArgs, writer: &mut impl Write) -> CtxResult<i32> {
    if args.prune {
        return run_baseline_prune(repo, writer);
    }
    let (code, report) = run_and_persist(repo, VerificationMode::All, &args.run, writer)?;
    if args.run.dry_run {
        return Ok(code);
    }
    // Issue #268's degraded-gate ban: a run that proves nothing either way
    // must never be recorded as evidence a set of tests is exactly this
    // repository's known-bad list -- that is what let a crashed runner (or
    // an empty selection) silently become "the baseline says this is fine".
    if let GateOutcome::Inconclusive(reason) = report.outcome() {
        crate::output::warn(format!(
            "baseline not recorded: this run was inconclusive ({}) -- re-run once the runner \
             produces a well-formed result",
            reason.as_str()
        ));
        return Ok(1);
    }
    let mut failing = std::collections::BTreeSet::new();
    let mut unrecordable = Vec::new();
    for check in &report.checks {
        if check.status == CheckStatus::Passed {
            continue;
        }
        if check.status != CheckStatus::Failed || check.kind != CheckKind::Unit {
            unrecordable.push(format!("{} ({:?})", check.id, check.status));
            continue;
        }
        if check.source.repo_supplied() {
            unrecordable.push(format!(
                "{} (a repository-defined check's output is never trusted to widen the \
                 operator's baseline)",
                check.id
            ));
            continue;
        }
        let names = failure_names_for(check);
        if names.is_empty() {
            unrecordable.push(format!(
                "{} (failed, but no individual test names could be parsed from its output)",
                check.id
            ));
            continue;
        }
        failing.extend(names);
    }
    let count = failing.len();
    let baseline = save_baseline(repo, failing)?;
    writeln!(
        writer,
        "recorded baseline for {}: {count} failing test name(s) at {}",
        scrub_line(&repo_slug(repo)),
        baseline.recorded_at
    )?;
    for note in &unrecordable {
        crate::output::warn(format!(
            "not recorded in the baseline (not a single named test failure): {}",
            scrub_line(note)
        ));
    }
    Ok(0)
}

/// `zirv test baseline --prune`: removes exactly the baseline entries that
/// have reached `PRUNE_AFTER_GREENS` consecutive green evaluations (see
/// `update_baseline_after_run`), without re-running any check -- pruning is a
/// maintenance action over evidence already accumulated by ordinary `zirv
/// test changed`/`zirv verify` runs, not a fresh recording.
fn run_baseline_prune(repo: &Path, writer: &mut impl Write) -> CtxResult<i32> {
    let Some(mut baseline) = load_baseline(repo)? else {
        writeln!(
            writer,
            "no recorded baseline for {}; nothing to prune",
            scrub_line(&repo_slug(repo))
        )?;
        return Ok(0);
    };
    let eligible: Vec<String> = baseline
        .failing_tests
        .iter()
        .filter(|name| {
            baseline.green_streaks.get(*name).copied().unwrap_or(0) >= PRUNE_AFTER_GREENS
        })
        .cloned()
        .collect();
    if eligible.is_empty() {
        writeln!(
            writer,
            "no baseline entries have reached {PRUNE_AFTER_GREENS} consecutive green runs yet"
        )?;
        return Ok(0);
    }
    baseline
        .failing_tests
        .retain(|name| !eligible.contains(name));
    for name in &eligible {
        baseline.green_streaks.remove(name);
    }
    write_baseline(repo, &baseline)?;
    writeln!(
        writer,
        "pruned {} baseline entry(ies): {}",
        eligible.len(),
        scrub_line(&eligible.join(", "))
    )?;
    Ok(0)
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Run only the named check; repeat for more than one.
    #[arg(long = "check")]
    pub checks: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BaselineArgs {
    #[command(flatten)]
    pub run: RunArgs,
    /// Remove baseline entries that have reached `PRUNE_AFTER_GREENS`
    /// consecutive green evaluations, instead of running every check and
    /// recording a fresh baseline (issue #268).
    #[arg(long)]
    pub prune: bool,
}

#[derive(Debug, Args)]
pub struct TestArgs {
    #[command(subcommand)]
    pub command: TestCommand,
}

#[derive(Debug, Subcommand)]
pub enum TestCommand {
    /// Run checks mapped to changed paths, safely falling back to all.
    Changed(RunArgs),
    /// Run every configured/discovered check.
    All(RunArgs),
    /// Run every check and record its failing test names as this
    /// repository's operator-owned baseline (issue #215), so a later step
    /// gate may waive exactly these pre-existing failures instead of
    /// blocking on them forever. Always an explicit operator action. Never
    /// records from an `Inconclusive` run (issue #268); `--prune` instead
    /// removes matured entries without re-running anything.
    Baseline(BaselineArgs),
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    #[command(flatten)]
    pub run: RunArgs,
}

fn resolved_repo(path: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match path {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

/// Every string here that a repository could have written goes through
/// `scrub_output` on the way to the terminal, not just the captured failure
/// output: a check's own `command` is repository text too, so
/// `command = "true # <ESC>[2J<ESC>[H all checks passed"` could clear the
/// screen and forge a summary from the *header* line, without the check ever
/// producing a byte of output. Ids and notes are zirv-shaped today, and are
/// scrubbed anyway rather than relying on that staying true. JSON output needs
/// none of this: serde escapes control characters.
fn write_report(writer: &mut impl Write, report: &VerificationReport, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, report)?;
        writeln!(writer)?;
    } else {
        writeln!(
            writer,
            "verification {} ({:?}, {}, targeted_fallback={})",
            scrub_line(&report.id),
            report.mode,
            scrub_line(&report.source),
            report.fallback_to_full
        )?;
        if !report.narrowed_to.is_empty() {
            writeln!(
                writer,
                "narrowed to: {} (not completion evidence for the change set)",
                scrub_line(&report.narrowed_to.join(", "))
            )?;
        }
        for note in &report.notes {
            writeln!(writer, "note: {}", scrub_line(note))?;
        }
        for check in &report.checks {
            writeln!(
                writer,
                "{}\t{:?}\t{:?}\t{} ms\t{}",
                scrub_line(&check.id),
                check.source,
                check.status,
                check.duration_ms,
                scrub_line(&check.command)
            )?;
            if let Some(output) = &check.failure_output {
                writeln!(writer, "{}", scrub_output(output))?;
            }
        }
    }
    Ok(())
}

pub fn run_test(args: &TestArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        TestCommand::Changed(args) => run_and_report(
            &resolved_repo(args.repo.as_deref())?,
            VerificationMode::Changed,
            args,
            writer,
        ),
        TestCommand::All(args) => run_and_report(
            &resolved_repo(args.repo.as_deref())?,
            VerificationMode::All,
            args,
            writer,
        ),
        TestCommand::Baseline(args) => {
            run_baseline(&resolved_repo(args.run.repo.as_deref())?, args, writer)
        }
    }
}

pub fn run_verify(args: &VerifyArgs, writer: &mut impl Write) -> CtxResult<i32> {
    run_and_report(
        &resolved_repo(args.run.repo.as_deref())?,
        VerificationMode::Final,
        &args.run,
        writer,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_rust_checks_without_external_services() {
        let repo = tempdir().unwrap();
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        let resolved = load_or_discover(repo.path()).unwrap();
        assert_eq!(resolved.origin, "discovered");
        assert_eq!(
            resolved
                .checks
                .iter()
                .map(|check| (check.spec.id.as_str(), check.source))
                .collect::<Vec<_>>(),
            [
                ("format", CheckSource::DiscoveredToolchain),
                ("clippy", CheckSource::DiscoveredToolchain),
                ("test", CheckSource::DiscoveredToolchain)
            ]
        );
    }

    #[test]
    fn discovered_npm_scripts_are_labeled_as_repository_authored() {
        let repo = tempdir().unwrap();
        std::fs::write(
            repo.path().join("package.json"),
            "{\"scripts\":{\"lint\":\"eslint .\",\"test\":\"vitest run\"}}",
        )
        .unwrap();
        let resolved = load_or_discover(repo.path()).unwrap();
        assert!(
            resolved
                .checks
                .iter()
                .all(|check| check.source == CheckSource::DiscoveredScript)
        );
    }

    #[test]
    fn repo_supplied_timeouts_are_clamped_with_a_note() {
        let repo = tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".zirv")).unwrap();
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            "schema_version=1\n[[checks]]\nid='slow'\nkind='unit'\ncommand='true'\ntimeout_secs=86400\n",
        )
        .unwrap();
        let resolved = load_or_discover(repo.path()).unwrap();
        assert_eq!(resolved.checks[0].spec.timeout_secs, MAX_REPO_TIMEOUT_SECS);
        assert!(resolved.notes.iter().any(|note| note.contains("clamped")));
    }

    #[test]
    fn too_many_repo_supplied_checks_are_truncated_with_a_note() {
        let repo = tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".zirv")).unwrap();
        let mut config = String::from("schema_version=1\n");
        for index in 0..MAX_REPO_CHECKS + 8 {
            config.push_str(&format!(
                "[[checks]]\nid='check-{index}'\nkind='custom'\ncommand='true'\n"
            ));
        }
        std::fs::write(repo.path().join(".zirv/verify.toml"), config).unwrap();
        let resolved = load_or_discover(repo.path()).unwrap();
        assert_eq!(resolved.checks.len(), MAX_REPO_CHECKS);
        assert!(resolved.notes.iter().any(|note| note.contains("truncated")));
    }

    #[test]
    fn a_repository_cannot_re_enable_its_own_checks() {
        let repo = tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".zirv")).unwrap();
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nrepo_checks_enabled = true\n",
        )
        .unwrap();
        let error = crate::commands::ctx::config::CtxConfig::load(repo.path(), &|_| None)
            .expect_err("a repo layer must not set workflow.repo_checks_enabled");
        assert!(error.to_string().contains("workflow.repo_checks_enabled"));
    }

    /// A repository with one commit, so the Git-backed halves of a run
    /// (`changed_paths`, `change_fingerprint`) have something real to read.
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

    /// Every state-touching run in this module writes under `state_root`, so a
    /// test never reads or writes the operator's own state directory.
    fn with_state<T>(state_root: &Path, body: impl FnOnce() -> T) -> T {
        // SAFETY: this suite runs single-threaded (`--test-threads=1`).
        unsafe {
            std::env::set_var(crate::commands::ctx::state::STATE_ENV, state_root);
        }
        let value = body();
        unsafe {
            std::env::remove_var(crate::commands::ctx::state::STATE_ENV);
        }
        value
    }

    fn write_verify_toml(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join(".zirv")).unwrap();
        std::fs::write(repo.join(".zirv/verify.toml"), body).unwrap();
    }

    /// The command a repo check would run if the gate let it: it leaves a file
    /// behind, so "was it executed" is a filesystem fact rather than a status
    /// this test reads back from the report it is testing.
    fn marker_command() -> &'static str {
        if cfg!(windows) {
            "type nul > ran"
        } else {
            "touch ran"
        }
    }

    #[test]
    fn disabled_repo_checks_are_listed_but_never_executed() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        write_verify_toml(
            repo.path(),
            &format!(
                "schema_version=1\n[[checks]]\nid='sneaky'\nkind='custom'\ncommand='{}'\n",
                marker_command()
            ),
        );
        let report = with_state(state_root.path(), || {
            // SAFETY: single-threaded suite.
            unsafe {
                std::env::set_var("ZIRV_CTX_WORKFLOW_REPO_CHECKS", "false");
            }
            let report = run_mode(repo.path(), VerificationMode::Final, &[], false);
            unsafe {
                std::env::remove_var("ZIRV_CTX_WORKFLOW_REPO_CHECKS");
            }
            report.expect("a disabled gate is not an error")
        });
        assert!(
            !repo.path().join("ran").exists(),
            "the gated command must not have executed"
        );
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, CheckStatus::Skipped);
        assert_eq!(report.checks[0].source, CheckSource::RepoConfig);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("workflow.repo_checks_enabled")),
            "the skip must be stated: {:?}",
            report.notes
        );
        assert!(!report.passed(), "a skipped check is not a passing check");
    }

    /// A plain TOML *syntax* error in the repo's `ctx.toml` must not brick
    /// verification (`run_mode` still succeeds). It also no longer disables
    /// repo-supplied checks: `workflow.repo_checks_enabled` is itself
    /// `REPO_FORBIDDEN` -- a repo file could never set it either way, parsed
    /// or not -- so a merely-unparsable repo layer neither widens nor
    /// narrows this gate (2026-08-23: `config.rs`'s `read_layer`/
    /// `UnparsableLayer` skip a broken layer instead of failing the whole
    /// load; see the sibling tests in `skill.rs` for the gate a repo
    /// genuinely cannot widen, which still fails closed).
    #[test]
    fn an_unparseable_repo_config_does_not_disable_a_gate_it_never_controlled() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        write_verify_toml(
            repo.path(),
            &format!(
                "schema_version=1\n[[checks]]\nid='sneaky'\nkind='custom'\ncommand='{}'\n",
                marker_command()
            ),
        );
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "this is not = = toml\n").unwrap();
        let report = with_state(state_root.path(), || {
            run_mode(repo.path(), VerificationMode::Final, &[], false)
                .expect("a malformed repo config must not brick verification")
        });
        assert!(
            repo.path().join("ran").exists(),
            "repo_checks_enabled defaults true and a repo file could never set it either way, so \
             a syntax error in that same file must not disable it"
        );
        assert_eq!(report.checks[0].status, CheckStatus::Passed);
    }

    /// A fingerprint taken *after* the checks records edits made during the
    /// run as if they had been tested. This check edits a tracked file, so the
    /// recorded fingerprint must differ from the tree's fingerprint afterwards.
    #[test]
    fn the_change_fingerprint_is_taken_before_the_checks_run() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        write_verify_toml(
            repo.path(),
            "schema_version=1\n[[checks]]\nid='edit'\nkind='custom'\ncommand='echo two >> tracked.txt'\n",
        );
        let report = with_state(state_root.path(), || {
            run_mode(repo.path(), VerificationMode::Final, &[], false).expect("run")
        });
        assert_eq!(report.checks[0].status, CheckStatus::Passed);
        assert_ne!(
            report.change_fingerprint,
            change_fingerprint(repo.path()).unwrap(),
            "the report must describe the tree as it was before the edit"
        );
    }

    /// #229: an operator ticking a checkbox in the untracked
    /// `.zirv/work/<id>/plan.md` -- or any other write under `.zirv/work/`
    /// -- must never move `change_fingerprint`, or an independent review
    /// running concurrently gets refused with "the change set changed
    /// during review" for a file the reviewer was never asked to look at.
    #[test]
    fn change_fingerprint_ignores_edits_under_zirv_work() {
        let repo = git_repo();
        let before = change_fingerprint(repo.path()).unwrap();

        let work_dir = repo.path().join(".zirv").join("work").join("wf-1");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::write(work_dir.join("plan.md"), "- [ ] task one\n").unwrap();
        assert_eq!(
            change_fingerprint(repo.path()).unwrap(),
            before,
            "creating a new untracked .zirv/work file must not move the fingerprint"
        );

        std::fs::write(work_dir.join("plan.md"), "- [x] task one\n").unwrap();
        assert_eq!(
            change_fingerprint(repo.path()).unwrap(),
            before,
            "editing an existing untracked .zirv/work file must not move the fingerprint either"
        );

        // Sanity: an edit outside `.zirv/work` still moves the fingerprint,
        // so this test is not just asserting a broken no-op fingerprint.
        std::fs::write(repo.path().join("src_marker.txt"), "changed\n").unwrap();
        assert_ne!(
            change_fingerprint(repo.path()).unwrap(),
            before,
            "an ordinary untracked file must still move the fingerprint"
        );
    }

    /// #251: after a workflow step commits its edits, `changed_paths`
    /// (uncommitted-only against bare HEAD) goes blind to them, which is
    /// exactly what let a whole-repository frontend detector scan run
    /// against a clean working tree. `changed_paths_since_base` must still
    /// see a change committed since the workflow's diff base, even with
    /// nothing left uncommitted.
    #[test]
    fn changed_paths_since_base_sees_committed_changes_invisible_to_changed_paths() {
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
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            repo.path().join("Component.tsx"),
            "export const X = () => null;\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add component"]);

        // Fully committed: the ordinary uncommitted-only view sees nothing.
        assert!(changed_paths(repo.path()).unwrap().is_empty());

        let since_base = changed_paths_since_base(repo.path()).unwrap();
        assert_eq!(since_base, vec![PathBuf::from("Component.tsx")]);
    }

    /// The since-base set also covers untracked files and drops anything
    /// that no longer exists on disk (a path git still reports for an
    /// uncommitted deletion), since nothing downstream can scan a path that
    /// is not there.
    #[test]
    fn changed_paths_since_base_includes_untracked_and_drops_deleted_paths() {
        let repo = git_repo();
        std::fs::write(repo.path().join("tracked.txt"), "removed-next\n").unwrap();
        let status = Command::new("git")
            .args(["rm", "-q", "--cached", "tracked.txt"])
            .current_dir(repo.path())
            .status()
            .expect("git rm --cached");
        assert!(status.success());
        std::fs::remove_file(repo.path().join("tracked.txt")).unwrap();
        std::fs::write(repo.path().join("new.tsx"), "export {};\n").unwrap();

        let since_base = changed_paths_since_base(repo.path()).unwrap();
        assert!(
            !since_base.contains(&PathBuf::from("tracked.txt")),
            "a path deleted from disk must be dropped: {since_base:?}"
        );
        assert!(
            since_base.contains(&PathBuf::from("new.tsx")),
            "an untracked file must be included: {since_base:?}"
        );
    }

    /// A `--check`-narrowed run is evidence about the checks it ran, not about
    /// the change set, so it can never satisfy a step gate.
    #[test]
    fn a_narrowed_run_never_satisfies_the_freshness_gate() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        let state = StateDir::from_root(state_root.path().to_path_buf());
        let fingerprint = change_fingerprint(repo.path()).unwrap();
        let mut report = VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: "narrowed".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec!["format".into()],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![CheckResult {
                id: "format".into(),
                kind: CheckKind::Format,
                command: "true".into(),
                source: CheckSource::DiscoveredToolchain,
                status: CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        save_report(&state, &report).unwrap();
        assert!(
            !latest_is_fresh_and_passing(&state, repo.path(), true).unwrap(),
            "a narrowed run is not completion evidence"
        );

        report.id = "full".into();
        report.narrowed_to.clear();
        save_report(&state, &report).unwrap();
        assert!(
            latest_is_fresh_and_passing(&state, repo.path(), true).unwrap(),
            "the same run, un-narrowed, is"
        );
    }

    /// Results are printed before they are persisted, so a state-directory
    /// failure costs the report but not the operator's own output.
    #[test]
    fn results_are_printed_even_when_persistence_fails() {
        let repo = git_repo();
        let blocker = tempdir().unwrap();
        // A *file* where the state directory should be: `StateDir` resolves,
        // and every write under it fails.
        let state_path = blocker.path().join("not-a-dir");
        std::fs::write(&state_path, "").unwrap();
        write_verify_toml(
            repo.path(),
            "schema_version=1\n[[checks]]\nid='ok'\nkind='custom'\ncommand='true'\n",
        );
        let mut out = Vec::new();
        let code = with_state(&state_path, || {
            run_and_report(
                repo.path(),
                VerificationMode::Final,
                &RunArgs {
                    repo: None,
                    checks: vec![],
                    dry_run: false,
                    json: false,
                },
                &mut out,
            )
            .expect("a persistence failure is a warning, not an error")
        });
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\tok\t") || text.contains("ok\t"),
            "got {text}"
        );
    }

    /// The `command` field is repository text printed straight to the
    /// terminal, so it needs the same scrubbing the captured output gets: an
    /// escape sequence there could clear the screen and forge a summary from
    /// the header line alone, with the check producing no output at all.
    #[test]
    fn a_report_never_prints_repo_controlled_escape_sequences() {
        let report = VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: "report".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: PathBuf::from("/repo"),
            change_fingerprint: 1,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec!["note \u{1b}[2Jforged".into()],
            checks: vec![CheckResult {
                id: "ok".into(),
                kind: CheckKind::Custom,
                command: "true # \u{1b}[2J\u{1b}[H\nverification all checks passed".into(),
                source: CheckSource::RepoConfig,
                status: CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
            started_at: 0,
            finished_at: 0,
        };
        let mut out = Vec::new();
        write_report(&mut out, &report, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\u{1b}'), "got {text:?}");
        assert_eq!(
            text.lines().count(),
            3,
            "a command's own newline must not start a line of its own: {text:?}"
        );
    }

    /// #91: mirrors telemetry's own
    /// `retention_and_event_caps_still_bound_an_operator_value` -- verification
    /// shares telemetry's own retention config (see `resolved_retention_days_
    /// from_config`'s doc comment), so an operator value bound for telemetry
    /// is, by construction, bound the same way here.
    #[test]
    fn verification_retention_is_clamped_the_same_way_telemetry_is() {
        let cfg = crate::commands::ctx::config::WorkflowConfig {
            telemetry_retention_days: super::super::telemetry::MAX_CONFIGURED_RETENTION_DAYS * 4,
            ..Default::default()
        };
        assert_eq!(
            resolved_retention_days_from_config(&cfg),
            super::super::telemetry::MAX_CONFIGURED_RETENTION_DAYS
        );
    }

    fn minimal_report(repo: &Path, id: &str, finished_at: u64) -> VerificationReport {
        VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: id.into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: repo.to_path_buf(),
            change_fingerprint: 1,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: finished_at,
            finished_at,
            checks: vec![CheckResult {
                id: "ok".into(),
                kind: CheckKind::Custom,
                command: "true".into(),
                source: CheckSource::DiscoveredToolchain,
                status: CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        }
    }

    /// #91: after writes spanning past the retention window, the expired
    /// report is pruned, the current `latest` report survives, and
    /// `load_latest` still works against the pruned directory.
    #[test]
    fn expired_verification_reports_are_pruned_but_latest_survives() {
        let repo = git_repo();
        let root = tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());

        // Default retention is 30 days (2,592,000s). `new_ts` is set far
        // enough past `old_ts` that `old_ts` falls outside the window
        // measured from `new_ts` -- deliberately independent of real
        // wall-clock time, so the test has no flakiness budget to spend.
        let old_ts: u64 = 10_000_000;
        let new_ts: u64 = old_ts + 3_000_000;

        let old = minimal_report(repo.path(), "old-report", old_ts);
        save_report(&state, &old).unwrap();
        let old_path = report_dir(&state, repo.path()).join(report_filename(&old));
        assert!(old_path.exists(), "the first report was written");

        let newest = minimal_report(repo.path(), "new-report", new_ts);
        save_report(&state, &newest).unwrap();

        assert!(
            !old_path.exists(),
            "the expired report must be pruned once a newer one is saved"
        );
        let loaded = load_latest(&state, repo.path())
            .unwrap()
            .expect("load_latest still succeeds against a pruned directory");
        assert_eq!(loaded.id, "new-report");
    }

    /// #91: a lone report older than the retention window is still `latest`
    /// -- pruning must never remove the file the `latest` pointer names, even
    /// though nothing else is around to protect it.
    #[test]
    fn a_lone_expired_report_is_still_latest() {
        let repo = git_repo();
        let root = tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let report = minimal_report(repo.path(), "only-report", 10_000_000);
        save_report(&state, &report).unwrap();

        let loaded = load_latest(&state, repo.path())
            .unwrap()
            .expect("the only report on disk is still latest");
        assert_eq!(loaded.id, "only-report");
    }

    #[test]
    fn configured_checks_reject_unknown_fields_and_duplicate_ids() {
        let repo = tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".zirv")).unwrap();
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            "schema_version=1\n[[checks]]\nid='x'\nkind='unit'\ncommand='true'\nsurprise=true\n",
        )
        .unwrap();
        assert!(load_or_discover(repo.path()).is_err());
    }

    #[test]
    fn path_patterns_select_relevant_checks() {
        assert!(path_matches("src/", "src/lib.rs"));
        assert!(path_matches("*.rs", "src/lib.rs"));
        assert!(!path_matches("docs/", "src/lib.rs"));
    }

    #[test]
    fn verbose_output_is_drained_but_only_a_bounded_tail_is_retained() {
        let input: Vec<u8> = (0..MAX_FAILURE_OUTPUT_BYTES + 4096)
            .map(|index| (index % 251) as u8)
            .collect();
        let (retained, errored, _names, _summary_seen, _summary_total) =
            read_capped_tail_and_scan(std::io::Cursor::new(&input), MAX_FAILURE_OUTPUT_BYTES);
        assert!(!errored);
        assert_eq!(retained.len(), MAX_FAILURE_OUTPUT_BYTES);
        assert_eq!(retained, input[input.len() - MAX_FAILURE_OUTPUT_BYTES..]);
    }

    #[test]
    fn a_capped_tail_keeps_the_end_even_when_lossy_decoding_inflates_it() {
        let mut bytes = vec![0xffu8; 64];
        bytes.extend_from_slice(b"the actionable tail");
        let text = tail_text(&bytes, 64);
        assert!(text.ends_with("the actionable tail"), "got {text}");
        assert!(text.len() <= 64 + 4);
    }

    #[test]
    fn control_characters_in_check_output_cannot_forge_a_summary() {
        let scrubbed = scrub_output("\u{1b}[2J\u{1b}[Hall checks passed\nreal line\n");
        assert!(!scrubbed.contains('\u{1b}'));
        assert!(scrubbed.contains("real line\n"));
    }

    fn spec(id: &str, command: &str) -> ResolvedCheck {
        ResolvedCheck {
            spec: CheckSpec {
                id: id.into(),
                kind: CheckKind::Custom,
                command: command.into(),
                paths: vec![],
                changed: true,
                final_check: true,
                timeout_secs: 5,
            },
            source: CheckSource::RepoConfig,
        }
    }

    #[test]
    fn successful_output_stays_compact_and_failures_remain_actionable() {
        let repo = tempdir().unwrap();
        let passed = run_check(
            repo.path(),
            &spec(
                "ok",
                if cfg!(windows) {
                    "echo ok"
                } else {
                    "printf ok"
                },
            ),
            false,
            &[],
        );
        assert_eq!(passed.status, CheckStatus::Passed);
        assert!(passed.failure_output.is_none());

        let failed = run_check(
            repo.path(),
            &spec(
                "bad",
                if cfg!(windows) {
                    "echo actionable & exit /b 3"
                } else {
                    "echo actionable >&2; exit 3"
                },
            ),
            false,
            &[],
        );
        assert_eq!(failed.status, CheckStatus::Failed);
        assert!(failed.failure_output.unwrap().contains("actionable"));
    }

    // -- #233: verification checks see the operator's SSH-agent env family -

    /// Round-trips `SSH_AUTH_SOCK` through a real check child with no
    /// `[workflow] check_env_passthrough` configuration at all (the "safe
    /// defaults" requirement): the check prints the variable back out on the
    /// failure path (where output is retained), and the captured text must
    /// contain the value the *test* set on zirv's own process, not the
    /// literal, unexpanded `$SSH_AUTH_SOCK`/`%SSH_AUTH_SOCK%` the shell
    /// prints for an unset variable.
    #[test]
    fn a_check_child_sees_ssh_auth_sock_via_the_default_allowlist_with_no_config() {
        let repo = tempdir().unwrap();
        let marker = "zirv-issue-233-ssh-auth-sock-marker";
        // SAFETY: this suite runs single-threaded / process-per-test.
        unsafe {
            std::env::set_var("SSH_AUTH_SOCK", marker);
        }
        let result = run_check(
            repo.path(),
            &spec(
                "ssh-auth-sock",
                if cfg!(windows) {
                    "echo %SSH_AUTH_SOCK% & exit /b 3"
                } else {
                    "echo \"$SSH_AUTH_SOCK\" >&2; exit 3"
                },
            ),
            false,
            &[],
        );
        unsafe {
            std::env::remove_var("SSH_AUTH_SOCK");
        }
        assert_eq!(result.status, CheckStatus::Failed);
        let output = result.failure_output.unwrap();
        assert!(
            output.contains(marker),
            "the check child must see the operator's own SSH_AUTH_SOCK with no config \
             at all: got {output}"
        );
    }

    /// A name in the operator's own `[workflow] check_env_passthrough`
    /// reaches a real check child end to end via `run_check`'s explicit
    /// `command.env(name, value)` call. `Command::new` already inherits the
    /// parent's full environment by default (no `env_clear`/`env_remove`
    /// sits between zirv and the check child -- see
    /// `DEFAULT_CHECK_ENV_PASSTHROUGH`'s own doc comment), so this variable
    /// would reach the child regardless; the ADD-not-replace contract itself
    /// is pinned at the unit level, with no process spawn involved, by
    /// `resolved_check_env_passthrough_adds_to_not_replaces_the_defaults`
    /// right below. What this test actually catches is a regression in the
    /// explicit-passthrough loop itself (e.g. the wrong name/value pair, or
    /// the loop silently dropped).
    #[test]
    fn a_check_env_passthrough_entry_reaches_the_check_child() {
        let repo = tempdir().unwrap();
        let custom_marker = "zirv-issue-233-custom-marker";
        // SAFETY: this suite runs single-threaded / process-per-test.
        unsafe {
            std::env::set_var("ZIRV_ISSUE_233_CUSTOM", custom_marker);
        }
        let command = if cfg!(windows) {
            "echo %ZIRV_ISSUE_233_CUSTOM% & exit /b 3"
        } else {
            "echo \"$ZIRV_ISSUE_233_CUSTOM\" >&2; exit 3"
        };
        let result = run_check(
            repo.path(),
            &spec("custom", command),
            false,
            &["ZIRV_ISSUE_233_CUSTOM".to_string()],
        );
        unsafe {
            std::env::remove_var("ZIRV_ISSUE_233_CUSTOM");
        }
        let output = result.failure_output.unwrap();
        assert!(
            output.contains(custom_marker),
            "an operator-configured check_env_passthrough entry must reach the check \
             child: got {output}"
        );
    }

    /// Pure unit coverage of the merge itself, independent of process spawn:
    /// the operator's own entries are appended to the built-in defaults,
    /// matched case-insensitively on Windows (so a differently-cased
    /// duplicate of a default name does not produce a second entry) and
    /// case-sensitively on unix (so it does).
    #[test]
    fn resolved_check_env_passthrough_adds_to_not_replaces_the_defaults() {
        let resolved = resolved_check_env_passthrough(&["MY_CUSTOM_VAR".to_string()]);
        for default in DEFAULT_CHECK_ENV_PASSTHROUGH {
            assert!(
                resolved.iter().any(|name| name == default),
                "the default '{default}' must survive: {resolved:?}"
            );
        }
        assert!(
            resolved.iter().any(|name| name == "MY_CUSTOM_VAR"),
            "the operator's own entry must be added: {resolved:?}"
        );
        assert_eq!(
            resolved.len(),
            DEFAULT_CHECK_ENV_PASSTHROUGH.len() + 1,
            "adds one name on top of the defaults, never fewer: {resolved:?}"
        );

        // An exact-cased duplicate of a default is never added twice, on
        // either platform.
        let exact_dup = resolved_check_env_passthrough(&["SSH_AUTH_SOCK".to_string()]);
        assert_eq!(exact_dup.len(), DEFAULT_CHECK_ENV_PASSTHROUGH.len());

        let differently_cased = resolved_check_env_passthrough(&["ssh_auth_sock".to_string()]);
        if cfg!(windows) {
            assert_eq!(
                differently_cased.len(),
                DEFAULT_CHECK_ENV_PASSTHROUGH.len(),
                "Windows matching is case-insensitive: {differently_cased:?}"
            );
        } else {
            assert_eq!(
                differently_cased.len(),
                DEFAULT_CHECK_ENV_PASSTHROUGH.len() + 1,
                "unix matching is case-sensitive, so this is a distinct addition: \
                 {differently_cased:?}"
            );
        }
    }

    /// Pure, cross-platform coverage of the resolution order itself (issue
    /// #233's macOS fallback), with a fake resolver on both sides -- no
    /// process spawn, no real `launchctl`, so this runs and means the same
    /// thing on every CI platform including this Windows dev machine.
    #[test]
    fn resolve_one_check_env_var_prefers_process_env_over_the_launchd_fallback() {
        let process = |name: &str| (name == "SSH_AUTH_SOCK").then(|| "from-process".to_string());
        let launchd = |name: &str| (name == "SSH_AUTH_SOCK").then(|| "from-launchd".to_string());
        assert_eq!(
            resolve_one_check_env_var("SSH_AUTH_SOCK", &process, &launchd),
            Some(ResolvedCheckEnvValue::Process("from-process".to_string())),
            "a value already in zirv's own process environment must win outright"
        );
    }

    #[test]
    fn resolve_one_check_env_var_falls_back_to_launchd_only_when_process_env_is_absent() {
        let no_process = |_: &str| None;
        let launchd = |name: &str| (name == "SSH_AUTH_SOCK").then(|| "from-launchd".to_string());
        assert_eq!(
            resolve_one_check_env_var("SSH_AUTH_SOCK", &no_process, &launchd),
            Some(ResolvedCheckEnvValue::Launchd("from-launchd".to_string())),
            "absent from the process env must fall back to the login session"
        );
    }

    #[test]
    fn resolve_one_check_env_var_is_none_when_both_resolvers_come_up_empty() {
        let no_process = |_: &str| None;
        let no_launchd = |_: &str| None;
        assert_eq!(
            resolve_one_check_env_var("SSH_AUTH_SOCK", &no_process, &no_launchd),
            None,
            "neither source has it, so the child's copy stays unset"
        );
    }

    /// A blank/whitespace-only `launchctl getenv` result (the shape an unset
    /// login-session variable actually prints) must not be treated as a real
    /// value -- otherwise every unset variable would resolve to an empty
    /// string on the check child instead of staying unset.
    #[test]
    fn resolve_one_check_env_var_treats_a_blank_launchd_result_as_absent() {
        let no_process = |_: &str| None;
        let blank_launchd = |name: &str| (name == "SSH_AUTH_SOCK").then(|| "   ".to_string());
        assert_eq!(
            resolve_one_check_env_var("SSH_AUTH_SOCK", &no_process, &blank_launchd),
            None
        );
    }

    /// On every platform but macOS, the real `launchd_getenv` is always
    /// `None` and never spawns `launchctl` -- this is the seam
    /// `resolve_one_check_env_var`'s fake-resolver tests above stand in for
    /// in `run_check` itself.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn launchd_getenv_is_always_none_off_macos() {
        assert_eq!(launchd_getenv("SSH_AUTH_SOCK"), None);
    }

    // -- #215: baseline-waivable test gate ---------------------------------

    #[test]
    fn cargo_test_failure_names_are_parsed_from_the_final_summary_not_the_verbose_dump() {
        let output = "running 2 tests\n\
test wrap::tests::a ... FAILED\n\
test wrap::tests::b ... ok\n\
\n\
failures:\n\
\n\
---- wrap::tests::a stdout ----\n\
thread 'wrap::tests::a' panicked at src/lib.rs:1:\n\
    left: 1\n\
   right: 2\n\
\n\
\n\
failures:\n\
    wrap::tests::a\n\
\n\
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let names = parse_cargo_test_failure_names(output);
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["wrap::tests::a".to_string()])
        );
    }

    #[test]
    fn cargo_test_failure_names_are_unioned_across_multiple_binaries() {
        let output = "\
failures:\n\
    bin_one::tests::x\n\
\n\
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
\n\
running 1 test\n\
test bin_two::tests::y ... FAILED\n\
\n\
failures:\n\
    bin_two::tests::y\n\
\n\
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let names = parse_cargo_test_failure_names(output);
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "bin_one::tests::x".to_string(),
                "bin_two::tests::y".to_string()
            ])
        );
    }

    #[test]
    fn cargo_test_failure_names_are_empty_when_there_is_no_failures_section() {
        let output = "running 1 test\ntest tests::a ... ok\n\ntest result: ok. 1 passed; 0 failed; \
                       0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        assert!(parse_cargo_test_failure_names(output).is_empty());
    }

    fn unit_check(id: &str, status: CheckStatus, failure_output: Option<&str>) -> CheckResult {
        CheckResult {
            id: id.into(),
            kind: CheckKind::Unit,
            command: "cargo test".into(),
            source: CheckSource::DiscoveredToolchain,
            status,
            exit_code: (status != CheckStatus::Passed).then_some(101),
            duration_ms: 1,
            failure_output: failure_output.map(str::to_string),
            failure_test_names: Vec::new(),
            inconclusive_reason: None,
        }
    }

    fn report_with_checks(checks: Vec<CheckResult>) -> VerificationReport {
        VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: "evaluate".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: PathBuf::from("/repo"),
            change_fingerprint: 1,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks,
        }
    }

    fn cargo_failure_output(name: &str) -> String {
        format!(
            "failures:\n    {name}\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 \
             measured; 0 filtered out; finished in 0.00s\n"
        )
    }

    fn baseline_of(names: &[&str]) -> TestBaseline {
        TestBaseline {
            schema_version: TEST_BASELINE_SCHEMA_VERSION,
            failing_tests: names.iter().map(|name| name.to_string()).collect(),
            recorded_at: 0,
            green_streaks: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn a_passing_report_gates_regardless_of_any_baseline() {
        let report = report_with_checks(vec![unit_check("test", CheckStatus::Passed, None)]);
        let evaluation = report.evaluate_against_baseline(None);
        assert!(evaluation.gate_passed);
        assert!(evaluation.waived.is_empty());
    }

    #[test]
    fn an_empty_baseline_keeps_the_gate_strict() {
        let report = report_with_checks(vec![unit_check(
            "test",
            CheckStatus::Failed,
            Some(&cargo_failure_output("wrap::tests::a")),
        )]);
        let evaluation = report.evaluate_against_baseline(None);
        assert!(
            !evaluation.gate_passed,
            "no baseline must never waive anything"
        );
        assert!(
            evaluation
                .blocking
                .iter()
                .any(|line| line.contains("wrap::tests::a"))
        );
    }

    #[test]
    fn a_failure_set_that_is_a_subset_of_the_baseline_passes_and_reports_the_waiver() {
        let report = report_with_checks(vec![unit_check(
            "test",
            CheckStatus::Failed,
            Some(&cargo_failure_output("wrap::tests::a")),
        )]);
        let baseline = baseline_of(&["wrap::tests::a", "win::tests::b"]);
        let evaluation = report.evaluate_against_baseline(Some(&baseline));
        assert!(evaluation.gate_passed);
        assert_eq!(evaluation.waived, vec!["wrap::tests::a".to_string()]);
    }

    #[test]
    fn a_failure_not_in_the_baseline_blocks_the_gate_and_names_the_new_failure() {
        let output = "failures:\n    wrap::tests::a\n    new::regression\n\ntest result: FAILED. 0 \
                       passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let report =
            report_with_checks(vec![unit_check("test", CheckStatus::Failed, Some(output))]);
        let baseline = baseline_of(&["wrap::tests::a"]);
        let evaluation = report.evaluate_against_baseline(Some(&baseline));
        assert!(!evaluation.gate_passed);
        assert!(
            evaluation
                .blocking
                .iter()
                .any(|line| line.contains("new::regression")),
            "got {:?}",
            evaluation.blocking
        );
    }

    #[test]
    fn a_non_unit_check_failure_is_never_waivable_even_with_a_matching_baseline() {
        let report = report_with_checks(vec![CheckResult {
            id: "clippy".into(),
            kind: CheckKind::Lint,
            command: "cargo clippy".into(),
            source: CheckSource::DiscoveredToolchain,
            status: CheckStatus::Failed,
            exit_code: Some(1),
            duration_ms: 1,
            failure_output: Some("warning: unused variable".into()),
            failure_test_names: Vec::new(),
            inconclusive_reason: None,
        }]);
        // Even a baseline that happens to contain the exact failure text must
        // not waive a check that names no individual tests at all.
        let baseline = baseline_of(&["warning: unused variable"]);
        let evaluation = report.evaluate_against_baseline(Some(&baseline));
        assert!(!evaluation.gate_passed);
    }

    #[test]
    fn a_timed_out_unit_check_is_never_waivable() {
        let report = report_with_checks(vec![unit_check("test", CheckStatus::TimedOut, None)]);
        let baseline = baseline_of(&["wrap::tests::a"]);
        let evaluation = report.evaluate_against_baseline(Some(&baseline));
        assert!(!evaluation.gate_passed);
    }

    #[test]
    fn baseline_round_trips_through_the_operator_home_directory() {
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = PathBuf::from("/some/repo");
        let failing: std::collections::BTreeSet<String> =
            ["b::two", "a::one"].iter().map(|s| s.to_string()).collect();
        let saved = save_baseline(&repo, failing).expect("save baseline");
        assert_eq!(
            saved.failing_tests,
            vec!["a::one", "b::two"],
            "stored sorted"
        );

        let loaded = load_baseline(&repo)
            .expect("load baseline")
            .expect("baseline exists");
        assert_eq!(loaded.failing_tests, saved.failing_tests);
        assert_eq!(loaded.recorded_at, saved.recorded_at);

        let path = home
            .path()
            .join(".zirv")
            .join("test-baseline")
            .join(format!("{}.json", repo_slug(&repo)));
        assert!(
            path.exists(),
            "baseline must live under ~/.zirv/, not the repo"
        );
    }

    #[test]
    fn no_baseline_file_loads_as_none() {
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        assert!(
            load_baseline(&PathBuf::from("/never/recorded"))
                .unwrap()
                .is_none()
        );
    }

    /// #238: `evaluate_against_operator_baseline` is the exact seam
    /// `latest_is_fresh_and_passing` (the gate) and `review::package`'s
    /// `VerificationEvidence` (the review package) now share -- a report
    /// whose only failure is covered by the recorded baseline evaluates to
    /// `gate_passed:true` with the sorted waived name(s).
    #[test]
    fn evaluate_against_operator_baseline_reports_sorted_waived_names_for_a_covered_failure() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let output = "failures:\n    b::two\n    a::one\n\ntest result: FAILED. 0 passed; 2 \
                       failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let report =
            report_with_checks(vec![unit_check("test", CheckStatus::Failed, Some(output))]);
        save_baseline(
            repo.path(),
            std::collections::BTreeSet::from(["a::one".to_string(), "b::two".to_string()]),
        )
        .unwrap();

        let evaluation = evaluate_against_operator_baseline(&report, repo.path());
        assert!(evaluation.gate_passed);
        assert_eq!(
            evaluation.waived,
            vec!["a::one".to_string(), "b::two".to_string()],
            "waived names must be sorted"
        );
    }

    /// #238: with no baseline recorded at all (or an unreadable one), this
    /// must degrade to strict "no baseline" behavior -- identical to what the
    /// gate has always done -- never a silent pass.
    #[test]
    fn evaluate_against_operator_baseline_is_strict_with_no_baseline_recorded() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let report = report_with_checks(vec![unit_check(
            "test",
            CheckStatus::Failed,
            Some(&cargo_failure_output("wrap::tests::a")),
        )]);

        let evaluation = evaluate_against_operator_baseline(&report, repo.path());
        assert!(!evaluation.gate_passed);
        assert!(evaluation.waived.is_empty());
    }

    /// End-to-end through the real gate seam (`latest_is_fresh_and_passing`,
    /// the exact function `engine.rs`'s test-step gate and `deploy.rs`'s
    /// production gate both call): a baseline recorded for this repository
    /// lets a report whose only failure is in that baseline satisfy the gate.
    #[test]
    fn the_freshness_gate_accepts_a_report_whose_failures_are_baselined() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_root = tempdir().unwrap();
        let state = StateDir::from_root(state_root.path().to_path_buf());
        let fingerprint = change_fingerprint(repo.path()).unwrap();

        let mut report = report_with_checks(vec![unit_check(
            "test",
            CheckStatus::Failed,
            Some(&cargo_failure_output("wrap::tests::a")),
        )]);
        report.repo = repo.path().to_path_buf();
        report.mode = VerificationMode::Final;
        report.change_fingerprint = fingerprint;
        save_report(&state, &report).unwrap();

        assert!(
            !latest_is_fresh_and_passing(&state, repo.path(), true).unwrap(),
            "no baseline recorded yet: must still be strict"
        );

        save_baseline(
            repo.path(),
            std::collections::BTreeSet::from(["wrap::tests::a".to_string()]),
        )
        .unwrap();
        assert!(
            latest_is_fresh_and_passing(&state, repo.path(), true).unwrap(),
            "the same failure, now baselined, satisfies the gate"
        );
    }

    /// Regression for a real `zirv test baseline` run on this Windows host
    /// (recorded 2026-08-30, repo slug
    /// `D--GitHub-zirv-dynamic-cli--claude-worktrees-fix-bug-batch-213-215-
    /// 218-203`, ts 1788109790): `cargo test --verbose` genuinely failed
    /// `commands::ctx::wrap::tests::win::a_supervised_wrap_binds_a_turn_
    /// signal_transport`, but the check's persisted `failure_output` -- the
    /// `MAX_FAILURE_OUTPUT_BYTES` capped *display* tail -- contained no
    /// `failures:` text anywhere at all (confirmed by grepping the actual
    /// stored report file byte-for-byte). A later-spawned subprocess in the
    /// same test run inherited the real stdout fd and printed far more than
    /// the cap afterwards, evicting the already-printed `failures:`/`test
    /// result: FAILED` summary entirely -- CRLF line endings throughout,
    /// since this is real cmd.exe/Cargo-on-Windows output.
    /// `parse_cargo_test_failure_names` against that capped text -- the only
    /// thing `run_baseline` consulted before this fix -- necessarily
    /// returned nothing, which is exactly the "0 failing test name(s)
    /// recorded" the operator saw. This test reproduces that exact shape (a
    /// real CRLF summary immediately followed by a same-stream flood that
    /// exceeds the cap) directly against the capture seam
    /// (`read_capped_tail_and_scan`, backed by `FailureNameScanner`), and
    /// proves the name is recovered even though the display tail still
    /// legitimately loses it.
    #[test]
    fn a_late_output_flood_cannot_evict_the_earlier_failing_name_from_capture() {
        let real_name =
            "commands::ctx::wrap::tests::win::a_supervised_wrap_binds_a_turn_signal_transport";
        let mut input = Vec::new();
        input.extend_from_slice(b"running 1 test\r\n");
        input.extend_from_slice(format!("test {real_name} ... FAILED\r\n").as_bytes());
        input.extend_from_slice(b"\r\nfailures:\r\n\r\n");
        input.extend_from_slice(format!("---- {real_name} stdout ----\r\n").as_bytes());
        input.extend_from_slice(b"thread panicked at src\\commands\\ctx\\wrap.rs:1:\r\n\r\n");
        input.extend_from_slice(b"failures:\r\n");
        input.extend_from_slice(format!("    {real_name}\r\n").as_bytes());
        input.extend_from_slice(
            b"\r\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered \
              out; finished in 7.04s\r\n",
        );
        // A later-spawned subprocess (a stubbed Claude/Codex launch, in the
        // real run) inheriting the real stdout fd and printing far more than
        // the cap -- the actual defect, not a contrived worst case. The cap
        // is on the *whole* stream's last `MAX_FAILURE_OUTPUT_BYTES` bytes,
        // so the filler alone (not the filler plus the block) must exceed
        // the cap for the block to be fully outside that window.
        let after_block = input.len();
        while input.len() - after_block <= MAX_FAILURE_OUTPUT_BYTES {
            input.extend_from_slice(b"[17:08:14] zirv \xe2\x96\xb8 sandbox posture noise\r\n");
        }

        let (retained, errored, names, _summary_seen, _summary_total) =
            read_capped_tail_and_scan(std::io::Cursor::new(&input), MAX_FAILURE_OUTPUT_BYTES);
        assert!(!errored);
        let retained_text = String::from_utf8_lossy(&retained);
        assert!(
            !retained_text.contains("failures:"),
            "sanity check: the capped display tail must actually have been evicted by the \
             flood, reproducing the real bug -- got {retained_text:?}"
        );
        assert!(
            names.contains(real_name),
            "the streaming scan must recover the name even though the display tail lost it: \
             {names:?}"
        );

        // The same recovery must reach the gate/baseline seam, not just the
        // scanner in isolation.
        let check = CheckResult {
            id: "test".into(),
            kind: CheckKind::Unit,
            command: "cargo test --verbose -- --test-threads=1".into(),
            source: CheckSource::DiscoveredToolchain,
            status: CheckStatus::Failed,
            exit_code: Some(101),
            duration_ms: 1,
            failure_output: Some(retained_text.into_owned()),
            failure_test_names: names.into_iter().collect(),
            inconclusive_reason: None,
        };
        assert!(
            failure_names_for(&check).contains(real_name),
            "evaluate_against_baseline/run_baseline must prefer the recovered name"
        );
    }

    /// #215 follow-up (security): a check whose command text was written in
    /// the repository's own `.zirv/verify.toml` is UNTRUSTED -- it must
    /// never be able to widen the operator's baseline by printing forged
    /// cargo-shaped `failures:`/`test result: FAILED` output for a test name
    /// the repository invents (and could later actually break, now
    /// pre-waived). `run_baseline` must record nothing from it, no matter
    /// how convincing its output looks.
    #[test]
    fn a_repo_defined_checks_forged_failure_output_cannot_enter_the_baseline() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_root = tempdir().unwrap();
        let forged_command = if cfg!(windows) {
            "echo failures: && echo     fake::forged && echo test result: FAILED \
             forged-by-repo-check && exit /b 101"
        } else {
            "printf \"failures:\\n    fake::forged\\ntest result: FAILED \
             forged-by-repo-check\\n\"; exit 101"
        };
        write_verify_toml(
            repo.path(),
            &format!(
                "schema_version=1\n[[checks]]\nid='forged'\nkind='unit'\ncommand='{forged_command}'\n"
            ),
        );
        let mut out = Vec::new();
        with_state(state_root.path(), || {
            run_baseline(
                repo.path(),
                &BaselineArgs {
                    run: RunArgs {
                        repo: None,
                        checks: vec![],
                        dry_run: false,
                        json: false,
                    },
                    prune: false,
                },
                &mut out,
            )
            .expect("run baseline")
        });
        let baseline = load_baseline(repo.path())
            .unwrap()
            .expect("a baseline is still recorded, just an empty one");
        assert!(
            !baseline
                .failing_tests
                .iter()
                .any(|name| name.contains("forged")),
            "a repository-authored check must never widen the operator's baseline: {:?}",
            baseline.failing_tests
        );
    }

    /// A check whose output stream contains no newlines at all (a hung or
    /// misbehaving tool writing an ever-growing single line) must not grow
    /// `FailureNameScanner::partial` without bound -- that would defeat the
    /// memory cap the 16 KiB display tail is supposed to provide. Feed
    /// several hundred KB across many chunks, entirely newline-free, and
    /// assert the scanner's internal buffer stays bounded (not just that it
    /// eventually finishes) and that no names are ever produced from it.
    #[test]
    fn a_newline_less_flood_keeps_the_scanners_partial_buffer_bounded() {
        let mut scanner = FailureNameScanner::default();
        let chunk = vec![b'x'; 4096];
        for _ in 0..128 {
            scanner.feed(&chunk);
            assert!(
                scanner.partial.len() <= FailureNameScanner::MAX_PARTIAL_LINE_BYTES,
                "partial must never be allowed to grow past the bound: got {} bytes",
                scanner.partial.len()
            );
        }
        // Fed 128 * 4096 = 512 KiB total with never a single newline.
        let (names, _summary_seen, _summary_total) = scanner.finish();
        assert!(
            names.is_empty(),
            "a newline-less flood can never contain a real failing test name: {names:?}"
        );
    }

    /// `test result: FAILED` with no preceding `failures:` line since the
    /// last reset must drain nothing into `names` -- otherwise arbitrary
    /// non-summary output lines that merely happened to precede an
    /// unrelated `test result: FAILED` line (from a different tool, or a
    /// `failures:` header lost to some earlier eviction) could be
    /// misreported as failing test names.
    #[test]
    fn test_result_failed_without_a_failures_header_yields_no_names() {
        let mut scanner = FailureNameScanner::default();
        scanner.feed(b"running 1 test\n");
        scanner.feed(b"not_actually_a_test_name\n");
        scanner.feed(b"neither is this one\n");
        scanner.feed(
            b"test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; \
              finished in 0.01s\n",
        );
        let (names, _summary_seen, _summary_total) = scanner.finish();
        assert!(
            names.is_empty(),
            "a `test result: FAILED` with no preceding `failures:` header must yield no names: \
             {names:?}"
        );
    }

    /// Multi-binary behavior must still work with the `failures:`-header
    /// gate in place: each binary's own `failures:` ... `test result:
    /// FAILED` block is independently recognized, and a block missing the
    /// header (simulating a binary whose `failures:` line was lost) drains
    /// nothing while leaving the properly-headered blocks around it intact.
    #[test]
    fn multi_binary_blocks_each_require_their_own_failures_header() {
        let mut scanner = FailureNameScanner::default();
        // First binary: proper `failures:` header, one real name.
        scanner.feed(b"failures:\n");
        scanner.feed(b"    crate_a::tests::first_failure\n");
        scanner.feed(b"test result: FAILED. 0 passed; 1 failed\n");
        // Second binary: no `failures:` header at all -- must contribute
        // nothing, even though it also ends in `test result: FAILED`.
        scanner.feed(b"some_unrelated_line\n");
        scanner.feed(b"test result: FAILED. 0 passed; 1 failed\n");
        // Third binary: proper header again, a different real name.
        scanner.feed(b"failures:\n");
        scanner.feed(b"    crate_b::tests::second_failure\n");
        scanner.feed(b"test result: FAILED. 0 passed; 1 failed\n");
        let (names, _summary_seen, _summary_total) = scanner.finish();
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "crate_a::tests::first_failure".to_string(),
                "crate_b::tests::second_failure".to_string(),
            ]),
            "each binary's block must be judged independently by its own header: {names:?}"
        );
    }

    // -- Issue #268: three-valued gate outcomes -----------------------------

    /// The STATUS_ACCESS_VIOLATION trap this issue exists to close: a
    /// crashed `cargo test` run leaves partial per-test lines but never
    /// reaches its own `test result:` summary, and exits non-zero. A bare
    /// exit-code check reads this exactly like an empty (i.e. clean) failure
    /// set; `classify_test_report` must call it `Inconclusive`, never `None`
    /// (which would let `run_check` leave it `Passed`/`Failed` off the raw
    /// exit code alone).
    #[test]
    fn a_captured_status_access_violation_transcript_classifies_as_runner_crashed() {
        let output = include_str!("../../../tests/fixtures/test-reports/cargo-test-crash.txt");
        assert_eq!(
            classify_test_report(output, false),
            Some(InconclusiveReason::RunnerCrashed)
        );
    }

    /// An empty nextest filter -- `0 tests run` with a *successful* exit --
    /// must never read as "the change set is clean"; it read nothing at
    /// all.
    #[test]
    fn an_empty_nextest_filter_classifies_as_no_tests_selected() {
        let output = include_str!("../../../tests/fixtures/test-reports/nextest-empty-filter.txt");
        assert_eq!(
            classify_test_report(output, true),
            Some(InconclusiveReason::NoTestsSelected)
        );
    }

    /// The same zero-tests summary with a *failing* exit is not "empty
    /// selection" -- nothing selected does not normally fail -- it is
    /// treated as a crash instead.
    #[test]
    fn a_zero_tests_summary_with_a_failing_exit_is_runner_crashed_not_no_tests_selected() {
        let output = include_str!("../../../tests/fixtures/test-reports/nextest-empty-filter.txt");
        assert_eq!(
            classify_test_report(output, false),
            Some(InconclusiveReason::RunnerCrashed)
        );
    }

    /// A well-formed clean run (either runner) is never `Inconclusive`: a
    /// real summary declaring at least one test run must defer entirely to
    /// the ordinary exit-code verdict.
    #[test]
    fn well_formed_reports_with_a_real_summary_are_never_inconclusive() {
        let cargo_clean = include_str!("../../../tests/fixtures/test-reports/cargo-test-clean.txt");
        assert_eq!(classify_test_report(cargo_clean, true), None);

        let cargo_failed =
            include_str!("../../../tests/fixtures/test-reports/cargo-test-failure.txt");
        assert_eq!(classify_test_report(cargo_failed, false), None);

        let nextest_clean = include_str!("../../../tests/fixtures/test-reports/nextest-clean.txt");
        assert_eq!(classify_test_report(nextest_clean, true), None);
    }

    /// A successful exit with no summary line at all (neither a crash --
    /// exit succeeded -- nor a recognizable empty-selection summary) is the
    /// residual `ReportUnparseable` case.
    #[test]
    fn a_successful_exit_with_no_summary_at_all_is_report_unparseable() {
        assert_eq!(
            classify_test_report("nothing recognizable here\n", true),
            Some(InconclusiveReason::ReportUnparseable)
        );
    }

    #[test]
    fn is_cargo_test_runner_matches_both_known_runners_and_nothing_else() {
        assert!(is_cargo_test_runner("cargo test --verbose"));
        assert!(is_cargo_test_runner("cargo nextest run --no-fail-fast"));
        assert!(!is_cargo_test_runner("npm run test"));
        assert!(!is_cargo_test_runner("cargo build"));
    }

    #[test]
    fn looks_like_tool_missing_matches_exit_127_and_shell_messages() {
        assert!(looks_like_tool_missing(Some(127), ""));
        assert!(looks_like_tool_missing(
            Some(1),
            "sh: some-tool: command not found"
        ));
        assert!(looks_like_tool_missing(
            Some(1),
            "'some-tool' is not recognized as an internal or external command"
        ));
        assert!(!looks_like_tool_missing(Some(1), "assertion failed"));
    }

    /// `classify_gate_status` is the seam `run_check` actually calls: a
    /// non-runner `Unit` check (`npm run test`, say) must never be
    /// reclassified by `classify_test_report` at all, even if its output
    /// happens to contain no cargo-shaped summary -- only `cargo test`/
    /// `cargo nextest run` commands are in scope (`is_cargo_test_runner`).
    #[test]
    fn classify_gate_status_never_touches_a_non_runner_unit_check() {
        let (status, reason) = classify_gate_status(
            CheckStatus::Passed,
            Some(0),
            "jest output, no cargo shape",
            false,
            None,
        );
        assert_eq!(status, CheckStatus::Passed);
        assert_eq!(reason, None);
    }

    #[test]
    fn classify_gate_status_reclassifies_a_crashed_test_runner_check() {
        let output = include_str!("../../../tests/fixtures/test-reports/cargo-test-crash.txt");
        let total = extract_test_total(output);
        let (status, reason) =
            classify_gate_status(CheckStatus::Failed, Some(1), output, true, total);
        assert_eq!(status, CheckStatus::Inconclusive);
        assert_eq!(reason, Some(InconclusiveReason::RunnerCrashed));
    }

    #[test]
    fn classify_gate_status_reclassifies_exit_127_regardless_of_check_kind() {
        let (status, reason) =
            classify_gate_status(CheckStatus::Failed, Some(127), "", false, None);
        assert_eq!(status, CheckStatus::Inconclusive);
        assert_eq!(reason, Some(InconclusiveReason::ToolMissing));
    }

    /// `classify_gate_status` must classify off the total the full,
    /// uncapped stream declared, not off `output`'s own (possibly
    /// summary-less) capped display text -- a doubly-capped `output`
    /// buffer that lost its summary line entirely to noise must not, on its
    /// own, produce `Inconclusive` once the real full-stream total is
    /// supplied. See `a_late_output_flood_cannot_evict_the_earlier_
    /// summary_line_from_capture` below for the actual capping/eviction
    /// this seam is exercised against in `run_check`.
    #[test]
    fn classify_gate_status_is_immune_to_the_capped_display_text_losing_the_summary() {
        let output_with_no_summary: String = "noisy compiler output\n".repeat(50);
        assert_eq!(extract_test_total(&output_with_no_summary), None);

        let (passed_status, passed_reason) = classify_gate_status(
            CheckStatus::Passed,
            Some(0),
            &output_with_no_summary,
            true,
            Some(3),
        );
        assert_eq!(passed_status, CheckStatus::Passed, "{passed_reason:?}");
        assert_eq!(passed_reason, None);

        let (failed_status, failed_reason) = classify_gate_status(
            CheckStatus::Failed,
            Some(101),
            &output_with_no_summary,
            true,
            Some(2),
        );
        assert_eq!(failed_status, CheckStatus::Failed, "{failed_reason:?}");
        assert_eq!(failed_reason, None);
    }

    /// The actual bug this issue's review caught: `run_check` capped each
    /// stream's own tail to `MAX_FAILURE_OUTPUT_BYTES`, concatenated
    /// stdout-then-stderr, then capped the result a *second* time from the
    /// front -- so more than `MAX_FAILURE_OUTPUT_BYTES` of noise on either
    /// stream *ahead of* a valid summary line could evict the summary from
    /// the final display buffer entirely, exactly like
    /// `a_late_output_flood_cannot_evict_the_earlier_failing_name_from_
    /// capture` already proved for failing test names. `FailureNameScanner`
    /// must recover the summary the identical way: from the full,
    /// pre-capping stream as it is fed in, immune to the capped tail's own
    /// eviction.
    #[test]
    fn a_late_output_flood_cannot_evict_the_earlier_summary_line_from_capture() {
        let mut input = Vec::new();
        input.extend_from_slice(
            b"running 3 tests\ntest tests::a ... ok\ntest tests::b ... ok\ntest tests::c ... ok\n\n",
        );
        input.extend_from_slice(
            b"test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; \
              finished in 0.01s\n",
        );
        let after_summary = input.len();
        while input.len() - after_summary <= MAX_FAILURE_OUTPUT_BYTES {
            input.extend_from_slice(b"[compiler] warning: unused import noise\n");
        }

        let (retained, errored, _names, summary_seen, summary_total) =
            read_capped_tail_and_scan(std::io::Cursor::new(&input), MAX_FAILURE_OUTPUT_BYTES);
        assert!(!errored);
        let retained_text = String::from_utf8_lossy(&retained);
        assert!(
            !retained_text.contains("test result:"),
            "sanity check: the capped display tail must actually have evicted the summary, \
             reproducing the real bug -- got {retained_text:?}"
        );
        assert!(
            summary_seen,
            "the streaming scan must recover the summary even though the display tail lost it"
        );
        assert_eq!(summary_total, 3);
        assert_eq!(
            classify_test_outcome(Some(summary_total), true),
            None,
            "a recovered real summary must never classify as Inconclusive"
        );
    }

    /// `VerificationReport::outcome`: a genuinely `Failed` check always wins
    /// over an `Inconclusive` one on the same run -- real evidence something
    /// broke must never be reported as "proves nothing" just because another
    /// check on the same run happened to be inconclusive.
    #[test]
    fn report_outcome_prefers_fail_over_inconclusive_in_a_mixed_report() {
        let report = report_with_checks(vec![
            unit_check("clippy-ish", CheckStatus::Failed, None),
            CheckResult {
                id: "test".into(),
                kind: CheckKind::Unit,
                command: "cargo test".into(),
                source: CheckSource::DiscoveredToolchain,
                status: CheckStatus::Inconclusive,
                exit_code: None,
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: Some(InconclusiveReason::RunnerCrashed),
            },
        ]);
        assert_eq!(report.outcome(), GateOutcome::Fail);
    }

    /// A report with no genuine `Failed` check but at least one
    /// `Inconclusive` one is still `Inconclusive`, not `Fail` -- the ranking
    /// only promotes `Fail` above `Inconclusive` when a real failure is
    /// actually present.
    #[test]
    fn report_outcome_is_inconclusive_with_no_failed_check_present() {
        let report = report_with_checks(vec![
            unit_check("format", CheckStatus::Passed, None),
            CheckResult {
                id: "test".into(),
                kind: CheckKind::Unit,
                command: "cargo test".into(),
                source: CheckSource::DiscoveredToolchain,
                status: CheckStatus::Inconclusive,
                exit_code: None,
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: Some(InconclusiveReason::NoTestsSelected),
            },
        ]);
        assert_eq!(
            report.outcome(),
            GateOutcome::Inconclusive(InconclusiveReason::NoTestsSelected)
        );
    }

    #[test]
    fn report_outcome_is_pass_only_when_every_check_passed() {
        let passing = report_with_checks(vec![unit_check("test", CheckStatus::Passed, None)]);
        assert_eq!(passing.outcome(), GateOutcome::Pass);

        let failing = report_with_checks(vec![unit_check(
            "test",
            CheckStatus::Failed,
            Some(&cargo_failure_output("wrap::tests::a")),
        )]);
        assert_eq!(failing.outcome(), GateOutcome::Fail);
    }

    /// The core acceptance criterion: `latest_is_fresh_and_passing` must
    /// return `false` for a fresh, un-narrowed report whose only check is
    /// `Inconclusive` -- it must never be read as passing just because
    /// `evaluate_against_baseline`'s existing waiver path only knows how to
    /// waive a `Failed` `Unit` check, not an `Inconclusive` one.
    #[test]
    fn latest_is_fresh_and_passing_returns_false_for_an_inconclusive_report() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        let state = StateDir::from_root(state_root.path().to_path_buf());
        let fingerprint = change_fingerprint(repo.path()).unwrap();
        let report = VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: "inconclusive".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![CheckResult {
                id: "test".into(),
                kind: CheckKind::Unit,
                command: "cargo test".into(),
                source: CheckSource::DiscoveredToolchain,
                status: CheckStatus::Inconclusive,
                exit_code: Some(1),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: Some(InconclusiveReason::RunnerCrashed),
            }],
        };
        save_report(&state, &report).unwrap();
        assert!(
            !latest_is_fresh_and_passing(&state, repo.path(), true).unwrap(),
            "an Inconclusive report must never satisfy the gate"
        );

        // Even with the exact same failing (absent) names as a recorded
        // baseline would waive, `Inconclusive` is never eligible for that
        // waiver -- `evaluate_against_baseline` only ever waives a `Failed`
        // `Unit` check with parseable names.
        let evaluation = evaluate_against_operator_baseline(&report, repo.path());
        assert!(!evaluation.gate_passed);
    }

    /// The `proves:`/`fix:` announcement must name the reason and never
    /// claim the run proved anything.
    #[test]
    fn gate_announcement_names_the_inconclusive_reason() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        let state = StateDir::from_root(state_root.path().to_path_buf());
        let fingerprint = change_fingerprint(repo.path()).unwrap();
        let report = VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: "inconclusive".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![CheckResult {
                id: "test".into(),
                kind: CheckKind::Unit,
                command: "cargo test".into(),
                source: CheckSource::DiscoveredToolchain,
                status: CheckStatus::Inconclusive,
                exit_code: Some(1),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: Some(InconclusiveReason::RunnerCrashed),
            }],
        };
        save_report(&state, &report).unwrap();
        let announcement = gate_announcement(&state, repo.path(), true);
        assert!(announcement.contains("proves: nothing"), "{announcement}");
        assert!(announcement.contains("runner-crashed"), "{announcement}");
        assert!(announcement.contains("fix:"), "{announcement}");
    }

    /// A mixed run (one check genuinely failed, another's runner crashed)
    /// announces `Fail`, not `Inconclusive` -- but must still name the
    /// inconclusive check by id rather than silently dropping it, since a
    /// plain "Fail" alone would wrongly imply every check produced real
    /// evidence.
    #[test]
    fn gate_announcement_lists_inconclusive_checks_alongside_a_real_failure() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        let state = StateDir::from_root(state_root.path().to_path_buf());
        let fingerprint = change_fingerprint(repo.path()).unwrap();
        let report = VerificationReport {
            schema_version: VERIFY_REPORT_SCHEMA_VERSION,
            id: "mixed".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![
                unit_check("clippy", CheckStatus::Failed, Some("warning: unused")),
                CheckResult {
                    id: "test".into(),
                    kind: CheckKind::Unit,
                    command: "cargo test".into(),
                    source: CheckSource::DiscoveredToolchain,
                    status: CheckStatus::Inconclusive,
                    exit_code: Some(1),
                    duration_ms: 1,
                    failure_output: None,
                    failure_test_names: Vec::new(),
                    inconclusive_reason: Some(InconclusiveReason::RunnerCrashed),
                },
            ],
        };
        save_report(&state, &report).unwrap();
        let announcement = gate_announcement(&state, repo.path(), true);
        assert!(announcement.contains("gate: Fail"), "{announcement}");
        assert!(
            announcement.contains("also inconclusive") && announcement.contains("test"),
            "must still name the inconclusive check: {announcement}"
        );
    }

    /// An operator-recorded baseline written before `green_streaks` existed
    /// must still load, with every name starting at 0 greens rather than
    /// failing to deserialize outright.
    #[test]
    fn a_pre_268_baseline_file_without_green_streaks_still_loads() {
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = PathBuf::from("/some/old/repo");
        let dir = home.path().join(".zirv").join("test-baseline");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", repo_slug(&repo))),
            r#"{"schema_version":1,"failing_tests":["a::one","b::two"],"recorded_at":123}"#,
        )
        .unwrap();

        let loaded = load_baseline(&repo).expect("load").expect("exists");
        assert_eq!(loaded.failing_tests, vec!["a::one", "b::two"]);
        assert!(loaded.green_streaks.is_empty());
    }

    /// Baseline hygiene end to end: three consecutive runs in which a
    /// baselined name does not appear among the observed failures advance
    /// its streak to the prune threshold; `--prune` removes exactly that
    /// name (leaving an untouched one behind), and a run where it *does*
    /// fail again resets the streak to 0.
    #[test]
    fn three_green_runs_make_a_name_prune_eligible_and_prune_removes_exactly_it() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        save_baseline(
            repo.path(),
            std::collections::BTreeSet::from([
                "wrap::tests::a".to_string(),
                "wrap::tests::b".to_string(),
            ]),
        )
        .unwrap();

        // A clean `Unit` run: neither baselined name appears among this
        // run's failures, so both should be advancing.
        let clean_report = report_with_checks(vec![unit_check("test", CheckStatus::Passed, None)]);
        for _ in 0..PRUNE_AFTER_GREENS {
            update_baseline_after_run(repo.path(), &clean_report);
        }
        let baseline = load_baseline(repo.path()).unwrap().unwrap();
        assert_eq!(
            baseline.green_streaks.get("wrap::tests::a").copied(),
            Some(PRUNE_AFTER_GREENS)
        );
        assert_eq!(
            baseline.green_streaks.get("wrap::tests::b").copied(),
            Some(PRUNE_AFTER_GREENS)
        );

        let mut out = Vec::new();
        run_baseline_prune(repo.path(), &mut out).unwrap();
        let pruned = String::from_utf8(out).unwrap();
        assert!(pruned.contains("wrap::tests::a"));
        assert!(pruned.contains("wrap::tests::b"));
        let baseline = load_baseline(repo.path()).unwrap().unwrap();
        assert!(
            baseline.failing_tests.is_empty(),
            "both matured entries must be gone: {:?}",
            baseline.failing_tests
        );
    }

    /// One red run resets the streak to 0, so an intermittently-green name
    /// never quietly matures for pruning.
    #[test]
    fn a_red_run_resets_the_green_streak() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        save_baseline(
            repo.path(),
            std::collections::BTreeSet::from(["wrap::tests::a".to_string()]),
        )
        .unwrap();

        let clean_report = report_with_checks(vec![unit_check("test", CheckStatus::Passed, None)]);
        update_baseline_after_run(repo.path(), &clean_report);
        update_baseline_after_run(repo.path(), &clean_report);
        assert_eq!(
            load_baseline(repo.path())
                .unwrap()
                .unwrap()
                .green_streaks
                .get("wrap::tests::a")
                .copied(),
            Some(2)
        );

        let red_report = report_with_checks(vec![unit_check(
            "test",
            CheckStatus::Failed,
            Some(&cargo_failure_output("wrap::tests::a")),
        )]);
        update_baseline_after_run(repo.path(), &red_report);
        assert_eq!(
            load_baseline(repo.path())
                .unwrap()
                .unwrap()
                .green_streaks
                .get("wrap::tests::a")
                .copied(),
            Some(0)
        );
    }

    /// An `Inconclusive`/`TimedOut` `Unit` check's own run must never
    /// advance (or reset) any baselined name's streak -- an unreliable run
    /// has no opinion on whether a name is actually clean.
    #[test]
    fn an_inconclusive_run_never_advances_any_green_streak() {
        let repo = git_repo();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        save_baseline(
            repo.path(),
            std::collections::BTreeSet::from(["wrap::tests::a".to_string()]),
        )
        .unwrap();

        let inconclusive_report = report_with_checks(vec![CheckResult {
            id: "test".into(),
            kind: CheckKind::Unit,
            command: "cargo test".into(),
            source: CheckSource::DiscoveredToolchain,
            status: CheckStatus::Inconclusive,
            exit_code: Some(1),
            duration_ms: 1,
            failure_output: None,
            failure_test_names: Vec::new(),
            inconclusive_reason: Some(InconclusiveReason::RunnerCrashed),
        }]);
        assert!(update_baseline_after_run(repo.path(), &inconclusive_report).is_none());
        assert_eq!(
            load_baseline(repo.path())
                .unwrap()
                .unwrap()
                .green_streaks
                .get("wrap::tests::a")
                .copied(),
            None,
            "an inconclusive run must leave the streak untouched"
        );
    }

    /// `zirv test baseline` must refuse to record from an `Inconclusive`
    /// run rather than silently treating "proves nothing" as "everything
    /// currently failing".
    #[test]
    fn run_baseline_refuses_to_record_from_an_inconclusive_run() {
        // A repo with none of the recognized project shapes (no Cargo.toml,
        // no package.json) and no `.zirv/verify.toml` resolves zero checks,
        // which `run_mode` now reports as `Inconclusive { NoChecks }` rather
        // than erroring outright.
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let mut out = Vec::new();
        let code = with_state(state_root.path(), || {
            run_baseline(
                repo.path(),
                &BaselineArgs {
                    run: RunArgs {
                        repo: None,
                        checks: vec![],
                        dry_run: false,
                        json: false,
                    },
                    prune: false,
                },
                &mut out,
            )
            .expect("must not hard-error")
        });
        assert_eq!(code, 1, "an inconclusive run must not exit 0");
        assert!(
            load_baseline(repo.path()).unwrap().is_none(),
            "nothing must be recorded from an inconclusive run"
        );
    }

    /// Issue #268's degraded-gate ban: zero checks configured or
    /// discoverable is `Inconclusive`, not a silent pass and not a hard
    /// `Err` that would brick `zirv verify` outright.
    #[test]
    fn zero_checks_is_inconclusive_not_a_silent_pass_or_a_hard_error() {
        let repo = git_repo();
        let report =
            run_mode(repo.path(), VerificationMode::Final, &[], false).expect("must not error");
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, CheckStatus::Inconclusive);
        assert_eq!(
            report.checks[0].inconclusive_reason,
            Some(InconclusiveReason::NoChecks)
        );
        assert!(!report.passed());
    }

    /// The operator-only override makes the same zero-checks run `Passed`
    /// instead, with the override's use visible in the report's notes.
    #[test]
    fn the_allow_empty_verify_override_makes_zero_checks_pass_and_says_so() {
        let repo = git_repo();
        // SAFETY: single-threaded suite (`--test-threads=1`).
        unsafe {
            std::env::set_var("ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY", "true");
        }
        let report = run_mode(repo.path(), VerificationMode::Final, &[], false);
        unsafe {
            std::env::remove_var("ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY");
        }
        let report = report.expect("must not error");
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, CheckStatus::Passed);
        assert!(report.passed());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("allow_empty_verify")),
            "the override's use must be visible in the report: {:?}",
            report.notes
        );
    }

    /// A repository's own `.zirv/ctx.toml` must never be able to set this
    /// override for itself -- `workflow.allow_empty_verify` is
    /// `REPO_FORBIDDEN` (see `config.rs`'s
    /// `repo_layer_cannot_set_workflow_allow_empty_verify`), so `CtxConfig::
    /// load` hard-errors on it exactly like `auto_spawn_on_gate`/
    /// `check_env_passthrough`, and `repo_gates` degrades that to its usual
    /// fail-closed default (`false`) rather than letting the repo's own
    /// attempt flip the gate.
    #[test]
    fn a_repo_ctx_toml_cannot_set_the_allow_empty_verify_override() {
        let repo = git_repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nallow_empty_verify = true\n",
        )
        .unwrap();
        let report =
            run_mode(repo.path(), VerificationMode::Final, &[], false).expect("must not error");
        assert_eq!(
            report.checks[0].status,
            CheckStatus::Inconclusive,
            "a repo-forbidden key set from the repo's own ctx.toml must fail closed, not flip \
             the override"
        );
    }

    /// A genuinely missing tool (not a cargo/nextest shape at all) reclassifies
    /// through the real `run_check` seam, not just the pure classifier --
    /// exercising `is_spawn_not_found`/`looks_like_tool_missing` end to end.
    #[test]
    fn a_missing_command_is_inconclusive_through_the_real_run_check_seam() {
        let repo = git_repo();
        let state_root = tempdir().unwrap();
        write_verify_toml(
            repo.path(),
            "schema_version=1\n[[checks]]\nid='missing'\nkind='custom'\ncommand='zirv-268-does-not-exist-cmd'\n",
        );
        let report = with_state(state_root.path(), || {
            run_mode(repo.path(), VerificationMode::Final, &[], false).expect("must not error")
        });
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, CheckStatus::Inconclusive);
        assert_eq!(
            report.checks[0].inconclusive_reason,
            Some(InconclusiveReason::ToolMissing)
        );
    }
}
