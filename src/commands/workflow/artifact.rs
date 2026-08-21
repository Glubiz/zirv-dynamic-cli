//! Artifact-first visual output and bounded interactive fallback.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::capability::{CapabilityId, CapabilityReport, SupportLevel};
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
    #[allow(dead_code)] // No current CLI harness exposes a verified native artifact API.
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

/// Current CLI adapters have no verified native artifact API. Static files
/// are therefore the honest default. The Context Compiler/provider adapter
/// work can flip this without changing skill instructions.
pub fn presentation_plan(adapter: &str, interactive_required: bool) -> CtxResult<PresentationPlan> {
    let capabilities = CapabilityReport::for_adapter(adapter);
    if capabilities.support(CapabilityId::ArtifactRender) == SupportLevel::Unsupported {
        return Err(format!("adapter '{adapter}' cannot render artifacts").into());
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

/// The one place a repository path becomes a state-directory slug. Every entry
/// point has to agree on it: `register` canonicalized while `load`/`list` used
/// the caller's path verbatim, so on a platform where the two differ (macOS
/// `/var` -> `/private/var`) a just-registered artifact could not be read back.
fn artifact_dir(state: &StateDir, repo: &Path) -> PathBuf {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    state.artifacts().join(repo_slug(&repo))
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

/// `host:port` for a readiness probe. Only `http://` and `https://` are
/// accepted at all: `--url` is handed to the platform opener, and `file://`,
/// `vscode://` or any other registered scheme turns "open the local preview"
/// into "launch whatever this string names".
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
    if authority.starts_with('[') {
        // IPv6 literal: `[::1]` or `[::1]:8000`, already in connect form.
        return Ok(match authority.rsplit_once("]:") {
            Some(_) => authority.to_string(),
            None => format!("{authority}:{default_port}"),
        });
    }
    Ok(match authority.split_once(':') {
        Some(_) => authority.to_string(),
        None => format!("{authority}:{default_port}"),
    })
}

/// How long the spawned server gets to accept a connection before the run is
/// called a failure. Counted inside the artifact's own lifetime.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);

fn drain_stderr(child: &mut std::process::Child) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buffer = Vec::new();
    use std::io::Read;
    let _ = stderr.by_ref().take(4096).read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).trim().to_string()
}

fn run_interactive(args: &PresentArgs, repo: &Path) -> CtxResult<()> {
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
    // The clock starts at spawn, not after the blocking browser open: an
    // opener that sits waiting for a user used to extend the "hard" lifetime
    // by however long that took.
    let deadline = Instant::now() + Duration::from_secs(args.lifetime_secs);

    // Success means a live server, not a spawn that returned. A command that
    // exits immediately (typo, port in use) used to report success with its
    // own error message thrown away.
    let ready_by = Instant::now() + SERVER_READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            let stderr = drain_stderr(&mut child);
            job.close();
            return Err(format!(
                "server command exited before serving ({status}){}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            )
            .into());
        }
        if std::net::TcpStream::connect(&target).is_ok() {
            break;
        }
        if Instant::now() >= ready_by {
            let stderr = drain_stderr(&mut child);
            super::terminate_process_tree(&mut child)?;
            job.close();
            return Err(format!(
                "server did not accept a connection on {target} within {}s{}",
                SERVER_READY_TIMEOUT.as_secs(),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
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
            let plan = presentation_plan(&args.agent, args.interactive)?;
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
        let plan = presentation_plan("claude", false).unwrap();
        assert_eq!(plan.method, PresentationMethod::StaticFile);
        assert!(!plan.cleanup_required);
    }

    #[test]
    fn interactive_fallback_is_explicit_and_requires_cleanup() {
        let plan = presentation_plan("codex", true).unwrap();
        assert_eq!(plan.method, PresentationMethod::InteractiveServer);
        assert!(plan.cleanup_required);
    }

    #[test]
    fn only_http_urls_reach_the_platform_opener() {
        assert_eq!(
            probe_target("http://127.0.0.1:8000").unwrap(),
            "127.0.0.1:8000"
        );
        assert_eq!(
            probe_target("https://localhost/x").unwrap(),
            "localhost:443"
        );
        assert_eq!(probe_target("http://[::1]:8000/").unwrap(), "[::1]:8000");
        for rejected in [
            "file:///etc/passwd",
            "vscode://file/tmp",
            "127.0.0.1:8000",
            "http://user:pass@host/",
        ] {
            assert!(probe_target(rejected).is_err(), "{rejected} was accepted");
        }
    }

    #[test]
    fn a_server_command_that_exits_immediately_is_a_failure() {
        let repo = tempdir().unwrap();
        let args = PresentArgs {
            id: "unused".into(),
            agent: "claude".into(),
            interactive: true,
            server_command: Some(if cfg!(windows) {
                "exit /b 7".into()
            } else {
                "echo boom >&2; exit 7".into()
            }),
            // Port 0 never accepts, so a passing readiness probe is impossible
            // and the exit is what this must report.
            url: "http://127.0.0.1:1/".into(),
            lifetime_secs: 5,
            repo: None,
            json: false,
        };
        let error = run_interactive(&args, repo.path()).unwrap_err().to_string();
        assert!(error.contains("exited before serving"), "got {error}");
        assert!(error.contains("boom") || cfg!(windows), "got {error}");
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
}
