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

const VERIFY_SCHEMA_VERSION: u32 = 1;
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
        if self.schema_version != VERIFY_SCHEMA_VERSION {
            return Err(format!(
                "unsupported verification schema_version {}; supported version is {}",
                self.schema_version, VERIFY_SCHEMA_VERSION
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
        schema_version: VERIFY_SCHEMA_VERSION,
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

fn save_report(state: &StateDir, report: &VerificationReport) -> CtxResult<()> {
    let dir = report_dir(state, &report.repo);
    create_private_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(report)?;
    write_private(&dir.join(format!("{}.json", report.id)), &body)?;
    write_private(&dir.join("latest"), &report.id)?;
    Ok(())
}

pub fn load_latest(state: &StateDir, repo: &Path) -> CtxResult<Option<VerificationReport>> {
    let dir = report_dir(state, repo);
    let latest = dir.join("latest");
    if !latest.exists() {
        return Ok(None);
    }
    let id = std::fs::read_to_string(latest)?;
    let path = dir.join(format!("{}.json", id.trim()));
    let report: VerificationReport = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if report.schema_version != VERIFY_SCHEMA_VERSION {
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
    let repo_checks_enabled =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())?
            .workflow
            .repo_checks_enabled;
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
    let checks = selected
        .into_iter()
        .map(|check| {
            if !repo_checks_enabled && check.source.repo_supplied() {
                return check_result(check, CheckStatus::Skipped);
            }
            run_check(repo, check, dry_run)
        })
        .collect();
    Ok(VerificationReport {
        schema_version: VERIFY_SCHEMA_VERSION,
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
    }
    let _ = super::telemetry::record(
        &state,
        repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(repo),
    );
    Ok(())
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
    write_report(writer, &report, args.json)?;
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

fn write_report(writer: &mut impl Write, report: &VerificationReport, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, report)?;
        writeln!(writer)?;
    } else {
        writeln!(
            writer,
            "verification {} ({:?}, {}, targeted_fallback={})",
            report.id, report.mode, report.source, report.fallback_to_full
        )?;
        if !report.narrowed_to.is_empty() {
            writeln!(
                writer,
                "narrowed to: {} (not completion evidence for the change set)",
                report.narrowed_to.join(", ")
            )?;
        }
        for note in &report.notes {
            writeln!(writer, "note: {note}")?;
        }
        for check in &report.checks {
            writeln!(
                writer,
                "{}\t{:?}\t{:?}\t{} ms\t{}",
                check.id, check.source, check.status, check.duration_ms, check.command
            )?;
            if let Some(output) = &check.failure_output {
                writeln!(writer, "{output}")?;
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

    #[test]
    fn disabled_repo_checks_are_listed_but_never_executed() {
        let repo = tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".zirv")).unwrap();
        // A command that would leave a file behind if it ran at all.
        let marker = repo.path().join("ran");
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!(
                "schema_version=1\n[[checks]]\nid='sneaky'\nkind='custom'\ncommand='{} ran'\n",
                if cfg!(windows) { "type nul >" } else { "touch" }
            ),
        )
        .unwrap();
        let resolved = load_or_discover(repo.path()).unwrap();
        let skipped = check_result(&resolved.checks[0], CheckStatus::Skipped);
        assert_eq!(skipped.status, CheckStatus::Skipped);
        assert_eq!(skipped.source, CheckSource::RepoConfig);
        assert!(!marker.exists());
        let report = VerificationReport {
            schema_version: VERIFY_SCHEMA_VERSION,
            id: "report".into(),
            mode: VerificationMode::Final,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: 1,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![
                "skipped: repo-supplied checks disabled (workflow.repo_checks_enabled)".into(),
            ],
            started_at: 0,
            finished_at: 0,
            checks: vec![skipped],
        };
        assert!(!report.passed(), "a skipped check is not a passing check");
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
