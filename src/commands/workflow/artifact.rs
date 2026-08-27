//! Artifact-first visual output and bounded interactive fallback.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::capability::{CapabilityId, CapabilityReport, PolicyDecision, SupportLevel};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Image,
    Svg,
    Html,
    Diagram,
    Document,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub schema_version: u32,
    pub id: String,
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub workflow_id: Option<String>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationMethod {
    HarnessNative,
    StaticFile,
    InteractiveServer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresentationPlan {
    pub method: PresentationMethod,
    pub adapter: String,
    pub reason: String,
    pub cleanup_required: bool,
}

/// Select the first verified presentation rung. Adapter-native output wins
/// where an adapter declares a real mechanism; otherwise a static path is the
/// no-process default. Only an explicitly requested interactive fallback may
/// start a server or browser, and it must pass canonical policy first.
pub fn presentation_plan(
    adapter: &str,
    artifact: &Path,
    interactive_required: bool,
    approved: bool,
    capabilities: &CapabilityReport,
) -> CtxResult<PresentationPlan> {
    let native = crate::commands::ctx::adapters::native_artifact_presentation_for_agent_name(
        adapter,
        artifact,
        interactive_required,
    );
    presentation_plan_with_native(
        adapter,
        interactive_required,
        approved,
        capabilities,
        native,
    )
}

fn presentation_plan_with_native(
    adapter: &str,
    interactive_required: bool,
    approved: bool,
    capabilities: &CapabilityReport,
    native: Option<&'static str>,
) -> CtxResult<PresentationPlan> {
    if let Some(mechanism) = native {
        return Ok(PresentationPlan {
            method: PresentationMethod::HarnessNative,
            adapter: adapter.to_string(),
            reason: mechanism.into(),
            cleanup_required: false,
        });
    }
    if !interactive_required {
        return Ok(PresentationPlan {
            method: PresentationMethod::StaticFile,
            adapter: adapter.to_string(),
            reason: "no verified harness-native artifact API; use the static artifact".into(),
            cleanup_required: false,
        });
    }
    if capabilities.support(CapabilityId::BrowserOpen) == SupportLevel::Unsupported
        || capabilities.support(CapabilityId::ShellExec) == SupportLevel::Unsupported
    {
        return Err(format!(
            "adapter '{adapter}' lacks browser.open or shell.exec for an interactive fallback"
        )
        .into());
    }
    if !approved
        && [CapabilityId::BrowserOpen, CapabilityId::ShellExec]
            .into_iter()
            .any(|capability| capabilities.authorization(capability) == PolicyDecision::Ask)
    {
        return Err(
            "interactive artifact presentation requires explicit operator approval; rerun with --approve"
                .into(),
        );
    }
    Ok(PresentationPlan {
        method: PresentationMethod::InteractiveServer,
        adapter: adapter.to_string(),
        reason: "interaction was explicitly required; run a bounded local server".into(),
        cleanup_required: true,
    })
}

fn infer_kind(path: &Path) -> ArtifactKind {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp") => ArtifactKind::Image,
        Some("svg") => ArtifactKind::Svg,
        Some("html" | "htm") => ArtifactKind::Html,
        Some("mmd" | "mermaid") => ArtifactKind::Diagram,
        Some("pdf" | "md" | "docx") => ArtifactKind::Document,
        _ => ArtifactKind::Other,
    }
}

/// `register` canonicalized its repo path while `load`/`list` used the
/// caller's verbatim, so where the two spellings differ (macOS `/var` ->
/// `/private/var`) a just-registered artifact could not be read back. The
/// canonicalization now lives inside `repo_slug` itself, so every caller on
/// either side of that split agrees.
fn artifact_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.artifacts().join(repo_slug(repo))
}

fn record_path(state: &StateDir, repo: &Path, id: &str) -> CtxResult<PathBuf> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("invalid artifact id '{id}'").into());
    }
    Ok(artifact_dir(state, repo).join(format!("{id}.json")))
}

pub fn register(
    state: &StateDir,
    repo: &Path,
    path: &Path,
    kind: Option<ArtifactKind>,
    workflow_id: Option<String>,
) -> CtxResult<ArtifactRecord> {
    let repo = repo.canonicalize()?;
    let path = path.canonicalize()?;
    if !path.starts_with(&repo) {
        return Err("artifact path must remain inside the repository".into());
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("artifact must be a regular non-symlink file".into());
    }
    let record = ArtifactRecord {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        kind: kind.unwrap_or_else(|| infer_kind(&path)),
        path,
        size_bytes: metadata.len(),
        workflow_id,
        created_at: now_secs(),
    };
    let dir = artifact_dir(state, &repo);
    create_private_dir_all(&dir)?;
    write_private(
        &record_path(state, &repo, &record.id)?,
        &serde_json::to_string_pretty(&record)?,
    )?;
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::ArtifactProduced);
    event.workflow_id = record.workflow_id.clone();
    event.phase = Some(super::skill::WorkflowPhase::Present);
    event.artifact_count = 1;
    let _ = super::telemetry::record(
        state,
        &repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&repo),
    );
    Ok(record)
}

