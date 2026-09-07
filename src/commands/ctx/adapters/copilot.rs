//! Issue #387 (wave 2): the GitHub Copilot CLI adapter (`@github/copilot`).
//!
//! Copilot CLI ships as a tiny npm loader (`npm-loader.js`, `package.json`'s
//! `bin.copilot`) that `spawnSync`s a per-platform COMPILED binary pulled in
//! via an `optionalDependencies` package (`@github/copilot-win32-x64` etc.,
//! verified: `npm pack @github/copilot@latest` -> `1.0.83`, extracted under
//! this worktree's `target/copilot-survey/package/`, 2026-09-07). Unlike
//! `gemini-cli`/`pi-coding-agent`, there is no bundled JS to read source facts
//! from -- every fact below is either (a) GitHub's own published reference
//! docs, fetched as raw Markdown straight from `github/docs@main` (not the
//! rendered site, to quote verbatim and avoid a summarizer's paraphrase), or
//! (b) a real, independent open-source parser that reads the exact on-disk
//! file this adapter reads, cited by file. Copilot CLI itself is not
//! installed on this machine and was never run; nothing below was exercised
//! against a live process. Anything not cited to a specific doc file or
//! external source is UNSUPPORTED and left at the trait default rather than
//! guessed.
//!
//! # Verified facts and their source
//!
//! - **Binary**: `copilot` (npm `@github/copilot`). Non-interactive:
//!   `-p PROMPT`/`--prompt=PROMPT` ("Execute a prompt programmatically (exits
//!   after completion)") -- `github/docs@main:content/copilot/reference/
//!   copilot-cli-reference/cli-command-reference.md`, line "`-p PROMPT`,
//!   `--prompt=PROMPT`". Piping to stdin with `-p` OMITTED also runs
//!   non-interactively, reading the prompt from stdin: `content/copilot/
//!   how-tos/copilot-cli/automate-copilot-cli/run-cli-programmatically.md`,
//!   "Pipe a prompt to the `copilot` command: `echo \"...\" | copilot`" plus
//!   its own note "Piped input is ignored if you also provide a prompt with
//!   the `-p` or `--prompt` option." Interactive: bare `copilot` "Launch the
//!   interactive user interface" (`cli-command-reference.md`'s command
//!   table); an initial prompt for that session is `-i PROMPT`/
//!   `--interactive=PROMPT` ("Start an interactive session and automatically
//!   execute this prompt") -- there is no verified bare-positional form the
//!   way claude/gemini/pi/codex each have, so [`interactive_cmd`] uses `-i`
//!   instead of a positional token.
//! - **`--allow-all-tools` is REQUIRED for programmatic use**
//!   (`cli-command-reference.md`: "`--allow-all-tools` -- Allow all tools to
//!   run automatically without confirmation. Required when using the CLI
//!   programmatically (env: `COPILOT_ALLOW_ALL`)."): a headless run has no
//!   TTY to answer a per-tool approval prompt, so [`headless_cmd`]/
//!   [`headless_cmd_stdin`] always carry it -- never `interactive_cmd`, whose
//!   pane has a human attached who can actually answer a prompt.
//! - **Deny always wins over allow, even `--allow-all`**, verified verbatim:
//!   `content/copilot/how-tos/copilot-cli/use-copilot-cli/allowing-tools.md`,
//!   "Deny rules always take precedence over allow rules, even when
//!   `--allow-all` is set or a matching approval has been saved in
//!   `permissions-config.json`." This is what makes [`read_only_args`]'s
//!   `--deny-tool=shell,write` a genuine structural deny even though
//!   [`headless_cmd`] always adds `--allow-all-tools` first: the two compose
//!   safely regardless of order, unlike an adapter that would need the deny
//!   to be appended *after* the allow to win.
//! - **Tool kinds for `--allow-tool`/`--deny-tool`** (`cli-programmatic-
//!   reference.md`'s own table, quoted verbatim): `shell` ("Executing shell
//!   commands"), `write` ("Creating or modifying files"), `read` ("Reading
//!   files or directories"), `url` ("Fetching content from a URL"), `memory`
//!   ("Storing new facts to the agent's persistent memory"), and an
//!   MCP-server name. [`read_only_args`] denies exactly `shell,write` -- the
//!   same write/shell-shaped boundary `gemini::READ_ONLY_POLICY_TOML` and
//!   `pi::PiAdapter::read_only_args` each draw for their own tool
//!   vocabularies -- leaving `read`/`url`/`memory` untouched. `--available-
//!   tools`/`--excluded-tools` restrict which tools the MODEL even knows
//!   about (`allowing-tools.md`, "Restricting the choice of tools available
//!   to the AI model") rather than granting/denying permission for ones it
//!   does know about; `--deny-tool` is the closer analogue of every sibling
//!   adapter's own `read_only_args` (a permission floor, not a visibility
//!   cut), so this adapter uses that instead.
//! - **Session identity**: `--session-id ID` (`cli-command-reference.md`,
//!   quoted verbatim): "Use an exact session or task ID... If the ID matches
//!   an existing session or task, that session or task is resumed. If
//!   nothing matches, a new session is created only when the value is a
//!   valid UUID." [`SessionId::new_v4`] is always a real UUID v4 string, so
//!   this ONE flag both pins a fresh headless launch onto zirv's own session
//!   uuid ([`headless_cmd`]/[`session_pin_args`]) AND resumes that exact
//!   conversation later ([`resume_args`]) -- unlike claude's split `--
//!   session-id`(pin)/`--resume`(resume) pair or pi's `--session-id`(pin)/
//!   `--session`(resume) pair, copilot needs only the one flag either way.
//! - **Session storage**: `~/.copilot/session-state/<session-id>/
//!   events.jsonl`, honoring `COPILOT_HOME` in place of `~/.copilot`
//!   -- verified verbatim, `content/copilot/reference/copilot-cli-reference/
//!   cli-config-dir-reference.md`: "`session-state/` | Directory | Session
//!   history and workspace data" (directory listing) and, in the file's own
//!   per-entry detail section, "### `session-state/` -- Contains session
//!   history data, organized by session ID in subdirectories. Each session
//!   directory stores an event log (`events.jsonl`) and workspace artifacts
//!   (plans, checkpoints, tracked files)." `COPILOT_HOME` itself: "Override
//!   the configuration and state directory. Default: `$HOME/.copilot`."
//!   Because `--session-id` pins the directory name to zirv's own uuid
//!   (previous bullet), [`transcript_path`] is a plain join with no
//!   registry/pin-file lookup at all -- unlike `gemini`/`pi`/`codex`, which
//!   each need one because their own harness mints an unpredictable id or
//!   filename component zirv cannot control.
//! - **`events.jsonl` row shapes**: two independent, mutually-corroborating
//!   sources, since GitHub does not publish this format (open feature
//!   request `github/copilot-cli#3551`, "Formalize events.jsonl as an
//!   official hook/integration API", still open as of 2026-09-07):
//!   1. Jon Chew's blog post <https://jonmagic.com/posts/github-copilot-
//!      session-search-and-resume-cli/> (a GitHub employee; the post states
//!      "I work at GitHub, which helped me confirm some of the original
//!      findings by reviewing internal code"), quoted verbatim (raw HTML
//!      fetched directly, not paraphrased): `{"type":"session.start",
//!      "timestamp":"...","data":{"context":{"repository":"...",
//!      "branch":"..."}}}`, `{"type":"user.message","timestamp":"...",
//!      "data":{"content":"..."}}`, `{"type":"assistant.turn_start",
//!      "timestamp":"...","data":{"turnId":"..."}}`,
//!      `{"type":"tool.execution_start","timestamp":"...",
//!      "data":{"toolName":"...","arguments":{...}}}`,
//!      `{"type":"tool.execution_complete","timestamp":"...",
//!      "data":{"success":true}}`, `{"type":"session.shutdown",
//!      "timestamp":"...","data":{...}}` -- with the caveat, also quoted
//!      verbatim, "The event payloads evolve, but the broad structure still
//!      looks like this" and "these files are not a supported public API and
//!      may change."
//!   2. `ccusage/ccusage` (MIT-licensed, open source; `rust/adapters/copilot/
//!      src/parser.rs`, fetched via `gh api repos/ccusage/ccusage/contents/
//!      ...`, commit as of 2026-09-07): its `CopilotSessionStateEvent`/
//!      `CopilotSessionStateData`/`CopilotSessionModelMetrics`/
//!      `CopilotSessionUsage` structs, and their own inline test fixtures,
//!      independently confirm the `"session.shutdown"` row's `data` shape
//!      down to field names: `{"modelMetrics":{"<model>":{"usage":
//!      {"inputTokens":N,"outputTokens":N,"cacheReadTokens":N,
//!      "cacheWriteTokens":N,"reasoningTokens":N},"requests":
//!      {"count":N}}}}`. ccusage's own `uncached_session_input_tokens`
//!      (`usage.inputTokens - (cacheReadTokens + cacheWriteTokens)`) is what
//!      [`AgentAdapter::transcript_usage`]'s own implementation below mirrors
//!      for `input_tokens`: Copilot's own `inputTokens` counts BOTH cache
//!      classes inclusively, unlike `TranscriptUsage`'s own contract
//!      (`input_tokens` excludes them; see its doc comment's
//!      `context_total`).
//!
//!   Only the fields cited above are used; every other observed field (`id`
//!   on some rows, `turnId`, tool `arguments` contents, `requests.count`'s
//!   `cost`) is either hashed (never interpreted) or ignored outright.
//!
//! # Deliberately UNSUPPORTED in this wave
//!
//! - **The assistant's own final response text.** Neither source above names
//!   an event carrying the model's generated reply text -- `user.message`
//!   carries the human's `content`, `assistant.turn_start` carries only a
//!   `turnId`, and neither source's event list contains an
//!   `assistant.message`-shaped row with an outgoing `content`/`text` field.
//!   [`parse_events`] therefore never emits [`NormalizedEvent::
//!   AssistantFirstText`]/[`NormalizedEvent::AssistantFinal`] at all -- an
//!   honest gap, not a guess at a field name that might not exist. This also
//!   means [`structural_context`]'s `assistant_texts` stays permanently
//!   empty, unlike every other registered adapter's own partial coverage.
//! - **System prompt injection.** No CLI flag or documented mechanism adds or
//!   replaces per-run system-prompt text (`--agent=AGENT` selects a whole
//!   pre-defined custom agent, not ad hoc text; `.github/copilot-
//!   instructions.md`/`AGENTS.md` are repo-owned, always-on files, the same
//!   category `gemini::GEMINI_SYSTEM_MD` and `AGENTS.md`/`CLAUDE.md` already
//!   are for their own adapters). [`system_prompt_args`] returns nothing;
//!   [`Capabilities::system_prompt`] is `false`.
//! - **Compaction/quit, headless.** `/compact`/`/exit`/`/quit` (verified,
//!   `cli-command-reference.md`'s slash-command table) are interactive-only;
//!   no headless equivalent is documented, so [`supports_headless_compact`]
//!   stays at the trait default (`false`), mirroring every other adapter.
//! - **Turn-boundary signal.** Copilot CLI does have a real hooks system
//!   (`.github/hooks/*.json`, `~/.copilot/hooks/`), but no fact below
//!   verifies a hook event zirv's own turn-signal socket protocol could ride
//!   on, so [`register_turn_signal`] is a no-op, matching `gemini`/`codex`.
//! - **Provider-per-model billing.** Unlike `pi`/`opencode` (each a genuine
//!   pass-through onto the CALLER's own separate provider account per
//!   model), Copilot CLI meters every prompt against the operator's OWN
//!   GitHub Copilot subscription regardless of which underlying model
//!   answered it -- verified, the shipped npm `README.md`: "Each time you
//!   submit a prompt to GitHub Copilot CLI, your monthly quota of premium
//!   requests is reduced by one." [`provider`] is therefore the constant
//!   `"github"` and [`provider_for_model`] is left at the trait default
//!   (which also answers `"github"` regardless of `model`) -- deliberately
//!   NOT overridden with `catalogue::vendor_of` the way `pi`/`opencode` are,
//!   since doing so would misreport the billed account. The model ladder
//!   methods below ([`review_model_below`]/[`model_strength`]/
//!   [`context_window_tokens`]) still resolve per-model through `catalogue::
//!   vendor_of`, because those describe a property of the MODEL itself
//!   (context window, relative strength), not who pays for the call.

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
pub struct CopilotAdapter {
    program: String,
    bin_args: Vec<String>,
    /// Test seam only: pins [`Self::copilot_home`]'s answer directly, instead
    /// of resolving `COPILOT_HOME`/`$HOME/.copilot`. Deliberately a field
    /// rather than mutating the real `COPILOT_HOME` process environment
    /// variable for a test -- `opencode::OpenCodeAdapter`'s own
    /// `forced_db_path` doc comment already states why: edition 2024 makes
    /// `std::env::set_var` `unsafe` precisely because it races other
    /// threads, and the full (non-nextest) serial suite runs every test in
    /// one process.
    forced_home: Option<PathBuf>,
}

