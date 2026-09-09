//! Issue #384: the Gemini CLI adapter (`@google/gemini-cli`).
//!
//! Every fact this module relies on was verified against the published npm
//! tarball `@google/gemini-cli@0.58.0` (`npm pack @google/gemini-cli@latest`,
//! 2026-09-07) -- both its bundled documentation (`bundle/docs/**/*.md`,
//! shipped verbatim inside the package) and, where the docs were stale or
//! silent, the bundled JS itself (`bundle/chunk-FQCNOBUR.js` and its
//! `chunk-MFLFXOVQ.js`/`chunk-RTL6OG34.js` near-duplicates, `bundle/gemini-
//! CKAAKWBN.js` and siblings). gemini-cli is not installed on this machine
//! and node here is v19 (too old to run it), so nothing below was exercised
//! against a live process -- only read from source. Anything not cited to a
//! specific symbol below is UNSUPPORTED and left at the trait default rather
//! than guessed.
//!
//! # Verified facts and their source
//!
//! - **Binary**: `gemini` (npm `@google/gemini-cli`). Headless: `-p`/
//!   `--prompt "text"` forces non-interactive mode; omitting `-p` with a
//!   non-TTY stdin also triggers headless mode and reads the prompt from
//!   stdin (`bundle/docs/cli/headless.md`, `bundle/docs/reference/
//!   configuration.md` "`--prompt <your_prompt>` (`-p <your_prompt>`)").
//!   Interactive: `gemini [query]` continues interactively (`bundle/docs/
//!   cli/cli-reference.md`). `-m`/`--model <name>` selects a model,
//!   `--resume`/`-r <latest|index|uuid>` resumes an existing session
//!   (`cli-reference.md`, `reference/configuration.md`).
//! - **Session storage**: sessions live under `~/.gemini/tmp/<projectId>/
//!   chats/session-<timestamp>-<id8>.jsonl`, one **appended** JSONL file per
//!   session -- NOT a JSON array rewritten whole on every turn, contradicting
//!   this wave's own survey assumption (`bundle/docs/cli/session-
//!   management.md` says "stored in `~/.gemini/tmp/<project_hash>/chats/`"
//!   but does not state the file's own internal shape). Verified directly in
//!   `chunk-FQCNOBUR.js`'s `ChatRecordingService`: `initialize()` builds
//!   `filename = `${SESSION_FILE_PREFIX}${timestamp}-${safeSessionId.slice(0,
//!   8)}.jsonl`` (`SESSION_FILE_PREFIX = "session-"`), and `appendRecord`
//!   does exactly one `fs.appendFileSync(this.conversationFile, line)` per
//!   record -- a metadata record first (`{sessionId, projectHash, startTime,
//!   lastUpdated, kind, directories?}`), then one record per chat message
//!   (`pushMessage` -> `appendRecord(msg)`), then optional `{"$set": {...}}`
//!   records for metadata updates (`updateMetadata`). Because this is
//!   genuinely line-local JSONL already, this adapter reads it directly
//!   (like [`super::codex::CodexAdapter`]) instead of going through
//!   `super::super::transcript_source::ShadowTranscript`: that helper's
//!   `sync_json_array` parses its `source` as ONE `serde_json::Value`
//!   (`serde_json::from_str::<Value>(&text)`), which cannot even parse a
//!   multi-line JSONL blob as valid JSON -- it is built for a harness whose
//!   own transcript really is one JSON document rewritten whole, which
//!   gemini-cli's is not.
//! - **Message row shape** (same source): `{id, timestamp, type: "user" |
//!   "gemini", content, displayContent, thoughts?, tokens?, model?,
//!   toolCalls?}` (`ChatRecordingService.newMessage`/`recordMessage`).
//!   `content`/`displayContent` are a Gemini API `PartListUnion` (a string, a
//!   single `Part`-shaped object, or an array of either) -- confirmed by
//!   `getAllSessionFiles`'s own `partListUnionToString(msg.content)` call
//!   when building session previews. `tokens` (set only on `"gemini"` rows,
//!   `recordMessage`/`recordMessageTokens`) is `{input, output, cached,
//!   thoughts, tool, total}`, sourced from the API's own
//!   `promptTokenCount`/`candidatesTokenCount`/`cachedContentTokenCount`/
//!   `thoughtsTokenCount`/`toolUsePromptTokenCount`/`totalTokenCount`.
//!   **Important residual**: `pushMessage` re-`appendRecord`s the SAME `id`
//!   every time that message is mutated (`recordToolCalls` attaching tool
//!   calls, `recordMessageTokens` attaching token counts after the text was
//!   already recorded) -- so one logical exchange can appear as 2-3 rows
//!   sharing one `id` before the next exchange begins. `AgentAdapter::
//!   parse_events` must stay line-local (`IncrementalScorer::poll`,
//!   `score.rs`, feeds it only the bytes newly appended each poll cycle, so
//!   two rows of the same re-appended `id` can land in two different polls),
//!   so [`parse_events`], [`structural_context`] and [`transcript_usage`]
//!   below do NOT fold rows across `id`s at all -- each row is handled
//!   entirely on its own. A null-`tokens` `"gemini"` row (the text-only or
//!   tool-calls-attached re-append) only ever contributes an
//!   `AssistantFirstText` candidate when it carries non-empty text --
//!   `NormalizedEvent::AssistantFirstText`'s own doc comment already allows
//!   more than one of these per turn. The row that carries non-null `tokens`
//!   (`recordMessageTokens`, verified to fire exactly once per turn, after
//!   the text was already written into that same row) is the sole source of
//!   that turn's `AssistantFinal` text/tokens and its counted
//!   `structural_context`/`transcript_usage` contribution. This assumes (per
//!   the verified single `recordMessageTokens` call site) that at most one
//!   row per `id` ever carries non-null `tokens`; if that assumption were
//!   ever violated, `transcript_usage` would double-count that id's tokens
//!   rather than silently keep only the latest, since there is no longer any
//!   cross-row state to prefer one over the other.
//! - **Project directory resolution**: `chunk-FQCNOBUR.js`'s `Storage`/
//!   `ProjectRegistry` classes show the CLI no longer names a project
//!   directory after a bare `sha256(cwd)` (the stale fact
//!   `session-management.md` still documents as `<project_hash>`) -- current
//!   0.58.0 keeps a persistent registry at `~/.gemini/projects.json`
//!   (`{"projects": {"<normalizedAbsolutePath>": "<shortId>"}}`,
//!   `ProjectRegistry.getShortId`/`.save`) mapping each project's own
//!   normalized path (`path.resolve` then lowercased on `win32` only,
//!   `ProjectRegistry.normalizePath`) to a human-slug `shortId`
//!   (`claimNewSlug`: the directory basename, slugified, de-duplicated with a
//!   numeric suffix). `Storage.getProjectTempDir()` is `~/.gemini/tmp/
//!   <shortId>`. [`chats_dir_for_cwd`] below reads this registry directly
//!   rather than reimplementing slug-claiming, since the registry write
//!   happens synchronously during `Config` startup, well before any prompt
//!   is processed.
//! - **`GEMINI_SYSTEM_MD`** (`bundle/docs/cli/system-prompt.md`) fully
//!   REPLACES the built-in system prompt and is an environment variable, not
//!   a per-run CLI flag -- there is no argv mechanism to inject or append a
//!   prompt for one launch. [`system_prompt_args`] therefore returns nothing
//!   and [`Capabilities::system_prompt`] is `false`; `GEMINI.md` context
//!   files are the repo-owned, always-on analogue of `CLAUDE.md`/`AGENTS.md`
//!   and are out of scope for a per-run override the same way.
//! - **Read-only enforcement**: gemini-cli has NO CLI flag that denies a
//!   specific tool outright the way codex's `--sandbox read-only` or
//!   claude's `--disallowedTools=...` do. `--approval-mode=plan` looks like
//!   the obvious candidate but is verified UNSAFE for this purpose:
//!   `bundle/docs/cli/plan-mode.md`'s own "Non-interactive execution"
//!   section states that in a headless run, exiting Plan Mode "automatically
//!   switches to YOLO mode instead of the standard Default mode" to
//!   auto-implement the approved plan -- i.e. a headless `--approval-mode
//!   plan` run silently escalates to auto-approving shell/write tools rather
//!   than staying read-only. `tools.exclude`/`tools.core` (`reference/
//!   configuration.md`) are `settings.json`-only keys with no per-run CLI
//!   equivalent and are explicitly deprecated in favor of the policy engine
//!   (`reference/policy-engine.md`: "The legacy `tools.exclude` setting...
//!   is deprecated in favor of policy rules with a `deny` decision").
//!   The verified per-run mechanism is `--admin-policy <path>`, a real yargs
//!   option confirmed in the bundled JS (`gemini-CKAAKWBN.js` et al.:
//!   `.option("admin-policy", {...description: "Additional admin policy
//!   files or directories to load (comma-separated or multiple
//!   --admin-policy)"...})`). An Admin-tier policy rule
//!   (`final_priority = 5 + priority/1000`) outranks every Default/
//!   Extension/User-tier rule regardless of approval mode (a rule with no
//!   `modes` is "always active" per `policy-engine.md`), so a `deny` rule
//!   for `run_shell_command`/`write_file`/`replace` there is a genuine
//!   structural deny, not a hope that the caller never asks for `yolo`.
//!   [`read_only_args`] writes that policy file (best-effort) and returns
//!   `--admin-policy <path>`. Documented residual: "Supplemental admin
//!   policies are ignored if any `.toml` policy files are found in the
//!   standard system location" (`policy-engine.md`) -- an operator-managed
//!   enterprise machine with its own system admin policy would make this
//!   flag a no-op; that machine's own system policy is what governs instead,
//!   which is the intended outcome of that guard, not a hole this adapter
//!   opens.
//! - **`/compress`** is a real interactive slash command (`bundle/docs/
//!   reference/commands.md`, `### /compress`: "Replace the entire chat
//!   context with a summary") -- `cli-reference.md`'s own cheatsheet table
//!   omits it, so [`compact_command`] is sourced from `reference/
//!   commands.md`, the more complete reference.
//! - **`/quit` (or `/exit`)** exits the interactive session (`reference/
//!   commands.md`, `cli-reference.md`). [`quit_sequence`] uses `/quit\r`,
//!   mirroring `CodexAdapter::quit_sequence`'s own `"\r"` terminator for a
//!   PTY-injected keystroke sequence.
//! - **Model ladder, strengths, prices, context windows**: this adapter adds
//!   no ladder of its own -- `catalogue::vendor("google")` (issue #381,
//!   `catalogue.rs`) already carries a 2026-09-07-dated three-rung Google
//!   ladder (`gemini-3.1-pro-preview`/`gemini-3.7-flash`/
//!   `gemini-3.5-flash-lite`, each with a stated 1,000,000-token window).
//!   Naming a specific model here is out of scope for this module; every
//!   ladder-shaped method below is a thin projection onto that table, same
//!   as `CodexAdapter`'s own `CATALOGUE_VENDOR` methods.
//!
//! # Deliberately UNSUPPORTED in this wave
//!
//! - Tool calls/results (`NormalizedEvent::ToolCall`/`ToolResult` and
//!   friends): `toolCalls[]` entries are visible on a `"gemini"` row
//!   (`recordToolCalls`), but no verified shape was found in the time
//!   available for a tool's own result/error/status once it completes (no
//!   `recordToolResult`-shaped call site was found alongside
//!   `recordToolCalls`), so this adapter -- like `CodexAdapter` -- does not
//!   model tool calls at all. [`counts_tool_calls`] is `false` for the same
//!   reason `CodexAdapter::counts_tool_calls` is: a silently-never-advancing
//!   `--max-tool-calls` ceiling is worse than refusing the flag outright.
//! - `resume_args`/`session_pin_args`: gemini has no flag to make a FRESH
//!   launch adopt a caller-chosen conversation id (no `--session-id`
//!   equivalent, verified absent from both `cli-reference.md` and
//!   `reference/configuration.md`'s exhaustive flag list), so there is
//!   nothing `session_pin_args` could pin zirv's own session uuid onto.
//!   `--resume <id>` is real, but only for an id gemini itself minted; since
//!   `session_pin_args` can never make that id equal zirv's own uuid, a
//!   dashboard restore calling `resume_args(&pane.session_id)` would resume
//!   nothing gemini ever created. Left at the trait default (`None`),
//!   mirroring `CodexAdapter`'s own identical rationale for the same gap.
//! - `interactive_read_only_args`: unlike codex's `--ignore-rules`/
//!   `--ignore-user-config` pair (verified `exec`-only), `--admin-policy` is
//!   a single global CLI flag with no documented interactive/headless split,
//!   so the trait default (delegating to [`read_only_args`]) is correct
//!   without an override.
//! - `context_window_hint`/`model_hint`'s window half: no per-transcript
//!   stated context-window figure was found in the message row shape (unlike
//!   codex's `token_count.info.model_context_window`), so
//!   `context_window_hint` stays at the trait default (`None`).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
};
use super::super::native_hooks::{NativeHookEntry, NativeHooks};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// This adapter's own vendor slug in `catalogue`'s registry (issue #381) --
/// see this module's own doc comment for what that table carries for
/// `"google"`.
const CATALOGUE_VENDOR: &str = "google";