pub fn load(state: &StateDir, repo: &Path, id: &str) -> CtxResult<ArtifactRecord> {
    Ok(serde_json::from_str(&std::fs::read_to_string(
        record_path(state, repo, id)?,
    )?)?)
}

pub fn list(state: &StateDir, repo: &Path) -> CtxResult<Vec<ArtifactRecord>> {
    let dir = artifact_dir(state, repo);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            && let Ok(record) = serde_json::from_str(&std::fs::read_to_string(entry.path())?)
        {
            records.push(record);
        }
    }
    records.sort_by_key(|record: &ArtifactRecord| (record.created_at, record.id.clone()));
    Ok(records)
}

#[derive(Debug, Args)]
pub struct ArtifactArgs {
    #[command(subcommand)]
    pub command: ArtifactCommand,
}

#[derive(Debug, Subcommand)]
pub enum ArtifactCommand {
    /// Register a static file as a workflow artifact.
    Render(RenderArgs),
    /// List registered artifacts.
    List(ListArgs),
    /// Show one artifact record.
    Show(ShowArgs),
    /// Present an artifact using static-first fallback selection.
    Present(PresentArgs),
}

#[derive(Debug, Args)]
pub struct RenderArgs {
    pub path: PathBuf,
    #[arg(long, value_enum)]
    pub kind: Option<ArtifactKind>,
    #[arg(long)]
    pub workflow: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PresentArgs {
    pub id: String,
    #[arg(long, default_value = "claude")]
    pub agent: String,
    /// Require live interaction rather than a static artifact.
    #[arg(long)]
    pub interactive: bool,
    /// Confirm an effective canonical `ask` policy for browser/shell access.
    #[arg(long)]
    pub approve: bool,
    /// Explicit local server command required for interactive fallback.
    #[arg(long)]
    pub server_command: Option<String>,
    /// URL opened after the explicit server starts.
    #[arg(long, default_value = "http://127.0.0.1:8000")]
    pub url: String,
    /// Hard server lifetime; Zirv kills the process tree afterwards.
    #[arg(long, default_value_t = 30)]
    pub lifetime_secs: u64,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

fn repo(path: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match path {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

fn state() -> CtxResult<StateDir> {
    StateDir::resolve(&|key| std::env::var(key).ok())
}

fn shell(command: &str, repo: &Path) -> Command {
    #[cfg(windows)]
    let mut value = {
        let mut value = Command::new("cmd");
        value.args(["/D", "/S", "/C", command]);
        value
    };
    #[cfg(not(windows))]
    let mut value = {
        let mut value = Command::new("sh");
        value.args(["-c", command]);
        value
    };
    super::isolate_process_tree(&mut value);
    value.current_dir(repo);
    value
}

fn open_url(url: &str) -> CtxResult<()> {
    let status = if cfg!(target_os = "windows") {
        Command::new("explorer.exe").arg(url).status()?
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()?
    } else {
        Command::new("xdg-open").arg(url).status()?
    };
    if !status.success() {
        return Err(format!("browser opener failed for '{url}'").into());
    }
    Ok(())
}

/// `host:port` for a readiness probe. `--url` is both dialed by this process
/// and handed to the platform opener, so it is restricted twice over: to
/// `http`/`https` (a `file://` or `vscode://` value turns "open the local
/// preview" into "launch whatever this string names"), and to a loopback host
/// (any other host makes `artifact present` a way to make the operator's
/// machine reach out to, and open a browser on, an address the caller chose).
fn probe_target(url: &str) -> CtxResult<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("--url must be an http:// or https:// address")?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        other => return Err(format!("--url scheme '{other}' is not http or https").into()),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("--url has no host".into());
    }
    if authority.contains('@') {
        return Err("--url must not carry credentials".into());
    }
    // IPv6 literals arrive bracketed (`[::1]`, `[::1]:8000`) and are already
    // in connect form; everything else splits on the last colon.
    let (host, target) = match authority.strip_prefix('[') {
        Some(_) => (
            authority
                .split_once(']')
                .map(|(host, _)| host.trim_start_matches('[').to_string())
                .ok_or("--url has an unterminated IPv6 host")?,
            match authority.rsplit_once("]:") {
                Some(_) => authority.to_string(),
                None => format!("{authority}:{default_port}"),
            },
        ),
        None => match authority.split_once(':') {
            Some((host, _)) => (host.to_string(), authority.to_string()),
            None => (authority.to_string(), format!("{authority}:{default_port}")),
        },
    };
    if !is_loopback_host(&host) {
        return Err(format!(
            "--url host '{host}' is not loopback; only localhost, 127.0.0.0/8 and ::1 are allowed"
        )
        .into());
    }
    Ok(target)
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host == "::1" {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// How long the spawned server gets to accept a connection before the run is
/// called a failure. Counted inside the artifact's own lifetime.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How much of the server's stderr is kept for a diagnostic message. The rest
/// is read and discarded: a chatty server must never wedge on a full pipe.
const MAX_SERVER_STDERR_BYTES: usize = 4096;

/// Drains the child's stderr continuously on its own thread, keeping only the
/// last [`MAX_SERVER_STDERR_BYTES`] for diagnostics.
///
/// Both halves matter. Reading it *only on failure* deadlocked a still-live
/// child (`read_to_end` waits for EOF, and EOF needs the child to exit, which
/// on the readiness-timeout path it has not), and not reading it at all wedged
/// a server that printed more than a pipe buffer's worth of log lines during
/// its lifetime -- the failure `Stdio::null()` never had.
fn spawn_stderr_drain(
    stderr: Option<std::process::ChildStderr>,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    std::thread::JoinHandle<()>,
) {
    let tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let Some(mut stderr) = stderr else {
        return (tail, std::thread::spawn(|| {}));
    };
    let sink = std::sync::Arc::clone(&tail);
    let handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut chunk = [0u8; 4096];
        while let Ok(count) = stderr.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let Ok(mut kept) = sink.lock() else { break };
            kept.extend_from_slice(&chunk[..count]);
            let overflow = kept.len().saturating_sub(MAX_SERVER_STDERR_BYTES);
            if overflow > 0 {
                kept.drain(..overflow);
            }
        }
    });
    (tail, handle)
}

fn stderr_note(tail: &std::sync::Mutex<Vec<u8>>) -> String {
    let text = tail
        .lock()
        .map(|kept| String::from_utf8_lossy(&kept).trim().to_string())
        .unwrap_or_default();
    if text.is_empty() {
        return String::new();
    }
    format!(": {text}")
}

/// How long a failure path waits for [`spawn_stderr_drain`]'s reader thread
/// to catch up before [`stderr_note`] reads its tail. Every call site sits
/// downstream of the child already being dead (an immediate exit) or just
/// killed (a readiness timeout), so `try_wait`/termination observing the
/// OS-level exit races the reader thread actually being scheduled to drain
/// the pipe -- the exact mechanism `supervise::FINAL_DRAIN_BUDGET` bounds
/// for the analogous `OutputTap` race. A child that wrote its last line and
/// exited in the same instant used to lose that line here: `try_wait`
/// noticed the exit before the reader thread had drained it.
const STDERR_DRAIN_BUDGET: Duration = Duration::from_millis(500);

/// Waits (bounded by `budget`) for `handle` to finish. Never blocks past the
/// budget even if the thread is somehow wedged -- this is a diagnostic best
/// effort, not a correctness requirement, so a slow drain must not turn into
/// a slow failure report.
fn wait_for_stderr_drain(handle: &std::thread::JoinHandle<()>, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run_interactive(args: &PresentArgs, repo: &Path) -> CtxResult<()> {
    run_interactive_with(args, repo, SERVER_READY_TIMEOUT)
}

fn run_interactive_with(args: &PresentArgs, repo: &Path, ready_timeout: Duration) -> CtxResult<()> {
    let command = args
        .server_command
        .as_deref()
        .ok_or("--server-command is required with --interactive")?;
    if args.lifetime_secs == 0 || args.lifetime_secs > 3600 {
        return Err("--lifetime-secs must be in 1..=3600".into());
    }
    let target = probe_target(&args.url)?;
    let mut child = shell(command, repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut job = crate::commands::ctx::supervise::JobGuard::adopt(child.id());
    let (stderr, stderr_thread) = spawn_stderr_drain(child.stderr.take());
    // The clock starts at spawn, not after the blocking browser open: an
    // opener that sits waiting for a user used to extend the "hard" lifetime
    // by however long that took.
    let deadline = Instant::now() + Duration::from_secs(args.lifetime_secs);

    // Success means a live server, not a spawn that returned. A command that
    // exits immediately (typo, port in use) used to report success with its
    // own error message thrown away.
    let ready_by = Instant::now() + ready_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            job.close();
            wait_for_stderr_drain(&stderr_thread, STDERR_DRAIN_BUDGET);
            return Err(format!(
                "server command exited before serving ({status}){}",
                stderr_note(&stderr)
            )
            .into());
        }
        if std::net::TcpStream::connect(&target).is_ok() {
            break;
        }
        if Instant::now() >= ready_by {
            // Terminate before reading: the tail is a live snapshot either
            // way, but the child has no reason to keep running once this run
            // has been called a failure.
            super::terminate_process_tree(&mut child)?;
            job.close();
            wait_for_stderr_drain(&stderr_thread, STDERR_DRAIN_BUDGET);
            return Err(format!(
                "server did not accept a connection on {target} within {}s{}",
                ready_timeout.as_secs(),
                stderr_note(&stderr)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if let Err(error) = open_url(&args.url) {
        super::terminate_process_tree(&mut child)?;
        job.close();
        return Err(error);
    }
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            super::terminate_process_tree(&mut child)?;
            job.close();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    super::terminate_process_tree(&mut child)?;
    job.close();
    Ok(())
}

pub fn run(args: &ArtifactArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        ArtifactCommand::Render(args) => {
            let repo = repo(args.repo.as_deref())?;
            let path = if args.path.is_absolute() {
                args.path.clone()
            } else {
                repo.join(&args.path)
            };
            let record = register(&state()?, &repo, &path, args.kind, args.workflow.clone())?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &record)?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "{}\t{:?}\t{}",
                    record.id,
                    record.kind,
                    record.path.display()
                )?;
            }
        }
        ArtifactCommand::List(args) => {
            let records = list(&state()?, &repo(args.repo.as_deref())?)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &records)?;
                writeln!(writer)?;
            } else {
                for record in records {
                    writeln!(
                        writer,
                        "{}\t{:?}\t{}",
                        record.id,
                        record.kind,
                        record.path.display()
                    )?;
                }
            }
        }
        ArtifactCommand::Show(args) => {
            let record = load(&state()?, &repo(args.repo.as_deref())?, &args.id)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &record)?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "{}\nkind: {:?}\npath: {}\nsize: {} B",
                    record.id,
                    record.kind,
                    record.path.display(),
                    record.size_bytes
                )?;
            }
        }
        ArtifactCommand::Present(args) => {
            let repo = repo(args.repo.as_deref())?;
            let record = load(&state()?, &repo, &args.id)?;
            let capabilities = CapabilityReport::for_repo(&args.agent, &repo)?;
            let plan = presentation_plan(
                &args.agent,
                &record.path,
                args.interactive,
                args.approve,
                &capabilities,
            )?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &plan)?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "method: {:?}\nartifact: {}",
                    plan.method,
                    record.path.display()
                )?;
            }
            if plan.method == PresentationMethod::InteractiveServer {
                run_interactive(args, &repo)?;
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn static_output_is_preferred_without_an_interaction_requirement() {
        let report = CapabilityReport::for_adapter("claude");
        let plan =
            presentation_plan("claude", Path::new("mock.svg"), false, false, &report).unwrap();
        assert_eq!(plan.method, PresentationMethod::StaticFile);
        assert!(!plan.cleanup_required);
    }

    #[test]
    fn interactive_fallback_is_explicit_and_requires_cleanup() {
        let report = CapabilityReport::for_adapter("codex");
        let plan = presentation_plan("codex", Path::new("mock.svg"), true, false, &report).unwrap();
        assert_eq!(plan.method, PresentationMethod::InteractiveServer);
        assert!(plan.cleanup_required);
    }

    #[test]
    fn only_loopback_http_urls_are_dialed_or_opened() {
        assert_eq!(
            probe_target("http://127.0.0.1:8000").unwrap(),
            "127.0.0.1:8000"
        );
        assert_eq!(
            probe_target("https://localhost/x").unwrap(),
            "localhost:443"
        );
        assert_eq!(probe_target("http://[::1]:8000/").unwrap(), "[::1]:8000");
        assert_eq!(probe_target("http://127.9.9.9/").unwrap(), "127.9.9.9:80");
        for rejected in [
            "file:///etc/passwd",
            "vscode://file/tmp",
            "127.0.0.1:8000",
            "http://user:pass@host/",
            // The residual this closes: a non-loopback host was both dialed
            // by this process and handed to the platform opener.
            "http://attacker.example.com/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[2001:db8::1]:8000/",
            "http://0.0.0.0:8000/",
        ] {
            assert!(probe_target(rejected).is_err(), "{rejected} was accepted");
        }
    }

    fn present_args(server_command: &str) -> PresentArgs {
        PresentArgs {
            id: "unused".into(),
            agent: "claude".into(),
            interactive: true,
            approve: false,
            server_command: Some(server_command.to_string()),
            // Port 1 never accepts, so a passing readiness probe is impossible
            // and the child's own fate is what must be reported.
            url: "http://127.0.0.1:1/".into(),
            lifetime_secs: 5,
            repo: None,
            json: false,
        }
    }

    #[test]
    fn a_server_command_that_exits_immediately_is_a_failure() {
        let repo = tempdir().unwrap();
        let args = present_args(if cfg!(windows) {
            "zirv-command-that-does-not-exist"
        } else {
            "echo boom >&2; exit 7"
        });
        let error = run_interactive_with(&args, repo.path(), Duration::from_secs(2))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("exited before serving")
                || (cfg!(windows) && error.contains("did not accept a connection")),
            "got {error}"
        );
        assert!(
            error.contains("boom") || error.contains("not recognized") || cfg!(windows),
            "got {error}"
        );
    }

    /// The readiness-timeout path used to `read_to_end` a still-live child's
    /// stderr, which waits for EOF, which waits for the child to exit: a
    /// server slower than the readiness window that had printed only a short
    /// banner hung `artifact present --interactive` forever. This child stays
    /// alive well past the window, so the test only returns if the fix holds.
    #[test]
    fn a_slow_server_that_stays_alive_times_out_instead_of_hanging() {
        let repo = tempdir().unwrap();
        let args = present_args(if cfg!(windows) {
            "echo starting up 1>&2 & timeout /t 30 /nobreak"
        } else {
            "echo starting up >&2; sleep 30"
        });
        let started = Instant::now();
        let error = run_interactive_with(&args, repo.path(), Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not accept a connection"), "got {error}");
        assert!(
            error.contains("starting up") || cfg!(windows),
            "the drained stderr tail should still be reported: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the readiness timeout must not wait for the child's own EOF"
        );
    }

    #[test]
    fn artifacts_are_referenced_by_id_without_copying_payloads_into_state() {
        let repo = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        let state = StateDir::from_root(state_root.path().to_path_buf());
        let path = repo.path().join("mock.svg");
        std::fs::write(&path, "<svg/>").unwrap();
        let record = register(&state, repo.path(), &path, None, Some("wf".into())).unwrap();
        assert_eq!(record.kind, ArtifactKind::Svg);
        assert_eq!(load(&state, repo.path(), &record.id).unwrap(), record);
        assert!(!state_root.path().join("mock.svg").exists());
    }

    #[test]
    fn interactive_fallback_obeys_deny_and_requires_approval_for_ask() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};

        let denied = CapabilityReport::for_policy(
            "claude",
            &EffectivePolicy {
                shell_exec: Stance::Deny,
                ..EffectivePolicy::default()
            },
        );
        assert!(presentation_plan("claude", Path::new("mock.svg"), true, true, &denied).is_err());

        let asks = CapabilityReport::for_policy(
            "claude",
            &EffectivePolicy {
                shell_exec: Stance::Ask,
                ..EffectivePolicy::default()
            },
        );
        let error = presentation_plan("claude", Path::new("mock.svg"), true, false, &asks)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--approve"), "got {error}");
        assert!(presentation_plan("claude", Path::new("mock.svg"), true, true, &asks).is_ok());
    }

    #[test]
    fn current_adapters_do_not_claim_an_unverified_native_output_api() {
        for adapter in ["claude", "codex"] {
            assert!(
                crate::commands::ctx::adapters::native_artifact_presentation_for_agent_name(
                    adapter,
                    Path::new("mock.png"),
                    false
                )
                .is_none()
            );
        }
    }

    #[test]
    fn a_verified_adapter_native_rung_wins_without_a_local_program() {
        let report = CapabilityReport::for_adapter("claude");
        let plan = presentation_plan_with_native(
            "future",
            false,
            false,
            &report,
            Some("verified harness artifact panel"),
        )
        .unwrap();
        assert_eq!(plan.method, PresentationMethod::HarnessNative);
        assert!(!plan.cleanup_required);
        assert_eq!(plan.reason, "verified harness artifact panel");
    }
}
