use std::collections::BTreeSet;
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ctx;

type SetupResult<T> = Result<T, Box<dyn Error>>;

const HARNESS_HOOKS: [(&str, Option<&str>, &str); 4] = [
    ("Stop", None, "zirv ctx hook stop"),
    ("UserPromptSubmit", None, "zirv ctx hook prompt"),
    ("PreCompact", None, "zirv ctx hook pre-compact"),
    ("PreToolUse", Some("Agent|Task"), "zirv ctx hook pretool"),
];

#[derive(Debug, Parser)]
#[command(
    name = "zirv setup",
    about = "Configure Zirv's AI features and migrate from Claude Code or Codex.",
    disable_help_subcommand = true
)]
pub struct SetupCli {
    #[command(subcommand)]
    pub verb: Option<SetupVerb>,
}

#[derive(Debug, Subcommand)]
pub enum SetupVerb {
    /// Inspect harnesses, canonical context, memory, and harness hooks.
    Status(StatusArgs),
    /// Apply the complete, non-destructive Zirv AI setup.
    Apply(ApplyArgs),
    /// Back up and reset Claude/Codex custom settings.
    Reset(ResetArgs),
    /// List or restore a previous `apply`/`reset` backup.
    Restore(RestoreArgs),
}

#[derive(Debug, Args, Clone)]
pub struct StatusArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ApplyArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    #[arg(long, default_value_t = false)]
    pub no_context: bool,
    #[arg(long, default_value_t = false)]
    pub no_memory: bool,
    #[arg(long, default_value_t = false)]
    pub no_claude_hooks: bool,
    #[arg(long, default_value_t = false)]
    pub no_codex_hooks: bool,
    #[arg(long)]
    pub memory_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetProvider {
    Claude,
    Codex,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetScope {
    Project,
    Global,
    All,
}

#[derive(Debug, Args, Clone)]
pub struct ResetArgs {
    #[arg(value_enum)]
    pub provider: ResetProvider,
    #[arg(long, value_enum, default_value_t = ResetScope::Project)]
    pub scope: ResetScope,
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value_t = false)]
    pub include_auth: bool,
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    #[arg(long, default_value_t = false)]
    pub yes: bool,
}

/// A backup run's origin: `reset` removed files, `apply` merged into a
/// harness config file, `restore` is the automatic safety copy a restore
/// takes of the state it is about to overwrite. All three share one manifest
/// shape and one directory layout under `.zirv/backups/ai-reset`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BackupKind {
    #[default]
    Reset,
    Apply,
    Restore,
}

/// A manifest schema_version this build understands. Bump only alongside a
/// reader that still understands every older version, or accept that older
/// backups become list-only (readable for `--list`, refused for `restore`).
const SUPPORTED_MANIFEST_SCHEMA_VERSION: u32 = 1;

fn default_existed_before() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestTarget {
    source: PathBuf,
    relative_path: PathBuf,
    /// Whether this target existed before the run that produced this backup.
    /// `false` means the run *created* the file (an `apply` merge into a
    /// fresh settings file); restoring such a target means removing it, not
    /// copying anything back. Old reset-only manifests predate this field
    /// and always recorded existing targets, so it defaults to `true`.
    #[serde(default = "default_existed_before")]
    existed_before: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    #[serde(default)]
    kind: BackupKind,
    provider: ResetProvider,
    scope: ResetScope,
    base: PathBuf,
    created: u64,
    targets: Vec<ManifestTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetState {
    Absent,
    Identical,
    Differs,
}

fn describe_state(state: TargetState) -> &'static str {
    match state {
        TargetState::Absent => "currently absent",
        TargetState::Identical => "currently present, identical to the backup",
        TargetState::Differs => {
            "currently present, DIFFERS from the backup -- restoring will overwrite it"
        }
    }
}

#[derive(Debug, Args, Clone)]
pub struct RestoreArgs {
    /// Backup run id (a directory name under `.zirv/backups/ai-reset`).
    pub id: Option<String>,
    /// Enumerate available backup runs instead of restoring one.
    #[arg(long, default_value_t = false)]
    pub list: bool,
    /// Restore the most recent matching run instead of naming an id.
    #[arg(long, default_value_t = false)]
    pub latest: bool,
    #[arg(long, value_enum)]
    pub provider: Option<ResetProvider>,
    #[arg(long, value_enum)]
    pub scope: Option<ResetScope>,
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value_t = false)]
    pub include_auth: bool,
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    #[arg(long, default_value_t = false)]
    pub yes: bool,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

struct BackupRun {
    dir: PathBuf,
    id: String,
    manifest: Manifest,
}

const CLAUDE_JSON_NOTE: &str = "note: ~/.claude.json (MCP server registrations, OAuth/account linkage, project trust state) is never reset or restored by zirv; it is out of scope because clearing it would sign the operator out. Manage it by hand.";

/// Guided-menu wording, pulled into named constants so it is directly
/// assertable from a test rather than only reachable through an interactive
/// `dialoguer` prompt.
const GUIDED_RESET_CONFIRM_PROMPT: &str = "Back up these settings, then reset them? This is \
     recoverable: `zirv setup restore` can bring this exact configuration back at any time.";
const GUIDED_NO_BACKUPS_MESSAGE: &str = "No backup runs found yet. A backup is created \
     automatically every time `zirv setup apply` merges Zirv's hooks into a harness config \
     file, and every time `zirv setup reset` clears one -- there is nothing to restore until \
     one of those has actually run.";
const GUIDED_RESTORE_CONFIRM_PROMPT: &str = "Apply this restore now? The overwrites listed \
     above will happen; the current state is backed up first, so this restore can itself be \
     undone.";

#[derive(Debug, Serialize)]
struct HarnessStatus {
    installed: bool,
    config_dir: PathBuf,
    settings_present: bool,
}

#[derive(Debug, Serialize)]
struct SetupStatus {
    schema_version: u32,
    repo: PathBuf,
    zirv_initialized: bool,
    context_common: bool,
    context_claude: bool,
    context_codex: bool,
    shared_memory_entries: usize,
    claude_hooks_installed: usize,
    claude_statusline_installed: bool,
    codex_hooks_installed: usize,
    claude: HarnessStatus,
    codex: HarnessStatus,
}

fn home_dir() -> SetupResult<PathBuf> {
    crate::utils::home_dir()
}

fn claude_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
}

fn codex_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

fn executable_exists(name: &str) -> bool {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file();
    }
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .map(|ext| ext.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            extensions.iter().any(|extension| {
                let candidate = if extension.is_empty() {
                    dir.join(name)
                } else {
                    dir.join(format!("{name}{extension}"))
                };
                candidate.is_file()
            })
        })
    })
}

fn contains_command(value: &Value, command: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get("command").and_then(Value::as_str) == Some(command)
                || map.values().any(|value| contains_command(value, command))
        }
        Value::Array(values) => values.iter().any(|value| contains_command(value, command)),
        _ => false,
    }
}

fn load_json_object(path: &Path) -> SetupResult<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if !value.is_object() {
        return Err(format!("{} must contain a JSON object", path.display()).into());
    }
    Ok(value)
}

fn ensure_harness_hook(
    settings: &mut Value,
    event: &str,
    matcher: Option<&str>,
    command: &str,
) -> SetupResult<bool> {
    if contains_command(settings, command) {
        return Ok(false);
    }
    let root = settings
        .as_object_mut()
        .ok_or("Claude settings root is not an object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or("Claude settings `hooks` must be an object")?;
    let entries = hooks.entry(event).or_insert_with(|| json!([]));
    let entries = entries
        .as_array_mut()
        .ok_or_else(|| format!("Claude hook event `{event}` must be an array"))?;
    let mut entry = json!({
        "hooks": [{"type": "command", "command": command}]
    });
    if let Some(matcher) = matcher {
        entry["matcher"] = Value::String(matcher.to_string());
    }
    entries.push(entry);
    Ok(true)
}

fn install_claude_integration(home: &Path, dry_run: bool) -> SetupResult<(usize, bool)> {
    let settings_path = claude_config_dir(home).join("settings.json");
    if std::fs::symlink_metadata(&settings_path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!("refusing to write symlink {}", settings_path.display()).into());
    }
    let mut settings = load_json_object(&settings_path)?;
    let mut hooks_added = 0;
    for (event, matcher, command) in HARNESS_HOOKS {
        if ensure_harness_hook(&mut settings, event, matcher, command)? {
            hooks_added += 1;
        }
    }
    let root = settings.as_object_mut().expect("validated object");
    let statusline_added = if root.contains_key("statusLine") {
        false
    } else {
        root.insert(
            "statusLine".to_string(),
            json!({"type": "command", "command": "zirv ctx usage tee"}),
        );
        true
    };
    if !dry_run && (hooks_added > 0 || statusline_added) {
        std::fs::create_dir_all(
            settings_path
                .parent()
                .ok_or("settings path has no parent")?,
        )?;
        write_backup_run(
            &home.join(".zirv/backups/ai-reset"),
            BackupKind::Apply,
            ResetProvider::Claude,
            ResetScope::Global,
            &claude_config_dir(home),
            std::slice::from_ref(&settings_path),
        )?;
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings)? + "\n",
        )?;
    }
    Ok((hooks_added, statusline_added))
}

fn install_codex_hooks(home: &Path, hooks_path: &Path, dry_run: bool) -> SetupResult<usize> {
    if std::fs::symlink_metadata(hooks_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!("refusing to write symlink {}", hooks_path.display()).into());
    }
    let mut hooks = load_json_object(hooks_path)?;
    let mut hooks_added = 0;
    for (event, matcher, command) in HARNESS_HOOKS {
        if ensure_harness_hook(&mut hooks, event, matcher, command)? {
            hooks_added += 1;
        }
    }
    if !dry_run && hooks_added > 0 {
        let base = hooks_path.parent().ok_or("hooks path has no parent")?;
        std::fs::create_dir_all(base)?;
        write_backup_run(
            &home.join(".zirv/backups/ai-reset"),
            BackupKind::Apply,
            ResetProvider::Codex,
            ResetScope::Global,
            base,
            &[hooks_path.to_path_buf()],
        )?;
        std::fs::write(hooks_path, serde_json::to_string_pretty(&hooks)? + "\n")?;
    }
    Ok(hooks_added)
}

