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
const VERIFY_REPORT_SCHEMA_VERSION: u32 = 2;
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
                .map(PathBuf::from),
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

/// The retained tail, plus whether the stream ended in a read error rather
/// than at EOF. An error read as a clean end silently turned a truncated
/// failure log into a complete-looking one.
fn read_capped_tail(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::with_capacity(cap);
    let mut chunk = [0u8; 8192];
    let mut errored = false;
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Err(_) => {
                errored = true;
                break;
            }
            Ok(count) => count,
        };
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
    (kept, errored)
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
    }
}

fn run_check(repo: &Path, check: &ResolvedCheck, dry_run: bool) -> CheckResult {
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
    let result = (|| -> CtxResult<(CheckStatus, Option<i32>, Vec<u8>)> {
        let mut child = command.spawn()?;
        let mut job = crate::commands::ctx::supervise::JobGuard::adopt(child.id());
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || {
            stdout
                .map(|stdout| read_capped_tail(stdout, MAX_FAILURE_OUTPUT_BYTES))
                .unwrap_or_default()
        });
        let stderr_thread = std::thread::spawn(move || {
            stderr
                .map(|stderr| read_capped_tail(stderr, MAX_FAILURE_OUTPUT_BYTES))
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
        let (mut output, mut errored) = stdout_thread.join().unwrap_or_default();
        let (stderr_output, stderr_errored) = stderr_thread.join().unwrap_or_default();
        output.extend(stderr_output);
        errored |= stderr_errored;
        if output.len() > MAX_FAILURE_OUTPUT_BYTES {
            output.drain(..output.len() - MAX_FAILURE_OUTPUT_BYTES);
        }
        if errored {
            output.extend_from_slice(b"\n[output stream ended in a read error]");
        }
        Ok((status, code, output))
    })();

    let (status, exit_code, output) =
        result.unwrap_or_else(|err| (CheckStatus::Failed, None, err.to_string().into_bytes()));
    let failure_output = (status != CheckStatus::Passed)
        .then(|| scrub_output(&tail_text(&output, MAX_FAILURE_OUTPUT_BYTES)));
    CheckResult {
        id: check.id.clone(),
        kind: check.kind,
        command: check.command.clone(),
        source: check_source,
        status,
        exit_code,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        failure_output,
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
fn save_report(state: &StateDir, report: &VerificationReport) -> CtxResult<()> {
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
    Ok(report.passed()
        && (!final_only || report.mode == VerificationMode::Final)
        // A `--check format` run is evidence about formatting, not about the
        // change set, so it can never satisfy a step gate.
        && report.narrowed_to.is_empty()
        && report.change_fingerprint == change_fingerprint(repo)?)
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
    let repo_checks_enabled = super::repo_gates(repo).checks;
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
        let result = run_check(repo, check, dry_run);
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
fn run_and_report(
    repo: &Path,
    mode: VerificationMode,
    args: &RunArgs,
    writer: &mut impl Write,
) -> CtxResult<i32> {
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
    Ok(if args.dry_run || report.passed() {
        0
    } else {
        1
    })
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
    let (mode, args) = match &args.command {
        TestCommand::Changed(args) => (VerificationMode::Changed, args),
        TestCommand::All(args) => (VerificationMode::All, args),
    };
    run_and_report(&resolved_repo(args.repo.as_deref())?, mode, args, writer)
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
        let (retained, errored) =
            read_capped_tail(std::io::Cursor::new(&input), MAX_FAILURE_OUTPUT_BYTES);
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
        );
        assert_eq!(failed.status, CheckStatus::Failed);
        assert!(failed.failure_output.unwrap().contains("actionable"));
    }
}
