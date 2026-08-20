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

pub fn load_or_discover(repo: &Path) -> CtxResult<(VerificationConfig, &'static str)> {
    let path = repo.join(".zirv").join("verify.toml");
    if path.exists() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!("verification config '{}' must be a regular file", path.display()).into());
        }
        if usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_CONFIG_BYTES {
            return Err(format!("verification config '{}' exceeds {MAX_CONFIG_BYTES} bytes", path.display()).into());
        }
        let config: VerificationConfig = toml::from_str(&std::fs::read_to_string(&path)?)?;
        config.validate()?;
        return Ok((config, "configured"));
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
    config.validate().map_err(|_| {
        "no verification checks configured or safely discoverable; add .zirv/verify.toml".into()
    })?;
    Ok((config, "discovered"))
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    if pattern.ends_with('/') {
        return path.starts_with(&pattern);
    }
    if !pattern.contains(['*', '?']) {
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

pub fn changed_paths(repo: &Path) -> CtxResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for args in [
        &["diff", "--name-only", "HEAD"][..],
        &["ls-files", "--others", "--exclude-standard"][..],
    ] {
        let output = Command::new("git").arg("-C").arg(repo).args(args).output()?;
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
    let diff = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--binary", "HEAD"])
        .output()?;
    if !diff.status.success() {
        return Err("cannot fingerprint repository diff".into());
    }
    let mut input = String::from_utf8_lossy(&diff.stdout).into_owned();
    for path in changed_paths(repo)? {
        let absolute = repo.join(&path);
        if absolute.is_file()
            && let Ok(metadata) = std::fs::metadata(&absolute)
            && metadata.len() <= 1024 * 1024
        {
            input.push_str("\nuntracked-or-changed:");
            input.push_str(&path.to_string_lossy());
            if let Ok(bytes) = std::fs::read(&absolute) {
                input.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    Ok(input_hash(&input))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationMode {
    Changed,
    All,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Passed,
    Failed,
    TimedOut,
    DryRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub id: String,
    pub kind: CheckKind,
    pub command: String,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationReport {
    pub schema_version: u32,
    pub id: String,
    pub mode: VerificationMode,
    pub source: String,
    pub repo: PathBuf,
    pub change_fingerprint: u64,
    pub changed_paths: Vec<PathBuf>,
    pub fallback_to_full: bool,
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

fn run_check(repo: &Path, check: &CheckSpec, dry_run: bool) -> CheckResult {
    if dry_run {
        return CheckResult {
            id: check.id.clone(),
            kind: check.kind,
            command: check.command.clone(),
            status: CheckStatus::DryRun,
            exit_code: None,
            duration_ms: 0,
            failure_output: None,
        };
    }
    let started = Instant::now();
    let mut command = command_for_shell(&check.command);
    command
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let result = (|| -> CtxResult<(CheckStatus, Option<i32>, Vec<u8>)> {
        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_end(&mut bytes);
            }
            bytes
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stderr) = stderr {
                let _ = stderr.read_to_end(&mut bytes);
            }
            bytes
        });

        let timeout = Duration::from_secs(check.timeout_secs);
        let (status, code) = loop {
            if let Some(status) = child.try_wait()? {
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
                if !crate::commands::ctx::supervise::kill_tree(child.id()) {
                    let _ = child.kill();
                }
                let _ = child.wait();
                break (CheckStatus::TimedOut, None);
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let mut output = stdout_thread.join().unwrap_or_default();
        output.extend(stderr_thread.join().unwrap_or_default());
        Ok((status, code, output))
    })();

    let (status, exit_code, output) = result.unwrap_or_else(|err| {
        (CheckStatus::Failed, None, err.to_string().into_bytes())
    });
    let failure_output = (status != CheckStatus::Passed).then(|| {
        crate::utils::truncate_bytes(
            String::from_utf8_lossy(&output).into_owned(),
            Some(MAX_FAILURE_OUTPUT_BYTES),
        )
    });
    CheckResult {
        id: check.id.clone(),
        kind: check.kind,
        command: check.command.clone(),
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
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

pub fn latest_is_fresh_and_passing(state: &StateDir, repo: &Path, final_only: bool) -> CtxResult<bool> {
    let Some(report) = load_latest(state, repo)? else {
        return Ok(false);
    };
    Ok(report.passed()
        && (!final_only || report.mode == VerificationMode::Final)
        && report.change_fingerprint == change_fingerprint(repo)?)
}

fn run_mode(
    repo: &Path,
    mode: VerificationMode,
    only: &[String],
    dry_run: bool,
) -> CtxResult<VerificationReport> {
    let (config, source) = load_or_discover(repo)?;
    let paths = changed_paths(repo)?;
    let mut fallback_to_full = false;
    let mut selected: Vec<&CheckSpec> = config
        .checks
        .iter()
        .filter(|check| match mode {
            VerificationMode::Changed => check.changed,
            VerificationMode::Final => check.final_check,
            VerificationMode::All => true,
        })
        .filter(|check| only.is_empty() || only.contains(&check.id))
        .collect();

    if mode == VerificationMode::Changed {
        let targeted: Vec<_> = selected
            .iter()
            .copied()
            .filter(|check| {
                check.paths.is_empty()
                    || paths.iter().any(|path| {
                        let path = path.to_string_lossy();
                        check.paths.iter().any(|pattern| path_matches(pattern, &path))
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

    let started_at = now_secs();
    let checks = selected
        .into_iter()
        .map(|check| run_check(repo, check, dry_run))
        .collect();
    let report = VerificationReport {
        schema_version: VERIFY_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        mode,
        source: source.to_string(),
        repo: repo.to_path_buf(),
        change_fingerprint: change_fingerprint(repo)?,
        changed_paths: paths,
        fallback_to_full,
        started_at,
        finished_at: now_secs(),
        checks,
    };
    if !dry_run {
        let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
        save_report(&state, &report)?;
        let mut event = super::telemetry::TelemetryEvent::new(
            super::telemetry::TelemetryKind::VerificationRun,
        );
        event.phase = Some(match mode {
            VerificationMode::Final => super::skill::WorkflowPhase::Verify,
            VerificationMode::Changed | VerificationMode::All => {
                super::skill::WorkflowPhase::Test
            }
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
            &super::telemetry::TelemetryConfig::from_env(),
        );
    }
    Ok(report)
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
        for check in &report.checks {
            writeln!(
                writer,
                "{}\t{:?}\t{} ms\t{}",
                check.id,
                check.status,
                check.duration_ms,
                check.command
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
    let report = run_mode(
        &resolved_repo(args.repo.as_deref())?,
        mode,
        &args.checks,
        args.dry_run,
    )?;
    write_report(writer, &report, args.json)?;
    Ok(if args.dry_run || report.passed() { 0 } else { 1 })
}

pub fn run_verify(args: &VerifyArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let report = run_mode(
        &resolved_repo(args.run.repo.as_deref())?,
        VerificationMode::Final,
        &args.run.checks,
        args.run.dry_run,
    )?;
    write_report(writer, &report, args.run.json)?;
    Ok(if args.run.dry_run || report.passed() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_rust_checks_without_external_services() {
        let repo = tempdir().unwrap();
        std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname='x'\nversion='0.1.0'\n")
            .unwrap();
        let (config, source) = load_or_discover(repo.path()).unwrap();
        assert_eq!(source, "discovered");
        assert_eq!(
            config.checks.iter().map(|check| check.id.as_str()).collect::<Vec<_>>(),
            ["format", "clippy", "test"]
        );
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
    fn successful_output_stays_compact_and_failures_remain_actionable() {
        let repo = tempdir().unwrap();
        let passed = run_check(
            repo.path(),
            &CheckSpec {
                id: "ok".into(),
                kind: CheckKind::Custom,
                command: if cfg!(windows) { "echo ok".into() } else { "printf ok".into() },
                paths: vec![],
                changed: true,
                final_check: true,
                timeout_secs: 5,
            },
            false,
        );
        assert_eq!(passed.status, CheckStatus::Passed);
        assert!(passed.failure_output.is_none());

        let failed = run_check(
            repo.path(),
            &CheckSpec {
                id: "bad".into(),
                kind: CheckKind::Custom,
                command: if cfg!(windows) {
                    "echo actionable & exit /b 3".into()
                } else {
                    "echo actionable >&2; exit 3".into()
                },
                paths: vec![],
                changed: true,
                final_check: true,
                timeout_secs: 5,
            },
            false,
        );
        assert_eq!(failed.status, CheckStatus::Failed);
        assert!(failed.failure_output.unwrap().contains("actionable"));
    }
}
