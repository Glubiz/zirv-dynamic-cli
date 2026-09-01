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
        if self.checks.is_empty() {
            return Err("verification config has no checks".into());
        }
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
    config
        .validate()
        .map_err(|_| -> Box<dyn std::error::Error> {
            "no verification checks configured or discoverable; add .zirv/verify.toml".into()
        })?;
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
}

fn default_check_source() -> CheckSource {
    CheckSource::RepoConfig
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
#[derive(Default)]
struct FailureNameScanner {
    names: std::collections::BTreeSet<String>,
    pending: Vec<String>,
    partial: Vec<u8>,
    partial_overflowed: bool,
    saw_failures_header: bool,
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
    /// that ends without a trailing newline) before returning the names.
    fn finish(mut self) -> std::collections::BTreeSet<String> {
        if !self.partial.is_empty() && !self.partial_overflowed {
            let line = String::from_utf8_lossy(&self.partial).into_owned();
            self.observe_line(line.trim_end_matches(['\n', '\r']));
        }
        self.names
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
}

const TEST_BASELINE_SCHEMA_VERSION: u32 = 1;

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
    let baseline = TestBaseline {
        schema_version: TEST_BASELINE_SCHEMA_VERSION,
        failing_tests: failing_tests.into_iter().collect(),
        recorded_at: now_secs(),
    };
    create_private_dir_all(&test_baseline_dir()?)?;
    write_private(
        &test_baseline_path(repo)?,
        &serde_json::to_string_pretty(&baseline)?,
    )?;
    Ok(baseline)
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
/// turned a truncated failure log into a complete-looking one), plus the
/// failing test names a [`FailureNameScanner`] recognized in the *full*,
/// uncapped stream as it went by -- see that struct's doc comment for why
/// this must happen during capture rather than against the capped tail
/// alone. Every check's output goes through this path (`run_check`); only
/// the retained-tail element is ever a display artifact.
fn read_capped_tail_and_scan(
    mut reader: impl Read,
    cap: usize,
) -> (Vec<u8>, bool, std::collections::BTreeSet<String>) {
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
    (kept, errored, scanner.finish())
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
    /// output, and the failing test names a `FailureNameScanner` recognized
    /// while that output streamed by.
    type RawCheckOutcome = (
        CheckStatus,
        Option<i32>,
        Vec<u8>,
        std::collections::BTreeSet<String>,
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
        let (mut output, mut errored, mut names) = stdout_thread.join().unwrap_or_default();
        let (stderr_output, stderr_errored, stderr_names) =
            stderr_thread.join().unwrap_or_default();
        output.extend(stderr_output);
        errored |= stderr_errored;
        names.extend(stderr_names);
        if output.len() > MAX_FAILURE_OUTPUT_BYTES {
            output.drain(..output.len() - MAX_FAILURE_OUTPUT_BYTES);
        }
        if errored {
            output.extend_from_slice(b"\n[output stream ended in a read error]");
        }
        Ok((status, code, output, names))
    })();

    let (status, exit_code, output, failure_test_names) = result.unwrap_or_else(|err| {
        (
            CheckStatus::Failed,
            None,
            err.to_string().into_bytes(),
            std::collections::BTreeSet::new(),
        )
    });
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
    }
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
    let report = run_mode(repo, mode, &args.checks, args.dry_run)?;
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
fn run_baseline(repo: &Path, args: &RunArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let (code, report) = run_and_persist(repo, VerificationMode::All, args, writer)?;
    if args.dry_run {
        return Ok(code);
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
    /// blocking on them forever. Always an explicit operator action.
    Baseline(RunArgs),
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
            run_baseline(&resolved_repo(args.repo.as_deref())?, args, writer)
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
        let (retained, errored, _names) =
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

        let (retained, errored, names) =
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
                &RunArgs {
                    repo: None,
                    checks: vec![],
                    dry_run: false,
                    json: false,
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
        let names = scanner.finish();
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
        let names = scanner.finish();
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
        let names = scanner.finish();
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "crate_a::tests::first_failure".to_string(),
                "crate_b::tests::second_failure".to_string(),
            ]),
            "each binary's block must be judged independently by its own header: {names:?}"
        );
    }
}