impl CopilotAdapter {
    /// `bin` may carry arguments, mirroring `CodexAdapter::new`/
    /// `ClaudeAdapter::new` exactly.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("copilot").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "copilot".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            forced_home: None,
        }
    }

    /// Test seam: see [`Self::forced_home`]'s own doc comment.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.forced_home = Some(home);
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

    /// `$COPILOT_HOME`, or `$HOME/.copilot` when unset -- see this module's
    /// own doc comment ("Session storage") for the verified source. The real
    /// process environment is read directly (never `home_dir()` alone, and
    /// never zirv's own `config::env_from_process` seam, which exists for
    /// zirv's OWN `ZIRV_CTX_*` variables, not a third-party CLI's).
    fn copilot_home(&self) -> PathBuf {
        if let Some(forced) = &self.forced_home {
            return forced.clone();
        }
        std::env::var_os("COPILOT_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| {
                crate::utils::home_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(".copilot")
            })
    }
}

/// One `session.shutdown` row's per-model usage, folded from
/// `data.modelMetrics.<model>.usage` -- see this module's own doc comment
/// ("`events.jsonl` row shapes") for the verified field names.
#[derive(Debug, Clone, Copy, Default)]
struct ShutdownUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

fn shutdown_usage_of(usage: &Value) -> ShutdownUsage {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    ShutdownUsage {
        input_tokens: field("inputTokens"),
        output_tokens: field("outputTokens"),
        cache_read_tokens: field("cacheReadTokens"),
        cache_write_tokens: field("cacheWriteTokens"),
    }
}