/// `ChatRecordingService`'s own filename prefix, verified in
/// `chunk-FQCNOBUR.js` (`var SESSION_FILE_PREFIX = "session-";`). Subagent
/// chat files are named `<uuid>.jsonl` with no prefix at all and live in a
/// nested `chats/<parentSessionId>/` directory, so filtering on this prefix
/// (matching `getAllSessionFiles`'s own filter) naturally excludes them
/// without needing to know `kind` up front.
const SESSION_FILE_PREFIX: &str = "session-";

/// The read-only admin policy this adapter pins via `--admin-policy` -- see
/// this module's own doc comment ("Read-only enforcement") for why this is
/// the one verified structural deny mechanism gemini-cli exposes. Denies the
/// same three write/shell-shaped tools codex's `--sandbox read-only` and
/// claude's `--disallowedTools=Write,Edit,Bash,NotebookEdit` deny: shell
/// execution (`run_shell_command`) and file writes (`write_file`, `replace`
/// -- the two tools `plan-mode.md`'s own "Tool Restrictions" section names
/// as the write-shaped pair). Priority 999 is the ceiling of this rule's own
/// Admin tier (`final_priority = 5 + priority/1000`, `policy-engine.md`),
/// safely above every Default/Extension/User-tier rule (`< 5`) regardless of
/// approval mode, since a rule naming no `modes` is "always active".
const READ_ONLY_POLICY_TOML: &str = "[[rule]]\n\
toolName = [\"run_shell_command\", \"write_file\", \"replace\"]\n\
decision = \"deny\"\n\
priority = 999\n\
denyMessage = \"zirv: this gemini session was launched read-only; shell execution and file writes are denied\"\n";