fn install_codex_integration(home: &Path, dry_run: bool) -> SetupResult<usize> {
    install_codex_hooks(home, &codex_config_dir(home).join("hooks.json"), dry_run)
}

fn read_regular(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn normalized(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

fn write_new(path: &Path, text: &str, dry_run: bool) -> SetupResult<bool> {
    if path.exists() {
        return Ok(false);
    }
    if !dry_run {
        let parent = path.parent().ok_or("context path has no parent")?;
        for candidate in parent.ancestors().take(2) {
            if std::fs::symlink_metadata(candidate).is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return Err(
                    format!("refusing to write through symlink {}", candidate.display()).into(),
                );
            }
        }
        std::fs::create_dir_all(parent)?;
        std::fs::write(path, normalized(text) + "\n")?;
    }
    Ok(true)
}

fn migrate_context(repo: &Path, dry_run: bool) -> SetupResult<Vec<PathBuf>> {
    let claude = read_regular(&repo.join("CLAUDE.md"));
    let codex = read_regular(&repo.join("AGENTS.md"));
    let mut created = Vec::new();
    match (claude, codex) {
        (Some(claude), Some(codex)) if normalized(&claude) == normalized(&codex) => {
            let path = ctx::context::common_path(repo);
            if write_new(&path, &claude, dry_run)? {
                created.push(path);
            }
        }
        (claude, codex) => {
            if let Some(claude) = claude {
                let path = ctx::context::claude_path(repo);
                if write_new(&path, &claude, dry_run)? {
                    created.push(path);
                }
            }
            if let Some(codex) = codex {
                let path = ctx::context::codex_path(repo);
                if write_new(&path, &codex, dry_run)? {
                    created.push(path);
                }
            }
        }
    }
    Ok(created)
}

#[derive(Debug, Clone)]
pub struct MemoryInitOptions {
    pub source: Option<PathBuf>,
    pub dry_run: bool,
    pub merge: bool,
    pub max_entries: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryInitReport {
    pub proposed: usize,
    pub written: usize,
    pub skipped_existing: usize,
    pub body_bytes: usize,
}

#[derive(Debug, Clone)]
struct MemoryProposal {
    key: String,
    body: String,
    tags: Vec<String>,
    paths: Vec<String>,
}

fn memory_key(value: &str) -> String {
    let key: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let key = key
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if key.is_empty() {
        "project-knowledge".to_string()
    } else {
        key.chars().take(64).collect()
    }
}

fn relative_path(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn validation_proposal(repo: &Path) -> Option<MemoryProposal> {
    let mut commands = BTreeSet::new();
    let mut paths = Vec::new();
    if repo.join("Cargo.toml").is_file() {
        paths.push("Cargo.toml".to_string());
        commands.extend([
            "cargo fmt --check".to_string(),
            "cargo test".to_string(),
            "cargo clippy --all-targets --all-features".to_string(),
        ]);
    }
    let package_path = repo.join("package.json");
    if let Some(package) = read_regular(&package_path)
        && let Ok(value) = serde_json::from_str::<Value>(&package)
        && let Some(scripts) = value.get("scripts").and_then(Value::as_object)
    {
        paths.push("package.json".to_string());
        let runner = if repo.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if repo.join("yarn.lock").is_file() {
            "yarn"
        } else if repo.join("bun.lock").is_file() || repo.join("bun.lockb").is_file() {
            "bun run"
        } else {
            "npm run"
        };
        for name in ["test", "lint", "check", "typecheck", "build"] {
            if scripts.contains_key(name) {
                commands.insert(format!("{runner} {name}"));
            }
        }
    }
    if repo.join("Makefile").is_file() {
        paths.push("Makefile".to_string());
        commands.insert("make test (when the target exists)".to_string());
    }
    if commands.is_empty() {
        return None;
    }
    Some(MemoryProposal {
        key: "project-validation".to_string(),
        body: format!(
            "Use the repository-defined validation commands that apply to the changed area:\n{}",
            commands
                .into_iter()
                .map(|command| format!("- `{command}`"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        tags: vec!["validation".to_string(), "commands".to_string()],
        paths,
    })
}

fn toolchain_proposal(repo: &Path) -> Option<MemoryProposal> {
    let manifests = [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "Makefile",
    ]
    .into_iter()
    .filter(|name| repo.join(name).is_file())
    .map(str::to_string)
    .collect::<Vec<_>>();
    if manifests.is_empty() {
        return None;
    }
    Some(MemoryProposal {
        key: "project-toolchain".to_string(),
        body: format!(
            "The repository's authoritative build/toolchain surfaces are: {}. Prefer their configured scripts and versions over guessed global defaults.",
            manifests
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        tags: vec!["toolchain".to_string(), "build".to_string()],
        paths: manifests,
    })
}

fn high_signal_markdown(path: &Path, repo: &Path) -> Vec<MemoryProposal> {
    let Some(text) = read_regular(path) else {
        return Vec::new();
    };
    let keywords = [
        "architecture",
        "testing",
        "development",
        "workflow",
        "convention",
        "constraint",
        "decision",
        "deployment",
        "contributing",
    ];
    let mut sections = Vec::new();
    let mut heading: Option<String> = None;
    let mut body = String::new();
    let flush = |heading: &mut Option<String>, body: &mut String, sections: &mut Vec<_>| {
        let Some(title) = heading.take() else {
            body.clear();
            return;
        };
        let content = normalized(body);
        body.clear();
        if content.is_empty() {
            return;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("docs");
        sections.push(MemoryProposal {
            key: memory_key(&format!("{stem}-{title}")),
            body: crate::utils::truncate_bytes(content, Some(768)),
            tags: vec!["documentation".to_string()],
            paths: vec![relative_path(repo, path)],
        });
    };
    for line in text.lines() {
        if let Some(title) = line.trim_start().strip_prefix("## ") {
            flush(&mut heading, &mut body, &mut sections);
            let title = title.trim();
            if keywords
                .iter()
                .any(|keyword| title.to_ascii_lowercase().contains(keyword))
            {
                heading = Some(title.to_string());
            }
        } else if heading.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&mut heading, &mut body, &mut sections);
    sections
}

fn markdown_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, files: &mut Vec<PathBuf>, limit: usize) {
        if files.len() >= limit {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if files.len() >= limit {
                break;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                walk(&path, files, limit);
            } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
    } else {
        walk(root, &mut files, limit);
    }
    files
}

fn memory_proposals(repo: &Path, source: Option<&Path>) -> Vec<MemoryProposal> {
    let mut proposals = Vec::new();
    if let Some(proposal) = validation_proposal(repo) {
        proposals.push(proposal);
    }
    if let Some(proposal) = toolchain_proposal(repo) {
        proposals.push(proposal);
    }
    let roots = source
        .map(|path| vec![path.to_path_buf()])
        .unwrap_or_else(|| vec![repo.join("README.md"), repo.join("docs")]);
    for root in roots {
        for path in markdown_files(&root, 32) {
            proposals.extend(high_signal_markdown(&path, repo));
        }
    }
    let mut seen = BTreeSet::new();
    proposals.retain(|proposal| seen.insert(proposal.key.clone()));
    proposals
}

fn initialize_memory_with(
    repo: &Path,
    options: &MemoryInitOptions,
    env: ctx::config::EnvLookup<'_>,
) -> SetupResult<MemoryInitReport> {
    if let Some(source) = &options.source {
        let metadata = std::fs::symlink_metadata(source).map_err(|error| {
            format!(
                "could not inspect memory source {}: {error}",
                source.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!("refusing symlinked memory source {}", source.display()).into());
        }
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(format!(
                "memory source must be a Markdown file or directory: {}",
                source.display()
            )
            .into());
        }
    }
    let cfg = ctx::config::CtxConfig::load(repo, env)?;
    let state = ctx::state::StateDir::resolve(env)?;
    let slug = ctx::state::repo_slug(repo);
    let existing =
        ctx::memory::list_scoped_unchecked(ctx::memory::MemoryScope::Shared, repo, &state, &slug)?;
    if !existing.is_empty() && !options.merge {
        return Err(format!(
            "shared memory is already initialized with {} entries; use --merge to add only missing keys",
            existing.len()
        )
        .into());
    }
    let existing_keys = existing
        .iter()
        .map(|(_, entry)| entry.key.clone())
        .collect::<BTreeSet<_>>();
    let proposals = memory_proposals(repo, options.source.as_deref());
    let proposed = proposals.len().min(options.max_entries);
    let mut written = 0;
    let mut skipped_existing = 0;
    let mut body_bytes = 0;
    let timestamp = ctx::state::now_secs();
    for mut proposal in proposals.into_iter().take(options.max_entries) {
        if existing_keys.contains(&proposal.key) {
            skipped_existing += 1;
            continue;
        }
        let remaining = options.max_bytes.saturating_sub(body_bytes);
        if remaining == 0 {
            break;
        }
        proposal.body = crate::utils::truncate_bytes(proposal.body, Some(remaining));
        if proposal.body.is_empty() {
            break;
        }
        body_bytes += proposal.body.len();
        if !options.dry_run {
            let entry = ctx::memory::Entry {
                key: proposal.key,
                written_by: "zirv-setup".to_string(),
                written: timestamp,
                verified: timestamp,
                source: "setup".to_string(),
                body: proposal.body,
                importance: Some("normal".to_string()),
                confidence: Some("high".to_string()),
                tags: proposal.tags,
                paths: proposal.paths,
            };
            ctx::memory::upsert_scoped(
                ctx::memory::MemoryScope::Shared,
                repo,
                &state,
                &slug,
                &cfg,
                &entry,
            )?;
            written += 1;
        }
    }
    Ok(MemoryInitReport {
        proposed,
        written,
        skipped_existing,
        body_bytes,
    })
}

pub fn initialize_memory(
    repo: &Path,
    options: &MemoryInitOptions,
) -> SetupResult<MemoryInitReport> {
    let env = ctx::config::env_from_process();
    initialize_memory_with(repo, options, &env)
}

fn project_candidates(provider: ResetProvider, repo: &Path) -> Vec<PathBuf> {
    match provider {
        ResetProvider::Claude => [
            "CLAUDE.md",
            "CLAUDE.local.md",
            ".claude/CLAUDE.md",
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".claude/rules",
            ".claude/commands",
            ".claude/agents",
            ".claude/agent-memory",
            ".claude/skills",
            ".claude/plugins",
            ".claude/output-styles",
            ".mcp.json",
        ]
        .into_iter()
        .map(|path| repo.join(path))
        .collect(),
        ResetProvider::Codex => [
            "AGENTS.md",
            "AGENTS.override.md",
            ".codex/config.toml",
            ".codex/hooks.json",
            ".codex/rules",
            ".codex/skills",
            ".codex/plugins",
        ]
        .into_iter()
        .map(|path| repo.join(path))
        .collect(),
        ResetProvider::All => Vec::new(),
    }
}

fn global_candidates(provider: ResetProvider, base: &Path, include_auth: bool) -> Vec<PathBuf> {
    let mut candidates = match provider {
        ResetProvider::Claude => [
            "settings.json",
            "CLAUDE.md",
            "rules",
            "commands",
            "agents",
            "agent-memory",
            "skills",
            "plugins",
            "output-styles",
            "keybindings.json",
            "themes",
        ]
        .into_iter()
        .map(|path| base.join(path))
        .collect::<Vec<_>>(),
        ResetProvider::Codex => [
            "config.toml",
            "hooks.json",
            "AGENTS.md",
            "AGENTS.override.md",
            "rules",
            "skills",
            "plugins",
        ]
        .into_iter()
        .map(|path| base.join(path))
        .collect::<Vec<_>>(),
        ResetProvider::All => Vec::new(),
    };
    if matches!(provider, ResetProvider::Codex)
        && let Ok(entries) = std::fs::read_dir(base)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".config.toml") {
                candidates.push(entry.path());
            }
        }
    }
    if include_auth {
        let auth = match provider {
            ResetProvider::Claude => ".credentials.json",
            ResetProvider::Codex => "auth.json",
            ResetProvider::All => "",
        };
        if !auth.is_empty() {
            candidates.push(base.join(auth));
        }
    }
    candidates
}

fn copy_for_backup(source: &Path, destination: &Path) -> SetupResult<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing to reset symlink {}", source.display()).into());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(destination)?;
        let mut entries = std::fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_for_backup(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination)?;
    } else {
        return Err(format!("refusing to reset non-regular target {}", source.display()).into());
    }
    Ok(())
}

fn validate_reset_target(path: &Path) -> SetupResult<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing to reset symlink {}", path.display()).into());
    }
    if metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!("refusing to reset non-regular target {}", path.display()).into());
    }
    let mut entries = std::fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        validate_reset_target(&entry.path())?;
    }
    Ok(())
}