/// Every `"session.shutdown"` row's own `data.modelMetrics` object, in file
/// order -- the one shared walk [`parse_events`], [`model_hint`] and
/// [`transcript_usage`] all use so none of the three can ever disagree about
/// which rows are shutdown rows.
fn shutdown_rows(jsonl: &str) -> Vec<serde_json::Map<String, Value>> {
    jsonl
        .lines()
        .filter_map(|line| {
            let row: Value = serde_json::from_str(line.trim()).ok()?;
            if row.get("type").and_then(Value::as_str) != Some("session.shutdown") {
                return None;
            }
            row.get("data")?.get("modelMetrics")?.as_object().cloned()
        })
        .collect()
}

impl AgentAdapter for CopilotAdapter {
    fn name(&self) -> &'static str {
        "copilot"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// The account Copilot CLI actually meters against -- see this module's
    /// own doc comment ("Provider-per-model billing") for the verified
    /// rationale for why this is a constant rather than a per-model
    /// resolution the way `pi`/`opencode` each need.
    fn provider(&self) -> &'static str {
        "github"
    }

    // No `provider_for_model` override -- see this module's own doc comment,
    // "Provider-per-model billing": the trait default (`self.provider()`
    // regardless of `model`) is the honest answer here, unlike `pi`/
    // `opencode`.

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
                f == "copilot" || f == "copilot.cmd" || f == "copilot.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt> --allow-all-tools --session-id <session>` -- see this
    /// module's own doc comment for both verified facts: `-p` for a
    /// programmatic run, and `--allow-all-tools` being REQUIRED for one since
    /// there is no TTY to answer a permission prompt. `extra` (e.g.
    /// `read_only_args()`) is appended last, so a `--deny-tool=...` there
    /// always wins over the `--allow-all-tools` this method itself adds
    /// (verified: "Deny rules always take precedence over allow rules, even
    /// when `--allow-all` is set").
    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--allow-all-tools")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// For the Windows `.cmd`-shim case: no `-p` and no positional token, so
    /// the verified stdin-becomes-the-prompt fallback (this module's own doc
    /// comment, "Pipe a prompt to the `copilot` command") supplies it instead
    /// of an argv token cmd.exe could reparse.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("--allow-all-tools")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        Some(cmd)
    }

    /// `-i PROMPT`/`--interactive=PROMPT` ("Start an interactive session and
    /// automatically execute this prompt") -- see this module's own doc
    /// comment for why this, rather than a bare positional, is the verified
    /// mechanism for copilot specifically.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg("-i").arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// A one-shot, stdin-to-stdout judgment call (no positional prompt --
    /// `handoff::run_model` pipes it in, mirroring every other adapter's own
    /// `distiller_cmd`). `--allow-all-tools` plus [`read_only_args`]'s
    /// `--deny-tool=shell,write` keeps it from ever running a command or
    /// writing a file; `-s` (`--silent`, verified: "Output only the agent
    /// response... useful for scripting with `-p`") keeps stray usage-stats
    /// decoration out of the judgment text. Unlike `pi`'s verified
    /// `--no-session`, no equivalent "don't persist a session" flag is
    /// documented for copilot, so a distiller call still leaves a
    /// `session-state/<uuid>/` directory behind -- a residual, not a gap this
    /// adapter has a verified mechanism to close.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("--allow-all-tools");
        cmd.args(self.read_only_args());
        cmd.arg("-s");
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        cmd
    }

    /// `--deny-tool=shell,write` -- see this module's own doc comment
    /// ("Tool kinds") for the verified tool-kind names and why `--deny-tool`,
    /// not `--available-tools`/`--excluded-tools`, is the analogue of every
    /// sibling adapter's own read-only pin.
    fn read_only_args(&self) -> Vec<String> {
        vec!["--deny-tool".to_string(), "shell,write".to_string()]
    }

    /// No verified per-run system-prompt mechanism -- see this module's own
    /// doc comment, "Deliberately UNSUPPORTED".
    fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
        Vec::new()
    }

    /// `$COPILOT_HOME/session-state/<session-id>/events.jsonl` -- see this
    /// module's own doc comment ("Session storage") for the verified path and
    /// why, unlike every sibling multi-step adapter, this is a plain join
    /// with no pin-file or registry lookup: `--session-id` (always pinned by
    /// [`headless_cmd`]/[`session_pin_args`]) makes the directory name
    /// zirv's own uuid, not one copilot mints unpredictably.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        self.copilot_home()
            .join("session-state")
            .join(session.id.as_str())
            .join("events.jsonl")
    }

    /// Maps the two independently-verified event shapes (this module's own
    /// doc comment) onto the existing `NormalizedEvent` vocabulary:
    /// - `user.message` -> [`NormalizedEvent::TurnStart`], plus
    ///   [`NormalizedEvent::UserText`] whenever `data.content` is a non-empty
    ///   string (never guessed when `content` is some other shape).
    /// - `tool.execution_start` -> [`NormalizedEvent::ToolCall`] (`name` from
    ///   `data.toolName`, `input_hash` over the raw `data.arguments` JSON).
    /// - `tool.execution_complete` -> [`NormalizedEvent::ToolResult`] from
    ///   `data.success`, only when that field is present (never guessed).
    /// - `session.shutdown` -> one [`NormalizedEvent::ModelId`] per key of
    ///   `data.modelMetrics`, the verified enumeration of every model that
    ///   answered during the session (`ccusage`'s own `CopilotSessionStateData`,
    ///   this module's own doc comment).
    ///
    /// NOT mapped: `session.start` (no textual/token content in its verified
    /// shape) and `assistant.turn_start` (`turnId` only -- no verified
    /// assistant response text at all, see this module's own doc comment,
    /// "Deliberately UNSUPPORTED").
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
            let Some(kind) = row.get("type").and_then(Value::as_str) else {
                continue;
            };
            let data = row.get("data");
            match kind {
                "user.message" => {
                    events.push(NormalizedEvent::TurnStart { at_ms });
                    if let Some(text) = data.and_then(|d| d.get("content")).and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        events.push(NormalizedEvent::UserText {
                            byte_len: text.len() as u64,
                        });
                    }
                }
                "tool.execution_start" => {
                    let Some(data) = data else { continue };
                    let Some(name) = data.get("toolName").and_then(Value::as_str) else {
                        continue;
                    };
                    let raw = data
                        .get("arguments")
                        .map(Value::to_string)
                        .unwrap_or_default();
                    events.push(NormalizedEvent::ToolCall {
                        name: name.to_string(),
                        input_hash: input_hash(&raw),
                        at_ms,
                    });
                }
                "tool.execution_complete" => {
                    let Some(success) =
                        data.and_then(|d| d.get("success")).and_then(Value::as_bool)
                    else {
                        continue;
                    };
                    events.push(NormalizedEvent::ToolResult { is_error: !success });
                }
                "session.shutdown" => {
                    let Some(models) = data
                        .and_then(|d| d.get("modelMetrics"))
                        .and_then(Value::as_object)
                    else {
                        continue;
                    };
                    for model in models.keys() {
                        events.push(NormalizedEvent::ModelId { id: model.clone() });
                    }
                }
                _ => {}
            }
        }
        events
    }

    /// Only `user_messages` is populated, from every `user.message` row's
    /// string `data.content` -- see this module's own doc comment,
    /// "Deliberately UNSUPPORTED", for why `assistant_texts` stays
    /// permanently empty (no verified assistant response text event exists
    /// at all) and `files_read`/`files_modified`/`tool_errors` stay empty
    /// (tool `arguments`/`success` carry no verified per-tool-kind file path
    /// or error text shape).
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut user_messages: Vec<String> = jsonl
            .lines()
            .filter_map(|line| {
                let row: Value = serde_json::from_str(line.trim()).ok()?;
                if row.get("type").and_then(Value::as_str) != Some("user.message") {
                    return None;
                }
                let text = row.get("data")?.get("content")?.as_str()?;
                (!text.trim().is_empty()).then(|| text.to_string())
            })
            .collect();
        if user_messages.len() > last_n {
            user_messages.drain(..user_messages.len() - last_n);
        }
        StructuralContext {
            user_messages,
            ..StructuralContext::default()
        }
    }

    /// The LAST `"session.shutdown"` row's own `data.modelMetrics` keys, when
    /// there is exactly one -- "most recently observed", mirroring
    /// `gemini::model_hint`/`pi::model_hint`'s own `.rev()`-style contract.
    /// `None` when the fragment carries no shutdown row yet (the common case
    /// for a still-running session -- `session.shutdown` fires once, at the
    /// very end, per this module's own doc comment) or when that row names
    /// more than one model: with no verified way to tell which of several
    /// models answered LAST within one shutdown summary, reporting one of
    /// them as "the" current model would be a guess, not a hint.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        let last = shutdown_rows(jsonl).pop()?;
        let mut keys = last.keys();
        let only = keys.next()?;
        if keys.next().is_some() {
            return None;
        }
        Some(only.clone())
    }

    /// Sums EVERY `"session.shutdown"` row's own `data.modelMetrics` found in
    /// `jsonl`, across every model each row names -- never just the last one.
    /// A session that spans more than one copilot process (a resume after a
    /// crash, for instance) may append more than one shutdown row, and
    /// whether a later row RESTATES the whole session's total or reports only
    /// that run's own segment is not verified either way; summing every
    /// occurrence is the one choice that stays correct under EITHER reading
    /// when there is only the single shutdown row every session normally
    /// produces, and is also the only choice that keeps this method
    /// line-local (plain summation is associative, so a transcript cut at
    /// newlines and parsed in pieces sums to the same total either way).
    /// `input_tokens` subtracts both cache classes out of copilot's own
    /// `inputTokens` reading (`ccusage`'s own `uncached_session_input_tokens`,
    /// this module's own doc comment) since `TranscriptUsage::input_tokens`
    /// excludes them by contract (see that type's own `context_total` doc
    /// comment). `reasoningTokens` has no field on `TranscriptUsage` to land
    /// in and is dropped, the same honest gap every adapter's own unmodeled
    /// token class already is.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let mut usage = TranscriptUsage::default();
        let mut observed = false;
        for row in shutdown_rows(jsonl) {
            for metrics in row.values() {
                let Some(raw_usage) = metrics.get("usage") else {
                    continue;
                };
                let tokens = shutdown_usage_of(raw_usage);
                if tokens.input_tokens == 0
                    && tokens.output_tokens == 0
                    && tokens.cache_read_tokens == 0
                    && tokens.cache_write_tokens == 0
                {
                    continue;
                }
                observed = true;
                usage.input_tokens = usage.input_tokens.saturating_add(
                    tokens.input_tokens.saturating_sub(
                        tokens
                            .cache_read_tokens
                            .saturating_add(tokens.cache_write_tokens),
                    ),
                );
                usage.output_tokens = usage.output_tokens.saturating_add(tokens.output_tokens);
                usage.cache_creation_input_tokens = usage
                    .cache_creation_input_tokens
                    .saturating_add(tokens.cache_write_tokens);
                usage.cache_read_input_tokens = usage
                    .cache_read_input_tokens
                    .saturating_add(tokens.cache_read_tokens);
            }
        }
        observed.then_some(usage)
    }

    /// Each shutdown row's own `usage` is summed once as it is found (see
    /// [`transcript_usage`]'s own doc comment) rather than read as one
    /// running total a caller must not double-add -- the same `false`
    /// convention `pi`/`claude` already use for their own additive folds.
    fn transcript_usage_is_cumulative(&self) -> bool {
        false
    }

    /// Verified: [`parse_events`] emits a real [`NormalizedEvent::ToolCall`]
    /// from `tool.execution_start` (this module's own doc comment) -- unlike
    /// `gemini`/`codex`, which have no verified tool-call shape at all and
    /// must refuse the flag outright, a `--max-tool-calls` ceiling has real
    /// signal to count against here.
    fn counts_tool_calls(&self) -> bool {
        true
    }

    /// Verified: `cli-command-reference.md`'s slash-command table, `/compact
    /// [FOCUS-INSTRUCTIONS]` ("Summarize the conversation history to reduce
    /// context window usage").
    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    /// Verified: `cli-command-reference.md`, `/exit`, `/quit` ("Close the
    /// current session"). `\r` mirrors every other adapter's own PTY-injected
    /// keystroke terminator.
    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            turn_signal: false,
            system_prompt: false,
            events: true,
            token_usage: true,
            // Unverified: copilot's own interactive composer paste/submit
            // behavior was never observed (the CLI is not installed and
            // could not be run here). `false` is the same conservative
            // default `Capabilities::default()` already gives, not a
            // positive claim of "submits correctly".
            defer_injection_submit: false,
            context_window_tokens: None,
        }
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let vendor = model
            .and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)?;
        catalogue::context_window(vendor, model)
    }

    /// This adapter's own vendor is resolved per-MODEL for the ladder
    /// methods only -- see this module's own doc comment,
    /// "Provider-per-model billing", for why that is deliberately NOT true of
    /// [`provider_for_model`] above. Mirrors `pi::PiAdapter::
    /// review_model_below` exactly.
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        seat.and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("")
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor_of(model)
            .and_then(catalogue::vendor)
            .and_then(|v| catalogue::strength(v, model))
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    /// Verified: `cli-command-reference.md`, `--model=MODEL`.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    /// `--session-id <id>` resumes the EXISTING conversation at that id --
    /// see this module's own doc comment ("Session identity") for the
    /// verified quote. The same flag [`session_pin_args`] pins a fresh launch
    /// with; unlike claude/pi, copilot needs only the one flag either way.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["--session-id".to_string(), session_id.to_string()])
    }

    /// The same `--session-id <uuid>` flag [`headless_cmd`] already pins
    /// every headless run with, offered here so an interactive dashboard
    /// pane can be pinned too -- see this module's own doc comment for why
    /// this is what makes [`resume_args`] resolve to a real conversation
    /// after a quit.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        vec!["--session-id".to_string(), session.to_string()]
    }

    /// No verified per-run turn-boundary signal -- see this module's own doc
    /// comment, "Deliberately UNSUPPORTED". Mirrors `gemini`/`codex`'s own
    /// no-op.
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

    fn adapter() -> CopilotAdapter {
        CopilotAdapter::new(Some("copilot"))
    }

    fn built_args(adapter: &CopilotAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    // -- command shapes -----------------------------------------------

    #[test]
    fn headless_cmd_carries_prompt_allow_all_tools_and_session_pin() {
        let adapter = adapter();
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter.headless_cmd("do the thing", &session, &["--extra".to_string()]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "-p".to_string(),
                "do the thing".to_string(),
                "--allow-all-tools".to_string(),
                "--session-id".to_string(),
                "11111111-2222-4333-8444-555555555555".to_string(),
                "--extra".to_string(),
            ]
        );
    }

    #[test]
    fn headless_cmd_stdin_carries_no_prompt_token() {
        let adapter = adapter();
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter
            .headless_cmd_stdin(&session, &[])
            .expect("copilot has a verified stdin form");
        let args = built_args(&adapter, &cmd);
        assert!(!args.contains(&"-p".to_string()));
        assert!(args.contains(&"--allow-all-tools".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn interactive_cmd_uses_the_dash_i_flag_not_a_positional() {
        let adapter = adapter();
        let cmd = adapter.interactive_cmd(Some("explain this project"), &[]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec!["-i".to_string(), "explain this project".to_string()]
        );

        let bare = adapter.interactive_cmd(None, &[]);
        assert!(built_args(&adapter, &bare).is_empty());
    }

    #[test]
    fn distiller_cmd_denies_shell_and_write_and_carries_the_model() {
        let adapter = adapter();
        let cmd = adapter.distiller_cmd("claude-haiku-4.5");
        let args = built_args(&adapter, &cmd);
        assert!(args.contains(&"--allow-all-tools".to_string()));
        assert!(args.contains(&"--deny-tool".to_string()));
        assert!(args.contains(&"shell,write".to_string()));
        assert!(args.contains(&"-s".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"claude-haiku-4.5".to_string()));
        assert!(!args.contains(&"-p".to_string()));

        let no_model = adapter.distiller_cmd("");
        assert!(!built_args(&adapter, &no_model).contains(&"--model".to_string()));
    }

    #[test]
    fn read_only_args_denies_exactly_shell_and_write() {
        assert_eq!(
            adapter().read_only_args(),
            vec!["--deny-tool".to_string(), "shell,write".to_string()]
        );
    }

    #[test]
    fn model_args_resume_args_and_session_pin_args_use_the_verified_flag() {
        let adapter = adapter();
        assert_eq!(
            adapter.model_args("gpt-5.3-codex"),
            vec!["--model".to_string(), "gpt-5.3-codex".to_string()]
        );
        assert_eq!(
            adapter.resume_args("abc123"),
            Some(vec!["--session-id".to_string(), "abc123".to_string()])
        );
        assert_eq!(
            adapter.session_pin_args("abc123"),
            vec!["--session-id".to_string(), "abc123".to_string()]
        );
    }

    #[test]
    fn quit_and_compact_use_the_verified_slash_commands() {
        assert_eq!(adapter().quit_sequence(), "/quit\r");
        assert_eq!(adapter().compact_command(), Some("/compact"));
    }

    #[test]
    fn provider_is_github_regardless_of_model() {
        let adapter = adapter();
        assert_eq!(adapter.provider(), "github");
        assert_eq!(adapter.provider_for_model(None), "github");
        assert_eq!(
            adapter.provider_for_model(Some("claude-sonnet-4.5")),
            "github"
        );
        assert_eq!(adapter.provider_for_model(Some("gpt-5.3-codex")), "github");
    }

    #[test]
    fn ladder_methods_answer_from_the_models_own_vendor() {
        let adapter = adapter();
        let anthropic = catalogue::vendor("anthropic").expect("anthropic is registered");
        assert_eq!(
            adapter.context_window_tokens(Some("claude-sonnet-5")),
            catalogue::context_window(anthropic, Some("claude-sonnet-5"))
        );
        assert_eq!(
            adapter.model_strength("claude-sonnet-5"),
            catalogue::strength(anthropic, "claude-sonnet-5")
        );

        let openai = catalogue::vendor("openai").expect("openai is registered");
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-sol")),
            catalogue::rung_below(openai, Some("gpt-5.6-sol"))
        );

        // An unrecognized model never falls back to a guessed vendor.
        assert_eq!(adapter.context_window_tokens(Some("totally-unknown")), None);
        assert_eq!(adapter.model_strength("totally-unknown"), None);
        assert_eq!(adapter.review_model_below(Some("totally-unknown")), "");
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shims() {
        let adapter = adapter();
        assert!(adapter.detect(&["copilot".to_string()]));
        assert!(adapter.detect(&["copilot.cmd".to_string()]));
        assert!(adapter.detect(&["copilot.ps1".to_string()]));
        assert!(adapter.detect(&["/usr/local/bin/copilot".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_copilot() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"copilot"), "got {names:?}");
    }

    #[test]
    fn capabilities_report_real_events_and_usage_but_no_marker_turn_or_prompt_signal() {
        let caps = adapter().capabilities();
        assert!(caps.events);
        assert!(caps.token_usage);
        assert!(!caps.marker_signal);
        assert!(!caps.turn_signal);
        assert!(!caps.system_prompt);
        assert!(adapter().counts_tool_calls());
    }

    #[test]
    fn launch_prefix_len_counts_every_bin_arg_token() {
        let a = CopilotAdapter::new(Some("sh /tmp/stub.sh"));
        assert_eq!(a.launch_prefix_len(), 2);
        assert_eq!(CopilotAdapter::new(None).launch_prefix_len(), 1);
    }

    // -- transcript_path ------------------------------------------------

    #[test]
    fn transcript_path_is_a_plain_join_with_no_registry_lookup() {
        let home = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };
        assert_eq!(
            a.transcript_path(&session),
            home.path()
                .join("session-state")
                .join("11111111-2222-4333-8444-555555555555")
                .join("events.jsonl")
        );
    }

    #[test]
    fn copilot_home_honours_the_forced_test_seam() {
        // Production `copilot_home()` reads the real `COPILOT_HOME` process
        // env var, which this test deliberately never mutates (see
        // `forced_home`'s own doc comment for why) -- this only proves the
        // resolution formula the test seam stands in for.
        let home = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_home(home.path().to_path_buf());
        assert_eq!(a.copilot_home(), home.path());
    }

    // -- parse_events -----------------------------------------------------

    fn fixture_jsonl() -> String {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/copilot/events.jsonl");
        std::fs::read_to_string(path).expect("read fixture")
    }

    #[test]
    fn parse_events_reports_turns_tool_calls_and_results() {
        let events = adapter().parse_events(&fixture_jsonl());

        let turn_starts = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
            .count();
        assert_eq!(turn_starts, 2, "fixture has two user turns");

        let user_texts: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::UserText { byte_len } => Some(*byte_len),
                _ => None,
            })
            .collect();
        assert_eq!(user_texts.len(), 2);

        let tool_calls: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls, vec!["read", "write", "write"]);

        let tool_results: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolResult { is_error } => Some(*is_error),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results, vec![false, true, false]);

        // No verified assistant-response-text event exists at all (this
        // module's own doc comment) -- `parse_events` must never invent one.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::AssistantFinal { .. }))
        );
    }

    #[test]
    fn parse_events_emits_model_ids_from_every_shutdown_rows_metrics() {
        let events = adapter().parse_events(&fixture_jsonl());
        let model_ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ModelId { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(model_ids, vec!["claude-sonnet-4.5", "gpt-5.3-codex"]);
    }

    #[test]
    fn model_hint_reports_the_last_shutdown_rows_single_model() {
        assert_eq!(
            adapter().model_hint(&fixture_jsonl()),
            Some("gpt-5.3-codex".to_string())
        );
    }

    #[test]
    fn model_hint_is_none_when_a_shutdown_row_names_more_than_one_model() {
        let jsonl = r#"{"type":"session.shutdown","id":"s1","timestamp":"2026-09-07T10:00:00.000Z","data":{"modelMetrics":{"a":{"usage":{"inputTokens":1,"outputTokens":1}},"b":{"usage":{"inputTokens":1,"outputTokens":1}}}}}"#;
        assert_eq!(adapter().model_hint(jsonl), None);
    }

    #[test]
    fn transcript_usage_sums_every_shutdown_row_and_excludes_cache_from_input() {
        let usage = adapter()
            .transcript_usage(&fixture_jsonl())
            .expect("fixture carries shutdown usage");
        // row 1: input 1200 - (200 + 100) = 900, output 300
        // row 2: input 500 - 0 = 500, output 120
        assert_eq!(usage.input_tokens, 900 + 500);
        assert_eq!(usage.output_tokens, 300 + 120);
        assert_eq!(usage.cache_creation_input_tokens, 100);
        assert_eq!(usage.cache_read_input_tokens, 200);
        assert!(!adapter().transcript_usage_is_cumulative());
    }

    #[test]
    fn transcript_usage_is_none_with_no_shutdown_row() {
        let jsonl = r#"{"type":"user.message","timestamp":"2026-09-07T10:00:00.000Z","data":{"content":"hi"}}"#;
        assert_eq!(adapter().transcript_usage(jsonl), None);
    }

    #[test]
    fn structural_context_carries_only_user_text_capped_at_last_n() {
        let ctx = adapter().structural_context(&fixture_jsonl(), 1);
        assert_eq!(ctx.user_messages, vec!["Also add a test".to_string()]);
        assert!(ctx.assistant_texts.is_empty());
        assert!(ctx.files_read.is_empty());
        assert!(ctx.files_modified.is_empty());
    }

    #[test]
    fn parse_events_is_line_local_across_a_split_chunk() {
        // The incremental scoring path feeds fragments cut at newlines; a
        // whole-file parse must equal the concatenation of piecewise parses.
        let adapter = adapter();
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

    #[test]
    fn transcript_usage_is_line_local_across_a_split_chunk() {
        let adapter = adapter();
        let whole = fixture_jsonl();
        let lines: Vec<&str> = whole.lines().collect();
        let mid = lines.len() / 2;
        let first_half = lines[..mid].join("\n");
        let second_half = lines[mid..].join("\n");

        let a = adapter.transcript_usage(&first_half).unwrap_or_default();
        let b = adapter.transcript_usage(&second_half).unwrap_or_default();
        let whole_usage = adapter.transcript_usage(&whole).expect("fixture has usage");
        assert_eq!(
            whole_usage.input_tokens,
            a.input_tokens.saturating_add(b.input_tokens)
        );
        assert_eq!(
            whole_usage.output_tokens,
            a.output_tokens.saturating_add(b.output_tokens)
        );
    }

    /// Sanity check on the fixture file itself: every non-blank line parses
    /// as JSON, so a typo in the hand-built fixture fails here rather than
    /// silently zeroing out one of the assertions above.
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
