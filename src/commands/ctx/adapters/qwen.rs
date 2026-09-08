//! Issue #389 (wave 2): the Qwen Code adapter (`@qwen-code/qwen-code`).
//!
//! Qwen Code is a fork of gemini-cli (see `gemini.rs`'s own doc comment for
//! that lineage) that has since diverged substantially -- its own transcript
//! shape, tool-permission model and session-id story are all different from
//! upstream gemini-cli's, and every fact below was re-verified against
//! Qwen Code itself rather than assumed from the fork parent. Not installed
//! on this machine and node here is v19 (too old to run it), so nothing below
//! was exercised against a live process. Two sources were used:
//!
//! - GitHub source at `QwenLM/qwen-code@main` (fetched 2026-09-07), for the
//!   yargs flag definitions and the CLI-level session-id/resume wiring
//!   (`packages/cli/src/config/{config,top-level-options}.ts`).
//! - The published npm tarball `@qwen-code/qwen-code@0.23.0` (`npm pack
//!   @qwen-code/qwen-code@latest`, 2026-09-07), whose bundled (but
//!   `--keep-names`, not minified past readability) JS was grepped directly
//!   for the runtime facts no doc file states: the transcript record shape
//!   (`packages/core/src/services/chatRecordingService.ts`, bundled into
//!   `chunks/chunk-4F7GQGXB.js`), the project-directory slug algorithm
//!   (`packages/core/src/utils/paths.ts` / `packages/core/src/config/
//!   storage.ts`, bundled into `chunks/chunk-EAHOJF6V.js`), the canonical
//!   tool-name constants (`packages/core/src/tools/tool-names.ts`, bundled
//!   into `chunks/chunk-7JMGTOH7.js`), and the `--exclude-tools` enforcement
//!   path (`chunks/chunk-4F7GQGXB.js`, `isToolEnabled`/`getPermissionsDeny`).
//!   Anything not cited to one of these two sources below is UNSUPPORTED and
//!   left at the trait default rather than guessed.
//!
//! # Verified facts and their source
//!
//! - **Binary**: `qwen` (npm `@qwen-code/qwen-code`, version 0.23.0 at
//!   verification time). `-p`/`--prompt <text>` runs one non-interactive turn
//!   (`top-level-options.ts`: `prompt: { alias: 'p', type: 'string', ... }`);
//!   unlike gemini-cli's own `-p`, this is a plain string-valued flag, not a
//!   mode-forcing switch. `-i`/`--prompt-interactive <text>` ("Execute the
//!   provided prompt and continue in interactive mode",
//!   `top-level-options.ts`) is the verified interactive-launch-with-an-
//!   initial-prompt flag. `-m`/`--model <name>` selects a model
//!   (`top-level-options.ts`).
//! - **Piped-stdin prompt delivery**: verified directly in the bundled JS
//!   (`llm-K26SUDVK.js`, both the sandboxed-relaunch path around
//!   `injectStdinIntoArgs` and the direct non-sandbox path): whenever
//!   `process.stdin.isTTY` is false and `--input-format` is not
//!   `stream-json`, the CLI reads stdin to completion and prepends it to
//!   whatever `input` already resolved from `-p`/the positional `query` --
//!   or, if neither was given, stdin alone becomes `input`. This is the same
//!   "headless when stdin is piped" mechanism `gemini.rs`'s own
//!   `headless_cmd_stdin` doc comment cites (there from bundled docs only;
//!   here from the literal source). [`QwenAdapter::headless_cmd_stdin`] omits
//!   `-p` entirely and relies on this for the Windows `.cmd`-shim case.
//! - **Approval/tool flags**: `--approval-mode {plan,default,auto-edit,auto,
//!   yolo}` (`packages/core/src/config/approval-mode.ts`'s `ApprovalMode`
//!   enum) and `-y`/`--yolo` are mutually exclusive (`config.ts`'s own
//!   validation). Neither is used by this adapter -- see "Read-only
//!   enforcement" below for why `--exclude-tools` is the mechanism instead.
//! - **Read-only enforcement**: `--exclude-tools <names>` (`top-level-
//!   options.ts`: `type: 'array'`, coerced by splitting each token on `,` --
//!   `config.ts`'s own `.option('exclude-tools', { ...string: true, coerce:
//!   (tools) => tools.flatMap((t) => t.split(',').map((x) => x.trim())) })`,
//!   so one comma-joined argument works) feeds `Config.excludeTools`, which
//!   `Config.getPermissionsDeny()` merges into the session's deny rules
//!   (`chunk-4F7GQGXB.js`: "Merges: settings.permissions.deny (persistent) +
//!   excludeTools param (SDK / argv blocklist)"). Every tool call is gated by
//!   `pm.isToolEnabled(canonicalName)` before it runs (same file, the
//!   `executeToolCall`-shaped block building `permissionErrorMessage`), and
//!   this check happens unconditionally -- before approval-mode is even
//!   consulted -- so a denied tool never reaches a confirmation prompt to
//!   ask about, `--yolo` or not. This is a genuine per-run structural deny,
//!   the same shape claude's `--disallowedTools=Write,Edit,Bash,
//!   NotebookEdit` and pi's `--tools <allow-list>` are.
//!   [`READ_ONLY_EXCLUDED_TOOLS`] below names the canonical tool-name
//!   constants (`packages/core/src/tools/tool-names.ts`, bundled into
//!   `chunk-7JMGTOH7.js`'s `ToolNames` object) for the shell tool and the
//!   three mutating file tools -- `run_shell_command`, `write_file`, `edit`,
//!   `notebook_edit` -- deliberately the same four-tool boundary claude's own
//!   deny list draws (`edit`/`write_file`/`notebook_edit` are qwen's
//!   `Edit`/`Write`/`NotebookEdit`; `run_shell_command` is `Bash`). No other
//!   registered tool name in `ToolNames` reads or executes shell/network
//!   code by that same table.
//! - **Session id / resume**: verified directly in `packages/cli/src/config/
//!   config.ts` (GitHub source): `--session-id <id>` on a run with neither
//!   `--continue` nor `--resume` passes that id straight through as the
//!   session's own id (`sessionId = normalizeSessionIdForLookup(argv
//!   ['sessionId'])`), which the core `Config` constructor then adopts
//!   verbatim (`this.sessionId = params.sessionId ?? randomUUID()`,
//!   `chunk-4F7GQGXB.js`) -- unlike gemini-cli and codex, which always mint
//!   their own id. `--resume <id>` reuses that id (`sessionId =
//!   argv.resume`) and loads its transcript; `--continue`/`-c` resumes the
//!   most recently modified session's id instead of naming one. This is what
//!   makes [`QwenAdapter::session_pin_args`]/[`QwenAdapter::resume_args`]
//!   real (verified) rather than the "no mechanism" gap gemini.rs documents
//!   for the identical-looking flag on that harness.
//! - **Transcript storage**: fully deterministic given `cwd` and the session
//!   id, with no registry file or directory scan required at all --
//!   substantially simpler than gemini-cli's own project registry or
//!   codex's dated rollout tree. `chatRecordingService.ts`'s
//!   `ensureConversationFile` (`chunk-4F7GQGXB.js`): `conversationFile =
//!   path.join(ensureChatsDir(), `${getSessionId()}.jsonl`)`, and
//!   `ensureChatsDir` is `path.join(config.storage.getProjectDir(),
//!   "chats")`. `storage.ts`'s `Storage.getProjectDir()`
//!   (`chunk-EAHOJF6V.js`): `path.join(runtimeBaseDir, "projects",
//!   sanitizeCwd(getProjectRoot()))`, where `runtimeBaseDir` is
//!   `getGlobalQwenDir()` (`~/.qwen`, or `$QWEN_HOME` when set -- the env
//!   override this adapter does not honor, the same scope cut `pi.rs`
//!   documents for its own directory-override env var) absent an operator's
//!   `QWEN_RUNTIME_DIR`/programmatic override, neither of which a zirv-
//!   launched session sets. [`sanitize_cwd`] below is `sanitizeCwd` verbatim
//!   (`chunk-EAHOJF6V.js`): lowercase the whole path on Windows only (no
//!   `path.resolve`/lexical normalization -- `session.cwd` is always already
//!   absolute, same residual `gemini.rs`'s own `normalized_project_path`
//!   documents), then replace every character outside `[a-zA-Z0-9]` with
//!   `-`. The file itself is append-only JSONL (`packages/core/src/utils/
//!   jsonl-utils.ts`'s `writeLine`: one `JSON.stringify(data) + "\n"` per
//!   call, `fs.promises.appendFile`), so this adapter reads it directly like
//!   `codex.rs`/`gemini.rs` rather than through `transcript_source::
//!   ShadowTranscript` (that helper parses its whole source as one
//!   `serde_json::Value`, which cannot parse multi-line JSONL).
//! - **Record shape** (`chatRecordingService.ts`'s `createBaseRecord`,
//!   `recordUserMessage`, `recordAssistantTurn`, `chunk-4F7GQGXB.js`): every
//!   record carries `{uuid, parentUuid, sessionId, timestamp (ISO 8601 via
//!   `Date.toISOString()`), type: "user" | "assistant" | "tool_result" |
//!   "system", provenance, cwd, version, gitBranch}`. A `"user"` record adds
//!   `message: createUserContent(text)` -- the `@google/genai` `Content`
//!   helper, so `message` is `{role: "user", parts: [{text}, ...]}` -- and,
//!   for every synthetic (non-turn) user-shaped record (`recordMidTurnUserMessage`,
//!   `recordNotificationLike`/cron/notification, `recordGoalRuntimeMessage`),
//!   a `subtype` field. `restoreSessionState`'s own turn-tracking check
//!   (`if (record.type === "user" && record.subtype === void 0)`) is the
//!   verified rule this adapter's [`parse_events`] copies for "is this row a
//!   real user turn": exactly the records qwen itself would count as one.
//!   An `"assistant"` record (`recordAssistantTurn`) adds `model`, an
//!   optional `message: createModelContent(...)` and an optional
//!   `usageMetadata` -- a `GenerateContentResponseUsageMetadata` with the
//!   same field names gemini-cli's own API usage carries
//!   (`promptTokenCount`/`candidatesTokenCount`/`cachedContentTokenCount`,
//!   confirmed present in this codebase too, `chunk-4F7GQGXB.js`) -- built
//!   and appended ONCE per turn, unlike gemini-cli's own `ChatRecordingService`,
//!   which re-appends the same row id as tool calls/tokens arrive
//!   incrementally. There is therefore no multi-row-per-turn folding residual
//!   here at all: [`parse_events`]/[`transcript_usage`] are fully line-local
//!   with no cross-line accumulator needed, simpler than `gemini.rs`'s own
//!   `fold_gemini_rows`.
//! - **`--append-system-prompt <text>`** (append) and **`--system-prompt
//!   <text>`** (replace) both exist and are documented as combinable
//!   (`top-level-options.ts`: each one's description names the other).
//!   [`system_prompt_args`] uses the append form, the same choice `pi.rs`
//!   makes for the identical reason: replacing the harness's own base prompt
//!   is a strictly bigger behavior change than appending to it.
//! - **`/compress`** (alt `/summarize`) is the verified compaction slash
//!   command (`packages/cli/src/ui/commands/compressCommand.ts`, bundled
//!   into `chunk-NVVJTUSL.js`: `name: "compress", altNames: ["summarize"]`,
//!   description "Compresses the context by replacing it with a summary"),
//!   identical to gemini-cli's own `/compress`. **`/quit`** (alt `/exit`) is
//!   the verified quit command (`quitCommand.ts`, same bundle: `name:
//!   "quit", altNames: ["exit"]`).
//!
//! # Deliberately UNSUPPORTED in this wave
//!
//! - Tool calls/results (`NormalizedEvent::ToolCall`/`ToolResult`): a
//!   `"tool_result"`-typed record carries a `toolCallResult` field
//!   (`recordToolResult`, `chunk-4F7GQGXB.js`) that is run through
//!   `sanitizeToolCallResultForRecording` and a `resultDisplay`-shaped
//!   special case before being written -- no stable, fully-specified shape
//!   was found in the time available, the same gap `gemini.rs`/`codex.rs`
//!   each document for their own harness. [`counts_tool_calls`] is `false`
//!   for the same reason those two give: a silently-never-advancing
//!   `--max-tool-calls` ceiling is worse than refusing the flag outright.
//! - `supports_headless_compact`: `/compress` is verified only as an
//!   interactive slash command (`compressCommand.ts`'s own
//!   `supportedModes: ["interactive", "non_interactive", "acp"]` line does
//!   list `non_interactive`, but no verified way was found to INJECT a slash
//!   command into a headless `-p` run rather than type it into a live REPL,
//!   so "compact, then keep talking headlessly" is not claimed).
//!   [`headless_resume_cmd`] is still implemented: resuming and prompting are
//!   independently verified (`--resume <id>` plus `-p <text>`, both real CLI
//!   flags processed by the same yargs parser), it is only the
//!   compact-first half of `supports_headless_compact`'s contract that has no
//!   verified mechanism.
//! - `interactive_read_only_args`: `--exclude-tools` is a single
//!   constructor-level `Config` field with no documented interactive/
//!   headless split (unlike codex's `exec`-only `--ignore-rules`), so the
//!   trait default (delegating to `read_only_args`) is correct unchanged.
//! - `context_window_hint`: no per-transcript stated context-window figure
//!   was found in the record shape above (`contextWindowSize` exists on an
//!   assistant record per `recordAssistantTurn`'s own doc comment, but no
//!   verified read call site was found in the time available to trust it),
//!   so this stays at the trait default (`None`) -- `context_window_tokens`
//!   (model-name-keyed, via the catalogue) still answers.
//! - `$QWEN_HOME`/`$QWEN_RUNTIME_DIR`: real environment overrides of the
//!   directories this adapter computes (`storage.ts`, this module's own doc
//!   comment), not honored here -- the same scope cut `pi.rs` documents for
//!   its own `<APP_NAME>_CODING_AGENT_DIR`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
};
use super::super::window;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// This adapter's own vendor slug in `catalogue`'s registry (issue #381) --
/// see this module's own doc comment for the two rungs (`qwen3.8-max`,
/// `qwen3-coder-plus`, ...) that table carries for `"qwen"`.
const CATALOGUE_VENDOR: &str = "qwen";