fn remove_reset_target(path: &Path, base: &Path) -> SetupResult<()> {
    if path == base || !path.starts_with(base) {
        return Err(format!(
            "refusing to remove target outside the reset root: {}",
            path.display()
        )
        .into());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing to remove symlink {}", path.display()).into());
    }
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn unique_backup_dir(root: &Path, label: &str) -> PathBuf {
    let base = root.join(format!("{}-{label}", ctx::state::now_secs()));
    if !base.exists() {
        return base;
    }
    (1..1000)
        .map(|suffix| root.join(format!("{}-{label}-{suffix:03}", ctx::state::now_secs())))
        .find(|path| !path.exists())
        .unwrap_or_else(|| root.join(format!("{}-{label}-overflow", ctx::state::now_secs())))
}

fn relative_path_buf(base: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.file_name().map(PathBuf::from).unwrap_or_default())
}

/// Writes one versioned, manifest-tracked backup run: every target that
/// currently exists is copied verbatim into `<run>/files/<relative_path>`,
/// and every target (existing or not) gets a manifest row recording whether
/// it existed. Shared by `reset` (before deletion), `apply` (before merging
/// into a harness config file), and `restore` (before overwriting the
/// current state) -- one backup format, one place that writes it.
fn write_backup_run(
    backup_root: &Path,
    kind: BackupKind,
    provider: ResetProvider,
    scope: ResetScope,
    base: &Path,
    targets: &[PathBuf],
) -> SetupResult<PathBuf> {
    let label = format!("{:?}-{:?}", provider, scope).to_ascii_lowercase();
    let backup_dir = unique_backup_dir(backup_root, &label);
    std::fs::create_dir_all(&backup_dir)?;
    let mut manifest_targets = Vec::new();
    for path in targets {
        let relative = relative_path_buf(base, path);
        let existed_before = path.exists();
        if existed_before {
            copy_for_backup(path, &backup_dir.join("files").join(&relative))?;
        }
        manifest_targets.push(json!({
            "source": path,
            "relative_path": relative,
            "existed_before": existed_before,
        }));
    }
    let manifest = json!({
        "schema_version": SUPPORTED_MANIFEST_SCHEMA_VERSION,
        "kind": kind,
        "provider": provider,
        "scope": scope,
        "base": base,
        "created": ctx::state::now_secs(),
        "targets": manifest_targets,
    });
    std::fs::write(
        backup_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    Ok(backup_dir)
}

fn reset_one<W: Write>(
    writer: &mut W,
    provider: ResetProvider,
    scope: ResetScope,
    base: &Path,
    candidates: Vec<PathBuf>,
    backup_root: &Path,
    dry_run: bool,
) -> SetupResult<usize> {
    let existing = candidates
        .into_iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        writeln!(
            writer,
            "{:?} {:?}: no AI-specific settings found",
            provider, scope
        )?;
        return Ok(0);
    }
    if dry_run {
        for path in &existing {
            writeln!(writer, "would back up and reset {}", path.display())?;
        }
        return Ok(existing.len());
    }

    for path in &existing {
        validate_reset_target(path)?;
    }

    let backup_dir = write_backup_run(
        backup_root,
        BackupKind::Reset,
        provider,
        scope,
        base,
        &existing,
    )?;
    for path in &existing {
        remove_reset_target(path, base)?;
    }
    writeln!(
        writer,
        "reset {} {:?} setting(s); backup: {}",
        existing.len(),
        provider,
        backup_dir.display()
    )?;
    Ok(existing.len())
}

fn resolved_repo(path: &Path) -> SetupResult<PathBuf> {
    Ok(std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve repository {}: {error}", path.display()))?)
}

fn status(repo: &Path) -> SetupResult<SetupStatus> {
    let home = home_dir()?;
    let claude_dir = claude_config_dir(&home);
    let codex_dir = codex_config_dir(&home);
    let claude_settings_path = claude_dir.join("settings.json");
    let claude_settings = load_json_object(&claude_settings_path).unwrap_or_else(|_| json!({}));
    let claude_hooks_installed = HARNESS_HOOKS
        .iter()
        .filter(|(_, _, command)| contains_command(&claude_settings, command))
        .count();
    let codex_hooks = load_json_object(&codex_dir.join("hooks.json")).unwrap_or_else(|_| json!({}));
    let codex_hooks_installed = HARNESS_HOOKS
        .iter()
        .filter(|(_, _, command)| contains_command(&codex_hooks, command))
        .count();
    let claude_statusline_installed = claude_settings
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.starts_with("zirv ctx usage tee"));
    let shared_memory_entries = std::fs::read_dir(repo.join(".zirv/memory"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_type()
                        .is_ok_and(|kind| kind.is_file() && !kind.is_symlink())
                        && entry.path().extension().and_then(|value| value.to_str()) == Some("md")
                })
                .count()
        })
        .unwrap_or(0);
    Ok(SetupStatus {
        schema_version: 1,
        repo: repo.to_path_buf(),
        zirv_initialized: repo.join(".zirv").is_dir(),
        context_common: ctx::context::common_path(repo).is_file(),
        context_claude: ctx::context::claude_path(repo).is_file(),
        context_codex: ctx::context::codex_path(repo).is_file(),
        shared_memory_entries,
        claude_hooks_installed,
        claude_statusline_installed,
        codex_hooks_installed,
        claude: HarnessStatus {
            installed: executable_exists("claude"),
            settings_present: claude_settings_path.is_file(),
            config_dir: claude_dir,
        },
        codex: HarnessStatus {
            installed: executable_exists("codex"),
            settings_present: codex_dir.join("config.toml").is_file()
                || codex_dir.join("hooks.json").is_file(),
            config_dir: codex_dir,
        },
    })
}

fn run_status<W: Write>(args: &StatusArgs, writer: &mut W) -> SetupResult<i32> {
    let repo = resolved_repo(&args.repo)?;
    let status = status(&repo)?;
    if args.json {
        writeln!(writer, "{}", serde_json::to_string_pretty(&status)?)?;
        return Ok(0);
    }
    writeln!(writer, "Zirv AI setup for {}", status.repo.display())?;
    writeln!(
        writer,
        "  harnesses: Claude {}, Codex {}",
        if status.claude.installed {
            "found"
        } else {
            "not found"
        },
        if status.codex.installed {
            "found"
        } else {
            "not found"
        }
    )?;
    writeln!(
        writer,
        "  canonical context: common={}, claude={}, codex={}",
        status.context_common, status.context_claude, status.context_codex
    )?;
    writeln!(
        writer,
        "  shared memory: {} entries",
        status.shared_memory_entries
    )?;
    writeln!(
        writer,
        "  Claude integration: {}/{} hooks, statusline={}",
        status.claude_hooks_installed,
        HARNESS_HOOKS.len(),
        status.claude_statusline_installed
    )?;
    writeln!(
        writer,
        "  Codex integration: {}/{} hooks",
        status.codex_hooks_installed,
        HARNESS_HOOKS.len()
    )?;
    Ok(0)
}