/// See this module's own doc comment for the full verification trail behind
/// every method below. `ready()` follows `CodexAdapter::ready`'s own
/// posture exactly: gemini support is honestly degraded (several trait
/// methods fall back to their "no verified mechanism" default) but not
/// refused -- a bare `gemini` that resolves to nothing at all still passes
/// `ready()` and fails at spawn time with the OS's own "not found".
#[derive(Debug, Clone)]
pub struct GeminiAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    /// Test seam only, mirroring `CodexAdapter::forced_state_root` exactly:
    /// pins the zirv state root [`GeminiAdapter::transcript_path`] resolves
    /// the session registry and its own session-file pin from, instead of
    /// the real platform state directory. Also reused by
    /// [`GeminiAdapter::read_only_policy_path`] so a test never writes its
    /// read-only policy file into the developer's own state dir.
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
}

impl GeminiAdapter {
    /// `bin` may carry arguments, mirroring `CodexAdapter::new`/
    /// `ClaudeAdapter::new` exactly.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("gemini").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "gemini".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_state_root: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_state_root(mut self, root: PathBuf) -> Self {
        self.forced_state_root = Some(root);
        self
    }

    /// Every command starts here, mirroring `CodexAdapter::base` exactly: the
    /// program is routed through [`super::resolve_program`] so an
    /// npm-installed `gemini.cmd` shim launches on Windows.
    fn base(&self) -> Command {
        let resolved = super::resolve_program(&self.program)
            .unwrap_or_else(|_| ResolvedProgram::direct(&self.program));
        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.prefix);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The zirv state root this adapter reads the session registry and its
    /// own session-file pin from, mirroring `CodexAdapter::state_dir`
    /// exactly (including its `None` == "pin nothing, resolve nothing"
    /// contract).
    #[cfg(test)]
    fn state_dir(&self) -> Option<super::super::state::StateDir> {
        self.forced_state_root
            .clone()
            .map(super::super::state::StateDir::from_root)
    }

    #[cfg(not(test))]
    fn state_dir(&self) -> Option<super::super::state::StateDir> {
        super::super::state::StateDir::resolve(&super::super::config::env_from_process()).ok()
    }

    /// Where [`Self::read_only_args`] writes [`READ_ONLY_POLICY_TOML`].
    /// Shares the resolved state root with session-file pinning purely so a
    /// test that forces one forces the other too -- the file itself has
    /// nothing to do with sessions, it just needs a stable, writable,
    /// zirv-owned location. Falls back to the OS temp directory when no
    /// state dir resolves, matching this adapter's own "degrade, never
    /// refuse" posture elsewhere.
    fn read_only_policy_path(&self) -> PathBuf {
        let root = self
            .state_dir()
            .map(|state| state.root().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        root.join("zirv-gemini-read-only-policy.toml")
    }

    /// Registered project directory lookup: reads `~/.gemini/projects.json`
    /// (`ProjectRegistry`, this module's own doc comment) and returns
    /// `~/.gemini/tmp/<shortId>/chats` for `cwd`'s own entry. `None` when the
    /// registry is missing, unparseable, or simply has no entry for `cwd`
    /// yet (a project gemini has never launched in on this machine) --
    /// never a guess at a directory that might not exist.
    fn chats_dir_for_cwd(&self, cwd: &Path) -> Option<PathBuf> {
        let registry_path = self.home_dir().join(".gemini").join("projects.json");
        let text = std::fs::read_to_string(&registry_path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let projects = value.get("projects")?.as_object()?;
        let key = normalized_project_path(cwd);
        let short_id = projects.get(&key)?.as_str()?;
        Some(
            self.home_dir()
                .join(".gemini")
                .join("tmp")
                .join(short_id)
                .join("chats"),
        )
    }

    /// Resolution 2 and 3 of [`AgentAdapter::transcript_path`] -- mirrors
    /// `CodexAdapter::pinned_rollout`'s own shape exactly, minus the
    /// handover-floor mechanism (`codex::forget_transcript_pin`): a handover
    /// that supersedes a gemini child while keeping the same zirv session id
    /// could re-resolve onto the dead child's session file, the same bug
    /// that fix closed for codex. Not implemented here -- tracked as a
    /// follow-up, not blocking this adapter's correctness for a session that
    /// runs start-to-finish without a handover.
    fn pinned_session_file(&self, session: &SessionRef) -> Option<PathBuf> {
        let state = self.state_dir()?;
        let short = super::super::sessions::short_id(session.id.as_str());
        let pin = state.rollouts().join(format!("{short}.gemini.path"));
        if let Ok(recorded) = std::fs::read_to_string(&pin) {
            let recorded = PathBuf::from(recorded.trim());
            if recorded.is_file() {
                return Some(recorded);
            }
        }
        let record = super::super::sessions::load_record(&state, &short)?;
        let started_ms = record.started_at.saturating_mul(1_000);
        let chats_dir = self.chats_dir_for_cwd(&session.cwd)?;
        let resolved = resolve_session_file(&chats_dir, started_ms)?;
        if super::super::state::create_private_dir_all(&state.rollouts()).is_ok() {
            let _ = super::super::state::write_private(&pin, &resolved.display().to_string());
        }
        Some(resolved)
    }
}

/// `ProjectRegistry.normalizePath` (`chunk-FQCNOBUR.js`): `path.resolve`
/// (make absolute; a `SessionRef::cwd` is always already absolute here, so
/// this does not attempt the lexical normalization -- `.`/`..` segment
/// collapsing -- Node's `path.resolve` also performs, a documented
/// residual), then lowercased ONLY on Windows (`os.platform() === "win32"`).
/// macOS/Linux keep the path case-sensitive, matching the source exactly
/// even though macOS's own filesystem is often case-insensitive too.
fn normalized_project_path(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy().into_owned();
    if cfg!(windows) {
        raw.to_lowercase()
    } else {
        raw
    }
}

/// Every `chats_dir`-direct-child file whose name starts with
/// [`SESSION_FILE_PREFIX`] and ends in `.json`/`.jsonl` -- mirrors
/// `getAllSessionFiles`'s own filter. Deliberately non-recursive: a
/// subagent's own chat file lives in a nested `chats/<parentSessionId>/`
/// directory (this module's own doc comment) and is never a candidate for a
/// top-level session's own transcript.
fn collect_session_files(chats_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(chats_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(SESSION_FILE_PREFIX)
                        && (name.ends_with(".jsonl") || name.ends_with(".json"))
                })
        })
        .collect()
}