/// The canonical tool names (`packages/core/src/tools/tool-names.ts`, see
/// this module's own doc comment "Read-only enforcement") this adapter denies
/// via `--exclude-tools` for a read-only launch: the shell tool and the three
/// mutating file tools. Comma-joined because `top-level-options.ts`'s own
/// `--exclude-tools` coerce function splits each argv token on `,`.
const READ_ONLY_EXCLUDED_TOOLS: &str = "run_shell_command,write_file,edit,notebook_edit";

/// See this module's own doc comment ("Read-only enforcement", "Session id /
/// resume") for the full verification trail behind every method below.
#[derive(Debug, Clone)]
pub struct QwenAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    /// Test seam only, mirroring `GeminiAdapter::forced_state_root` exactly:
    /// pins the zirv state root [`QwenAdapter::transcript_path`] resolves its
    /// pin file from, instead of the real platform state directory.
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
}

impl QwenAdapter {
    /// `bin` may carry arguments, mirroring every other adapter's own `new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("qwen").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "qwen".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_state_root: None,
        }
    }

    /// Test seam: pins the home directory [`transcript_path`](AgentAdapter::
    /// transcript_path) resolves `~/.qwen` from.
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

    /// Every command starts here, mirroring every other adapter's own `base`:
    /// the program is routed through [`super::resolve_program`] so an
    /// npm-installed `qwen.cmd` shim launches on Windows.
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

    /// `~/.qwen/projects/<sanitize_cwd(cwd)>/chats` -- fully deterministic,
    /// no registry file or directory scan needed to find it (see this
    /// module's own doc comment, "Transcript storage").
    fn chats_dir(&self, cwd: &Path) -> PathBuf {
        self.home_dir()
            .join(".qwen")
            .join("projects")
            .join(sanitize_cwd(cwd))
            .join("chats")
    }

    /// Resolutions 2 and 3 of [`AgentAdapter::transcript_path`] -- mirrors
    /// `GeminiAdapter::pinned_session_file`'s own shape exactly, for a launch
    /// that never pinned `--session-id` (a plain `wrap` relaunch, see
    /// `session_pin_args`'s own doc comment for why that never happens
    /// inside `interactive_cmd`). Not needed at all for a headless or
    /// dashboard-pane launch, both of which pin the id and hit resolution 1
    /// (`transcript_path`'s own direct join) instead.
    fn pinned_chat_file(&self, chats_dir: &Path, session: &SessionRef) -> Option<PathBuf> {
        let state = self.state_dir()?;
        let short = super::super::sessions::short_id(session.id.as_str());
        let pin = state.rollouts().join(format!("{short}.qwen.path"));
        if let Ok(recorded) = std::fs::read_to_string(&pin) {
            let recorded = PathBuf::from(recorded.trim());
            if recorded.is_file() {
                return Some(recorded);
            }
        }
        let record = super::super::sessions::load_record(&state, &short)?;
        let started_ms = record.started_at.saturating_mul(1_000);
        let resolved = resolve_chat_file(chats_dir, started_ms)?;
        if super::super::state::create_private_dir_all(&state.rollouts()).is_ok() {
            let _ = super::super::state::write_private(&pin, &resolved.display().to_string());
        }
        Some(resolved)
    }
}