fn run_apply<W: Write>(args: &ApplyArgs, writer: &mut W) -> SetupResult<i32> {
    let repo = resolved_repo(&args.repo)?;
    if args.dry_run {
        writeln!(writer, "dry run: no files will be changed")?;
    } else {
        std::fs::create_dir_all(repo.join(".zirv"))?;
    }
    if !args.no_context {
        let created = migrate_context(&repo, args.dry_run)?;
        if created.is_empty() {
            writeln!(
                writer,
                "context: nothing to migrate or targets already exist"
            )?;
        } else {
            for path in created {
                writeln!(
                    writer,
                    "context: {} {}",
                    if args.dry_run {
                        "would create"
                    } else {
                        "created"
                    },
                    path.display()
                )?;
            }
        }
    }
    if !args.no_memory {
        let options = MemoryInitOptions {
            source: args.memory_source.clone(),
            dry_run: args.dry_run,
            merge: false,
            max_entries: 16,
            max_bytes: 8192,
        };
        match initialize_memory(&repo, &options) {
            Ok(report) => writeln!(
                writer,
                "memory: {} entries proposed, {} written, {} body bytes",
                report.proposed, report.written, report.body_bytes
            )?,
            Err(error) if error.to_string().contains("already initialized") => writeln!(
                writer,
                "memory: already initialized; existing entries left untouched"
            )?,
            Err(error) => return Err(error),
        }
    }
    if !args.no_claude_hooks {
        let (hooks, statusline) = install_claude_integration(&home_dir()?, args.dry_run)?;
        writeln!(
            writer,
            "Claude: {} hook(s) {}, statusline {}",
            hooks,
            if args.dry_run {
                "would be added"
            } else {
                "added"
            },
            if statusline {
                "configured"
            } else {
                "preserved"
            }
        )?;
    }
    if !args.no_codex_hooks {
        let hooks = install_codex_integration(&home_dir()?, args.dry_run)?;
        writeln!(
            writer,
            "Codex: {} hook(s) {}; review new hooks with `/hooks` in Codex",
            hooks,
            if args.dry_run {
                "would be added"
            } else {
                "added"
            }
        )?;
    }
    let completion = if args.dry_run {
        "setup dry run complete"
    } else {
        "setup complete"
    };
    writeln!(writer, "{completion}; run `zirv setup status` to verify")?;
    Ok(0)
}

fn run_reset<W: Write>(args: &ResetArgs, writer: &mut W) -> SetupResult<i32> {
    if !args.dry_run && !args.yes {
        return Err("reset is destructive; inspect with --dry-run, then repeat with --yes".into());
    }
    if args.dry_run && !matches!(args.provider, ResetProvider::Codex) {
        writeln!(writer, "{CLAUDE_JSON_NOTE}")?;
    }
    let repo = resolved_repo(&args.repo)?;
    let home = home_dir()?;
    let providers = match args.provider {
        ResetProvider::Claude => vec![ResetProvider::Claude],
        ResetProvider::Codex => vec![ResetProvider::Codex],
        ResetProvider::All => vec![ResetProvider::Claude, ResetProvider::Codex],
    };
    let scopes = match args.scope {
        ResetScope::Project => vec![ResetScope::Project],
        ResetScope::Global => vec![ResetScope::Global],
        ResetScope::All => vec![ResetScope::Project, ResetScope::Global],
    };
    for provider in providers {
        for scope in &scopes {
            match scope {
                ResetScope::Project => {
                    reset_one(
                        writer,
                        provider,
                        *scope,
                        &repo,
                        project_candidates(provider, &repo),
                        &repo.join(".zirv/backups/ai-reset"),
                        args.dry_run,
                    )?;
                }
                ResetScope::Global => {
                    let base = match provider {
                        ResetProvider::Claude => claude_config_dir(&home),
                        ResetProvider::Codex => codex_config_dir(&home),
                        ResetProvider::All => unreachable!(),
                    };
                    let resolved_base =
                        std::fs::canonicalize(&base).unwrap_or_else(|_| base.clone());
                    if resolved_base.starts_with(&repo) {
                        return Err(format!(
                            "refusing global reset because {} is inside repository {}",
                            base.display(),
                            repo.display()
                        )
                        .into());
                    }
                    reset_one(
                        writer,
                        provider,
                        *scope,
                        &base,
                        global_candidates(provider, &base, args.include_auth),
                        &home.join(".zirv/backups/ai-reset"),
                        args.dry_run,
                    )?;
                }
                ResetScope::All => unreachable!(),
            }
        }
    }
    if !args.include_auth {
        writeln!(
            writer,
            "authentication and session/history data were preserved"
        )?;
    }
    Ok(0)
}

fn run_guided<W: Write>(writer: &mut W) -> SetupResult<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "interactive setup requires a terminal; use `zirv setup apply` in automation".into(),
        );
    }
    let action = dialoguer::Select::new()
        .with_prompt("Zirv AI setup")
        .items([
            "Apply full setup",
            "Show status",
            "Factory reset",
            "Restore a previous configuration",
            "Cancel",
        ])
        .default(0)
        .interact()?;
    match action {
        0 => run_apply(
            &ApplyArgs {
                repo: PathBuf::from("."),
                dry_run: false,
                no_context: false,
                no_memory: false,
                no_claude_hooks: false,
                no_codex_hooks: false,
                memory_source: None,
            },
            writer,
        ),
        1 => run_status(
            &StatusArgs {
                repo: PathBuf::from("."),
                json: false,
            },
            writer,
        ),
        2 => {
            let provider = dialoguer::Select::new()
                .with_prompt("Harness settings to reset")
                .items(["Claude", "Codex", "Both"])
                .default(2)
                .interact()?;
            let scope = dialoguer::Select::new()
                .with_prompt("Reset scope")
                .items(["Project", "Global", "Both"])
                .default(0)
                .interact()?;
            if !dialoguer::Confirm::new()
                .with_prompt(GUIDED_RESET_CONFIRM_PROMPT)
                .default(false)
                .interact()?
            {
                return Ok(0);
            }
            run_reset(
                &ResetArgs {
                    provider: [
                        ResetProvider::Claude,
                        ResetProvider::Codex,
                        ResetProvider::All,
                    ][provider],
                    scope: [ResetScope::Project, ResetScope::Global, ResetScope::All][scope],
                    repo: PathBuf::from("."),
                    include_auth: false,
                    dry_run: false,
                    yes: true,
                },
                writer,
            )
        }
        3 => run_guided_restore(writer),
        _ => Ok(0),
    }
}

/// The guided counterpart to `restore --list` + `restore <id>`: presents the
/// same absent/identical/differs classification as a readable menu instead
/// of a column dump, previews with a dry run before asking for the same
/// explicit confirmation the flag surface requires, and never skips that
/// confirmation -- the guided path is a friendlier front end onto
/// `restore_run`, not a second, looser gate.
fn run_guided_restore<W: Write>(writer: &mut W) -> SetupResult<i32> {
    let repo = resolved_repo(&PathBuf::from("."))?;
    let home = home_dir()?;
    let roots = vec![
        repo.join(".zirv/backups/ai-reset"),
        home.join(".zirv/backups/ai-reset"),
    ];
    let runs = all_runs(&roots);
    if runs.is_empty() {
        writeln!(writer, "{GUIDED_NO_BACKUPS_MESSAGE}")?;
        return Ok(0);
    }

    writeln!(writer, "{}", backup_summary_line(&runs))?;
    let mut items = runs.iter().map(format_run_line).collect::<Vec<_>>();
    items.push("Cancel".to_string());

    let choice = dialoguer::Select::new()
        .with_prompt("Restore which backup run?")
        .items(&items)
        .default(0)
        .interact()?;
    if choice == runs.len() {
        return Ok(0);
    }
    let run = &runs[choice];

    let preview = RestoreArgs {
        id: Some(run.id.clone()),
        list: false,
        latest: false,
        provider: None,
        scope: None,
        repo: PathBuf::from("."),
        include_auth: false,
        dry_run: true,
        yes: false,
        json: false,
    };
    restore_run(writer, run, &preview)?;

    if !dialoguer::Confirm::new()
        .with_prompt(GUIDED_RESTORE_CONFIRM_PROMPT)
        .default(false)
        .interact()?
    {
        return Ok(0);
    }

    let apply = RestoreArgs {
        yes: true,
        dry_run: false,
        ..preview
    };
    restore_run(writer, run, &apply)
}

fn matches_provider(filter: ResetProvider, actual: ResetProvider) -> bool {
    matches!(filter, ResetProvider::All) || filter == actual
}

fn matches_scope(filter: ResetScope, actual: ResetScope) -> bool {
    matches!(filter, ResetScope::All) || filter == actual
}

fn is_auth_relative_path(relative: &Path) -> bool {
    matches!(
        relative.to_string_lossy().as_ref(),
        ".credentials.json" | "auth.json"
    )
}

