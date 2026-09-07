//! Issue #386 (wave 1): the Pi coding-agent adapter.
//!
//! Pi is `pi-coding-agent` (npm `pi-coding-agent`, published as
//! `@earendil-works/pi-coding-agent`), source at `github.com/badlogic/
//! pi-mono`. Not installed on this machine -- every fact below is verified
//! against the upstream source at the commit on `main` fetched 2026-09-07,
//! never guessed. Unlike claude/codex, Pi is a genuinely multi-provider CLI
//! (`--provider`/`--model provider/id`, Anthropic/OpenAI/Google/Azure/
//! DeepSeek/Mistral/Groq/xAI/OpenRouter and more -- `packages/coding-agent/
//! src/cli/args.ts`'s own `--api-key`/env-var help text), so this adapter
//! overrides every ladder method (`provider_for_model`, `review_model_below`,
//! `model_strength`, `context_window_tokens`) to resolve through `catalogue::
//! vendor_of(model)` instead of one hardcoded vendor constant the way
//! `codex::CATALOGUE_VENDOR`/`claude::CATALOGUE_VENDOR` do -- `catalogue::
//! normalize_id`/`vendor_of`'s own doc comments (issue #381) name this
//! adapter and `#385` (OpenCode) as their first callers.
//!
//! Verified facts and their source files (`packages/coding-agent/src/`
//! unless noted, badlogic/pi-mono @ 2026-09-07):
//! - `cli/args.ts`: `--print, -p` ("Non-interactive mode: process prompt and
//!   exit") is a boolean flag; a following non-flag, non-`@`-prefixed token
//!   is consumed as the prompt. `--mode <text|json|rpc>`. `--model <pattern>`
//!   ("supports \"provider/id\" and optional \":<thinking>\""). `--provider
//!   <name>` (default `google`). `--system-prompt <text>` (replace) and
//!   `--append-system-prompt <text>` (append, repeatable) -- a file-path form
//!   is upstream issue #5131, still open, so only the inline-text flag is
//!   used here. `--session <path|id>` ("Use specific session file or partial
//!   UUID") loads an EXISTING session; `--session-id <id>` ("Use exact
//!   project session ID, creating it if missing") is the one flag that pins a
//!   chosen id onto a NEW conversation, mirroring `claude::ClaudeAdapter`'s
//!   own `--session-id`-pins/`--resume`-restores split (`headless_cmd`/
//!   `session_pin_args` use `--session-id`; `resume_args` uses `--session`).
//!   Built-in tool names: `read, bash, powershell, edit, write, grep, find,
//!   ls` (quoted verbatim from the `--tools`/`--no-tools`/`--no-builtin-tools`
//!   help block); `read_only_args` allow-lists the four inspection-shaped
//!   ones (`read,grep,find,ls`), excluding the two shell tools and the two
//!   mutating file tools.
//! - `core/slash-commands.ts`: built-in commands include `/compact` and
//!   `/quit` (quoted verbatim from the file's own command list).
//! - `core/session-manager.ts`: a session entry is `{type, id, parentId,
//!   timestamp, ...}` (`SessionEntryBase`); a `type: "message"` entry carries
//!   `message: AgentMessage`. `getDefaultSessionDirPath` computes
//!   `join(agentDir, "sessions", safePath)` where `safePath =
//!   "--" + resolvedCwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-") +
//!   "--"` (quoted verbatim) -- `session_dir_slug` below is that same
//!   transform. A session file is named `${fileTimestamp}_${sessionId}
//!   .jsonl`; the timestamp cannot be predicted, so [`find_session_file`]
//!   locates the real file by id SUFFIX inside the (fully deterministic)
//!   per-cwd directory, the same shape `codex::find_rollout` already uses
//!   for codex's own unpredictable rollout paths. `CompactionEntry` carries
//!   `summary`/`firstKeptEntryId`/`tokensBefore`/optional `usage`.
//! - `config.ts`: `getAgentDir()` returns `join(homedir(), CONFIG_DIR_NAME,
//!   "agent")`, `CONFIG_DIR_NAME` defaults to `".pi"` (`pkg.piConfig?.
//!   configDir || ".pi"`) -- i.e. `~/.pi/agent` -- overridable by an
//!   `<APP_NAME>_CODING_AGENT_DIR` env var this adapter does not honor (a
//!   deliberate scope cut, not a verification gap: the default path is what
//!   every fact above is verified against).
//! - `packages/agent/src/types.ts`: `AgentMessage` wraps the `Message` union
//!   below (plus extension-defined custom messages, irrelevant here).
//! - `packages/ai/src/types.ts`: `Message = UserMessage | AssistantMessage |
//!   ToolResultMessage`. `AssistantMessage` carries `content: (TextContent |
//!   ThinkingContent | ToolCall)[]`, `model: string`, `usage: Usage`.
//!   `ToolResultMessage` carries `toolCallId`, `toolName`, `isError: bool`.
//!   `TextContent { type: "text", text }`, `ToolCall { type: "toolCall", id,
//!   name, arguments }`. `Usage { input, output, cacheRead, cacheWrite,
//!   totalTokens, cost: {..} }` (quoted verbatim) -- the four raw classes
//!   [`TranscriptUsage`] already separates, so no adapter-side folding is
//!   needed (unlike codex's inclusive `input_tokens`, see `codex::
//!   parse_events`'s own doc comment).
//! - `main.ts`: piped (non-TTY) stdin becomes the prompt when `--print` is
//!   given no positional message (`readPipedStdin`/`prepareInitialMessage`),
//!   the same stdin-delivery shape `headless_cmd_stdin` needs for a Windows
//!   `.cmd`-shim launch.
//!
//! NOT mapped/verified, and therefore never claimed: `system_prompt_file_flag`
//! (upstream issue #5131 is open), an env-var override of the sessions root,
//! `--continue`/`--fork`, and this harness's own TUI composer paste/`\r`
//! folding behavior (`Capabilities::defer_injection_submit` is left `false`,
//! the same "nothing verified, assume the safer common case" default
//! `claude::ClaudeAdapter` ships).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
    input_hash,
};
use super::super::window;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