/// `sanitizeCwd` verbatim (`packages/core/src/utils/paths.ts`, bundled into
/// `chunk-EAHOJF6V.js` -- see this module's own doc comment): lowercase the
/// whole path on Windows only (no `path.resolve`; `session.cwd` is always
/// already absolute here), then replace every character outside
/// `[a-zA-Z0-9]` with `-`.
fn sanitize_cwd(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy().into_owned();
    let normalized = if cfg!(windows) {
        raw.to_lowercase()
    } else {
        raw
    };
    normalized
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Every direct-child `.jsonl` file of `chats_dir` -- every file that lands
/// there is a conversation file named `${sessionId}.jsonl` (this module's own
/// doc comment, "Transcript storage"), so no prefix filter is needed the way
/// `gemini.rs`'s own `collect_session_files` needs one to skip subagent chat
/// files living in a nested directory.
fn collect_chat_files(chats_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(chats_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        })
        .collect()
}

/// The first record's own `timestamp` (unix ms), or `None` for a file whose
/// first line is not a parseable chat record. Mirrors `codex::
/// rollout_session_meta`/`gemini::session_start_ms`'s own "only the first
/// line" reading exactly: every record type carries `timestamp` (this
/// module's own doc comment, `createBaseRecord`), so the file's own first
/// line always states when the session began, regardless of that record's
/// `type`.
fn record_start_ms(path: &Path) -> Option<u64> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    let row: Value = serde_json::from_str(line.trim()).ok()?;
    row.get("sessionId")?;
    row.get("timestamp")
        .and_then(Value::as_str)
        .and_then(window::parse_iso8601_utc_ms)
}