/// Days-since-epoch to proleptic Gregorian (y, m, d), Howard Hinnant's
/// `civil_from_days` algorithm -- avoids pulling in a date/time dependency
/// for one human-readable timestamp column.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn format_epoch(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// Byte-for-byte comparison of two files, or a recursive comparison of two
/// directories (same entry names, each entry equal in turn). Symlinks never
/// compare equal -- restore's safety classification must never call a
/// symlink target "identical" and therefore harmless to overwrite.
fn paths_equal(a: &Path, b: &Path) -> SetupResult<bool> {
    let (Ok(ma), Ok(mb)) = (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) else {
        return Ok(false);
    };
    if ma.file_type().is_symlink() || mb.file_type().is_symlink() {
        return Ok(false);
    }
    if ma.is_dir() != mb.is_dir() {
        return Ok(false);
    }
    if ma.is_dir() {
        let mut entries_a = std::fs::read_dir(a)?.collect::<Result<Vec<_>, _>>()?;
        let mut entries_b = std::fs::read_dir(b)?.collect::<Result<Vec<_>, _>>()?;
        entries_a.sort_by_key(|entry| entry.file_name());
        entries_b.sort_by_key(|entry| entry.file_name());
        if entries_a.len() != entries_b.len() {
            return Ok(false);
        }
        for (entry_a, entry_b) in entries_a.iter().zip(entries_b.iter()) {
            if entry_a.file_name() != entry_b.file_name() {
                return Ok(false);
            }
            if !paths_equal(&entry_a.path(), &entry_b.path())? {
                return Ok(false);
            }
        }
        Ok(true)
    } else {
        Ok(std::fs::read(a)? == std::fs::read(b)?)
    }
}

fn target_state(run_dir: &Path, target: &ManifestTarget) -> SetupResult<TargetState> {
    let current_exists = target.source.exists();
    if !target.existed_before {
        return Ok(if current_exists {
            TargetState::Differs
        } else {
            TargetState::Absent
        });
    }
    if !current_exists {
        return Ok(TargetState::Absent);
    }
    let backup_path = run_dir.join("files").join(&target.relative_path);
    Ok(if paths_equal(&backup_path, &target.source)? {
        TargetState::Identical
    } else {
        TargetState::Differs
    })
}

fn aggregate_state(states: &[TargetState]) -> &'static str {
    if states.is_empty() {
        return "empty";
    }
    if states.contains(&TargetState::Differs) {
        "differs"
    } else if states.iter().all(|state| *state == TargetState::Absent) {
        "absent"
    } else if states.iter().all(|state| *state == TargetState::Identical) {
        "identical"
    } else {
        "mixed"
    }
}

fn read_manifest(dir: &Path) -> SetupResult<Manifest> {
    let text = std::fs::read_to_string(dir.join("manifest.json"))?;
    Ok(serde_json::from_str(&text)?)
}

fn discover_runs(root: &Path) -> Vec<BackupRun> {
    let mut runs = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return runs;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join("manifest.json").is_file() {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir) else {
            continue;
        };
        runs.push(BackupRun {
            id: entry.file_name().to_string_lossy().to_string(),
            dir,
            manifest,
        });
    }
    runs
}

fn all_runs(roots: &[PathBuf]) -> Vec<BackupRun> {
    let mut runs = roots
        .iter()
        .flat_map(|root| discover_runs(root))
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| std::cmp::Reverse(run.manifest.created));
    runs
}

fn find_run_by_id(roots: &[PathBuf], id: &str) -> SetupResult<BackupRun> {
    for root in roots {
        let dir = root.join(id);
        if dir.join("manifest.json").is_file() {
            let manifest = read_manifest(&dir)?;
            return Ok(BackupRun {
                dir,
                id: id.to_string(),
                manifest,
            });
        }
    }
    Err(format!("no backup run found with id {id}").into())
}

fn pick_latest(
    roots: &[PathBuf],
    provider: Option<ResetProvider>,
    scope: Option<ResetScope>,
) -> SetupResult<BackupRun> {
    all_runs(roots)
        .into_iter()
        .find(|run| {
            provider.is_none_or(|filter| matches_provider(filter, run.manifest.provider))
                && scope.is_none_or(|filter| matches_scope(filter, run.manifest.scope))
        })
        .ok_or_else(|| "no matching backup runs found".into())
}

fn run_target_states(run: &BackupRun) -> Vec<TargetState> {
    run.manifest
        .targets
        .iter()
        .map(|target| target_state(&run.dir, target).unwrap_or(TargetState::Differs))
        .collect()
}

/// One human-readable line for a backup run: id, timestamp, provider/scope,
/// kind, file count, and the absent/identical/differs/mixed classification
/// that tells an operator whether restoring is safe or will clobber current
/// work. Shared by `restore --list`'s text output and the guided menu, so
/// the two surfaces read the same way.
fn format_run_line(run: &BackupRun) -> String {
    let states = run_target_states(run);
    format!(
        "{}  {}  {:?} {:?} ({:?})  {} file(s)  status={}",
        run.id,
        format_epoch(run.manifest.created),
        run.manifest.provider,
        run.manifest.scope,
        run.manifest.kind,
        run.manifest.targets.len(),
        aggregate_state(&states),
    )
}

/// Recursively sums file sizes under `path`. Errors (a vanished entry mid-walk,
/// a permission denial) are treated as zero for that entry rather than failing
/// the whole listing -- this number is informational, not load-bearing.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => dir_size(&entry.path()),
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        })
        .sum()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Makes unbounded backup growth visible instead of invisible until a disk
/// fills: nothing here prunes old runs (that is separate, filed work), but
/// every listing states the running count and on-disk size so an operator
/// can see it accumulating.
fn backup_summary_line<'a>(runs: impl IntoIterator<Item = &'a BackupRun>) -> String {
    let mut count = 0usize;
    let mut total_bytes: u64 = 0;
    for run in runs {
        count += 1;
        total_bytes += dir_size(&run.dir);
    }
    format!(
        "{count} backup run(s), {} on disk under .zirv/backups/ai-reset (old runs are never \
         automatically pruned)",
        human_bytes(total_bytes)
    )
}

fn print_restore_list<W: Write>(
    writer: &mut W,
    runs: &[BackupRun],
    args: &RestoreArgs,
) -> SetupResult<()> {
    let filtered = runs
        .iter()
        .filter(|run| {
            args.provider
                .is_none_or(|filter| matches_provider(filter, run.manifest.provider))
        })
        .filter(|run| {
            args.scope
                .is_none_or(|filter| matches_scope(filter, run.manifest.scope))
        })
        .collect::<Vec<_>>();
    if args.json {
        let rows = filtered
            .iter()
            .map(|run| {
                let states = run_target_states(run);
                json!({
                    "id": run.id,
                    "created": run.manifest.created,
                    "timestamp": format_epoch(run.manifest.created),
                    "kind": run.manifest.kind,
                    "provider": run.manifest.provider,
                    "scope": run.manifest.scope,
                    "file_count": run.manifest.targets.len(),
                    "status": aggregate_state(&states),
                    "bytes_on_disk": dir_size(&run.dir),
                })
            })
            .collect::<Vec<_>>();
        let total_bytes: u64 = filtered.iter().map(|run| dir_size(&run.dir)).sum();
        writeln!(
            writer,
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SUPPORTED_MANIFEST_SCHEMA_VERSION,
                "run_count": filtered.len(),
                "total_bytes_on_disk": total_bytes,
                "runs": rows,
                "not_covered": ["~/.claude.json"],
            }))?
        )?;
        return Ok(());
    }
    if filtered.is_empty() {
        writeln!(writer, "no backup runs found")?;
    } else {
        writeln!(writer, "{}", backup_summary_line(filtered.iter().copied()))?;
    }
    for run in &filtered {
        writeln!(writer, "{}", format_run_line(run))?;
    }
    writeln!(writer, "{CLAUDE_JSON_NOTE}")?;
    Ok(())
}

fn restore_target(backup_path: &Path, dest: &Path) -> SetupResult<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(dest) {
        if metadata.file_type().is_symlink() {
            return Err(format!("refusing to restore over symlink {}", dest.display()).into());
        }
        if metadata.is_dir() {
            std::fs::remove_dir_all(dest)?;
        } else {
            std::fs::remove_file(dest)?;
        }
    }
    copy_for_backup(backup_path, dest)
}

fn restore_run<W: Write>(writer: &mut W, run: &BackupRun, args: &RestoreArgs) -> SetupResult<i32> {
    if !args.dry_run && !args.yes {
        return Err(
            "restore is destructive; inspect with --dry-run, then repeat with --yes".into(),
        );
    }
    let manifest = &run.manifest;
    if manifest.schema_version != SUPPORTED_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "backup run {} uses manifest schema_version {}, which this build of zirv does not understand (supports schema_version {})",
            run.id, manifest.schema_version, SUPPORTED_MANIFEST_SCHEMA_VERSION
        )
        .into());
    }

    let mut targets = Vec::new();
    let mut skipped_auth = Vec::new();
    for target in &manifest.targets {
        if is_auth_relative_path(&target.relative_path) && !args.include_auth {
            skipped_auth.push(target.clone());
        } else {
            targets.push(target.clone());
        }
    }

    // Fail closed: validate every backup source and every current
    // destination before writing anything.
    for target in &targets {
        if target.existed_before {
            validate_reset_target(&run.dir.join("files").join(&target.relative_path))?;
        }
        if target.source.exists() {
            validate_reset_target(&target.source)?;
        }
        if let Some(parent) = target.source.parent() {
            for candidate in parent.ancestors().take(3) {
                if std::fs::symlink_metadata(candidate)
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    return Err(format!(
                        "refusing to restore through symlink {}",
                        candidate.display()
                    )
                    .into());
                }
            }
        }
    }

    if args.dry_run {
        writeln!(
            writer,
            "restoring backup run {} ({:?} {:?}, {})",
            run.id,
            manifest.provider,
            manifest.scope,
            format_epoch(manifest.created)
        )?;
        for target in &targets {
            let state = target_state(&run.dir, target)?;
            writeln!(
                writer,
                "would restore {} [{}]",
                target.source.display(),
                describe_state(state)
            )?;
        }
        for target in &skipped_auth {
            writeln!(
                writer,
                "would NOT restore {} (credentials; pass --include-auth)",
                target.source.display()
            )?;
        }
        return Ok(0);
    }

    // Back up the current state first, so this restore is itself undoable.
    let backup_root = run
        .dir
        .parent()
        .ok_or("backup run has no parent directory")?;
    let pre_restore_sources = targets
        .iter()
        .map(|target| target.source.clone())
        .collect::<Vec<_>>();
    let safety_backup = write_backup_run(
        backup_root,
        BackupKind::Restore,
        manifest.provider,
        manifest.scope,
        &manifest.base,
        &pre_restore_sources,
    )?;

    let mut written = Vec::new();
    let mut failed = Vec::new();
    for target in &targets {
        let result = if target.existed_before {
            let backup_path = run.dir.join("files").join(&target.relative_path);
            restore_target(&backup_path, &target.source)
        } else if target.source.exists() {
            remove_reset_target(&target.source, &manifest.base)
        } else {
            Ok(())
        };
        match result {
            Ok(()) => written.push(target.source.clone()),
            Err(error) => failed.push((target.source.clone(), error.to_string())),
        }
    }

    for path in &written {
        writeln!(writer, "restored {}", path.display())?;
    }
    for target in &skipped_auth {
        writeln!(
            writer,
            "not restored (credentials; pass --include-auth): {}",
            target.source.display()
        )?;
    }
    for (path, error) in &failed {
        writeln!(
            writer,
            "NOT restored (error): {} -- {error}",
            path.display()
        )?;
    }
    writeln!(
        writer,
        "safety backup of the pre-restore state: {}",
        safety_backup.display()
    )?;

    if !failed.is_empty() {
        return Err(format!(
            "restore partially completed: {} of {} target(s) failed",
            failed.len(),
            targets.len()
        )
        .into());
    }
    Ok(0)
}