#[derive(Debug, Clone)]
pub struct PiAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
}

impl PiAdapter {
    /// `bin` may carry arguments, mirroring `CodexAdapter::new`/
    /// `ClaudeAdapter::new` exactly (`"sh /tmp/stub.sh"`, `"/usr/bin/env pi"`
    /// both work).
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("pi").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "pi".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
        }
    }

    /// Test seam: pins the home directory [`transcript_path`](AgentAdapter::
    /// transcript_path) resolves the sessions root from, mirroring
    /// `CodexAdapter::with_home`/`ClaudeAdapter::with_home`.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

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

    /// `~/.pi/agent/sessions` -- see this module's own doc comment for the
    /// `config.ts` facts this is built from.
    fn sessions_root(&self) -> PathBuf {
        self.home_dir().join(".pi").join("agent").join("sessions")
    }

    /// The deterministic per-cwd session directory -- see
    /// [`session_dir_slug`].
    fn session_dir(&self, cwd: &Path) -> PathBuf {
        self.sessions_root().join(session_dir_slug(cwd))
    }
}

/// `getDefaultSessionDirPath`'s own `safePath` transform (`session-
/// manager.ts`, see this module's doc comment): strip exactly one leading
/// path separator, then replace every remaining `/`, `\` and `:` with `-`,
/// fenced in `--...--`.
fn session_dir_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy().into_owned();
    let stripped = raw
        .strip_prefix('/')
        .or_else(|| raw.strip_prefix('\\'))
        .unwrap_or(raw.as_str());
    let body: String = stripped
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{body}--")
}

/// Finds the real session file by its unpredictable-timestamp-prefixed name's
/// id SUFFIX inside `dir` -- see this module's own doc comment for why the
/// full filename cannot be computed. Mirrors `codex::find_rollout`, but
/// non-recursive: pi shards sessions by cwd, not by date.
fn find_session_file(dir: &Path, suffix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.to_string_lossy().ends_with(suffix) {
            return Some(path);
        }
    }
    None
}

/// The concatenated text of an `AssistantMessage`'s own `text`-typed content
/// blocks (`packages/ai/src/types.ts`'s `TextContent`), dropping `thinking`
/// and `toolCall` blocks -- the same "text blocks only" rule `claude::
/// text_of`/`codex`'s `last_agent_message` reporting already apply.
fn assistant_text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// A `Usage` field is a JS `number`; reads it as a raw integer, falling back
/// to a truncated float for the (unobserved in practice) case a provider's
/// SDK serializes one as `123.0`. Never a guess when the field is absent --
/// callers that need "no reading at all" distinguished from `0` check for the
/// enclosing `usage` object's presence first, as [`transcript_usage`] does.
fn token_count_of(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f as u64))
        .unwrap_or(0)
}