/// The chat file inside `chats_dir` this session most plausibly created: the
/// EARLIEST one whose own first record starts at or after `started_ms` --
/// mirrors `codex::resolve_rollout`/`gemini::resolve_session_file`'s own
/// "earliest, not newest" rationale exactly. No `cwd` cross-check is needed,
/// unlike codex: `chats_dir` is already scoped to this session's own project
/// via [`QwenAdapter::chats_dir`]'s deterministic slug, so the one residual
/// codex still carries (two runs in the same directory within the same
/// second) is this function's only inherited ambiguity.
fn resolve_chat_file(chats_dir: &Path, started_ms: u64) -> Option<PathBuf> {
    let mut candidates: Vec<(u64, PathBuf)> = collect_chat_files(chats_dir)
        .into_iter()
        .filter_map(|path| {
            let started = record_start_ms(&path)?;
            (started >= started_ms).then_some((started, path))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates.into_iter().next().map(|(_, path)| path)
}

/// A record's own `message` field: `{role, parts: [{text}, ...]}` --
/// `@google/genai`'s `createUserContent`/`createModelContent` shape (this
/// module's own doc comment). Only `.text` parts are joined (no separator);
/// non-text parts (`functionCall`/`functionResponse`) are dropped, since
/// `parse_events` does not model tool calls this wave either (see
/// "Deliberately UNSUPPORTED" above).
fn extract_text(message: Option<&Value>) -> String {
    message
        .and_then(|m| m.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// One `"assistant"` record's own `usageMetadata` reading, `None` when the
/// row carries no such object (only set when `recordAssistantTurn` is handed
/// `data.tokens`, this module's own doc comment).
#[derive(Debug, Clone, Copy, Default)]
struct QwenTokens {
    input: u64,
    output: u64,
    cached: u64,
}

fn extract_tokens(row: &Value) -> Option<QwenTokens> {
    let usage = row.get("usageMetadata")?;
    if usage.is_null() {
        return None;
    }
    Some(QwenTokens {
        input: usage
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: usage
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached: usage
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

/// Whether `row` is a genuine user TURN rather than a synthetic user-role
/// system record (a mid-turn drain, a cron/notification, a Goal-runtime
/// message -- this module's own doc comment). Copies qwen's own rule
/// verbatim: `restoreSessionState`'s `record.type === "user" && record.
/// subtype === void 0` (`chunk-4F7GQGXB.js`) is the exact check qwen itself
/// uses to decide which rows are real turns for its own title-tracking.
fn is_user_turn(row: &Value) -> bool {
    row.get("type").and_then(Value::as_str) == Some("user") && row.get("subtype").is_none()
}

impl AgentAdapter for QwenAdapter {
    fn name(&self) -> &'static str {
        "qwen"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Static default: qwen-code spends its own Qwen OAuth/DashScope account
    /// absent an operator override. [`provider_for_model`] resolves a
    /// per-launch account instead whenever a model string is in hand, since
    /// `--openai-api-key`/`--openai-base-url` (verified,
    /// `top-level-options.ts`) let this CLI spend an OpenAI-compatible
    /// endpoint's account instead -- the same multi-provider shape `pi.rs`
    /// documents its own identical override for.
    fn provider(&self) -> &'static str {
        "qwen"
    }

    fn provider_for_model(&self, model: Option<&str>) -> &'static str {
        model
            .and_then(catalogue::vendor_of)
            .unwrap_or(self.provider())
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
                f == "qwen" || f == "qwen.cmd" || f == "qwen.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt> --output-format stream-json --session-id <session>`:
    /// `-p` delivers the prompt (verified, `top-level-options.ts`),
    /// `stream-json` is the machine-readable streaming output form a
    /// supervisor parses, and pinning `--session-id` (verified, this
    /// module's own doc comment "Session id / resume") is what makes
    /// [`transcript_path`]'s direct-join resolution hit for every headless
    /// launch, with no scan ever needed.
    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    /// For the Windows `.cmd`-shim case (see [`launches_through_cmd_shim`]):
    /// omits `-p` so the verified piped-stdin fallback (this module's own doc
    /// comment) supplies the prompt instead of an argv token cmd.exe could
    /// reparse. Still pins `--session-id` for the same reason
    /// [`headless_cmd`] does.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        Some(cmd)
    }

    /// `-p <prompt> --output-format stream-json --resume <id>`: `--resume`
    /// genuinely reuses that conversation's own id (verified, "Session id /
    /// resume"), so this is real headless-resume, not a guess -- unlike
    /// `supports_headless_compact` (still `false`; see this module's own doc
    /// comment for why compaction itself is not claimed headless).
    fn headless_resume_cmd(
        &self,
        prompt: Option<&str>,
        session_id: &str,
        extra: &[String],
    ) -> Option<Command> {
        let mut cmd = self.base();
        if let Some(prompt) = prompt {
            cmd.arg("-p").arg(prompt);
        }
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--resume")
            .arg(session_id)
            .args(extra);
        Some(cmd)
    }

    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// `-i <prompt>` continues interactively after that first turn (verified:
    /// "Execute the provided prompt and continue in interactive mode",
    /// `top-level-options.ts`); a bare `qwen` with no prompt launches plain
    /// interactive mode. Never pins `--session-id` here -- see
    /// [`session_pin_args`]'s own doc comment for why that stays a
    /// dashboard-pane-only concern.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg("-i").arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// The distiller needs no session of its own and no prompt on argv (it is
    /// only ever handed a model, mirroring `gemini::distiller_cmd`'s own
    /// shape): the prompt reaches it on stdin via the same piped-stdin
    /// fallback [`headless_cmd_stdin`] uses, since omitting `-p` here has the
    /// identical effect.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        cmd.args(self.read_only_args());
        cmd
    }

    /// `--exclude-tools run_shell_command,write_file,edit,notebook_edit` --
    /// see this module's own doc comment ("Read-only enforcement") for the
    /// full verification trail on why this is a genuine structural deny, not
    /// merely an approval-mode hint.
    fn read_only_args(&self) -> Vec<String> {
        vec![
            "--exclude-tools".to_string(),
            READ_ONLY_EXCLUDED_TOOLS.to_string(),
        ]
    }

    /// `--append-system-prompt <prompt>` -- verified combinable with
    /// `--system-prompt` (this module's own doc comment); appending is the
    /// smaller, safer change, the same choice `pi.rs` makes.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        vec!["--append-system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt")
    }

    /// Resolution 1: the chat file named after zirv's own session id, which a
    /// headless launch ([`headless_cmd`]/[`headless_cmd_stdin`]) or a pinned
    /// dashboard pane ([`session_pin_args`]) always produces (verified,
    /// "Session id / resume") -- cheap to check directly since the filename
    /// is fully deterministic here (`${sessionId}.jsonl`, no timestamp
    /// prefix, no dated subtree), unlike `codex::CodexAdapter::transcript_path`'s
    /// identically-motivated but substring-scanned resolution 1. Resolutions
    /// 2 and 3 ([`QwenAdapter::pinned_chat_file`]) cover the one case that
    /// never pins the id at all: a plain `wrap` relaunch, whose child mints
    /// its own conversation id qwen never told zirv about. `session.id` can
    /// also come straight from an operator-supplied `--session-id`/resume
    /// value, so [`SessionId::is_safe_path_segment`] gates resolution 1: a
    /// hostile id (`../..`, an embedded separator) never gets joined
    /// verbatim into `chats_dir` at all, and this falls straight through to
    /// resolutions 2/3 as if no direct file existed. If those also come up
    /// empty, the final fallback is the safe id's own (still never-existing)
    /// direct path, or, for a hostile id, a
    /// [`sessions::short_id`](super::super::sessions::short_id)-keyed
    /// "unresolved" name -- never the raw id.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let chats_dir = self.chats_dir(&session.cwd);
        let safe_direct = session
            .id
            .is_safe_path_segment()
            .then(|| chats_dir.join(format!("{}.jsonl", session.id.as_str())));
        if let Some(direct) = &safe_direct
            && direct.is_file()
        {
            return direct.clone();
        }
        if let Some(pinned) = self.pinned_chat_file(&chats_dir, session) {
            return pinned;
        }
        safe_direct.unwrap_or_else(|| {
            chats_dir.join(format!(
                "unresolved-{}.jsonl",
                super::super::sessions::short_id(session.id.as_str())
            ))
        })
    }

    /// Fully line-local, with no cross-line folding needed at all (this
    /// module's own doc comment -- unlike `gemini::parse_events`'s own
    /// `fold_gemini_rows`, a qwen assistant turn is written exactly once).
    /// `TurnStart`/`UserText` come from a real user-turn row ([`is_user_turn`]);
    /// `AssistantFirstText`/`AssistantFinal`/`ModelId` come from every
    /// `"assistant"` row. Tool calls/results are deliberately not modeled
    /// (this module's own doc comment, "Deliberately UNSUPPORTED").
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        let mut events = Vec::new();
        for line in jsonl.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let at_ms = row
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(window::parse_iso8601_utc_ms);
            if is_user_turn(&row) {
                events.push(NormalizedEvent::TurnStart { at_ms });
                let text = extract_text(row.get("message"));
                if !text.trim().is_empty() {
                    events.push(NormalizedEvent::UserText {
                        byte_len: text.len() as u64,
                    });
                }
                continue;
            }
            if row.get("type").and_then(Value::as_str) == Some("assistant") {
                let text = extract_text(row.get("message"));
                if !text.trim().is_empty() {
                    events.push(NormalizedEvent::AssistantFirstText { at_ms });
                }
                let input_tokens = extract_tokens(&row).map(|t| t.input).unwrap_or(0);
                events.push(NormalizedEvent::AssistantFinal {
                    text,
                    input_tokens,
                    at_ms,
                });
                if let Some(model) = row.get("model").and_then(Value::as_str) {
                    events.push(NormalizedEvent::ModelId {
                        id: model.to_string(),
                    });
                }
            }
        }
        events
    }

    /// Only `user_messages`/`assistant_texts` are populated -- `files_read`/
    /// `files_modified`/`tool_errors` stay empty, the same honest gap
    /// `gemini::structural_context`/`codex::structural_context` each carry
    /// (no verified per-tool-call result shape, this module's own doc
    /// comment). `last_n` truncation on `assistant_texts` only, mirroring
    /// every other adapter's own `keep_last` convention.
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut user_messages = Vec::new();
        let mut assistant_texts = Vec::new();
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if is_user_turn(&row) {
                let text = extract_text(row.get("message"));
                if !text.trim().is_empty() {
                    user_messages.push(text);
                }
                continue;
            }
            if row.get("type").and_then(Value::as_str) == Some("assistant") {
                let text = extract_text(row.get("message"));
                if !text.trim().is_empty() {
                    assistant_texts.push(text);
                }
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

    /// The most recent `"assistant"` row's own `model` field, scanned from
    /// the end -- mirrors `gemini::model_hint`/`pi::model_hint`'s own
    /// `.rev()` approach.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        jsonl.lines().rev().find_map(|line| {
            let row = serde_json::from_str::<Value>(line.trim()).ok()?;
            if row.get("type").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            row.get("model").and_then(Value::as_str).map(str::to_string)
        })
    }

    /// Sums every `"assistant"` row's own `usageMetadata` exactly once each
    /// -- correct without folding, since a qwen assistant turn is written
    /// once (this module's own doc comment, unlike gemini's own re-appended
    /// rows). `cache_creation_input_tokens` is always `0`: qwen's usage
    /// breakdown has no separate cache-WRITE class in the fields verified
    /// above, only `cachedContentTokenCount` (tokens served FROM cache),
    /// which maps to `cache_read_input_tokens` -- an honest zero, the same
    /// choice `gemini::transcript_usage` makes for its own missing field.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let mut usage = TranscriptUsage::default();
        let mut observed = false;
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if row.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if let Some(tokens) = extract_tokens(&row) {
                usage.input_tokens = usage.input_tokens.saturating_add(tokens.input);
                usage.output_tokens = usage.output_tokens.saturating_add(tokens.output);
                usage.cache_read_input_tokens =
                    usage.cache_read_input_tokens.saturating_add(tokens.cached);
                observed = true;
            }
        }
        observed.then_some(usage)
    }

    /// Each `"assistant"` row's own `usageMetadata` is that ONE call's usage,
    /// never a restated running total, so this reports the sum over exactly
    /// the fragment it is handed -- the trait default meaning of `false`.
    fn transcript_usage_is_cumulative(&self) -> bool {
        false
    }

    /// See this module's own doc comment, "Deliberately UNSUPPORTED": no
    /// verified per-tool-call result shape, so `parse_events` never emits
    /// `NormalizedEvent::ToolCall` at all.
    fn counts_tool_calls(&self) -> bool {
        false
    }

    /// Verified: `compressCommand.ts`'s own `name: "compress"` (this
    /// module's own doc comment).
    fn compact_command(&self) -> Option<&'static str> {
        Some("/compress")
    }

    /// Verified: `quitCommand.ts`'s own `name: "quit"` (this module's own doc
    /// comment). `\r` mirrors every other adapter's own PTY keystroke
    /// terminator.
    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            turn_signal: false,
            system_prompt: true,
            events: true,
            token_usage: true,
            // Unverified: qwen-code's own interactive composer paste/submit
            // behavior was never observed (not installed, node here is too
            // old to run it). `false` is the conservative
            // `Capabilities::default()` reading, not a positive claim.
            defer_injection_submit: false,
            context_window_tokens: None,
        }
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        catalogue::vendor(CATALOGUE_VENDOR).and_then(|v| catalogue::context_window(v, model))
    }

    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        catalogue::vendor(CATALOGUE_VENDOR)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("qwen3-coder-plus")
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

    /// Verified: `top-level-options.ts`, `model: { alias: 'm', ... }`.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    /// `--resume <id>` genuinely reuses that conversation's own id (verified,
    /// this module's own doc comment, "Session id / resume") -- unlike
    /// gemini-cli's identically-spelled but unverified-for-this-purpose flag.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["--resume".to_string(), session_id.to_string()])
    }

    /// `--session-id <id>` on a fresh launch adopts `session` verbatim as the
    /// new conversation's own id (verified, this module's own doc comment) --
    /// which is also what makes [`transcript_path`]'s direct-join resolution
    /// find the right file with no scan. Never appended inside
    /// `interactive_cmd` itself, the same rule every other adapter's own
    /// `session_pin_args` doc comment states: a `wrap` relaunch lets the
    /// harness mint a fresh conversation on every restart, and a restored
    /// pane already carries [`resume_args`], which would conflict with a pin.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        vec!["--session-id".to_string(), session.to_string()]
    }

    /// No verified per-run turn-boundary signal for qwen (unrelated to
    /// `parse_events`'s own after-the-fact turn detection) -- mirrors every
    /// other adapter's own no-op.
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

    fn adapter() -> QwenAdapter {
        QwenAdapter::new(Some("qwen"))
    }

    fn flatten(cmd: Command) -> Vec<String> {
        super::super::flatten_command(cmd)
    }

    // -- command shapes -----------------------------------------------

    #[test]
    fn headless_cmd_carries_the_prompt_output_format_and_session_pin() {
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let args =
            flatten(adapter().headless_cmd("do the thing", &session, &["--extra".to_string()]));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"do the thing".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
        assert!(args.contains(&"11111111-2222-4333-8444-555555555555".to_string()));
        assert!(args.contains(&"--extra".to_string()));
    }

    #[test]
    fn headless_cmd_stdin_carries_no_prompt_token_but_still_pins_the_session() {
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let args = flatten(
            adapter()
                .headless_cmd_stdin(&session, &[])
                .expect("qwen has a verified stdin form"),
        );
        assert!(!args.contains(&"-p".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn headless_resume_cmd_combines_prompt_and_resume() {
        let args = flatten(
            adapter()
                .headless_resume_cmd(Some("keep going"), "abc-123", &[])
                .expect("resume is verified"),
        );
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"keep going".to_string()));
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"abc-123".to_string()));
    }

    #[test]
    fn headless_resume_cmd_with_no_prompt_omits_the_flag() {
        let args = flatten(
            adapter()
                .headless_resume_cmd(None, "abc-123", &[])
                .expect("resume is verified"),
        );
        assert!(!args.contains(&"-p".to_string()));
        assert!(args.contains(&"--resume".to_string()));
    }

    #[test]
    fn interactive_cmd_uses_the_dash_i_flag_for_an_initial_prompt() {
        let args = flatten(adapter().interactive_cmd(Some("explain this project"), &[]));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"explain this project".to_string()));
        assert!(!args.contains(&"-p".to_string()));

        let bare = flatten(adapter().interactive_cmd(None, &[]));
        assert_eq!(
            bare.len(),
            1,
            "only the program invocation itself: {bare:?}"
        );
    }

    #[test]
    fn distiller_cmd_carries_the_model_and_read_only_args_but_no_prompt() {
        let args = flatten(adapter().distiller_cmd("qwen3-coder-plus"));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"qwen3-coder-plus".to_string()));
        assert!(args.contains(&"--exclude-tools".to_string()));
        assert!(!args.contains(&"-p".to_string()));

        let no_model = flatten(adapter().distiller_cmd(""));
        assert!(!no_model.contains(&"--model".to_string()));
    }

    #[test]
    fn read_only_args_excludes_the_shell_and_mutating_file_tools() {
        let args = adapter().read_only_args();
        assert_eq!(args[0], "--exclude-tools");
        let excluded = &args[1];
        for tool in ["run_shell_command", "write_file", "edit", "notebook_edit"] {
            assert!(excluded.contains(tool), "{excluded} must deny {tool}");
        }
    }

    #[test]
    fn system_prompt_args_uses_the_append_flag() {
        assert_eq!(
            adapter().system_prompt_args("be careful"),
            vec![
                "--append-system-prompt".to_string(),
                "be careful".to_string()
            ]
        );
        assert_eq!(
            adapter().user_system_prompt_flag(),
            Some("--append-system-prompt")
        );
    }

    #[test]
    fn model_args_resume_args_and_session_pin_args_use_the_verified_flags() {
        let a = adapter();
        assert_eq!(
            a.model_args("qwen3.8-max"),
            vec!["--model".to_string(), "qwen3.8-max".to_string()]
        );
        assert_eq!(
            a.resume_args("abc-123"),
            Some(vec!["--resume".to_string(), "abc-123".to_string()])
        );
        assert_eq!(
            a.session_pin_args("abc-123"),
            vec!["--session-id".to_string(), "abc-123".to_string()]
        );
    }

    #[test]
    fn quit_and_compact_are_verified() {
        assert_eq!(adapter().quit_sequence(), "/quit\r");
        assert_eq!(adapter().compact_command(), Some("/compress"));
    }

    #[test]
    fn provider_for_model_resolves_the_billed_vendor_when_recognized() {
        let a = adapter();
        assert_eq!(a.provider(), "qwen");
        assert_eq!(a.provider_for_model(Some("qwen3-coder-plus")), "qwen");
        assert_eq!(
            a.provider_for_model(Some("anthropic/claude-sonnet-5")),
            "anthropic"
        );
        assert_eq!(a.provider_for_model(Some("totally-unknown")), "qwen");
        assert_eq!(a.provider_for_model(None), "qwen");
    }

    // -- sanitize_cwd ----------------------------------------------------

    #[test]
    fn sanitize_cwd_replaces_every_non_alphanumeric_character() {
        let slug = sanitize_cwd(Path::new("/Users/x/Documents/repo"));
        assert!(!slug.contains('/'));
        assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    // -- transcript_path ------------------------------------------------

    fn session_for(cwd: &Path, id: &str) -> SessionRef {
        SessionRef {
            id: SessionId::parse(id),
            cwd: cwd.to_path_buf(),
        }
    }

    #[test]
    fn transcript_path_resolution_1_is_the_direct_session_id_join() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let a = adapter().with_home(home.path().to_path_buf());
        let session = session_for(repo.path(), "22222222-2222-4222-8222-222222222222");

        let chats_dir = a.chats_dir(repo.path());
        std::fs::create_dir_all(&chats_dir).expect("mkdir chats");
        let expected = chats_dir.join("22222222-2222-4222-8222-222222222222.jsonl");
        std::fs::write(&expected, "").expect("write chat file");

        assert_eq!(a.transcript_path(&session), expected);
    }

    #[test]
    fn transcript_path_falls_back_to_the_scan_and_pins_it_when_unpinned() {
        let home = tempfile::tempdir().expect("home");
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let a = adapter()
            .with_home(home.path().to_path_buf())
            .with_state_root(state.path().to_path_buf());
        let session = session_for(repo.path(), "22222222-2222-4222-8222-222222222222");

        // No file named after zirv's own session id exists (a `wrap` relaunch
        // never pins it) -- but qwen's own randomly-minted conversation file
        // does, in the same deterministic chats dir.
        let chats_dir = a.chats_dir(repo.path());
        std::fs::create_dir_all(&chats_dir).expect("mkdir chats");
        let real = chats_dir.join("99999999-9999-4999-8999-999999999999.jsonl");
        std::fs::write(
            &real,
            format!(
                "{}\n",
                serde_json::json!({
                    "uuid": "u1",
                    "parentUuid": null,
                    "sessionId": "99999999-9999-4999-8999-999999999999",
                    "timestamp": "2026-09-07T00:00:00.000Z",
                    "type": "user",
                    "message": {"role": "user", "parts": [{"text": "hi"}]}
                })
            ),
        )
        .expect("write chat file");

        let state_dir =
            crate::commands::ctx::state::StateDir::from_root(state.path().to_path_buf());
        let short = crate::commands::ctx::sessions::short_id(session.id.as_str());
        std::fs::create_dir_all(state_dir.sessions()).expect("mkdir sessions");
        let mut record = crate::commands::ctx::sessions::Record::new(
            session.id.as_str(),
            "qwen",
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
        assert_eq!(resolved, real);

        // A second call must return the exact same file even if a newer chat
        // file later appears in the same directory.
        let newer = chats_dir.join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl");
        std::fs::write(
            &newer,
            format!(
                "{}\n",
                serde_json::json!({
                    "uuid": "u2",
                    "parentUuid": null,
                    "sessionId": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "timestamp": "2026-09-08T00:00:00.000Z",
                    "type": "user",
                    "message": {"role": "user", "parts": [{"text": "hi"}]}
                })
            ),
        )
        .expect("write chat file");
        assert_eq!(a.transcript_path(&session), resolved);
    }

    #[test]
    fn transcript_path_never_joins_a_traversal_session_id_verbatim() {
        let home = tempfile::tempdir().expect("home");
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let a = adapter()
            .with_home(home.path().to_path_buf())
            .with_state_root(state.path().to_path_buf());
        let session = session_for(repo.path(), "../../evil");

        let chats_dir = a.chats_dir(repo.path());
        std::fs::create_dir_all(&chats_dir).expect("mkdir chats");

        let resolved = a.transcript_path(&session);
        assert!(
            resolved.starts_with(&chats_dir),
            "a hostile session id must never escape the chats dir: {resolved:?}"
        );
        assert!(!resolved.to_string_lossy().contains(".."));
    }

    // -- parse_events -----------------------------------------------------

    fn fixture_jsonl() -> String {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qwen/chat-session.jsonl");
        std::fs::read_to_string(path).expect("read fixture")
    }

    #[test]
    fn parse_events_reports_real_turns_final_text_and_tokens() {
        let jsonl = fixture_jsonl();
        let events = adapter().parse_events(&jsonl);

        let turn_starts = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
            .count();
        assert_eq!(turn_starts, 2, "fixture has two real user turns");

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
        assert_eq!(*input_tokens, 150);

        let NormalizedEvent::AssistantFinal { input_tokens, .. } = finals[1] else {
            unreachable!()
        };
        assert_eq!(*input_tokens, 420);
    }

    #[test]
    fn parse_events_skips_synthetic_user_rows_with_a_subtype() {
        let jsonl = fixture_jsonl();
        let events = adapter().parse_events(&jsonl);
        // The fixture carries one mid-turn/notification-shaped row with a
        // `subtype`; it must never start a fresh turn.
        assert!(jsonl.contains("\"subtype\""), "fixture must exercise this");
        let turn_starts = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
            .count();
        assert_eq!(turn_starts, 2);
    }

    #[test]
    fn parse_events_reports_the_model_id() {
        let jsonl = fixture_jsonl();
        let events = adapter().parse_events(&jsonl);
        let model_ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ModelId { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(model_ids.contains(&"qwen3-coder-plus"));
    }

    #[test]
    fn model_hint_reports_the_most_recent_model() {
        let jsonl = fixture_jsonl();
        assert_eq!(
            adapter().model_hint(&jsonl),
            Some("qwen3-coder-plus".to_string())
        );
    }

    #[test]
    fn transcript_usage_sums_each_turn_once() {
        let jsonl = fixture_jsonl();
        let usage = adapter().transcript_usage(&jsonl).expect("usage present");
        assert_eq!(usage.input_tokens, 150 + 420);
        assert!(!adapter().transcript_usage_is_cumulative());
    }

    #[test]
    fn structural_context_carries_real_user_and_assistant_text_only() {
        let jsonl = fixture_jsonl();
        let ctx = adapter().structural_context(&jsonl, 10);
        assert_eq!(ctx.user_messages.len(), 2);
        assert_eq!(ctx.assistant_texts.len(), 2);
        assert!(ctx.files_read.is_empty());
    }

    #[test]
    fn structural_context_last_n_zero_keeps_nothing() {
        let jsonl = fixture_jsonl();
        let ctx = adapter().structural_context(&jsonl, 0);
        assert!(ctx.assistant_texts.is_empty());
    }

    #[test]
    fn parse_events_is_line_local_across_a_split_chunk() {
        let jsonl = fixture_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        let mid = lines.len() / 2;
        let first_half = lines[..mid].join("\n");
        let second_half = lines[mid..].join("\n");

        let adapter = adapter();
        let mut piecewise = adapter.parse_events(&first_half);
        piecewise.extend(adapter.parse_events(&second_half));
        assert_eq!(piecewise, adapter.parse_events(&jsonl));
    }

    // -- detect / registry -------------------------------------------------

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shims() {
        let a = adapter();
        assert!(a.detect(&["qwen".to_string()]));
        assert!(a.detect(&["qwen.cmd".to_string()]));
        assert!(a.detect(&["qwen.ps1".to_string()]));
        assert!(a.detect(&["/usr/local/bin/qwen".to_string()]));
        assert!(!a.detect(&["codex".to_string()]));
        assert!(!a.detect(&[]));
    }

    #[test]
    fn all_registers_qwen() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"qwen"), "got {names:?}");
    }

    #[test]
    fn ladder_answers_equal_the_catalogue() {
        let vendor = catalogue::vendor("qwen").expect("qwen is registered");
        let a = adapter();
        assert_eq!(
            a.review_model_below(None),
            catalogue::rung_below(vendor, None)
        );
        assert_eq!(
            a.model_strength("qwen3.8-max"),
            catalogue::strength(vendor, "qwen3.8-max")
        );
        assert_eq!(
            a.context_window_tokens(Some("qwen3.8-max")),
            catalogue::context_window(vendor, Some("qwen3.8-max"))
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
        assert!(caps.system_prompt);
        assert!(!caps.marker_signal);
        assert!(!caps.turn_signal);
        assert!(!adapter().counts_tool_calls());
        assert!(!adapter().supports_headless_compact());
    }

    #[test]
    fn launch_prefix_len_counts_every_bin_arg_token() {
        let a = QwenAdapter::new(Some("sh /tmp/stub.sh"));
        assert_eq!(a.launch_prefix_len(), 2);
        assert_eq!(QwenAdapter::new(None).launch_prefix_len(), 1);
    }
}