fn run_restore<W: Write>(args: &RestoreArgs, writer: &mut W) -> SetupResult<i32> {
    let repo = resolved_repo(&args.repo)?;
    let home = home_dir()?;
    let roots = vec![
        repo.join(".zirv/backups/ai-reset"),
        home.join(".zirv/backups/ai-reset"),
    ];

    let mode_count = [args.list, args.latest, args.id.is_some()]
        .iter()
        .filter(|value| **value)
        .count();
    if mode_count == 0 {
        return Err("provide a backup id, --list, or --latest".into());
    }
    if mode_count > 1 {
        return Err("pass exactly one of: a backup id, --list, or --latest".into());
    }

    if args.list {
        let runs = all_runs(&roots);
        print_restore_list(writer, &runs, args)?;
        return Ok(0);
    }

    let run = if args.latest {
        pick_latest(&roots, args.provider, args.scope)?
    } else {
        find_run_by_id(&roots, args.id.as_deref().expect("checked above"))?
    };

    restore_run(writer, &run, args)
}

pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv setup".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match SetupCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };
    let mut writer = std::io::stdout();
    let result = match &cli.verb {
        Some(SetupVerb::Status(args)) => run_status(args, &mut writer),
        Some(SetupVerb::Apply(args)) => run_apply(args, &mut writer),
        Some(SetupVerb::Reset(args)) => run_reset(args, &mut writer),
        Some(SetupVerb::Restore(args)) => run_restore(args, &mut writer),
        None => run_guided(&mut writer),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            crate::output::error(error);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::testenv::{HomeGuard, VarGuard};

    #[test]
    fn parses_status_apply_and_guarded_reset() {
        let status = SetupCli::try_parse_from(["zirv setup", "status", "--json"]).expect("status");
        assert!(matches!(status.verb, Some(SetupVerb::Status(_))));

        let apply = SetupCli::try_parse_from(["zirv setup", "apply", "--dry-run"]).expect("apply");
        assert!(matches!(apply.verb, Some(SetupVerb::Apply(_))));

        let reset = SetupCli::try_parse_from([
            "zirv setup",
            "reset",
            "all",
            "--scope",
            "all",
            "--include-auth",
            "--yes",
        ])
        .expect("reset");
        match reset.verb {
            Some(SetupVerb::Reset(args)) => {
                assert!(matches!(args.provider, ResetProvider::All));
                assert!(matches!(args.scope, ResetScope::All));
                assert!(args.include_auth);
                assert!(args.yes);
            }
            other => panic!("expected reset, got {other:?}"),
        }
    }

    #[test]
    fn context_migration_deduplicates_identical_native_instructions() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("CLAUDE.md"), "same\r\n").expect("claude");
        std::fs::write(repo.path().join("AGENTS.md"), "same\n").expect("agents");

        let created = migrate_context(repo.path(), false).expect("migrate");
        assert_eq!(created, vec![ctx::context::common_path(repo.path())]);
        assert_eq!(
            std::fs::read_to_string(ctx::context::common_path(repo.path())).expect("common"),
            "same\n"
        );
        assert!(
            migrate_context(repo.path(), false)
                .expect("idempotent")
                .is_empty()
        );
    }

    #[test]
    fn context_migration_keeps_different_harness_instructions_separate() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("CLAUDE.md"), "claude only").expect("claude");
        std::fs::write(repo.path().join("AGENTS.md"), "codex only").expect("agents");

        let created = migrate_context(repo.path(), false).expect("migrate");
        assert_eq!(created.len(), 2);
        assert_eq!(
            std::fs::read_to_string(ctx::context::claude_path(repo.path())).expect("claude"),
            "claude only\n"
        );
        assert_eq!(
            std::fs::read_to_string(ctx::context::codex_path(repo.path())).expect("codex"),
            "codex only\n"
        );
    }

    #[test]
    fn claude_hook_merge_preserves_existing_configuration_and_deduplicates() {
        let mut settings = json!({
            "permissions": {"allow": ["Read"]},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "custom stop"}]}]
            }
        });
        assert!(
            ensure_harness_hook(&mut settings, "Stop", None, "zirv ctx hook stop").expect("add")
        );
        assert!(
            !ensure_harness_hook(&mut settings, "Stop", None, "zirv ctx hook stop")
                .expect("dedupe")
        );
        assert_eq!(settings["permissions"]["allow"][0], "Read");
        assert!(contains_command(&settings, "custom stop"));
        assert!(contains_command(&settings, "zirv ctx hook stop"));
    }

    #[test]
    fn memory_init_is_bounded_dry_runnable_and_refuses_silent_overwrite() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let state = home.path().join("state");
        let env =
            |key: &str| (key == ctx::state::STATE_ENV).then(|| state.to_string_lossy().to_string());
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .expect("manifest");
        std::fs::write(
            repo.path().join("README.md"),
            "# Demo\n\n## Testing\nRun the integration suite before release.\n",
        )
        .expect("readme");
        let dry = MemoryInitOptions {
            source: None,
            dry_run: true,
            merge: false,
            max_entries: 3,
            max_bytes: 256,
        };
        let report = initialize_memory_with(repo.path(), &dry, &env).expect("dry run");
        assert!(report.proposed > 0);
        assert!(report.body_bytes <= 256);
        assert!(!repo.path().join(".zirv/memory").exists());

        let actual = MemoryInitOptions {
            dry_run: false,
            ..dry.clone()
        };
        let report = initialize_memory_with(repo.path(), &actual, &env).expect("write");
        assert!(report.written > 0);
        let error =
            initialize_memory_with(repo.path(), &actual, &env).expect_err("must refuse overwrite");
        assert!(error.to_string().contains("already initialized"));
    }

    #[test]
    fn codex_hook_merge_preserves_existing_configuration_and_backs_up() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        let hooks_path = root.path().join("hooks.json");
        std::fs::write(
            &hooks_path,
            r#"{"description":"keep me","hooks":{"Stop":[{"hooks":[{"type":"command","command":"custom stop"}]}]}}"#,
        )
        .expect("hooks");

        assert_eq!(
            install_codex_hooks(home.path(), &hooks_path, false).expect("install"),
            HARNESS_HOOKS.len()
        );
        let hooks = load_json_object(&hooks_path).expect("load");
        assert_eq!(hooks["description"], "keep me");
        assert!(contains_command(&hooks, "custom stop"));
        for (_, _, command) in HARNESS_HOOKS {
            assert!(contains_command(&hooks, command), "missing {command}");
        }
        assert_eq!(
            install_codex_hooks(home.path(), &hooks_path, false).expect("idempotent"),
            0
        );
        // The old ad-hoc sibling backup is gone; the backup is now
        // manifest-tracked under the home backup root, restorable via
        // `zirv setup restore`.
        assert!(
            !std::fs::read_dir(root.path())
                .expect("dir")
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("hooks.json.zirv-backup-"))
        );
        let backup_root = home.path().join(".zirv/backups/ai-reset");
        let run = std::fs::read_dir(&backup_root)
            .expect("backup root")
            .next()
            .expect("one backup run")
            .expect("entry")
            .path();
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(run.join("manifest.json")).expect("manifest"),
        )
        .expect("manifest json");
        assert_eq!(manifest["kind"], "apply");
        assert_eq!(manifest["provider"], "codex");
        assert!(run.join("files/hooks.json").is_file());
    }

    #[test]
    fn memory_init_reports_a_missing_source() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let state = home.path().join("state");
        let env =
            |key: &str| (key == ctx::state::STATE_ENV).then(|| state.to_string_lossy().to_string());
        let options = MemoryInitOptions {
            source: Some(repo.path().join("missing-vault")),
            dry_run: true,
            merge: false,
            max_entries: 3,
            max_bytes: 256,
        };
        let error =
            initialize_memory_with(repo.path(), &options, &env).expect_err("missing source");
        assert!(
            error
                .to_string()
                .contains("could not inspect memory source")
        );
    }

    #[test]
    fn reset_backs_up_exact_targets_and_preserves_auth_by_default() {
        let root = tempfile::tempdir().expect("root");
        let base = root.path().join(".codex");
        let backup = root.path().join("backups");
        std::fs::create_dir_all(&base).expect("base");
        std::fs::write(base.join("config.toml"), "model='demo'\n").expect("config");
        std::fs::write(base.join("auth.json"), "{}\n").expect("auth");

        let mut output = Vec::new();
        reset_one(
            &mut output,
            ResetProvider::Codex,
            ResetScope::Global,
            &base,
            vec![base.join("config.toml")],
            &backup,
            false,
        )
        .expect("reset");

        assert!(!base.join("config.toml").exists());
        assert!(base.join("auth.json").is_file());
        let run = std::fs::read_dir(&backup)
            .expect("backups")
            .next()
            .expect("one backup")
            .expect("entry")
            .path();
        assert!(run.join("files/config.toml").is_file());
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(run.join("manifest.json")).expect("manifest"),
        )
        .expect("manifest json");
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["provider"], "codex");
        assert_eq!(manifest["scope"], "global");
        assert_eq!(manifest["targets"][0]["relative_path"], "config.toml");
        assert_eq!(
            std::fs::read_to_string(run.join("files/config.toml")).expect("backup contents"),
            "model='demo'\n"
        );
    }

    #[test]
    fn reset_requires_confirmation_and_dry_run_is_write_free() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "keep until confirmed\n").expect("claude");

        let mut output = Vec::new();
        let args = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: false,
        };
        assert!(run_reset(&args, &mut output).is_err());
        assert!(repo.path().join("CLAUDE.md").is_file());
        assert!(!repo.path().join(".zirv/backups/ai-reset").exists());

        let dry_run = ResetArgs {
            dry_run: true,
            ..args
        };
        run_reset(&dry_run, &mut output).expect("preview");
        assert!(repo.path().join("CLAUDE.md").is_file());
        assert!(!repo.path().join(".zirv/backups/ai-reset").exists());
    }

    #[test]
    fn project_and_global_resets_stay_in_their_own_scopes_and_are_idempotent() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let claude_home = home.path().join("custom-claude");
        std::fs::create_dir_all(&claude_home).expect("claude home");
        std::fs::write(repo.path().join("CLAUDE.md"), "project\n").expect("project");
        std::fs::write(claude_home.join("settings.json"), "{}\n").expect("global");
        let claude_home_text = claude_home.to_string_lossy().to_string();
        let _config = VarGuard::set(&[("CLAUDE_CONFIG_DIR", Some(&claude_home_text))]);

        let mut output = Vec::new();
        let project = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&project, &mut output).expect("project reset");
        assert!(!repo.path().join("CLAUDE.md").exists());
        assert!(claude_home.join("settings.json").is_file());
        run_reset(&project, &mut output).expect("idempotent project reset");

        let global = ResetArgs {
            scope: ResetScope::Global,
            ..project
        };
        run_reset(&global, &mut output).expect("global reset");
        assert!(!claude_home.join("settings.json").exists());
    }

    #[test]
    fn global_reset_refuses_a_config_home_inside_the_repository() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let codex_home = repo.path().join(".codex");
        std::fs::create_dir_all(&codex_home).expect("codex home");
        std::fs::write(codex_home.join("config.toml"), "model='demo'\n").expect("config");
        let codex_home_text = codex_home.to_string_lossy().to_string();
        let _config = VarGuard::set(&[("CODEX_HOME", Some(&codex_home_text))]);
        let args = ResetArgs {
            provider: ResetProvider::Codex,
            scope: ResetScope::Global,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };

        let error = run_reset(&args, &mut Vec::new()).expect_err("must refuse");
        assert!(error.to_string().contains("inside repository"));
        assert!(codex_home.join("config.toml").is_file());
    }

    #[test]
    fn include_auth_is_explicit_and_backed_up_before_removal() {
        let root = tempfile::tempdir().expect("root");
        let base = root.path().join(".codex");
        let backup = root.path().join("backups");
        std::fs::create_dir_all(&base).expect("base");
        std::fs::write(base.join("auth.json"), "{\"token\":\"example\"}\n").expect("auth");

        reset_one(
            &mut Vec::new(),
            ResetProvider::Codex,
            ResetScope::Global,
            &base,
            global_candidates(ResetProvider::Codex, &base, true),
            &backup,
            false,
        )
        .expect("reset auth");

        assert!(!base.join("auth.json").exists());
        let run = std::fs::read_dir(&backup)
            .expect("backups")
            .next()
            .expect("one backup")
            .expect("entry")
            .path();
        assert_eq!(
            std::fs::read_to_string(run.join("files/auth.json")).expect("backup auth"),
            "{\"token\":\"example\"}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reset_refuses_a_symlink_anywhere_in_a_target_tree_before_deleting_anything() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let base = root.path().join(".claude");
        let commands = base.join("commands");
        let outside = root.path().join("outside.md");
        std::fs::create_dir_all(&commands).expect("commands");
        std::fs::write(commands.join("safe.md"), "safe\n").expect("safe");
        std::fs::write(&outside, "outside\n").expect("outside");
        symlink(&outside, commands.join("escape.md")).expect("symlink");

        let error = reset_one(
            &mut Vec::new(),
            ResetProvider::Claude,
            ResetScope::Global,
            &base,
            vec![commands.clone()],
            &root.path().join("backups"),
            false,
        )
        .expect_err("must refuse symlink");
        assert!(error.to_string().contains("symlink"));
        assert!(commands.join("safe.md").is_file());
        assert!(outside.is_file());
    }

    #[test]
    fn status_json_has_a_stable_versioned_shape() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let mut output = Vec::new();
        run_status(
            &StatusArgs {
                repo: repo.path().to_path_buf(),
                json: true,
            },
            &mut output,
        )
        .expect("status");
        let value: Value = serde_json::from_slice(&output).expect("status json");
        assert_eq!(value["schema_version"], 1);
        for key in [
            "repo",
            "zirv_initialized",
            "shared_memory_entries",
            "claude_hooks_installed",
            "codex_hooks_installed",
            "claude",
            "codex",
        ] {
            assert!(value.get(key).is_some(), "missing stable status key {key}");
        }
    }

    fn restore_args(repo: &Path) -> RestoreArgs {
        RestoreArgs {
            id: None,
            list: false,
            latest: false,
            provider: None,
            scope: None,
            repo: repo.to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: false,
            json: false,
        }
    }

    #[test]
    fn parses_restore_list_id_and_latest() {
        let list = SetupCli::try_parse_from(["zirv setup", "restore", "--list"]).expect("list");
        match list.verb {
            Some(SetupVerb::Restore(args)) => assert!(args.list),
            other => panic!("expected restore, got {other:?}"),
        }

        let by_id =
            SetupCli::try_parse_from(["zirv setup", "restore", "1700000000-claude-project"])
                .expect("by id");
        match by_id.verb {
            Some(SetupVerb::Restore(args)) => {
                assert_eq!(args.id.as_deref(), Some("1700000000-claude-project"))
            }
            other => panic!("expected restore, got {other:?}"),
        }

        let latest = SetupCli::try_parse_from([
            "zirv setup",
            "restore",
            "--latest",
            "--provider",
            "claude",
            "--scope",
            "global",
            "--yes",
        ])
        .expect("latest");
        match latest.verb {
            Some(SetupVerb::Restore(args)) => {
                assert!(args.latest);
                assert!(matches!(args.provider, Some(ResetProvider::Claude)));
                assert!(matches!(args.scope, Some(ResetScope::Global)));
                assert!(args.yes);
            }
            other => panic!("expected restore, got {other:?}"),
        }
    }

    #[test]
    fn restore_requires_exactly_one_of_id_list_or_latest() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());

        let none = run_restore(&restore_args(repo.path()), &mut Vec::new());
        assert!(none.unwrap_err().to_string().contains("--list"));

        let both = RestoreArgs {
            list: true,
            latest: true,
            ..restore_args(repo.path())
        };
        assert!(
            run_restore(&both, &mut Vec::new())
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn restore_list_classifies_absent_identical_and_differing_targets() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "project instructions\n").expect("claude");

        // Reset once: this both removes CLAUDE.md and produces a restorable run.
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");
        assert!(!repo.path().join("CLAUDE.md").exists());

        // Absent case: nothing recreated yet.
        let mut output = Vec::new();
        run_restore(
            &RestoreArgs {
                list: true,
                json: true,
                ..restore_args(repo.path())
            },
            &mut output,
        )
        .expect("list absent");
        let value: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["runs"][0]["status"], "absent");
        assert_eq!(value["not_covered"][0], "~/.claude.json");

        // Identical case: recreate the exact original content.
        std::fs::write(repo.path().join("CLAUDE.md"), "project instructions\n").expect("recreate");
        let mut output = Vec::new();
        run_restore(
            &RestoreArgs {
                list: true,
                json: true,
                ..restore_args(repo.path())
            },
            &mut output,
        )
        .expect("list identical");
        let value: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["runs"][0]["status"], "identical");

        // Differs case: current content diverges from the backup.
        std::fs::write(repo.path().join("CLAUDE.md"), "something else entirely\n")
            .expect("diverge");
        let mut output = Vec::new();
        run_restore(
            &RestoreArgs {
                list: true,
                json: true,
                ..restore_args(repo.path())
            },
            &mut output,
        )
        .expect("list differs");
        let value: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["runs"][0]["status"], "differs");
    }

    #[test]
    fn restore_requires_confirmation_and_dry_run_previews_without_writing() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "keep\n").expect("claude");
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");

        let mut listing = Vec::new();
        run_restore(
            &RestoreArgs {
                list: true,
                json: true,
                ..restore_args(repo.path())
            },
            &mut listing,
        )
        .expect("list");
        let listing: Value = serde_json::from_slice(&listing).expect("listing json");
        let id = listing["runs"][0]["id"].as_str().expect("id").to_string();
        let id = id.as_str();

        let unconfirmed = RestoreArgs {
            id: Some(id.to_string()),
            ..restore_args(repo.path())
        };
        assert!(run_restore(&unconfirmed, &mut Vec::new()).is_err());
        assert!(!repo.path().join("CLAUDE.md").exists());

        let mut preview = Vec::new();
        run_restore(
            &RestoreArgs {
                id: Some(id.to_string()),
                dry_run: true,
                ..restore_args(repo.path())
            },
            &mut preview,
        )
        .expect("dry run");
        assert!(!repo.path().join("CLAUDE.md").exists());
        let preview = String::from_utf8(preview).expect("utf8");
        assert!(preview.contains("would restore"));
    }

    #[test]
    fn restore_recovers_a_reset_and_is_itself_undoable() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "original content\n").expect("claude");
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");
        assert!(!repo.path().join("CLAUDE.md").exists());

        let run = all_runs(&[repo.path().join(".zirv/backups/ai-reset")]);
        assert_eq!(run.len(), 1);
        let id = run[0].id.clone();

        let mut output = Vec::new();
        run_restore(
            &RestoreArgs {
                id: Some(id),
                yes: true,
                ..restore_args(repo.path())
            },
            &mut output,
        )
        .expect("restore");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("CLAUDE.md")).expect("restored"),
            "original content\n"
        );

        // The restore itself produced its own backup run, so a restore can
        // be undone too -- two runs now exist under the same backup root.
        let runs_after = all_runs(&[repo.path().join(".zirv/backups/ai-reset")]);
        assert_eq!(runs_after.len(), 2);
        assert!(
            runs_after
                .iter()
                .any(|run| matches!(run.manifest.kind, BackupKind::Restore))
        );
    }

    #[test]
    fn restore_undoes_an_apply_by_removing_what_it_created() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let _home = HomeGuard::set(home.path());

        // No pre-existing settings.json: apply creates it from nothing.
        let settings_path = claude_config_dir(home.path()).join("settings.json");
        assert!(!settings_path.exists());
        install_claude_integration(home.path(), false).expect("apply");
        assert!(settings_path.is_file());

        let backup_root = home.path().join(".zirv/backups/ai-reset");
        let run = all_runs(&[backup_root]);
        assert_eq!(run.len(), 1);
        assert!(matches!(run[0].manifest.kind, BackupKind::Apply));
        assert!(!run[0].manifest.targets[0].existed_before);
        let id = run[0].id.clone();

        run_restore(
            &RestoreArgs {
                id: Some(id),
                yes: true,
                ..restore_args(repo.path())
            },
            &mut Vec::new(),
        )
        .expect("restore");
        assert!(
            !settings_path.exists(),
            "restoring an apply that created the file should remove it again"
        );
    }

    #[test]
    fn restore_never_touches_credentials_without_include_auth() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let base = home.path().join("custom-codex");
        std::fs::create_dir_all(&base).expect("base");
        std::fs::write(base.join("config.toml"), "model='demo'\n").expect("config");
        std::fs::write(base.join("auth.json"), "{\"token\":\"secret\"}\n").expect("auth");
        let base_text = base.to_string_lossy().to_string();
        let _codex_home = VarGuard::set(&[("CODEX_HOME", Some(&base_text))]);

        let reset = ResetArgs {
            provider: ResetProvider::Codex,
            scope: ResetScope::Global,
            repo: repo.path().to_path_buf(),
            include_auth: true,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");
        assert!(!base.join("config.toml").exists());
        assert!(!base.join("auth.json").exists());

        let backup_root = home.path().join(".zirv/backups/ai-reset");
        let run = all_runs(&[backup_root]);
        let id = run[0].id.clone();

        // Restore without --include-auth: config.toml comes back, auth.json does not.
        let mut output = Vec::new();
        run_restore(
            &RestoreArgs {
                id: Some(id.clone()),
                yes: true,
                ..restore_args(repo.path())
            },
            &mut output,
        )
        .expect("restore without auth");
        assert!(base.join("config.toml").is_file());
        assert!(!base.join("auth.json").exists());
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("not restored (credentials"));

        // Reset again so there is something to restore with --include-auth.
        std::fs::write(base.join("auth.json"), "{\"token\":\"secret\"}\n").expect("auth again");
        run_reset(&reset, &mut Vec::new()).expect("reset again");
        let run = all_runs(&[home.path().join(".zirv/backups/ai-reset")])
            .into_iter()
            .find(|run| run.id != id && matches!(run.manifest.kind, BackupKind::Reset))
            .expect("second reset run");
        run_restore(
            &RestoreArgs {
                id: Some(run.id),
                yes: true,
                include_auth: true,
                ..restore_args(repo.path())
            },
            &mut Vec::new(),
        )
        .expect("restore with auth");
        assert!(base.join("auth.json").is_file());
    }

    #[test]
    fn restore_refuses_an_unknown_manifest_schema_version() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let backup_root = repo.path().join(".zirv/backups/ai-reset");
        let run_dir = backup_root.join("1700000000-claude-project");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::write(
            run_dir.join("manifest.json"),
            r#"{"schema_version":99,"provider":"claude","scope":"project","base":".","created":1700000000,"targets":[]}"#,
        )
        .expect("manifest");

        let error = run_restore(
            &RestoreArgs {
                id: Some("1700000000-claude-project".to_string()),
                yes: true,
                ..restore_args(repo.path())
            },
            &mut Vec::new(),
        )
        .expect_err("must refuse");
        assert!(error.to_string().contains("schema_version 99"));
    }

    #[test]
    fn restore_latest_resolves_the_most_recent_matching_run() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "claude one\n").expect("claude");
        let claude_reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&claude_reset, &mut Vec::new()).expect("claude reset");

        std::fs::write(repo.path().join("AGENTS.md"), "codex one\n").expect("agents");
        let codex_reset = ResetArgs {
            provider: ResetProvider::Codex,
            ..claude_reset
        };
        run_reset(&codex_reset, &mut Vec::new()).expect("codex reset");

        let mut output = Vec::new();
        run_restore(
            &RestoreArgs {
                latest: true,
                provider: Some(ResetProvider::Claude),
                yes: true,
                ..restore_args(repo.path())
            },
            &mut output,
        )
        .expect("restore latest claude");
        assert!(repo.path().join("CLAUDE.md").is_file());
        assert!(!repo.path().join("AGENTS.md").exists());
    }

    #[test]
    fn global_plugins_are_now_covered_by_reset_symmetrically_with_project_scope() {
        let base = Path::new("/tmp/example-claude-home");
        let candidates = global_candidates(ResetProvider::Claude, base, false);
        assert!(
            candidates.contains(&base.join("plugins")),
            "global Claude reset should cover plugins, matching project scope"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_a_symlinked_destination_before_writing_anything() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "content\n").expect("claude");
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");
        let id = all_runs(&[repo.path().join(".zirv/backups/ai-reset")])[0]
            .id
            .clone();

        let outside = repo.path().join("outside.md");
        std::fs::write(&outside, "outside\n").expect("outside");
        symlink(&outside, repo.path().join("CLAUDE.md")).expect("symlink");

        let error = run_restore(
            &RestoreArgs {
                id: Some(id),
                yes: true,
                ..restore_args(repo.path())
            },
            &mut Vec::new(),
        )
        .expect_err("must refuse symlinked destination");
        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside untouched"),
            "outside\n"
        );
    }

    #[test]
    fn guided_reset_prompt_names_restore_as_the_way_back() {
        assert!(GUIDED_RESET_CONFIRM_PROMPT.contains("recoverable"));
        assert!(GUIDED_RESET_CONFIRM_PROMPT.contains("zirv setup restore"));
    }

    #[test]
    fn guided_no_backups_message_explains_when_one_is_created() {
        assert!(GUIDED_NO_BACKUPS_MESSAGE.contains("zirv setup apply"));
        assert!(GUIDED_NO_BACKUPS_MESSAGE.contains("zirv setup reset"));
    }

    #[test]
    fn guided_restore_confirm_prompt_names_the_self_backup_guarantee() {
        assert!(GUIDED_RESTORE_CONFIRM_PROMPT.contains("backed up first"));
        assert!(GUIDED_RESTORE_CONFIRM_PROMPT.contains("undone"));
    }

    #[test]
    fn human_bytes_formats_common_magnitudes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn format_run_line_surfaces_the_safety_classification() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "content\n").expect("claude");
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");
        let runs = all_runs(&[repo.path().join(".zirv/backups/ai-reset")]);
        let line = format_run_line(&runs[0]);
        assert!(line.starts_with(&runs[0].id));
        assert!(line.contains("status=absent"));
    }

    #[test]
    fn restore_list_makes_run_count_and_disk_usage_visible() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "content\n").expect("claude");
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");

        let mut text_output = Vec::new();
        run_restore(
            &RestoreArgs {
                list: true,
                ..restore_args(repo.path())
            },
            &mut text_output,
        )
        .expect("list");
        let text_output = String::from_utf8(text_output).expect("utf8");
        assert!(
            text_output.contains("1 backup run(s)"),
            "expected a run-count summary line, got: {text_output}"
        );
        assert!(text_output.contains("on disk under .zirv/backups/ai-reset"));

        let mut json_output = Vec::new();
        run_restore(
            &RestoreArgs {
                list: true,
                json: true,
                ..restore_args(repo.path())
            },
            &mut json_output,
        )
        .expect("list json");
        let value: Value = serde_json::from_slice(&json_output).expect("json");
        assert_eq!(value["run_count"], 1);
        assert!(value["total_bytes_on_disk"].as_u64().expect("bytes") > 0);
        assert!(value["runs"][0]["bytes_on_disk"].as_u64().expect("bytes") > 0);
    }

    #[test]
    fn dir_size_sums_a_backup_runs_files() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        std::fs::write(repo.path().join("CLAUDE.md"), "0123456789\n").expect("claude");
        let reset = ResetArgs {
            provider: ResetProvider::Claude,
            scope: ResetScope::Project,
            repo: repo.path().to_path_buf(),
            include_auth: false,
            dry_run: false,
            yes: true,
        };
        run_reset(&reset, &mut Vec::new()).expect("reset");
        let runs = all_runs(&[repo.path().join(".zirv/backups/ai-reset")]);
        // manifest.json + files/CLAUDE.md, both non-empty.
        assert!(dir_size(&runs[0].dir) >= 11);
    }
}
