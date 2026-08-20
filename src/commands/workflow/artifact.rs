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
        &super::telemetry::TelemetryConfig::from_env(),
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

fn run_interactive(args: &PresentArgs, repo: &Path) -> CtxResult<()> {
    let command = args
        .server_command
        .as_deref()
        .ok_or("--server-command is required with --interactive")?;
    if args.lifetime_secs == 0 || args.lifetime_secs > 3600 {
        return Err("--lifetime-secs must be in 1..=3600".into());
    }
    let mut child = shell(command, repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut job = crate::commands::ctx::supervise::JobGuard::adopt(child.id());
    std::thread::sleep(Duration::from_millis(300));
    if let Err(error) = open_url(&args.url) {
        super::terminate_process_tree(&mut child)?;
        job.close();
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(args.lifetime_secs);
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