impl AgentAdapter for PiAdapter {
    fn name(&self) -> &'static str {
        "pi"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Pi itself spends no account of its own -- it is a front end onto
    /// whichever provider `--model`/`--provider` names. `"pi"` is the honest
    /// static answer for a launch with no model in hand yet;
    /// [`provider_for_model`] resolves the real billed vendor whenever a
    /// model string is available, which is the common case for this adapter.
    fn provider(&self) -> &'static str {
        "pi"
    }

    /// The one override every registered adapter before this one had no
    /// reason to make: `codex`/`claude` each spend exactly one account, but
    /// pi's `--model provider/id` can pin any of them. `catalogue::vendor_of`
    /// strips the `provider/` (or `provider.`) namespace and matches the bare
    /// id against every known vendor's own ladder/`extra_prices` (issue
    /// #381) -- `None` (an id this catalogue does not recognize, on no named
    /// vendor prefix, or no model at all) falls back to this adapter's own
    /// static `"pi"`, never a guess.
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
                f == "pi" || f == "pi.cmd" || f == "pi.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt> --mode json --session-id <id>`: `--print`/`-p` plus a
    /// following non-flag token is the verified prompt-delivery shape
    /// (`args.ts`, this module's doc comment); `--mode json` is the
    /// machine-readable output mode a supervisor parses; `--session-id`
    /// creates the conversation at zirv's own chosen id when it does not yet
    /// exist, the same fact [`transcript_path`] and [`resume_args`] both
    /// depend on.
    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--mode")
            .arg("json")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    /// For the Windows `.cmd`-shim case (see [`launches_through_cmd_shim`]):
    /// `-p` with no positional token, so `main.ts`'s piped-stdin fallback
    /// (this module's doc comment) supplies the prompt instead of an argv
    /// token cmd.exe could reparse.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--mode")
            .arg("json")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        Some(cmd)
    }

    /// Same derivation every adapter gets by default (`AgentAdapter::
    /// launches_through_cmd_shim`'s own doc comment) -- overridden explicitly,
    /// mirroring `CodexAdapter`/`ClaudeAdapter`, so the launch-shape decision
    /// reads the same way across every adapter file rather than being visible
    /// only in the trait.
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// `pi [prompt]` with no subcommand is the interactive launch (verified:
    /// the same bare-positional-becomes-`result.messages` parsing `-p` itself
    /// uses, `args.ts`, this module's doc comment).
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// `--append-system-prompt <text>` (repeatable, inline text only -- the
    /// file-path form is upstream issue #5131, still open) -- see this
    /// module's own doc comment.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        vec!["--append-system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt")
    }

    /// The distiller needs no repository read access at all: its whole
    /// context is the prompt zirv composes for it, unlike `codex`/`claude`,
    /// whose harnesses auto-load AGENTS.md/CLAUDE.md. `--no-tools` (verified,
    /// `args.ts`) is therefore the stronger, correct pin here rather than the
    /// read-only allow-list [`read_only_args`] uses elsewhere -- and
    /// `--no-session` (verified, `args.ts`: "Don't save session (ephemeral)")
    /// means a one-off judgment call leaves no session file behind. The
    /// prompt itself is delivered the same stdin way [`headless_cmd_stdin`]
    /// uses: no positional token, so this method (which is never handed a
    /// prompt, only a model) never has to guess an argv length limit for it.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--mode")
            .arg("text")
            .arg("--no-tools")
            .arg("--no-session");
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        cmd
    }

    /// The four inspection-shaped built-in tools out of pi's own eight
    /// (`read, bash, powershell, edit, write, grep, find, ls` -- `args.ts`,
    /// this module's doc comment): `read`, `grep`, `find`, `ls`. Excludes the
    /// two shell tools (`bash`, `powershell`) and the two mutating file tools
    /// (`edit`, `write`) -- the same read/inspect-only boundary `codex`'s
    /// `--sandbox read-only` and `claude`'s `--disallowedTools=Write,Edit,
    /// Bash,NotebookEdit` each draw for their own tool vocabularies.
    fn read_only_args(&self) -> Vec<String> {
        vec!["--tools".to_string(), "read,grep,find,ls".to_string()]
    }

    /// This adapter's own vendor is resolved per-model (see
    /// [`provider_for_model`]'s own doc comment), so `review_model_below`
    /// answers from the SEAT's vendor when recognized, else the trait
    /// default (`""`, "no verified ladder for this seat") -- never a static
    /// vendor guess the way `codex`/`claude` can, since either could be
    /// spending a different provider's account than `seat` names.
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        seat.and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("")
    }

    /// Same per-model vendor resolution as [`review_model_below`].
    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor_of(model)
            .and_then(catalogue::vendor)
            .and_then(|v| catalogue::strength(v, model))
    }

    /// Same per-model vendor resolution as [`review_model_below`]/
    /// [`model_strength`]: the model's own vendor states a window when known,
    /// else `None` -- never the trait's blanket "no verified capacity" answer
    /// applied to a model this catalogue actually recognizes.
    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let vendor = model
            .and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)?;
        catalogue::context_window(vendor, model)
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    /// See this module's own doc comment for the `session-manager.ts` facts
    /// this is built from: a deterministic per-cwd directory
    /// ([`PiAdapter::session_dir`]), searched for the real, timestamp-prefixed
    /// file by its id suffix ([`find_session_file`]). The fallback (a file
    /// that has never existed) is the same honest "not found yet" shape
    /// `codex::CodexAdapter::transcript_path`'s own final fallback uses.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let dir = self.session_dir(&session.cwd);
        let suffix = format!("_{}.jsonl", session.id);
        if let Some(found) = find_session_file(&dir, &suffix) {
            return found;
        }
        dir.join(format!("pending{suffix}"))
    }

    /// Maps the verified `type: "message"` entry shape (this module's own
    /// doc comment) onto the existing `NormalizedEvent` vocabulary:
    /// - a `user`-role message -> [`NormalizedEvent::TurnStart`].
    /// - an `assistant`-role message -> its `model` field (when present) as
    ///   [`NormalizedEvent::ModelId`], its concatenated text blocks as
    ///   [`NormalizedEvent::AssistantFinal`] (`input_tokens` from
    ///   `usage.input`, `0` when the message carries no `usage` object at
    ///   all -- never a guess), an [`NormalizedEvent::AssistantFirstText`]
    ///   sibling whenever that text is non-empty (mirroring `claude::
    ///   parse_events`'s own per-row candidate), and one
    ///   [`NormalizedEvent::ToolCall`] per `toolCall`-typed content block.
    /// - a `toolResult`-role message -> [`NormalizedEvent::ToolResult`] from
    ///   its own `isError` field.
    /// - a `type: "compaction"` entry -> [`NormalizedEvent::Compaction`].
    /// - a `type: "model_change"` entry -> its `modelId` field as
    ///   [`NormalizedEvent::ModelId`].
    ///
    /// NOT mapped: `custom` messages (no verified role-specific shape) and
    /// tool-call `arguments`/results beyond a hash of the raw JSON, the same
    /// "hash, never the raw content" rule `claude::parse_events` already
    /// applies to `tool_use.input`.
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

            match row.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let Some(message) = row.get("message") else {
                        continue;
                    };
                    match message.get("role").and_then(Value::as_str) {
                        Some("user") => {
                            events.push(NormalizedEvent::TurnStart { at_ms });
                        }
                        Some("assistant") => {
                            if let Some(id) = message.get("model").and_then(Value::as_str) {
                                events.push(NormalizedEvent::ModelId { id: id.to_string() });
                            }
                            let input_tokens = message
                                .get("usage")
                                .and_then(|usage| usage.get("input"))
                                .map(token_count_of)
                                .unwrap_or(0);
                            let text = assistant_text_of(message);
                            if !text.trim().is_empty() {
                                events.push(NormalizedEvent::AssistantFirstText { at_ms });
                            }
                            events.push(NormalizedEvent::AssistantFinal {
                                text,
                                input_tokens,
                                at_ms,
                            });
                            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                                for block in blocks.iter().filter(|b| {
                                    b.get("type").and_then(Value::as_str) == Some("toolCall")
                                }) {
                                    let name = block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let raw = block
                                        .get("arguments")
                                        .map(Value::to_string)
                                        .unwrap_or_default();
                                    events.push(NormalizedEvent::ToolCall {
                                        name,
                                        input_hash: input_hash(&raw),
                                        at_ms,
                                    });
                                }
                            }
                        }
                        Some("toolResult") => {
                            let is_error = message
                                .get("isError")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            events.push(NormalizedEvent::ToolResult { is_error });
                        }
                        _ => {}
                    }
                }
                Some("compaction") => {
                    events.push(NormalizedEvent::Compaction);
                }
                Some("model_change") => {
                    if let Some(id) = row.get("modelId").and_then(Value::as_str) {
                        events.push(NormalizedEvent::ModelId { id: id.to_string() });
                    }
                }
                _ => {}
            }
        }
        events
    }

    /// The most recently observed `message.model` off an `assistant`-role
    /// row, newest-to-oldest -- mirrors `claude::model_hint` exactly, over
    /// pi's own entry shape.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        jsonl.lines().rev().find_map(|line| {
            let row = serde_json::from_str::<Value>(line.trim()).ok()?;
            if row.get("type").and_then(Value::as_str) != Some("message") {
                return None;
            }
            let message = row.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            message.get("model")?.as_str().map(str::to_string)
        })
    }

    /// Only `assistant_texts` is populated, from every non-empty `assistant`
    /// message's concatenated text blocks -- the same partial-but-real shape
    /// `codex::structural_context` ships, and for the same reason:
    /// `user_messages`/`files_read`/`files_modified`/`tool_errors` have no
    /// content-bearing verified shape mapped yet (tool `arguments`/results
    /// are hashed, never kept as text, in [`parse_events`]).
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut assistant_texts: Vec<String> = jsonl
            .lines()
            .filter_map(|line| {
                let row = serde_json::from_str::<Value>(line.trim()).ok()?;
                if row.get("type").and_then(Value::as_str) != Some("message") {
                    return None;
                }
                let message = row.get("message")?;
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    return None;
                }
                let text = assistant_text_of(message);
                (!text.trim().is_empty()).then_some(text)
            })
            .collect();
        if assistant_texts.len() > last_n {
            assistant_texts.drain(..assistant_texts.len() - last_n);
        }
        StructuralContext {
            assistant_texts,
            ..StructuralContext::default()
        }
    }

    /// Sums every `assistant` message's own `usage` object across the
    /// fragment: unlike codex's `token_count` lines (already a cumulative
    /// running total, see `codex::parse_events`'s doc comment), pi's `Usage`
    /// (`packages/ai/src/types.ts`) is the ONE LLM call that produced that
    /// message, so the cumulative figure has to be folded here -- the same
    /// shape `claude::transcript_usage`'s own fold uses. `cacheWrite`/
    /// `cacheRead` map straight onto `cache_creation_input_tokens`/
    /// `cache_read_input_tokens`; no class needs inventing or excluding.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let mut usage = TranscriptUsage::default();
        let mut observed = false;
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if row.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(message) = row.get("message") else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(u) = message.get("usage") else {
                continue;
            };
            observed = true;
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(u.get("input").map(token_count_of).unwrap_or(0));
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(u.get("output").map(token_count_of).unwrap_or(0));
            usage.cache_creation_input_tokens = usage
                .cache_creation_input_tokens
                .saturating_add(u.get("cacheWrite").map(token_count_of).unwrap_or(0));
            usage.cache_read_input_tokens = usage
                .cache_read_input_tokens
                .saturating_add(u.get("cacheRead").map(token_count_of).unwrap_or(0));
        }
        observed.then_some(usage)
    }

    /// This fold sums the fragment itself (see [`transcript_usage`]'s own
    /// doc comment) rather than reading a pre-existing cumulative snapshot.
    fn transcript_usage_is_cumulative(&self) -> bool {
        false
    }

    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    /// Verified built-in slash command (`slash-commands.ts`, this module's
    /// doc comment). Mirrors `codex::CodexAdapter::quit_sequence` exactly.
    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            token_usage: true,
            turn_signal: false,
            system_prompt: true,
            events: true,
            ..Capabilities::default()
        }
    }

    /// `--append-system-prompt` carries untrusted, zirv-composed text
    /// straight on argv, exactly the shape `codex`'s `-c developer_
    /// instructions=...` is -- and codex's own `system_prompt_supported`
    /// override exists for exactly this reparse risk on a Windows `.cmd`
    /// shim launch. Mirrors that override precisely: unsupported whenever the
    /// probed launch would reparse through cmd.exe, since there is no
    /// verified stdin-delivered system-prompt mechanism to fall back to.
    fn system_prompt_supported(&self, launch: &[String]) -> bool {
        let probe = if launch.is_empty() {
            super::flatten_command(self.interactive_cmd(None, &[]))
        } else {
            launch.to_vec()
        };
        !super::launch_reparses_through_shim(&probe)
    }

    /// Verified (`args.ts`, this module's doc comment): `--model <pattern>`.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    /// `--session <id>` ("Use specific session file or partial UUID",
    /// verified `args.ts`) loads the EXISTING conversation
    /// [`session_pin_args`]'s `--session-id` created -- the same
    /// pin-then-resume pairing `claude::ClaudeAdapter` already verifies for
    /// its own `--session-id`/`--resume`.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["--session".to_string(), session_id.to_string()])
    }

    /// The same `--session-id <id>` flag [`headless_cmd`] already pins every
    /// headless run with, offered here so an interactive dashboard pane can
    /// be pinned too -- see `claude::ClaudeAdapter::session_pin_args`'s own
    /// doc comment for why this is what makes [`resume_args`] resolve to a
    /// real conversation after a quit.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        vec!["--session-id".to_string(), session.to_string()]
    }

    /// No verified per-run turn-boundary signal for pi (unrelated to
    /// [`parse_events`]'s own after-the-fact turn detection) -- mirrors
    /// `codex::CodexAdapter::register_turn_signal`'s own no-op.
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

    fn built_args(adapter: &PiAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    #[test]
    fn headless_cmd_carries_the_print_mode_prompt_and_session_pin() {
        let adapter = PiAdapter::new(None);
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter.headless_cmd("do the thing", &session, &[]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "-p".to_string(),
                "do the thing".to_string(),
                "--mode".to_string(),
                "json".to_string(),
                "--session-id".to_string(),
                "11111111-2222-4333-8444-555555555555".to_string(),
            ]
        );
    }

    #[test]
    fn headless_cmd_stdin_carries_no_positional_prompt() {
        let adapter = PiAdapter::new(None);
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter
            .headless_cmd_stdin(&session, &[])
            .expect("pi has a verified stdin form");
        let args = built_args(&adapter, &cmd);
        assert_eq!(args[0], "-p");
        assert!(!args.contains(&"do the thing".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn interactive_cmd_carries_a_positional_prompt() {
        let adapter = PiAdapter::new(None);
        let cmd = adapter.interactive_cmd(Some("hello"), &[]);
        assert_eq!(built_args(&adapter, &cmd), vec!["hello".to_string()]);

        let bare = adapter.interactive_cmd(None, &[]);
        assert!(built_args(&adapter, &bare).is_empty());
    }

    #[test]
    fn system_prompt_args_uses_the_repeatable_append_flag() {
        let adapter = PiAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be careful"),
            vec![
                "--append-system-prompt".to_string(),
                "be careful".to_string()
            ]
        );
        assert_eq!(
            adapter.user_system_prompt_flag(),
            Some("--append-system-prompt")
        );
    }

    #[test]
    fn read_only_args_allow_lists_only_inspection_shaped_tools() {
        let adapter = PiAdapter::new(None);
        assert_eq!(
            adapter.read_only_args(),
            vec!["--tools".to_string(), "read,grep,find,ls".to_string()]
        );
        let allow = adapter.read_only_args();
        for mutating in ["bash", "powershell", "edit", "write"] {
            assert!(
                !allow.iter().any(|a| a.contains(mutating)),
                "{mutating} must never appear in the read-only allow-list"
            );
        }
    }

    #[test]
    fn distiller_cmd_disables_every_tool_and_persists_no_session() {
        let adapter = PiAdapter::new(None);
        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        let args = built_args(&adapter, &cmd);
        assert!(args.contains(&"--no-tools".to_string()));
        assert!(args.contains(&"--no-session".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"gpt-5.6-luna".to_string()));
        assert!(!args.contains(&"do the thing".to_string()));

        let no_model = adapter.distiller_cmd("");
        assert!(!built_args(&adapter, &no_model).contains(&"--model".to_string()));
    }

    #[test]
    fn model_args_resume_args_and_session_pin_args_use_the_verified_flags() {
        let adapter = PiAdapter::new(None);
        assert_eq!(
            adapter.model_args("anthropic/claude-sonnet-5"),
            vec![
                "--model".to_string(),
                "anthropic/claude-sonnet-5".to_string()
            ]
        );
        assert_eq!(
            adapter.resume_args("abc123"),
            Some(vec!["--session".to_string(), "abc123".to_string()])
        );
        assert_eq!(
            adapter.session_pin_args("abc123"),
            vec!["--session-id".to_string(), "abc123".to_string()]
        );
    }

    #[test]
    fn provider_for_model_resolves_the_billed_vendor_from_the_pinned_model() {
        let adapter = PiAdapter::new(None);
        assert_eq!(adapter.provider(), "pi");
        assert_eq!(
            adapter.provider_for_model(Some("anthropic/claude-sonnet-5")),
            "anthropic"
        );
        assert_eq!(adapter.provider_for_model(Some("gpt-5.6-terra")), "openai");
        assert_eq!(adapter.provider_for_model(Some("some-unknown-model")), "pi");
        assert_eq!(adapter.provider_for_model(None), "pi");
    }

    #[test]
    fn ladder_methods_answer_from_the_openai_vendor_when_the_model_is_recognized() {
        let adapter = PiAdapter::new(None);
        let openai = catalogue::vendor("openai").expect("openai is a registered vendor");
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-sol")),
            catalogue::rung_below(openai, Some("gpt-5.6-sol"))
        );
        assert_eq!(
            adapter.model_strength("gpt-5.6-terra"),
            catalogue::strength(openai, "gpt-5.6-terra")
        );
        assert_eq!(
            adapter.context_window_tokens(Some("gpt-5.6-terra")),
            catalogue::context_window(openai, Some("gpt-5.6-terra"))
        );

        let anthropic = catalogue::vendor("anthropic").expect("anthropic is a registered vendor");
        assert_eq!(
            adapter.context_window_tokens(Some("claude-sonnet-5")),
            catalogue::context_window(anthropic, Some("claude-sonnet-5"))
        );
        assert_eq!(
            adapter.context_window_tokens(Some("claude-sonnet-5")),
            Some(200_000)
        );

        // An unrecognized model/seat falls back to the trait's own "no
        // verified ladder"/"unknown capacity" answers, never a guessed vendor.
        assert_eq!(adapter.review_model_below(Some("totally-unknown")), "");
        assert_eq!(adapter.model_strength("totally-unknown"), None);
        assert_eq!(adapter.context_window_tokens(Some("totally-unknown")), None);
        assert_eq!(adapter.context_window_tokens(None), None);
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shim_extensions() {
        let adapter = PiAdapter::new(None);
        assert!(adapter.detect(&["pi".to_string()]));
        assert!(adapter.detect(&["pi.cmd".to_string()]));
        assert!(adapter.detect(&["pi.ps1".to_string()]));
        assert!(adapter.detect(&["/usr/local/bin/pi".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_pi() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"pi"), "got {names:?}");
    }

    #[test]
    fn quit_and_compact_use_the_verified_slash_commands() {
        let adapter = PiAdapter::new(None);
        assert_eq!(adapter.quit_sequence(), "/quit\r");
        assert_eq!(adapter.compact_command(), Some("/compact"));
    }

    #[test]
    fn capabilities_report_real_events_and_usage_but_no_marker_or_turn_signal() {
        let caps = PiAdapter::new(None).capabilities();
        assert!(caps.events);
        assert!(caps.token_usage);
        assert!(caps.system_prompt);
        assert!(!caps.marker_signal);
        assert!(!caps.turn_signal);
    }

    #[test]
    fn launch_prefix_len_counts_every_bin_arg_token() {
        let adapter = PiAdapter::new(Some("sh /tmp/stub.sh"));
        assert_eq!(adapter.launch_prefix_len(), 2);
        assert_eq!(PiAdapter::new(None).launch_prefix_len(), 1);
    }

    #[test]
    fn session_dir_slug_matches_the_verified_safe_path_transform() {
        assert_eq!(
            session_dir_slug(Path::new("/Users/x/Documents/repo")),
            "--Users-x-Documents-repo--"
        );
        assert_eq!(
            session_dir_slug(Path::new("/Users/x/repo/.pi-worktrees/b")),
            "--Users-x-repo-.pi-worktrees-b--"
        );
    }

    #[test]
    fn transcript_path_finds_the_real_timestamp_prefixed_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = PiAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };

        // Nothing written yet: an honest "not found" path, never a guess at
        // the unpredictable timestamp prefix.
        let before = adapter.transcript_path(&session);
        assert!(!before.exists());
        assert!(
            before
                .to_string_lossy()
                .ends_with("pending_11111111-2222-4333-8444-555555555555.jsonl")
        );

        let dir = home
            .path()
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--work-repo--");
        std::fs::create_dir_all(&dir).expect("create session dir");
        let real = dir.join("2026-09-07T10-00-00_11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&real, "").expect("write session file");

        assert_eq!(adapter.transcript_path(&session), real);
    }

    fn fixture_jsonl() -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/session.jsonl"),
        )
        .expect("fixture read")
    }

    #[test]
    fn parse_events_maps_turns_tool_calls_and_final_text_from_the_fixture() {
        let adapter = PiAdapter::new(None);
        let events = adapter.parse_events(&fixture_jsonl());

        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::TurnStart { .. })),
            "the user row must start a turn"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            NormalizedEvent::ToolCall { name, .. } if name == "read"
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolResult { is_error: false }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::Compaction))
        );

        let finals: Vec<&NormalizedEvent> = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::AssistantFinal { .. }))
            .collect();
        assert_eq!(finals.len(), 2);
        match finals[0] {
            NormalizedEvent::AssistantFinal {
                text, input_tokens, ..
            } => {
                assert_eq!(text, "I'll check the router first.");
                assert_eq!(*input_tokens, 1200);
            }
            _ => unreachable!(),
        }
        match finals[1] {
            NormalizedEvent::AssistantFinal {
                text, input_tokens, ..
            } => {
                assert_eq!(text, "Added GET /health returning 200 OK.");
                assert_eq!(*input_tokens, 1500);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn model_hint_reads_the_newest_assistant_model() {
        let adapter = PiAdapter::new(None);
        assert_eq!(
            adapter.model_hint(&fixture_jsonl()),
            Some("claude-sonnet-5".to_string())
        );
    }

    #[test]
    fn transcript_usage_sums_every_assistant_messages_own_usage() {
        let adapter = PiAdapter::new(None);
        let usage = adapter
            .transcript_usage(&fixture_jsonl())
            .expect("fixture carries assistant usage");
        assert_eq!(usage.input_tokens, 2700);
        assert_eq!(usage.output_tokens, 300);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert!(!adapter.transcript_usage_is_cumulative());
    }

    #[test]
    fn structural_context_keeps_only_non_empty_assistant_text_capped_at_last_n() {
        let adapter = PiAdapter::new(None);
        let ctx = adapter.structural_context(&fixture_jsonl(), 1);
        assert_eq!(
            ctx.assistant_texts,
            vec!["Added GET /health returning 200 OK.".to_string()]
        );
        assert!(ctx.user_messages.is_empty());
        assert!(ctx.files_read.is_empty());
    }

    #[test]
    fn parse_events_is_line_local_across_a_split_chunk() {
        // The incremental scoring path feeds fragments cut at newlines; a
        // whole-file parse must equal the concatenation of piecewise parses.
        let adapter = PiAdapter::new(None);
        let whole = fixture_jsonl();
        let lines: Vec<&str> = whole.lines().collect();
        let mid = lines.len() / 2;
        let first_half = lines[..mid].join("\n");
        let second_half = lines[mid..].join("\n");

        let mut piecewise = adapter.parse_events(&first_half);
        piecewise.extend(adapter.parse_events(&second_half));
        let whole_parse = adapter.parse_events(&whole);
        assert_eq!(piecewise, whole_parse);
    }

    /// Sanity check on the fixture file itself: every line parses as JSON, so
    /// a typo in the hand-built fixture fails here rather than silently
    /// zeroing out one of the assertions above.
    #[test]
    fn fixture_every_line_is_valid_json() {
        for line in fixture_jsonl().lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|e| panic!("invalid JSON line {line:?}: {e}"));
        }
    }
}