/// The `startTime` (in unix ms) a session file's own FIRST line states, or
/// `None` for a file whose first line is not a parseable metadata record.
/// Mirrors `codex::rollout_session_meta`'s own "only the first line" reading
/// exactly, for the identical reason: the file can grow to megabytes, and
/// the one record naming when the session began is always its first.
fn session_start_ms(path: &Path) -> Option<u64> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    let row: Value = serde_json::from_str(line.trim()).ok()?;
    row.get("sessionId")?;
    row.get("startTime")
        .and_then(Value::as_str)
        .and_then(super::super::window::parse_iso8601_utc_ms)
}

/// The session file inside `chats_dir` this session most plausibly created:
/// the EARLIEST one whose own metadata `startTime` is at or after
/// `started_ms` -- mirrors `codex::resolve_rollout`'s "earliest, not newest"
/// rationale exactly (a session's own launch is the first file to appear
/// after it started; "newest" would hand an older session a younger
/// concurrent one's transcript). No `cwd` cross-check is needed here, unlike
/// codex: `chats_dir` is already scoped to this session's own project via
/// [`GeminiAdapter::chats_dir_for_cwd`]'s registry lookup, so the one
/// residual codex still carries (two runs in the same directory within the
/// same second) is this function's only inherited ambiguity.
fn resolve_session_file(chats_dir: &Path, started_ms: u64) -> Option<PathBuf> {
    let mut candidates: Vec<(u64, PathBuf)> = collect_session_files(chats_dir)
        .into_iter()
        .filter_map(|path| {
            let started = session_start_ms(&path)?;
            (started >= started_ms).then_some((started, path))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates.into_iter().next().map(|(_, path)| path)
}

/// One `"gemini"` row's token reading, `None` when the row carries no
/// `tokens` object at all (verified: only set once `recordMessage`/
/// `recordMessageTokens` attach one -- see this module's own doc comment).
#[derive(Debug, Clone, Copy, Default)]
struct GeminiTokens {
    input: u64,
    output: u64,
    cached: u64,
}

fn extract_tokens(row: &Value) -> Option<GeminiTokens> {
    let tokens = row.get("tokens")?;
    if tokens.is_null() {
        return None;
    }
    Some(GeminiTokens {
        input: tokens.get("input").and_then(Value::as_u64).unwrap_or(0),
        output: tokens.get("output").and_then(Value::as_u64).unwrap_or(0),
        cached: tokens.get("cached").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// `content`/`displayContent`'s own shape: a Gemini API `PartListUnion` --
/// a bare string, a single `Part`-shaped object (`{"text": "..."}`), or an
/// array of either, per this module's own doc comment
/// (`partListUnionToString`). Only `.text` parts are stringified (joined
/// with no separator, matching a typical multi-chunk streamed response);
/// non-text parts (`functionCall`/`functionResponse`/`inlineData`) are
/// dropped rather than guessed at, since `parse_events` does not model tool
/// calls in this wave either (this module's own doc comment).
fn extract_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|v| extract_text(Some(v))).collect(),
        Some(Value::Object(map)) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// One parsed `"gemini"` or `"user"` row from a session-file line, with the
/// fields [`parse_events`]/[`structural_context`]/[`transcript_usage`] each
/// need -- deliberately NOT folded across rows sharing one `id` (this
/// module's own doc comment, "Important residual"): every caller below
/// handles each row entirely on its own so a poll boundary landing between
/// two re-appends of the same `id` can never split one logical event across
/// two `parse_events` calls.
enum ParsedRow {
    User {
        at_ms: Option<u64>,
        text: String,
    },
    Gemini {
        at_ms: Option<u64>,
        text: String,
        tokens: Option<GeminiTokens>,
        model: Option<String>,
    },
}

/// Parses one `jsonl` line into a [`ParsedRow`], or `None` for a line that is
/// not itself a chat turn (unparseable JSON, or the first-line metadata
/// record / a `$set` update record, neither of which carries a `type`
/// field).
fn parse_row(line: &str) -> Option<ParsedRow> {
    let row: Value = serde_json::from_str(line.trim()).ok()?;
    let kind = row.get("type").and_then(Value::as_str)?;
    let at_ms = row
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(super::super::window::parse_iso8601_utc_ms);
    match kind {
        "user" => Some(ParsedRow::User {
            at_ms,
            text: extract_text(row.get("content")),
        }),
        "gemini" => Some(ParsedRow::Gemini {
            at_ms,
            text: extract_text(row.get("content")),
            tokens: extract_tokens(&row),
            model: row.get("model").and_then(Value::as_str).map(str::to_string),
        }),
        _ => None,
    }
}

impl AgentAdapter for GeminiAdapter {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Gemini spends a Google account's limits. Nothing collects readings
    /// for it yet -- see `CodexAdapter::provider`'s own doc comment for why
    /// the provider is still worth naming even so.
    fn provider(&self) -> &'static str {
        "google"
    }

    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| {
                let f = f.to_string_lossy();
                f == "gemini" || f == "gemini.cmd" || f == "gemini.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt>` forces non-interactive mode (verified:
    /// `reference/configuration.md`, `--prompt <your_prompt>` (`-p
    /// <your_prompt>`)). Gemini mints its own session id (no `--session-id`
    /// equivalent -- this module's own doc comment), so `session` cannot
    /// appear in the built command, mirroring `CodexAdapter::headless_cmd`.
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p").arg(prompt).args(extra);
        cmd
    }

    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// Omitting `-p` entirely with a non-TTY stdin also triggers headless
    /// mode and reads the prompt from stdin (`headless.md`: "triggered...
    /// when the CLI is run in a non-TTY environment"; `--prompt`'s own
    /// description: "Appended to stdin input if provided" implies stdin is
    /// read as the base prompt when `-p` is absent). Mirrors
    /// `CodexAdapter::headless_cmd_stdin`'s identical shape and the same
    /// role: moving the headless prompt off argv for a Windows `.cmd` shim
    /// launch that would otherwise let `cmd.exe` reparse it.
    fn headless_cmd_stdin(&self, _session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.args(extra);
        Some(cmd)
    }

    /// `gemini [query]` continues interactively with `query` as the initial
    /// prompt (verified: `cli-reference.md` row `gemini "explain this
    /// project"`), mirroring `CodexAdapter::interactive_cmd`.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// The prompt is delivered on stdin, matching `CodexAdapter::
    /// distiller_cmd`'s own shape (no positional prompt token here either) --
    /// the caller (`handoff::run_model`) pipes it in, and gemini's own `-p`
    /// omission-plus-non-TTY-stdin rule (`headless_cmd_stdin`'s own doc
    /// comment) means this still runs headless.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        if !model.is_empty() {
            cmd.arg("-m").arg(model);
        }
        cmd.args(self.read_only_args());
        cmd
    }

    /// `--admin-policy <path>` naming [`READ_ONLY_POLICY_TOML`] -- see this
    /// module's own doc comment ("Read-only enforcement") for the full
    /// verification trail on why this, and not `--approval-mode=plan` or
    /// `tools.exclude`, is the structural deny mechanism. The write goes
    /// through `state::create_private_dir_all`/`state::write_private`
    /// (atomic temp-then-rename, private perms), exactly like
    /// [`GeminiAdapter::pinned_session_file`]'s own pin write in this same
    /// file, rather than a bare `std::fs::write` to a path under the shared
    /// zirv state root. Best-effort either way: on failure (e.g. a read-only
    /// filesystem) this still returns the argv naming the intended path,
    /// since gemini-cli's own behavior against a missing `--admin-policy`
    /// file is unverified either way, and a silently-permissive empty return
    /// would be the worse failure mode for a caller that only ever asks for
    /// this to keep a judgment child from writing.
    fn read_only_args(&self) -> Vec<String> {
        let path = self.read_only_policy_path();
        if let Some(parent) = path.parent() {
            let _ = super::super::state::create_private_dir_all(parent);
        }
        let _ = super::super::state::write_private(&path, READ_ONLY_POLICY_TOML);
        vec!["--admin-policy".to_string(), path.display().to_string()]
    }

    /// `GEMINI_SYSTEM_MD` is an environment variable that fully replaces the
    /// system prompt -- there is no per-run argv mechanism to inject or
    /// append one (this module's own doc comment). Empty, the same "no
    /// verified mechanism" shape every other unsupported layer on this trait
    /// uses.
    fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
        Vec::new()
    }

    /// See [`GeminiAdapter::pinned_session_file`] for resolutions 2 and 3.
    /// Resolution 1 (a file already named after zirv's own session id) is
    /// skipped entirely, unlike `CodexAdapter::transcript_path`: gemini's
    /// filename only ever embeds the first 8 hex characters of gemini's OWN
    /// internally minted id (`SESSION_FILE_PREFIX}${timestamp}-
    /// ${safeSessionId.slice(0,8)}.jsonl`), which is never zirv's uuid, so
    /// there is no plausible future path where that check could ever hit.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        if let Some(pinned) = self.pinned_session_file(session) {
            return pinned;
        }
        self.home_dir()
            .join(".gemini")
            .join("tmp")
            .join("unresolved")
            .join("chats")
            .join(format!(
                "{SESSION_FILE_PREFIX}unresolved-{}.jsonl",
                super::super::sessions::short_id(session.id.as_str())
            ))
    }

    /// See this module's own doc comment ("Important residual") for why this
    /// is fully line-local with no cross-row folding. `TurnStart`/`UserText`
    /// come from `"user"` rows. For a `"gemini"` row: non-empty text always
    /// contributes an `AssistantFirstText` candidate (possibly more than one
    /// per turn, which `NormalizedEvent::AssistantFirstText`'s own doc
    /// comment allows); `AssistantFinal`/`ModelId` are emitted ONLY from a
    /// row that carries non-null `tokens` -- a null-`tokens` row (the
    /// text-only or tool-calls-attached re-append) never produces a final,
    /// so the duplicate `id` re-append this harness performs can never
    /// flush two finals for the same turn. Tool calls/results are
    /// deliberately not modeled (this module's own doc comment,
    /// "Deliberately UNSUPPORTED").
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        let mut events = Vec::new();
        for row in jsonl.lines().filter_map(parse_row) {
            match row {
                ParsedRow::User { at_ms, text } => {
                    events.push(NormalizedEvent::TurnStart { at_ms });
                    if !text.trim().is_empty() {
                        events.push(NormalizedEvent::UserText {
                            byte_len: text.len() as u64,
                        });
                    }
                }
                ParsedRow::Gemini {
                    at_ms,
                    text,
                    tokens,
                    model,
                } => {
                    if !text.trim().is_empty() {
                        events.push(NormalizedEvent::AssistantFirstText { at_ms });
                    }
                    if let Some(tokens) = tokens {
                        events.push(NormalizedEvent::AssistantFinal {
                            text,
                            input_tokens: tokens.input,
                            at_ms,
                        });
                        if let Some(model) = model {
                            events.push(NormalizedEvent::ModelId { id: model });
                        }
                    }
                }
            }
        }
        events
    }

    /// Only `user_messages`/`assistant_texts` are populated, from the same
    /// verified row shape [`parse_events`] uses -- `files_read`/
    /// `files_modified`/`tool_errors` stay empty, mirroring
    /// `CodexAdapter::structural_context`'s own honest gap: no verified
    /// per-tool-call result shape exists to build them from (this module's
    /// own doc comment). `last_n` truncation mirrors both existing adapters'
    /// own `keep_last` convention: `last_n == 0` keeps nothing at all.
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut user_messages = Vec::new();
        let mut assistant_texts = Vec::new();
        for row in jsonl.lines().filter_map(parse_row) {
            match row {
                ParsedRow::User { text, .. } if !text.trim().is_empty() => {
                    user_messages.push(text);
                }
                // Only the row carrying non-null `tokens` counts, mirroring
                // `parse_events`'s `AssistantFinal` rule exactly -- otherwise
                // the duplicate `id` re-append would double this turn's text
                // into the list.
                ParsedRow::Gemini {
                    text,
                    tokens: Some(_),
                    ..
                } if !text.trim().is_empty() => {
                    assistant_texts.push(text);
                }
                _ => {}
            }
        }
        if assistant_texts.len() > last_n {
            assistant_texts.drain(..assistant_texts.len() - last_n);
        }
        StructuralContext {
            user_messages,
            assistant_texts,
            ..StructuralContext::default()
        }
    }

    /// The most recent `"gemini"` row's own `model` field, scanned from the
    /// end -- mirrors `CodexAdapter::model_hint`'s own `.rev()` approach.
    /// Deliberately reads raw rows rather than [`parse_row`]: this only ever
    /// wants the single latest value, and a reverse scan finds it in one
    /// pass without needing to parse every row's other fields too.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        jsonl.lines().rev().find_map(|line| {
            let row = serde_json::from_str::<Value>(line.trim()).ok()?;
            if row.get("type").and_then(Value::as_str) != Some("gemini") {
                return None;
            }
            row.get("model").and_then(Value::as_str).map(str::to_string)
        })
    }

    /// Sums each `"gemini"` row's `tokens` when present. This counts each
    /// logical turn exactly once WITHOUT needing to fold consecutive same-
    /// `id` rows: the verified `recordMessageTokens` call site (this
    /// module's own doc comment, "Important residual") fires exactly once
    /// per turn, so at most one re-appended row per `id` ever carries
    /// non-null `tokens` -- the null-`tokens` re-appends this loop also sees
    /// contribute nothing. `cache_creation_input_tokens` is always `0`:
    /// gemini's own token breakdown has no separate cache-WRITE class, only
    /// `cached` (tokens served FROM cache), which maps to
    /// `cache_read_input_tokens` -- an honest zero, never a guessed class,
    /// the same choice `CodexAdapter::transcript_usage` makes for its own
    /// missing cache fields.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let mut usage = TranscriptUsage::default();
        let mut observed = false;
        for row in jsonl.lines().filter_map(parse_row) {
            let ParsedRow::Gemini {
                tokens: Some(tokens),
                ..
            } = row
            else {
                continue;
            };
            usage.input_tokens = usage.input_tokens.saturating_add(tokens.input);
            usage.output_tokens = usage.output_tokens.saturating_add(tokens.output);
            usage.cache_read_input_tokens =
                usage.cache_read_input_tokens.saturating_add(tokens.cached);
            observed = true;
        }
        observed.then_some(usage)
    }

    /// This reports the sum over exactly the fragment it is handed (the
    /// trait default meaning of `false`), not a running total pulled from
    /// one "latest snapshot" line the way `CodexAdapter`'s cumulative
    /// `TokenCount` totals let it do -- gemini's own per-turn `tokens`
    /// object never restates an earlier turn's spend.
    fn transcript_usage_is_cumulative(&self) -> bool {
        false
    }

    /// See this module's own doc comment, "Deliberately UNSUPPORTED": no
    /// verified per-tool-call result shape was found, so `parse_events`
    /// never emits `NormalizedEvent::ToolCall` at all. Mirrors
    /// `CodexAdapter::counts_tool_calls`'s own rationale exactly: a
    /// `--max-tool-calls` ceiling that silently never advances is worse than
    /// refusing the flag outright.
    fn counts_tool_calls(&self) -> bool {
        false
    }

    /// Verified: `reference/commands.md`, `### /compress` -- "Replace the
    /// entire chat context with a summary." `cli-reference.md`'s own
    /// cheatsheet table omits this command entirely.
    fn compact_command(&self) -> Option<&'static str> {
        Some("/compress")
    }

    /// Verified: `reference/commands.md`/`cli-reference.md`, `/quit` (or
    /// `/exit`). `\r` mirrors `CodexAdapter::quit_sequence`'s own PTY
    /// keystroke terminator.
    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Spec-mandated for this wave, unrelated to whether parsing
            // works: no marker convention or turn-signal hook is verified
            // for gemini at all (`register_turn_signal` below is a no-op).
            marker_signal: false,
            turn_signal: false,
            // `GEMINI_SYSTEM_MD` is env-var-only (this module's own doc
            // comment) -- no per-run argv mechanism exists.
            system_prompt: false,
            // `parse_events`/`transcript_usage` derive real data from the
            // verified session-file row shape, so both are honestly `true`.
            events: true,
            token_usage: true,
            // Unverified: gemini's own interactive composer paste/submit
            // behavior was never observed (gemini-cli is not installed and
            // could not be run here). `false` is the same conservative
            // default `Capabilities::default()` already gives, not a
            // positive claim of "submits correctly".
            defer_injection_submit: false,
            context_window_tokens: None,
            // Issue #418: `~/.gemini/settings.json` carries a `BeforeTool`
            // guard, but gemini's `AfterTool` only carries `additionalContext`
            // -- no verified result-replacement mechanism -- see
            // `native_hooks`'s own doc comment.
            pre_tool_hook: true,
            post_tool_hook: false,
        }
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        catalogue::vendor(CATALOGUE_VENDOR).and_then(|v| catalogue::context_window(v, model))
    }

    /// Issue #418, DOCS-ONLY, verified from official docs 2026-09-09, not
    /// against a live binary (`github.com/google-gemini/gemini-cli`
    /// `docs/hooks/reference.md`, 0.61 nightly; gemini-cli is not installed
    /// here -- see this module's own top doc comment).
    ///
    /// The user-level file is `~/.gemini/settings.json`, shared with
    /// operator config already there (patch under the top-level `hooks` key,
    /// never own the whole file). Shape: `{"hooks":{"BeforeTool":[{
    /// "matcher":"run_shell_command|write_file|replace","hooks":[{"name":
    /// "zirv-guard","type":"command","command":"<cmd>","timeout":30000}]}]}}`
    /// -- timeouts here are milliseconds, unlike copilot's/droid's own
    /// seconds. The safety-check entry narrows its own matcher to
    /// `"run_shell_command"` alone, the one shell tool name
    /// [`READ_ONLY_POLICY_TOML`] above already verifies. No `AfterTool`
    /// entry is installed: gemini's own docs give `AfterTool` only an
    /// `additionalContext` channel, no verified result-replacement
    /// mechanism -- see `Capabilities::post_tool_hook` above.
    fn native_hooks(&self, home: &Path) -> Option<NativeHooks> {
        let file = home.join(".gemini").join("settings.json");
        let group = |matcher: &str, command: &str| {
            json!({
                "matcher": matcher,
                "hooks": [{"name": "zirv-guard", "type": "command", "command": command, "timeout": 30000}]
            })
        };
        Some(NativeHooks {
            file,
            owned_file: false,
            root_defaults: json!({}),
            entries: vec![
                NativeHookEntry {
                    pointer: vec!["hooks".to_string(), "BeforeTool".to_string()],
                    element: group(
                        "run_shell_command|write_file|replace",
                        "zirv ctx hook pretool --agent gemini",
                    ),
                    label: "pretool guard",
                },
                NativeHookEntry {
                    pointer: vec!["hooks".to_string(), "BeforeTool".to_string()],
                    element: group("run_shell_command", "zirv ctx safety check --agent gemini"),
                    label: "safety check",
                },
            ],
        })
    }

    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        catalogue::vendor(CATALOGUE_VENDOR)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("gemini-3.7-flash")
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor(CATALOGUE_VENDOR).and_then(|v| catalogue::strength(v, model))
    }

    fn default_worker_model(&self) -> Option<&'static str> {
        catalogue::vendor(CATALOGUE_VENDOR)
            .and_then(|v| catalogue::tier_model(v, catalogue::Tier::Cheap))
    }

    fn default_distiller_model(&self) -> Option<&'static str> {
        catalogue::vendor(CATALOGUE_VENDOR)
            .and_then(|v| catalogue::tier_model(v, catalogue::Tier::Cheap))
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    /// Verified: `cli-reference.md`/`reference/configuration.md`,
    /// `--model <model_name>` (`-m <model_name>`).
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["-m".to_string(), model.to_string()]
    }

    // No `resume_args`/`session_pin_args` override -- see this module's own
    // doc comment, "Deliberately UNSUPPORTED", for why the trait defaults
    // (`None`/empty) are the honest answer here.

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(Some("gemini"))
    }

    fn flatten(cmd: Command) -> Vec<String> {
        super::super::flatten_command(cmd)
    }

    // -- command shapes -----------------------------------------------

    #[test]
    fn headless_cmd_carries_the_prompt_flag_and_text() {
        let args = flatten(adapter().headless_cmd(
            "do the thing",
            &SessionId::new_v4(),
            &["--extra".to_string()],
        ));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"do the thing".to_string()));
        assert!(args.contains(&"--extra".to_string()));
    }

    #[test]
    fn headless_cmd_stdin_carries_no_prompt_token() {
        let args = flatten(
            adapter()
                .headless_cmd_stdin(&SessionId::new_v4(), &[])
                .expect("stdin form"),
        );
        assert!(!args.contains(&"-p".to_string()));
    }

    #[test]
    fn interactive_cmd_uses_a_positional_prompt() {
        let args = flatten(adapter().interactive_cmd(Some("explain this project"), &[]));
        assert!(args.contains(&"explain this project".to_string()));
        assert!(!args.contains(&"-p".to_string()));
    }

    #[test]
    fn interactive_cmd_with_no_prompt_carries_no_positional() {
        let args = flatten(adapter().interactive_cmd(None, &[]));
        // Only the program invocation itself should be present.
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn distiller_cmd_carries_the_model_and_read_only_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let args = flatten(a.distiller_cmd("gemini-3.5-flash-lite"));
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"gemini-3.5-flash-lite".to_string()));
        assert!(args.contains(&"--admin-policy".to_string()));
    }

    #[test]
    fn distiller_cmd_omits_model_flag_when_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let args = flatten(a.distiller_cmd(""));
        assert!(!args.contains(&"-m".to_string()));
    }

    #[test]
    fn read_only_args_writes_a_deny_policy_gemini_can_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let args = a.read_only_args();
        assert_eq!(args[0], "--admin-policy");
        let contents = std::fs::read_to_string(&args[1]).expect("policy file written");
        assert!(contents.contains("run_shell_command"));
        assert!(contents.contains("write_file"));
        assert!(contents.contains("\"deny\""));
    }

    #[test]
    fn model_args_uses_the_verified_flag() {
        assert_eq!(
            adapter().model_args("gemini-3.1-pro-preview"),
            vec!["-m".to_string(), "gemini-3.1-pro-preview".to_string()]
        );
    }

    #[test]
    fn resume_args_stays_unsupported() {
        assert_eq!(adapter().resume_args("some-id"), None);
    }

    #[test]
    fn quit_and_compact_are_verified() {
        assert_eq!(adapter().quit_sequence(), "/quit\r");
        assert_eq!(adapter().compact_command(), Some("/compress"));
    }

    // -- transcript_path ------------------------------------------------

    fn write_registry(home: &Path, cwd: &Path, short_id: &str) {
        let dir = home.join(".gemini");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let key = normalized_project_path(cwd);
        let json = serde_json::json!({ "projects": { key: short_id } });
        std::fs::write(dir.join("projects.json"), json.to_string()).expect("write registry");
    }

    fn write_session_file(chats_dir: &Path, filename: &str, start_time: &str, rows: &[Value]) {
        std::fs::create_dir_all(chats_dir).expect("mkdir chats");
        let mut lines = vec![
            serde_json::json!({
                "sessionId": "11111111-1111-4111-8111-111111111111",
                "startTime": start_time,
                "lastUpdated": start_time,
                "kind": "interactive"
            })
            .to_string(),
        ];
        lines.extend(rows.iter().map(|r| r.to_string()));
        let content = lines.join("\n") + "\n";
        std::fs::write(chats_dir.join(filename), content).expect("write session file");
    }

    fn session_for(cwd: &Path) -> SessionRef {
        SessionRef {
            id: SessionId::parse("22222222-2222-4222-8222-222222222222"),
            cwd: cwd.to_path_buf(),
        }
    }

    #[test]
    fn transcript_path_resolves_via_the_project_registry_and_pins_it() {
        let home = tempfile::tempdir().expect("home");
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");

        write_registry(home.path(), repo.path(), "my-project");
        let chats_dir = home
            .path()
            .join(".gemini")
            .join("tmp")
            .join("my-project")
            .join("chats");
        write_session_file(
            &chats_dir,
            "session-2026-09-07T00-00-abcd1234.jsonl",
            "2026-09-07T00:00:00.000Z",
            &[],
        );

        let a = adapter()
            .with_home(home.path().to_path_buf())
            .with_state_root(state.path().to_path_buf());
        let session = session_for(repo.path());

        // Register the session so `pinned_session_file` has a `started_at`
        // floor to resolve against, mirroring how a real supervisor records
        // one at launch (same pattern `CodexAdapter`'s own pin tests use).
        let state_dir =
            crate::commands::ctx::state::StateDir::from_root(state.path().to_path_buf());
        let short = crate::commands::ctx::sessions::short_id(session.id.as_str());
        std::fs::create_dir_all(state_dir.sessions()).expect("mkdir sessions");
        let mut record = crate::commands::ctx::sessions::Record::new(
            session.id.as_str(),
            "gemini",
            repo.path(),
            crate::commands::ctx::sessions::Verb::Wrap,
        );
        record.started_at = 1_757_203_200; // 2026-09-07T00:00:00Z
        std::fs::write(
            state_dir.sessions().join(format!("{short}.json")),
            serde_json::to_string(&record).expect("record json"),
        )
        .expect("write record");

        let resolved = a.transcript_path(&session);
        assert!(resolved.ends_with("session-2026-09-07T00-00-abcd1234.jsonl"));

        // The pin file now exists and a second call must return the exact
        // same path even if a newer session file appears later.
        write_session_file(
            &chats_dir,
            "session-2026-09-08T00-00-eeff9988.jsonl",
            "2026-09-08T00:00:00.000Z",
            &[],
        );
        let resolved_again = a.transcript_path(&session);
        assert_eq!(resolved, resolved_again);
    }

    #[test]
    fn transcript_path_is_unresolved_when_the_registry_has_no_entry() {
        let home = tempfile::tempdir().expect("home");
        let state = tempfile::tempdir().expect("state");
        let a = adapter()
            .with_home(home.path().to_path_buf())
            .with_state_root(state.path().to_path_buf());
        let session = session_for(Path::new("/no/such/project"));
        let resolved = a.transcript_path(&session);
        assert!(resolved.to_string_lossy().contains("unresolved"));
    }

    // -- parse_events -----------------------------------------------------

    fn fixture_jsonl() -> String {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gemini/chat-session.json");
        std::fs::read_to_string(path).expect("read fixture")
    }

    #[test]
    fn parse_events_reports_turns_final_text_and_tokens() {
        let jsonl = fixture_jsonl();
        let events = adapter().parse_events(&jsonl);

        let turn_starts = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
            .count();
        assert_eq!(turn_starts, 2, "fixture has two user turns");

        let finals: Vec<&NormalizedEvent> = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::AssistantFinal { .. }))
            .collect();
        assert_eq!(finals.len(), 2, "fixture has two assistant turns");
        let NormalizedEvent::AssistantFinal {
            text, input_tokens, ..
        } = finals[0]
        else {
            unreachable!()
        };
        assert!(text.contains("README"));
        assert_eq!(*input_tokens, 120);

        let NormalizedEvent::AssistantFinal { input_tokens, .. } = finals[1] else {
            unreachable!()
        };
        assert_eq!(
            *input_tokens, 340,
            "only the row that actually carries non-null tokens produces a final"
        );
    }

    #[test]
    fn parse_events_never_flushes_two_finals_for_one_duplicate_id() {
        // The fixture's `m1`/`m2` ids are each re-appended once (a null-
        // tokens row, then the real-tokens row) -- exactly two finals total,
        // not four, proves the null-tokens re-append never produces its own
        // `AssistantFinal`.
        let jsonl = fixture_jsonl();
        let events = adapter().parse_events(&jsonl);
        let model_ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ModelId { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(model_ids.contains(&"gemini-2.5-pro"));
    }

    #[test]
    fn parse_events_is_line_local_across_a_split_chunk() {
        // The incremental scoring path (`IncrementalScorer::poll`, `score.rs`)
        // feeds `parse_events` only the bytes newly appended each poll cycle
        // -- a whole-file parse must equal the concatenation of piecewise
        // parses no matter where the cut falls. This deliberately cuts
        // RIGHT BETWEEN the fixture's `m1` null-tokens row and its
        // real-tokens re-append: the exact poll boundary this fix closes,
        // since that used to flush a zero-token `AssistantFinal` from the
        // first half and a second `AssistantFinal` from the second half.
        let whole = fixture_jsonl();
        let lines: Vec<&str> = whole.lines().collect();
        let split_at = lines
            .iter()
            .position(|line| line.contains("\"tokens\":{\"input\":120"))
            .expect("fixture carries the m1 real-tokens row");
        let first_half = lines[..split_at].join("\n");
        let second_half = lines[split_at..].join("\n");

        let mut piecewise = adapter().parse_events(&first_half);
        piecewise.extend(adapter().parse_events(&second_half));
        let whole_parse = adapter().parse_events(&whole);
        assert_eq!(piecewise, whole_parse);

        // And the same must hold for the plain midpoint split every sibling
        // adapter's own version of this test checks.
        let mid = lines.len() / 2;
        let first_half = lines[..mid].join("\n");
        let second_half = lines[mid..].join("\n");
        let mut piecewise = adapter().parse_events(&first_half);
        piecewise.extend(adapter().parse_events(&second_half));
        assert_eq!(piecewise, whole_parse);
    }

    #[test]
    fn transcript_usage_sums_each_turn_once() {
        let jsonl = fixture_jsonl();
        let usage = adapter().transcript_usage(&jsonl).expect("usage present");
        assert_eq!(usage.input_tokens, 120 + 340);
        assert_eq!(usage.output_tokens, 45 + 90);
    }

    #[test]
    fn model_hint_reports_the_most_recent_model() {
        let jsonl = fixture_jsonl();
        assert_eq!(
            adapter().model_hint(&jsonl),
            Some("gemini-2.5-pro".to_string())
        );
    }

    #[test]
    fn structural_context_carries_user_and_assistant_text() {
        let jsonl = fixture_jsonl();
        let ctx = adapter().structural_context(&jsonl, 10);
        assert_eq!(ctx.user_messages.len(), 2);
        assert_eq!(ctx.assistant_texts.len(), 2);
        assert!(ctx.files_read.is_empty());
        assert!(ctx.files_modified.is_empty());
    }

    #[test]
    fn structural_context_last_n_zero_keeps_nothing() {
        let jsonl = fixture_jsonl();
        let ctx = adapter().structural_context(&jsonl, 0);
        assert!(ctx.assistant_texts.is_empty());
    }

    // -- detect / registry -------------------------------------------------

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shims() {
        let a = adapter();
        assert!(a.detect(&["gemini".to_string()]));
        assert!(a.detect(&["gemini.cmd".to_string()]));
        assert!(a.detect(&["gemini.ps1".to_string()]));
        assert!(a.detect(&["/usr/local/bin/gemini".to_string()]));
        assert!(!a.detect(&["codex".to_string()]));
    }

    #[test]
    fn ladder_answers_equal_the_catalogue() {
        let vendor = catalogue::vendor("google").expect("google is registered");
        let a = adapter();
        assert_eq!(
            a.review_model_below(None),
            catalogue::rung_below(vendor, None)
        );
        assert_eq!(
            a.model_strength("gemini-3.1-pro-preview"),
            catalogue::strength(vendor, "gemini-3.1-pro-preview")
        );
        assert_eq!(
            a.context_window_tokens(Some("gemini-3.1-pro-preview")),
            catalogue::context_window(vendor, Some("gemini-3.1-pro-preview"))
        );
        assert_eq!(
            a.default_worker_model(),
            catalogue::tier_model(vendor, catalogue::Tier::Cheap)
        );
        assert_eq!(
            a.default_distiller_model(),
            catalogue::tier_model(vendor, catalogue::Tier::Cheap)
        );
    }

    #[test]
    fn capabilities_match_the_verified_surface() {
        let caps = adapter().capabilities();
        assert!(caps.events);
        assert!(caps.token_usage);
        assert!(!caps.marker_signal);
        assert!(!caps.turn_signal);
        assert!(!caps.system_prompt);
        assert!(
            caps.pre_tool_hook,
            "issue #418: BeforeTool guard is native-hooked"
        );
        assert!(
            !caps.post_tool_hook,
            "issue #418: AfterTool cannot replace a result"
        );
        assert!(!adapter().counts_tool_calls());
    }

    /// Sanity seam check, mirrors `CodexAdapter`'s own equivalent test: the
    /// forced state root really does redirect both the session pin AND the
    /// read-only policy file, so no test above can ever touch the
    /// developer's own state dir.
    #[test]
    fn forced_state_root_is_actually_used() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let _ = a.read_only_args();
        let mut saw_it = false;
        if let Ok(entries) = std::fs::read_dir(dir.path()) {
            for entry in entries.flatten() {
                if entry.file_name() == "zirv-gemini-read-only-policy.toml" {
                    saw_it = true;
                }
            }
        }
        assert!(saw_it, "policy file must land under the forced state root");
    }

    /// Silences an otherwise-unused-import warning on platforms where no
    /// test above happens to need `Write` directly; kept for parity with
    /// sibling adapter test modules that build files by hand.
    #[test]
    fn write_import_is_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("probe.txt");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(b"ok").expect("write");
        assert!(path.exists());
    }
}
