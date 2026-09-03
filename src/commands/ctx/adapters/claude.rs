use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::CtxResult;
use super::super::event::input_hash;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, ToolInvocation,
    TranscriptUsage, error_text_hash, last_verification_run,
};
use super::super::window::parse_iso8601_utc_ms;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// Claude Code's own base layer, injected on every claude session zirv starts
/// (see `AgentAdapter::base_system_prompt`). Claude-specific by construction:
/// it names the Agent tool, `.claude/agents` and the `/code-review` skill, so
/// handing it to another agent would be handing it instructions about tools
/// that agent does not have.
///
/// Names the Agent tool's own `model` parameter tiers (`haiku`/`sonnet`/
/// `opus`) directly, unlike the rest of this file's model-agnostic framing:
/// that parameter's enum is the harness's own fixed vocabulary, not a vendor
/// lineup that renames out from under this text, so naming it here is the
/// only way to make "set the model parameter explicitly" concrete enough to
/// follow. It still never asks for `--model`: that flag picks *this seat's*
/// model, which stays the operator's choice, untouched by this text.
///
/// Issue #175: carries an explicit delegation-sizing rule so a seat does not
/// default to minting a sub-orchestrator for ordinary work -- native
/// Agent-tool subagents stay the default for any bounded task, and `zirv ctx
/// agent --role sub-orchestrator` is reserved for work that genuinely
/// decomposes into multiple coherently-scoped areas or must run under zirv's
/// own supervision independently of this seat.
///
/// Wrapper behaviour redesign (2026-09-01): rewritten so trivial and bounded
/// changes stay on this seat instead of always delegating -- the prior text's
/// "delegate every substantive piece of work" was absolute regardless of task
/// size, one of the process rules the wrapper-behaviour audit found was
/// turning small fixes into a full dispatch-and-review cycle. Model routing,
/// the fork ban, self-contained briefs and the sub-orchestrator carve-out are
/// unchanged. See
/// `docs/superpowers/specs/2026-09-01-wrapper-behaviour-redesign.md`.
pub const ORCHESTRATOR_PROMPT: &str = "\
zirv orchestrator conventions (claude)

This seat runs the most capable model; spend it on judgment -- sizing, design choices, \
integration, the final call -- not on ceremony.

- Trivial and bounded changes stay on this seat: a brief costs more than the fix. Delegate via \
the Agent tool when the work is larger than its brief or can run in parallel; bundle small \
related items into one checklist brief with a per-item output format, dispatch independent \
substantial work together in the background, and continue a worker you already briefed for \
follow-ups in its area instead of spawning a fresh one. Reserve a sub-orchestrator (`zirv ctx \
agent --role sub-orchestrator --scope \"<area>\"`) for work that splits into several \
coherently-scoped areas each needing its own coordination.
- Every Agent dispatch sets `model` explicitly -- haiku for mechanical and bulk work, sonnet \
for ordinary exploration, implementation, tests and review, opus only for hard debugging or \
design -- because an omitted model inherits this seat. Never use `subagent_type: \"fork\"` \
here; forks always inherit the seat model. Agents in .claude/agents that pin their own model \
keep it, except that reviews always run on the roster's review model.
- Briefs are self-contained -- goal, constraints, relevant paths, exact output format -- and \
tell the worker to run tests in the FOREGROUND and reply with compact structured findings, \
never raw file dumps. Subagents share none of your context.
- Decide rather than let a worker loop: choices between valid designs, architecture changes, \
and anything a worker has failed at twice come back to you. Hold implementers to the \
repository's standards and to the engineering standard above: reuse before adding, minimal \
diff, one focused test per behaviour change, format, lint and test before reporting back.
- Reviews follow the meta-harness rule: in proportion, once. This harness's own /code-review \
runs at low or medium effort on the roster's review model, never high or above (that forks \
this seat's model), and never when a `zirv workflow` review gate covers the change.
- Shared manifests and lockfiles (Cargo.toml, Cargo.lock, package.json, lockfiles) are edited \
only by you or one designated integrator; a writer touching one says so in its report.";

/// Claude's own layer for a delegated **Worker** session (see
/// `AgentAdapter::worker_system_prompt`), spliced in place of
/// [`ORCHESTRATOR_PROMPT`] for `PromptRole::Worker`. A worker never gets that
/// layer's coaching to delegate everything onward -- that would invite
/// recursion into a session that was itself already delegated to -- so this is
/// deliberately its own, much shorter text: execute the brief, do not spawn
/// further zirv workers, and report back plainly.
pub const WORKER_PROMPT: &str = "\
zirv worker conventions (claude)

You are a delegated worker session. Execute your brief directly and completely, then report \
compact results.

- Do not delegate onward: never run `zirv agent` or spawn further zirv workers; this task was \
already routed to you.
- If you use subagents for fan-out within your task, set each dispatch's model explicitly to the \
cheapest one that can do the job, never one above your own session's model, and never use \
fork-type subagents, which inherit this session's model and ignore overrides.
- Run code-review or verification passes only when your brief asks for them; the orchestrator that \
spawned you owns review rounds.
- Your final message is your report: lead with the outcome, keep it self-contained, and never dump \
raw file contents into it.";

/// Claude's own layer for a `PromptRole::SubOrchestrator` session (see
/// `AgentAdapter::sub_orchestrator_system_prompt`), spliced in place of
/// [`ORCHESTRATOR_PROMPT`] and [`WORKER_PROMPT`] for that role. Unlike a
/// Worker, a sub-orchestrator may split its own scope and dispatch Workers
/// via `zirv agent` -- so it gets that delegation vocabulary -- but unlike
/// the Orchestrator it must never learn to spawn another coordinator: an
/// unbounded delegation tree is exactly the cost failure this role exists to
/// bound. It also does not carry the Orchestrator layer's own review-round
/// rules -- the Orchestrator owns review gates.
///
/// Issue #170: extended with the scope contract a work group actually
/// enforces (`group::WorkGroup`) -- own one area end to end, dispatch at the
/// cheapest fitting tier per child, only ever Workers, and report ONE
/// integrated result against the group's completion contract rather than
/// each child's own outcome individually. `--group` itself is never named
/// here as something to type: `agent::resolve_group_binding`'s env fallback
/// (`WORK_GROUP_ENV`) already binds every child this session spawns to the
/// same group without it needing to remember to pass one.
pub const SUB_ORCHESTRATOR_PROMPT: &str = "\
zirv sub-orchestrator conventions (claude)

You are a sub-orchestrator: you own ONE scope end to end, handed to you by an orchestrator as a \
work group with its own budget and completion contract. You do not decide which harnesses run.

- Split your scope into worker briefs and dispatch each with `zirv agent <name> \"<prompt>\" -- \
--model <m>`, naming the cheapest tier that can do that one brief -- not uniformly the same model \
for every child.
- Spawn only Workers. Do not spawn another sub-orchestrator or a dashboard coordinator: delegation \
stops at one level below you, and every child you dispatch inherits your own work group \
automatically, with no `--group` of its own to remember.
- Keep your own replies to decisions and outcomes, not implementation: do not read large files or \
write code yourself unless the change is trivial.
- When every child you dispatched is done, report ONE integrated result against your work group's \
completion contract -- not each child's own outcome individually -- including any failures.";

/// The raw text of a `tool_result` block's `content`, falling back to a
/// JSON-stringified form for a non-string (array/object) content shape.
/// Shared by `parse_events` (which only needs it long enough to hash it for
/// `NormalizedEvent::ToolErrorText`) and `structural_context` (which keeps a
/// human-readable snippet of it).
fn tool_result_text(block: &Value) -> String {
    block
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            block
                .get("content")
                .map(Value::to_string)
                .unwrap_or_default()
        })
}

fn text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The four raw token classes from one `message.usage` object. A missing
/// field is `0`, the same tolerance `context_tokens_of` has always had.
pub fn usage_categories(usage: &Value) -> TranscriptUsage {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    TranscriptUsage {
        input_tokens: field("input_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        output_tokens: field("output_tokens"),
    }
}

/// Real context size is `input_tokens` plus both cache fields; the bare
/// `input_tokens` field is near zero once prompt caching kicks in. Now a
/// DERIVED helper over [`usage_categories`] rather than the only thing that
/// survives the adapter boundary -- same signature, same value, so
/// `parse_events`' `AssistantFinal { input_tokens }` (which feeds rot's
/// context gate) is byte-for-byte unchanged.
pub fn context_tokens_of(usage: &Value) -> u64 {
    usage_categories(usage).context_total()
}

pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent> {
    let mut events = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        // Issue #293: every row carries its own top-level `timestamp`
        // (verified in `tests/fixtures/claude-real-session.jsonl`), read
        // once per line and reused for whichever event(s) that line
        // produces below. `None` -- never a guess -- for a line with no
        // parseable timestamp.
        let at_ms = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc_ms);

        if row.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let message = row.get("message").cloned().unwrap_or(Value::Null);
            events.push(NormalizedEvent::ProviderError {
                class: super::classify_provider_error(&text_of(&message)),
            });
            continue;
        }

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let results: Vec<&Value> = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| {
                                b.get("type").and_then(Value::as_str) == Some("tool_result")
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                if results.is_empty() {
                    events.push(NormalizedEvent::TurnStart { at_ms });
                    continue;
                }
                for block in results {
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    events.push(NormalizedEvent::ToolResult { is_error });
                    // Issue #293: a sibling event, never a new field on
                    // `ToolResult` -- see `ToolResultTimestamp`'s own doc
                    // comment for why, the same reasoning
                    // `ToolErrorText` (right below) already applies.
                    if at_ms.is_some() {
                        events.push(NormalizedEvent::ToolResultTimestamp { at_ms });
                    }
                    // Same-error repetition (issue: `rot::Signals::
                    // same_error_repeats`): a sibling event, never a new
                    // field on `ToolResult` -- see that variant's own doc
                    // comment for why.
                    if is_error {
                        let detail = tool_result_text(block);
                        if !detail.is_empty() {
                            events.push(NormalizedEvent::ToolErrorText {
                                hash: error_text_hash(&detail),
                            });
                        }
                    }
                }
            }
            Some("assistant") => {
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                if let Some(id) = message.get("model").and_then(Value::as_str) {
                    events.push(NormalizedEvent::ModelId { id: id.to_string() });
                }
                let input_tokens = message.get("usage").map(context_tokens_of).unwrap_or(0);
                let text = text_of(&message);
                // Issue #293: a CANDIDATE first-text point, per row rather
                // than tracked across the whole parse -- see
                // `NormalizedEvent::AssistantFirstText`'s own doc comment
                // for why this must stay line-local.
                if !text.trim().is_empty() {
                    events.push(NormalizedEvent::AssistantFirstText { at_ms });
                }
                events.push(NormalizedEvent::AssistantFinal {
                    text,
                    input_tokens,
                    at_ms,
                });

                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                        events.push(NormalizedEvent::ToolCall {
                            name,
                            input_hash: input_hash(&raw),
                            at_ms,
                        });
                    }
                }
            }
            Some("system")
                if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                events.push(NormalizedEvent::Compaction);
            }
            _ => {}
        }
    }

    events
}

/// The most recently observed `message.model` id in `jsonl`, scanned
/// newest-to-oldest so a live `/model` switch mid-session is reflected
/// rather than the session's original model. Every assistant row Claude Code
/// writes carries this field on its own `message` object (the same object
/// [`usage_categories`] already reads `usage` off of), so this needs no
/// separate transcript pass beyond the one `parse_events`/`transcript_usage`
/// already make over the same lines.
pub fn model_hint(jsonl: &str) -> Option<String> {
    for line in jsonl.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(model) = row
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
        {
            return Some(model.to_string());
        }
    }
    None
}

/// The shared fold behind [`transcript_usage`] and [`sidechain_transcript_usage`]:
/// every assistant row whose `isSidechain` flag matches `want_sidechain`,
/// summed into the four raw classes. One fold, two filters, so the main and
/// sidechain readers can never drift on what counts as an assistant usage
/// row.
fn fold_assistant_usage(jsonl: &str, want_sidechain: bool) -> Option<TranscriptUsage> {
    let mut usage = TranscriptUsage::default();
    let mut observed = false;
    for line in jsonl.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let is_sidechain = row.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if row.get("type").and_then(Value::as_str) != Some("assistant")
            || is_sidechain != want_sidechain
        {
            continue;
        }
        let Some(current) = row.get("message").and_then(|message| message.get("usage")) else {
            continue;
        };
        observed = true;
        let row = usage_categories(current);
        usage.input_tokens = usage.input_tokens.saturating_add(row.input_tokens);
        usage.cache_creation_input_tokens = usage
            .cache_creation_input_tokens
            .saturating_add(row.cache_creation_input_tokens);
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(row.cache_read_input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(row.output_tokens);
    }
    observed.then_some(usage)
}

pub fn transcript_usage(jsonl: &str) -> Option<TranscriptUsage> {
    fold_assistant_usage(jsonl, false)
}

/// The same fold as [`transcript_usage`], over the rows it deliberately
/// skips: `isSidechain == true` assistant turns, i.e. subagent work. `None`
/// when the transcript has no sidechain rows at all -- an honest "no data",
/// never a zeroed reading, the same distinction `transcript_usage`'s own
/// `observed` flag draws.
pub fn sidechain_transcript_usage(jsonl: &str) -> Option<TranscriptUsage> {
    fold_assistant_usage(jsonl, true)
}

const FILE_KEYS: &[&str] = &["file_path", "notebook_path", "path"];
/// Tool names whose file-key argument is a modification, not a read (issue
/// #280). Anything else -- `Read`/`Grep`/`Glob`, or a tool this codebase does
/// not recognise -- lands in `files_read` instead, the conservative
/// direction: claiming a file was edited when it was not is the damaging
/// error, never the reverse.
const MODIFICATION_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
const ERROR_SNIPPET: usize = 200;

/// Conservative context window (issue #155) for a Claude model id this
/// adapter does not recognise as a long-window seat, and for an unstated
/// model. Conservative on purpose: an overstated capacity raises the
/// restart ceiling past what the seat can actually hold, and a session that
/// overruns its window is a far worse outcome than one rotated slightly
/// early.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;

/// A long-window Claude seat (1M tokens) is spelled with a `[1m]` or `-1m`
/// marker in the model id in this environment.
const LONG_CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;

pub fn structural_context(jsonl: &str, last_n: usize) -> StructuralContext {
    let mut out = StructuralContext::default();
    // Handoff verification section: every Bash invocation's command text,
    // captured verbatim, keyed by its `tool_use` block id so the paired
    // `tool_result` (matched by `tool_use_id`, however many other tool
    // calls fall between them) is attributed to the right command rather
    // than whichever result happens to come next. Never exposed on
    // `StructuralContext` itself -- only the derived
    // `event::last_verification_run` over it is.
    let mut pending_bash: HashMap<String, String> = HashMap::new();
    let mut invocations: Vec<ToolInvocation> = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = row.get("message").cloned().unwrap_or(Value::Null);

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let content = message.get("content");
                if let Some(text) = content.and_then(Value::as_str) {
                    out.user_messages.push(text.to_string());
                    continue;
                }
                let Some(blocks) = content.and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.user_messages.push(text.to_string());
                            }
                        }
                        Some("tool_result") => {
                            let is_error =
                                block.get("is_error").and_then(Value::as_bool) == Some(true);
                            let detail = tool_result_text(block);
                            if is_error {
                                out.tool_errors
                                    .push(detail.chars().take(ERROR_SNIPPET).collect());
                            }
                            if let Some(command) = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .and_then(|id| pending_bash.remove(id))
                            {
                                invocations.push(ToolInvocation {
                                    command,
                                    is_error,
                                    error_text: if is_error { detail } else { String::new() },
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                let text = text_of(&message);
                if !text.trim().is_empty() {
                    out.assistant_texts.push(text);
                }
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let Some(input) = block.get("input") else {
                        continue;
                    };
                    let tool_name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let is_modification = MODIFICATION_TOOLS
                        .iter()
                        .any(|t| tool_name.eq_ignore_ascii_case(t));
                    let target = if is_modification {
                        &mut out.files_modified
                    } else {
                        &mut out.files_read
                    };
                    for key in FILE_KEYS {
                        if let Some(path) = input.get(*key).and_then(Value::as_str)
                            && !target.iter().any(|p| p == path)
                        {
                            target.push(path.to_string());
                        }
                    }
                    let is_bash = tool_name.eq_ignore_ascii_case("Bash");
                    if is_bash
                        && let (Some(id), Some(command)) = (
                            block.get("id").and_then(Value::as_str),
                            input.get("command").and_then(Value::as_str),
                        )
                    {
                        pending_bash.insert(id.to_string(), command.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    out.last_verification = last_verification_run(&invocations);

    keep_last(&mut out.user_messages, last_n);
    keep_last(&mut out.assistant_texts, last_n);
    keep_last(&mut out.tool_errors, last_n);
    // Capped with everything else rather than left to accumulate: each is a
    // deduplicated list of every path the whole session ever named that way,
    // and it leaves as a single argv token in a handoff. Windows caps a
    // command line at 32,767 characters, so an uncapped list is a long
    // session that can no longer relaunch at all.
    keep_last(&mut out.files_read, last_n);
    keep_last(&mut out.files_modified, last_n);
    out
}

fn keep_last(items: &mut Vec<String>, last_n: usize) {
    if items.len() > last_n {
        items.drain(..items.len() - last_n);
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    #[cfg(test)]
    forced_file_support: Option<bool>,
    #[cfg(test)]
    forced_launch_settings: Option<Option<PathBuf>>,
}

impl ClaudeAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env claude"` both work. The first token is the program and the
    /// rest lead every command this adapter builds.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("claude").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "claude".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_file_support: None,
            #[cfg(test)]
            forced_launch_settings: Some(Some(PathBuf::from(
                "zirv-test-claude-launch-settings.json",
            ))),
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Test seam: bypasses the real `--help` probe so a unit test can force
    /// file-based or argv-based delivery without depending on the machine's
    /// installed binary.
    #[cfg(test)]
    pub fn with_file_support_forced(mut self, supported: bool) -> Self {
        self.forced_file_support = Some(supported);
        self
    }

    /// Test seam: avoids touching the developer's real home while pinning
    /// successful and failed settings-file materialization deterministically.
    #[cfg(test)]
    pub fn with_launch_settings_forced(mut self, path: Option<PathBuf>) -> Self {
        self.forced_launch_settings = Some(path);
        self
    }

    /// Test seam: exercises the real private-file writer under `with_home`.
    #[cfg(test)]
    fn with_live_launch_settings(mut self) -> Self {
        self.forced_launch_settings = None;
        self
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations,
    /// and so the Windows launcher rewrite (an npm-installed `claude` is
    /// `claude.cmd`, which `CreateProcess` refuses) is applied in exactly one
    /// place. A program zirv cannot resolve is spawned as written, which is
    /// today's behavior; `ready()` is where an unrunnable one is reported.
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

    /// Materializes the per-launch safety layer under the operator-owned
    /// Zirv home. The write is atomic and private on Unix; if either step
    /// fails, the caller deliberately falls back to Claude's native prompt
    /// flow without adding a blanket Bash allow.
    fn launch_settings_path(&self, safety: &super::super::safety::SafetyPolicy) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(forced) = &self.forced_launch_settings {
            return forced.clone();
        }

        let dir = self.home_dir().join(".zirv").join("runtime");
        let fingerprint = super::super::safety::policy_fingerprint(safety).ok()?;
        let policy_dir = dir.join("policies");
        let policy_path = policy_dir.join(format!("{fingerprint}.json"));
        let path = dir.join(format!("claude-launch-settings-{fingerprint}.json"));
        let launch_environment = LaunchEnvironment::resolve();
        let result = (|| -> std::io::Result<()> {
            super::super::state::create_private_dir_all(&dir)?;
            super::super::state::create_private_dir_all(&policy_dir)?;
            let mut policy_body =
                serde_json::to_string_pretty(safety).map_err(std::io::Error::other)?;
            policy_body.push('\n');
            super::super::state::write_private(&policy_path, &policy_body)?;
            let settings = launch_settings_value(safety, &policy_path, &launch_environment)
                .map_err(std::io::Error::other)?;
            let mut body =
                serde_json::to_string_pretty(&settings).map_err(std::io::Error::other)?;
            body.push('\n');
            super::super::state::write_private(&path, &body)
        })();
        match result {
            Ok(()) => Some(path),
            Err(error) => {
                warn_launch_settings_once(&path, &error);
                None
            }
        }
    }
}

/// A launch-local settings layer is stronger than relying on a one-time
/// `zirv setup apply`: every process Zirv starts attests the classifier it is
/// using, and a later reset or minimal Claude profile cannot silently remove
/// it. The operator's ordinary settings remain in force for keys omitted
/// here; Claude merges hook arrays across settings levels and applies the
/// most restrictive PreToolUse verdict (`deny > ask > allow`) among hooks.
///
/// Issue #147: this layer deliberately carries NO native
/// `permissions.ask`/`permissions.deny` rule naming
/// `Bash(dangerouslyDisableSandbox:true)`. One used to sit here; it is
/// documented behavior (code.claude.com/docs/en/permissions, "Extend
/// permissions with hooks") that a native settings rule is evaluated
/// independently of a PreToolUse hook's own decision -- a settings `ask`
/// rule still prompts even when the hook returns `allow`. That made every
/// hook-side `allow` for a sandbox-escape retry a no-op, including the
/// existing read-only-`gh` carve-out and the new `[safety] escape_allow`
/// gate (`safety::run_check_hook_mode_with_env`): an operator who pre-
/// cleared a family kept getting re-prompted on every repeat regardless.
/// The attested, fail-closed safety hook remains the final zirv-side decision
/// point for an escape; the operator's own native rules, if any, still apply
/// on top, per the same documented precedence.
///
/// Reserved Zirv built-ins and the explicit command-family table below form
/// the native projection. PreToolUse still evaluates every invocation before
/// execution, so dangerous `gh` and push forms and repo `deny`/`ask` rules
/// continue to narrow the broad native families.
struct CommandFamilyProjection {
    pattern: &'static str,
    sandbox_excluded: bool,
}

const PROMPT_FREE_COMMAND_FAMILIES: &[CommandFamilyProjection] = &[
    CommandFamilyProjection {
        pattern: "gh *",
        sandbox_excluded: true,
    },
    // Issue #329: a GitLab-first shop routes every review through `glab`, and
    // the forge CLI needs its own credential config plus network egress the
    // sandbox denies -- exactly the `gh` situation, so it gets the identical
    // treatment. The classifier still denies the destructive `glab` forms
    // (`safety::publish_or_destructive_action`) ahead of this native family,
    // the same way it narrows `gh *`.
    CommandFamilyProjection {
        pattern: "glab *",
        sandbox_excluded: true,
    },
    CommandFamilyProjection {
        pattern: "git push *",
        sandbox_excluded: true,
    },
    CommandFamilyProjection {
        pattern: "git worktree *",
        sandbox_excluded: false,
    },
];

/// Issue #329: `denyRead` blanks all of `~/.ssh`, which also hid the two
/// NON-secret files ssh must read to work at all -- `known_hosts` (host
/// verification) and `config` (host aliases, `IdentityAgent`). A sandboxed
/// `git fetch/push` therefore could not verify a host and died before it
/// ever reached authentication. Claude resolves overlapping read rules by
/// specificity ("the more specific path wins"), so naming these two files in
/// `allowRead` re-opens exactly them while every private key under `~/.ssh`
/// stays denied by the broader `denyRead` entry.
///
/// Deliberately NOT added to `safety::SANDBOX_DENY_READ_HOME_PATHS`'s own
/// derived credential screen: that screen guards what an UNSANDBOXED retry
/// may read, and these two files are not secrets.
#[cfg_attr(windows, allow(dead_code))]
const SSH_NON_SECRET_READ_PATHS: &[&str] = &["~/.ssh/known_hosts", "~/.ssh/config"];

#[derive(Debug, Default)]
struct LaunchEnvironment {
    // Native Windows has no OS sandbox, so the `#[cfg(not(windows))]`
    // filesystem block that reads this is compiled out there.
    #[cfg_attr(windows, allow(dead_code))]
    state_write_root: Option<PathBuf>,
    scratchpad_roots: Vec<String>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    unix_sockets: Vec<String>,
    ssh_auth_sock: Option<String>,
    /// Issue #329: the launch repository's own linked worktrees. They were
    /// already handed to Claude as `--add-dir` working directories, but a
    /// working directory is not a WRITE grant: the OS sandbox still refused
    /// every write under them, so ordinary gates in a worktree (`git status`
    /// writing `.git/FETCH_HEAD`, a test runner's cache, a commit) failed
    /// with `Operation not permitted` and had to be retried unsandboxed.
    /// Emitted into BOTH `sandbox.filesystem.allowWrite` and
    /// `permissions.additionalDirectories` as literal paths -- never globs,
    /// since `additionalDirectories` does not support them on any platform
    /// and `allowWrite` silently drops glob entries on Linux/WSL2.
    #[cfg_attr(windows, allow(dead_code))]
    workspace_write_roots: Vec<String>,
}

impl LaunchEnvironment {
    fn resolve() -> Self {
        let state_write_root =
            super::super::state::StateDir::resolve(&super::super::config::env_from_process())
                .ok()
                .map(|state| state.root().to_path_buf());
        let scratchpad_roots = super::scratchpad_roots(&std::env::temp_dir());
        let mut unix_sockets = resolve_docker_socket_paths(
            Path::new("/var/run/docker.sock"),
            std::env::var("DOCKER_HOST").ok().as_deref(),
        );
        let ssh_auth_sock = resolve_ssh_auth_sock();
        if let Some(socket) = &ssh_auth_sock
            && !unix_sockets.contains(socket)
        {
            unix_sockets.push(socket.clone());
        }
        #[cfg(not(test))]
        let workspace_write_roots = std::env::current_dir()
            .ok()
            .map(|repo| linked_worktree_roots(&repo))
            .unwrap_or_default()
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        #[cfg(test)]
        let workspace_write_roots = Vec::new();

        Self {
            state_write_root,
            scratchpad_roots,
            unix_sockets,
            ssh_auth_sock,
            workspace_write_roots,
        }
    }
}

fn resolve_docker_socket_paths(docker_socket: &Path, docker_host: Option<&str>) -> Vec<String> {
    let mut sockets = Vec::new();
    let mut push_existing = |path: &Path| {
        if path.exists() {
            let path = path.display().to_string();
            if !sockets.contains(&path) {
                sockets.push(path);
            }
        }
    };

    push_existing(docker_socket);
    if std::fs::symlink_metadata(docker_socket).is_ok_and(|meta| meta.file_type().is_symlink())
        && let Ok(target) = std::fs::canonicalize(docker_socket)
    {
        push_existing(&target);
    }
    if let Some(path) = docker_host.and_then(|host| host.strip_prefix("unix://"))
        && !path.is_empty()
    {
        push_existing(Path::new(path));
    }
    sockets
}

fn resolve_ssh_auth_sock() -> Option<String> {
    let process_value = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|path| !path.is_empty() && Path::new(path).exists());
    #[cfg(target_os = "macos")]
    let value = process_value
        .or_else(|| crate::commands::workflow::verification::launchd_getenv("SSH_AUTH_SOCK"));
    #[cfg(not(target_os = "macos"))]
    let value = process_value;

    value.filter(|path| !path.is_empty() && Path::new(path).exists())
}

fn launch_settings_value(
    safety: &super::super::safety::SafetyPolicy,
    policy_path: &Path,
    launch_environment: &LaunchEnvironment,
) -> Result<Value, serde_json::Error> {
    let fingerprint = super::super::safety::policy_fingerprint(safety)?;
    let reserved_zirv_patterns = super::super::safety::reserved_zirv_command_patterns();
    let mut reserved_zirv_permission_rules: Vec<String> = reserved_zirv_patterns
        .iter()
        .map(|pattern| format!("Bash({pattern})"))
        .collect();
    reserved_zirv_permission_rules.extend(
        PROMPT_FREE_COMMAND_FAMILIES
            .iter()
            .map(|family| format!("Bash({})", family.pattern)),
    );
    let mut sandbox_exclusions = super::super::safety::reserved_zirv_sandbox_exclusion_patterns();
    sandbox_exclusions.extend(
        PROMPT_FREE_COMMAND_FAMILIES
            .iter()
            .filter(|family| family.sandbox_excluded)
            .map(|family| family.pattern.to_string()),
    );
    #[cfg_attr(windows, allow(unused_mut))]
    let mut settings = serde_json::json!({
        "disableAllHooks": false,
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash|PowerShell",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx safety check"
                }]
            }],
            "PermissionRequest": [{
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook permission"
                }]
            }],
            "PermissionDenied": [{
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook permission"
                }]
            }]
        },
        "permissions": {
            "allow": reserved_zirv_permission_rules,
            "deny": [
                "Read(~/.ssh/**)",
                "Read(~/.aws/**)",
                "Read(~/.azure/**)",
                "Read(~/.config/gcloud/**)",
                "Read(~/.config/gh/hosts.yml)",
                "Read(~/.kube/config)",
                "Read(~/.docker/config.json)",
                "Read(~/.npmrc)",
                "Read(~/.pypirc)",
                "Read(~/.netrc)",
                "Read(~/.git-credentials)"
            ]
        },
        "env": {
            "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB": "1",
            super::super::safety::POLICY_FINGERPRINT_ENV: fingerprint,
            super::super::safety::POLICY_SNAPSHOT_ENV: policy_path.display().to_string()
        }
    });

    let additional_directories: Vec<&String> = launch_environment
        .scratchpad_roots
        .iter()
        .chain(launch_environment.workspace_write_roots.iter())
        .collect();
    if !additional_directories.is_empty() {
        settings["permissions"]["additionalDirectories"] =
            serde_json::json!(additional_directories);
    }
    if let Some(socket) = &launch_environment.ssh_auth_sock {
        settings["env"]["SSH_AUTH_SOCK"] = serde_json::json!(socket);
    }

    // Claude's OS sandbox is currently supported on macOS, Linux and WSL2,
    // but not native Windows. On supported hosts it is the hard containment
    // boundary beneath Zirv's semantic classifier: compatible Bash commands
    // need no prompt, initialization fails closed, and an incompatible
    // command may leave the sandbox only through the safety hook's own
    // escape gate above.
    #[cfg(not(windows))]
    if let Some(object) = settings.as_object_mut() {
        let mut filesystem = serde_json::json!({
            "denyRead": super::super::safety::SANDBOX_DENY_READ_HOME_PATHS,
            "allowRead": SSH_NON_SECRET_READ_PATHS
        });
        // Sandbox-confined Zirv built-ins need the exact platform state root;
        // the immutable policy snapshot is operator-owned launch state and is
        // not added separately by this rule.
        let allow_write: Vec<String> = launch_environment
            .state_write_root
            .iter()
            .map(|root| root.display().to_string())
            .chain(launch_environment.workspace_write_roots.iter().cloned())
            .collect();
        if !allow_write.is_empty() {
            filesystem["allowWrite"] = serde_json::json!(allow_write);
        }
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut sandbox = serde_json::json!({
            "enabled": true,
            "autoAllowBashIfSandboxed": true,
            "allowUnsandboxedCommands": true,
            "excludedCommands": sandbox_exclusions,
            "failIfUnavailable": true,
            "filesystem": filesystem
        });
        #[cfg(target_os = "macos")]
        if !launch_environment.unix_sockets.is_empty() {
            sandbox["network"] = serde_json::json!({
                "allowUnixSockets": &launch_environment.unix_sockets
            });
        }
        object.insert("sandbox".to_string(), sandbox);
    }

    Ok(settings)
}

fn warn_launch_settings_once(path: &Path, error: &std::io::Error) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "zirv: warning: could not attest Claude safety settings at {}: {error}; \
             falling back to native permission prompts without widening Bash",
            path.display()
        );
    }
}

const MAX_LINKED_WORKTREES: usize = 16;
#[cfg(not(test))]
const WORKTREE_LIST_TIMEOUT: Duration = Duration::from_secs(3);

fn parse_worktree_porcelain(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// The external, capped worktree set shared by the `--add-dir` projection
/// and (issue #329) the launch settings' own write grants, so a worktree can
/// never be a working directory Claude may read but not write.
fn additional_worktree_roots(
    canonical_repo: &Path,
    canonical_worktrees: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    canonical_worktrees
        .into_iter()
        .filter(|path| !path.starts_with(canonical_repo))
        .take(MAX_LINKED_WORKTREES)
        .collect()
}

/// Adds linked worktrees belonging to the launch repository to Claude's
/// working-directory set. Other repositories remain out of scope; callers
/// use `zirv agent --workdir` when a cross-repository target is intentional.
/// Discovery is best-effort, bounded, and never invokes a shell.
#[cfg(not(test))]
fn linked_worktree_args(repo: &Path) -> Vec<String> {
    linked_worktree_roots(repo)
        .into_iter()
        .flat_map(|path| ["--add-dir".to_string(), path.display().to_string()])
        .collect()
}

/// The discovery half of [`linked_worktree_args`], shared with
/// [`LaunchEnvironment::resolve`] so the same bounded set that becomes a
/// working directory also becomes a sandbox write grant (issue #329).
#[cfg(not(test))]
fn linked_worktree_roots(repo: &Path) -> Vec<PathBuf> {
    let Ok(canonical_repo) = std::fs::canonicalize(repo) else {
        return Vec::new();
    };
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(&canonical_repo)
        .args(["worktree", "list", "--porcelain"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };

    let mut stdout_pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = String::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_string(&mut output);
        }
        let _ = tx.send(output);
    });

    let deadline = Instant::now() + WORKTREE_LIST_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let Ok(output) = rx.recv_timeout(Duration::from_secs(1)) else {
                    return Vec::new();
                };
                let worktrees = parse_worktree_porcelain(&output)
                    .into_iter()
                    .filter_map(|path| std::fs::canonicalize(path).ok());
                return additional_worktree_roots(&canonical_repo, worktrees);
            }
            Ok(Some(_)) => return Vec::new(),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Vec::new()
}

/// Bounds the `--help` probe below: a hang here must never hang the whole
/// launch, which would be a worse failure mode than falling back to argv.
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Process-wide cache of `probe_system_prompt_file_support`'s answer, keyed
/// by the exact program invocation: the binary path plus its leading
/// arguments, since `ZIRV_CTX_AGENT_BIN` can point at different binaries (or
/// different versions of the same name resolved from a different PATH) and
/// each has its own answer. A restart inside the same `wrap`/`exec` run must
/// not re-spawn `--help` on every relaunch, which is what the cache is for;
/// it must not, in exchange, let one binary's probe answer for another's.
/// A tuple key rather than a joined string: joining `program` and `bin_args`
/// with spaces makes `("sh /tmp/x", ["--help"])` and `("sh", ["/tmp/x",
/// "--help"])` collide on the same string despite being different commands.
type ProbeKey = (PathBuf, Vec<String>);
static SYSTEM_PROMPT_FILE_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();

/// Probes the installed binary's own `--help` text for the file-based
/// system-prompt flag, so injection can move the composed prompt off argv
/// (visible to any other user on the machine via `ps`) without hard-coding a
/// version cutoff. Any failure to run or read the probe (binary missing,
/// timeout, whatever) is read as unsupported: this is a hardening on top of
/// argv delivery, never a new way to fail a launch.
fn probe_system_prompt_file_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = SYSTEM_PROMPT_FILE_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_help_flag(program, bin_args);
    map.insert(key, detected);
    detected
}

/// Verified against the real CLI (`claude --help`, v2.1.220): the flag is not
/// spelled out on its own line. It only appears folded into a shorthand,
/// `--append-system-prompt[-file]`, inside the `--bare` option's own
/// description ("Explicitly provide context via: --system-prompt[-file],
/// --append-system-prompt[-file], --add-dir ..."). Stripping `[` and `]`
/// before searching turns that shorthand into the plain flag text, and is a
/// no-op if some future help output ever spells the flag out on its own.
fn normalizes_to_advertise_the_file_flag(help_text: &str) -> bool {
    help_text
        .replace(['[', ']'], "")
        .contains("--append-system-prompt-file")
}

/// Runs `program --help` and reports whether its output names
/// `--append-system-prompt-file`. Stdin is nulled so an interactive TUI that
/// does not special-case `--help` (and just starts reading a line) gets an
/// immediate EOF instead of hanging the probe; stdout is drained on a
/// separate thread so a chatty `--help` cannot deadlock against the wait
/// loop by filling the pipe buffer.
fn detect_help_flag(program: &str, bin_args: &[String]) -> bool {
    // The same resolution the launch itself uses. Without it the probe and
    // the spawn disagree on Windows: `Command::new` only ever appends `.exe`,
    // so an npm-installed `claude.cmd` failed the probe here and then failed
    // the launch there, for two different reasons.
    let resolved =
        super::resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));

    // SECURITY (FINDING 1): `bin_args` carries repo-controlled tokens on the
    // interactive path (`program_invocation` forwards every positional before
    // the first flag, e.g. `zirv chat --resume`'s handoff summary). When
    // `resolve_program` routes an npm-installed `claude.cmd` through
    // `cmd.exe /c <shim>`, cmd.exe reparses this whole probe command line, so a
    // metacharacter in `bin_args` would execute *here*, before the real launch
    // ever reaches its own `guard_cmd_shim_reparse`. Run the identical
    // fail-closed guard against the exact argv about to be spawned, and on a
    // rejection report "unsupported" (the same value every probe failure
    // yields) WITHOUT spawning -- the caller keeps argv delivery and the
    // payload is never executed.
    let mut probe_args: Vec<String> =
        Vec::with_capacity(resolved.prefix.len() + bin_args.len() + 1);
    probe_args.extend(resolved.prefix.iter().cloned());
    probe_args.extend(bin_args.iter().cloned());
    probe_args.push("--help".to_string());
    if super::guard_cmd_shim_reparse(&resolved.program, &probe_args).is_err() {
        return false;
    }

    let Ok(mut child) = Command::new(&resolved.program)
        .args(&resolved.prefix)
        .args(bin_args)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let mut stdout_pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + HELP_PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return normalizes_to_advertise_the_file_flag(&text);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Claude stores transcripts under a slug of the cwd with every character
/// outside `[A-Za-z0-9-]` replaced by `-`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Claude Code's subscription windows are Anthropic's, and the account is
    /// what the limit belongs to: a different Anthropic-backed harness would
    /// answer `"anthropic"` here too and share these readings.
    fn provider(&self) -> &'static str {
        "anthropic"
    }

    /// The one thing that can make this adapter unusable before it is asked
    /// to do anything: a program that resolves to a file this OS has no way
    /// to execute. Reported here, by name, rather than left to surface as a
    /// raw `os error 193` out of the spawn. A program that resolves to
    /// nothing at all is not an error here: that is the OS's own
    /// "not found", raised at spawn time where it has always been raised.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "claude")
            .unwrap_or(false)
    }

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    fn headless_resume_cmd(
        &self,
        prompt: Option<&str>,
        session_id: &str,
        extra: &[String],
    ) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("-p");
        if let Some(prompt) = prompt {
            cmd.arg(prompt);
        }
        cmd.arg("--resume").arg(session_id).args(extra);
        Some(cmd)
    }

    fn supports_headless_compact(&self) -> bool {
        true
    }

    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        if prompt.trim().is_empty() {
            return Vec::new();
        }
        vec!["--append-system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt")
    }

    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt-file")
    }

    fn base_system_prompt(&self) -> Option<&'static str> {
        Some(ORCHESTRATOR_PROMPT)
    }

    fn worker_system_prompt(&self) -> Option<&'static str> {
        Some(WORKER_PROMPT)
    }

    fn sub_orchestrator_system_prompt(&self) -> Option<&'static str> {
        Some(SUB_ORCHESTRATOR_PROMPT)
    }

    /// Counted over the argv the operator wrote, not over the argv `base()`
    /// builds: `exec` uses this to strip the program tokens off the command
    /// it was handed before carrying the rest into a restart. The Windows
    /// launcher rewrite lives entirely inside `base()` and never touches that
    /// argv, so the prefix stays the program plus its own leading arguments.
    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_file_support {
            return forced;
        }
        // The binary that is about to run, not the one this adapter would
        // have chosen: wrap spawns the user's own argv.
        let (program, args) = super::program_invocation(launch)
            .unwrap_or_else(|| (self.program.clone(), self.bin_args.clone()));
        probe_system_prompt_file_support(&program, &args)
    }

    /// True on a Windows npm install, where `claude` is a `.cmd` shim that
    /// [`super::resolve_program`] routes through `cmd.exe /c`. That is the one
    /// launch shape where a headless prompt on argv would be reparsed by
    /// cmd.exe, so on it the prompt is delivered via stdin instead
    /// (`headless_cmd_stdin`).
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// The `-p` headless launch with **no positional prompt**: claude then
    /// reads the prompt from stdin (verified by the distiller, which does
    /// exactly this). Everything else matches `headless_cmd`, so a stdin
    /// launch and an argv launch differ only in where the prompt travels.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        Some(cmd)
    }

    /// The distillation prompt is piped to stdin so a long transcript tail
    /// never hits argv length limits. This child embeds untrusted repo
    /// CLAUDE.md text in its prompt (the judgment call) and its only job is
    /// to answer with text, so it never needs a tool. Verified against the
    /// real CLI (docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md,
    /// "I6 fix round"): `Bash` must be denied alongside `Write`/`Edit`, since
    /// a shell redirect otherwise recreates a Write tool, and the value must
    /// be one `=`-bound argv token, since the two-token form was verified to
    /// swallow the next argv entry.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--model")
            .arg(model)
            .arg("--output-format")
            .arg("text")
            .args(self.read_only_args());
        cmd
    }

    fn read_only_args(&self) -> Vec<String> {
        vec!["--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string()]
    }

    /// A real, verified cheap-model name for claude's own lineup -- the
    /// value `handoff.model`/`optimize.model` defaulted to before it became
    /// per-adapter (see `resolve_distiller_model` in `handoff.rs`). Specific
    /// to claude by construction: a hardcoded model name from one agent's
    /// lineup has no business leaking into another adapter's default.
    fn default_distiller_model(&self) -> Option<&'static str> {
        Some("haiku")
    }

    /// Claude's one verified per-run enforcement mechanism is the same
    /// `--disallowedTools=...` pin `distiller_cmd` above already relies on,
    /// probed against the real CLI (docs/superpowers/notes/2026-08-01-system-
    /// prompt-injection-facts.md). It names exactly four *tools*
    /// (`Write`/`Edit`/`Bash`/`NotebookEdit`), so it fully enforces exactly
    /// the two capabilities that pin denies outright:
    ///
    /// - **Repo filesystem writes** and **shell execution** -- `Write`/`Edit`
    ///   and `Bash` are two of the four denied tools, so a `Deny` stance is
    ///   `Enforced`.
    ///
    /// It answers everything else only partially or not at all:
    ///
    /// - **MCP/tool access** is `Degraded`, not `Enforced`: the pin denies
    ///   exactly `Write`/`Edit`/`Bash`/`NotebookEdit`, but `Read`, `Grep`,
    ///   `WebFetch`, `WebSearch`, `Task`, and every MCP server's own tools
    ///   remain available. Claiming `Enforced` here would claim a full tool
    ///   deny the pin does not deliver (docs/superpowers/notes/2026-08-01-
    ///   system-prompt-injection-facts.md:153).
    /// - **Approval** is `Unsupported`: the pin does not address approvals at
    ///   all. `WebFetch` domain approval and MCP tool approvals still prompt
    ///   even with all four tools denied, and `--permission-mode plan` was
    ///   probed and does not resolve in headless `-p` mode.
    /// - **Network** has no verified per-run flag at all.
    /// - **git push / destructive git** is reachable only through `Bash`, so
    ///   the only pin available denies *every* shell command, not git's --
    ///   over-broad enforcement that would break an ordinary session while
    ///   claiming to implement a git policy. Reported unsupported rather than
    ///   degraded so Task 14 cannot read this as "pin `--disallowedTools`".
    /// - **Writes outside the repo** are the same shape: no verified flag
    ///   scopes writes by path, and the available pin denies writes
    ///   everywhere, in-repo included.
    ///
    /// Interactive `Ask` reports as `Degraded` only where zirv now pins the
    /// default permission mode and its own safety-hook/path allow-list seam;
    /// headless `Ask` remains operator-controlled because `dontAsk` cannot
    /// carry that prompt posture.
    fn policy_support(
        &self,
        capability: crate::commands::ctx::policy::Capability,
        stance: crate::commands::ctx::policy::Stance,
        mode: super::LaunchMode,
    ) -> crate::commands::ctx::policy::CapabilityDescriptor {
        use crate::commands::ctx::policy::{Capability, CapabilityDescriptor, Stance};

        const TOOL_PIN: &str = "--disallowedTools=Write,Edit,Bash,NotebookEdit";
        const TOOL_PIN_PARTIAL: &str = "--disallowedTools=Write,Edit,Bash,NotebookEdit denies exactly those four \
             tools; Read, Grep, WebFetch, WebSearch, Task and every MCP server's own tools \
             remain available";
        const APPROVAL_UNSUPPORTED: &str = "the tool pin does not address approvals at all: WebFetch domain approval and \
             MCP tool approvals still prompt; `--permission-mode plan` was probed and does not \
             resolve in headless `-p` mode";
        const SETTINGS: &str = "claude's own permission prompts and `.claude/settings.json` permissions, which zirv \
             reads and never rewrites";
        // 2026-08-24: an INTERACTIVE launch carries `--permission-mode
        // default` plus the `zirv ctx safety check` PreToolUse hook as the
        // sole prompting gate. That is a real, verified per-run mechanism, so
        // an `Ask` stance stops being purely operator-controlled -- but only
        // `Degraded`: the hook is registered for the `Bash` tool alone, so
        // every other tool still lands on claude's own settings.
        //
        // KNOWN RESIDUAL (2026-08-24, filed rather than guessed at): this
        // `Degraded` claim assumes `launch_settings_path` actually wrote the
        // per-launch settings file this description promises. That write is
        // best-effort (`launch_settings_path`'s own doc comment) -- if it
        // fails, THIS launch has no hook and no deny at all, yet
        // `policy_support` is a static descriptor with no per-launch
        // success/failure to consult, so it still reports `Degraded` here.
        // Closing this needs either threading the real write outcome into
        // `policy_support` (a signature change reaching every caller) or an
        // argv-based fallback that does not depend on writing a file at
        // all; both are out of scope for this pass, so the gap is
        // documented rather than silently left implied-fixed.
        const ASK_INTERACTIVE: &str = "--permission-mode default plus the `zirv ctx safety check` PreToolUse hook as the \
             sole prompting gate, which allows everyday and unclassified commands outright and \
             prompts only on zirv's own short dangerous-command list; the hook matches the Bash \
             tool only, so every other tool still falls to claude's own settings";
        const OUTSIDE_REPO_ASK_INTERACTIVE: &str = "--permission-mode default with --allowedTools scoped to Edit(./**) plus the \
             workspace scratchpad: a write outside those paths is not pre-approved, so claude \
             prompts rather than failing silently";

        match capability {
            Capability::RepoFsWrite | Capability::ShellExec => match stance {
                Stance::Deny => CapabilityDescriptor::enforced(TOOL_PIN),
                Stance::Ask if mode.is_interactive() => {
                    CapabilityDescriptor::degraded(ASK_INTERACTIVE)
                }
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
            },
            Capability::ToolAccess => match stance {
                Stance::Deny => CapabilityDescriptor::degraded(TOOL_PIN_PARTIAL),
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
            },
            Capability::Approval => match stance {
                Stance::Deny => CapabilityDescriptor::unsupported(APPROVAL_UNSUPPORTED),
                Stance::Ask if mode.is_interactive() => {
                    CapabilityDescriptor::degraded(ASK_INTERACTIVE)
                }
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
            },
            Capability::OutsideRepoFsWrite => match stance {
                Stance::Ask if mode.is_interactive() => {
                    CapabilityDescriptor::degraded(OUTSIDE_REPO_ASK_INTERACTIVE)
                }
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
                Stance::Deny => CapabilityDescriptor::advisory_only(),
            },
            Capability::Network | Capability::GitPushDestructive => {
                CapabilityDescriptor::advisory_only()
            }
        }
    }

    /// The one stance this adapter has a verified per-run mechanism for
    /// (`policy_support` above): `RepoFsWrite`/`ShellExec` at `Deny` gets the
    /// exact same `--disallowedTools=...` pin `read_only_args`/
    /// `distiller_cmd` already use -- reusing `self.read_only_args()`
    /// directly rather than a second literal keeps the two from ever drifting
    /// on the exact flag spelling the "I6 fix round" verified matters (see
    /// `distiller_cmd`'s own doc comment). Every other stance is
    /// `OperatorControlled` per `policy_support` and stays untouched: the
    /// shipped default (`EffectivePolicy::default()`, all `Allow`) returns
    /// empty, so a launch with no `[policy]` configured is byte-for-byte
    /// unaffected.
    fn policy_args(
        &self,
        policy: &crate::commands::ctx::policy::EffectivePolicy,
        mode: super::LaunchMode,
    ) -> Vec<String> {
        use crate::commands::ctx::policy::Stance;
        let _ = mode;
        if policy.repo_fs_write == Stance::Deny || policy.shell_exec == Stance::Deny {
            self.read_only_args()
        } else {
            Vec::new()
        }
    }

    /// The claude side of the shipped-default "sandboxed, no prompts"
    /// posture (2026-08-22) -- verified against the actually-installed
    /// `claude 2.1.240` (`claude --help`, and confirmed at runtime against a
    /// real authenticated `-p` launch; both quoted in full in the
    /// 2026-08-22 addendum below and in [[Ctx Adapters]]). Claude has **no**
    /// real sandbox mechanism analogous to codex's `--sandbox
    /// workspace-write`: there is no flag that scopes writes/execution to
    /// the workspace while still allowing them freely. The two candidates
    /// that came closest were probed for real, not guessed:
    ///
    /// - `--dangerously-skip-permissions`/`bypassPermissions` removes the
    ///   permission system entirely -- explicitly excluded, per this fix's
    ///   own hard constraint: it satisfies "no prompts" only by also
    ///   satisfying "dangerous commands run", which this posture must never
    ///   do.
    /// - `--permission-mode acceptEdits` was probed live in headless `-p`
    ///   mode and, with no TTY to prompt through, silently **allowed**
    ///   both a `Write` and a destructive `rm <file>` `Bash` call with no
    ///   denial and no prompt -- effectively as permissive as the bypass
    ///   flag above in this launch shape. Disqualified for the same
    ///   reason.
    ///
    /// `--permission-mode dontAsk` is the one verified-safe match: probed
    /// live, it silently **denies** `Write`/`Bash` calls that are not
    /// pre-approved (`.claude/settings.json`'s own `permissions.allow`,
    /// which zirv reads and never writes) rather than prompting *or*
    /// running them, and its own embedded `--help` text (extracted from the
    /// installed binary) confirms this by design: `"'dontAsk' - Don't
    /// prompt for permissions, deny if not pre-approved."` This closes both
    /// halves of the posture's hard requirement (no prompts, nothing
    /// dangerous auto-runs), but `dontAsk` **alone**, with no pre-approved
    /// rules, is not "runs freely inside the workspace" -- it is inert: a
    /// legitimate in-repo `Write`/`Edit`/`Bash` action is denied outright.
    ///
    /// **Fix round 2 (2026-08-22): `SHIPPED_POSTURE_ALLOW`/`_DENY`**
    /// (`adapters/mod.rs`) is what makes `dontAsk` usable rather than merely
    /// safe -- generated `--allowedTools=...`/`--disallowedTools=...` argv,
    /// derived from that one shared list so this and codex's own posture
    /// cannot independently drift. Passed at launch, never written to
    /// `.claude/settings.json`: the operator's own file is untouched, and
    /// their own `permissions.allow`/`deny` there still governs anything
    /// this list is silent on. Verified live against the installed `claude
    /// 2.1.240` (see `SHIPPED_POSTURE_ALLOW`'s own doc comment for the
    /// specific findings -- `Edit(./**)` vs. bare `Write`, deny-over-allow
    /// precedence, prefix-wildcard semantics): an in-repo write succeeds
    /// with no prompt, a `cargo test` runs with no prompt, a write outside
    /// the workspace is refused, and `rm -rf` is refused even alongside a
    /// broader unrelated allow rule.
    ///
    /// **Fix round 3 (2026-08-22): `sandbox.extra_allow`/`extra_deny`**
    /// are appended after the shipped pair, not merged into it, so an
    /// operator's own addition can never silently replace a shipped entry --
    /// only add to either side. Deny still wins over allow regardless of
    /// which list (shipped or operator) an entry came from: both end up in
    /// the same `--allowedTools=`/`--disallowedTools=` argv, and the
    /// underlying CLI mechanism does not distinguish their origin.
    /// Projects `safety` (issue #83's harness-neutral command policy) onto
    /// claude's own `--allowedTools=`/`--disallowedTools=` vocabulary: every
    /// `SHIPPED_POSTURE_ALLOW` entry that is not a `Bash(...)` rule (file-
    /// scope and bare-tool rules -- outside `[safety]`'s own domain, see
    /// `safety::command_pattern_from_bash_rule`'s doc comment) is prepended
    /// directly, in declared order, then every `safety` rule is re-wrapped
    /// as `Bash(<pattern>)`, then `sandbox.extra_allow`/`extra_deny` are
    /// appended last, unchanged from before this method took a
    /// `SafetyPolicy` parameter.
    ///
    /// The static permission families still round-trip byte-identically:
    /// `safety::builtin_deny`/`builtin_allow` strip
    /// `SHIPPED_POSTURE_DENY`/`_ALLOW`'s own `Bash(...)` wrapper and this
    /// method re-adds it in the original order. Issue #224 then appends the
    /// reserved zirv patterns generated from `utils::RESERVED_COMMANDS`, and
    /// the launch-computed scratchpad rules remain last. The full order is
    /// pinned by `the_headless_projection_is_byte_exact_against_the_shipped_
    /// constants` below.
    ///
    /// **Fix round 4 (2026-08-23, issue #104):** `SHIPPED_POSTURE_ALLOW`
    /// gained more non-`Bash` entries (`Read(~/.claude/**)`, `Edit(~/.claude
    /// /projects/**)`, `Read(~/.zirv/**)`, `WebFetch`, `WebSearch`), all
    /// filtered out of `safety.allow` the same way `Read(./**)`/`Edit(./**)`
    /// always were (outside `[safety]`'s own command-only domain -- see
    /// `safety::command_pattern_from_bash_rule`). Rather than hand-list each
    /// one here too, every non-`Bash(` entry in the constant is now
    /// prepended in its original declared order, which also reproduces
    /// `Read(./**)`/`Edit(./**)` first exactly as before. The two scratchpad
    /// rules (`adapters::scratchpad_rules`) are computed here, from the real
    /// `std::env::temp_dir()`, rather than baked into the constant -- the
    /// path is per-machine, and the constant has to stay `&'static`.
    /// Appended after the safety-derived allow entries, before the
    /// operator's own `sandbox.extra_allow`.
    ///
    /// **Cross-harness permissions (2026-08-24): Design B.** A live probe
    /// against Claude Code 2.1.241 reached `permissionMode: default`, but the
    /// account rate-limited before the Bash request, so whether a hook's
    /// `"ask"` overrides a native `Bash(*)` allow could not be established
    /// live. **Answered (2026-08-26, issue #147) by the documented behavior**
    /// (code.claude.com/docs/en/permissions, "Extend permissions with
    /// hooks"; code.claude.com/docs/en/hooks, "Decision control"): a native
    /// settings rule is evaluated INDEPENDENTLY of a PreToolUse hook's own
    /// decision -- a settings `ask` rule still prompts even when the hook
    /// returns `allow`, and a settings `deny` beats a hook `allow` outright.
    /// So a hook's `"ask"`/`"allow"` never overrides a native `Bash(*)`
    /// allow OR ask/deny rule either way; native rules and the hook are two
    /// independent gates a command must clear. The conservative projection
    /// therefore still emits no blanket Bash allow: the hook's explicit
    /// `"allow"` carries ordinary commands, while an ask verdict cannot
    /// accidentally be bypassed by native pre-approval. Issue #224's narrow
    /// reserved-built-in rules are projected separately into that launch
    /// settings layer; the hook remains an independent gate, so repo ask/deny
    /// rules still narrow them. Every projected launch carries the Zirv-owned
    /// `--settings` layer
    /// that attests this hook for the process. On macOS/Linux/WSL2 it enables
    /// Claude's OS sandbox in auto-allow mode, fails closed if that boundary
    /// cannot start, denies common credential paths to Bash and the built-in
    /// Read tool, and scrubs cloud credentials from child environments.
    /// Native Windows receives the hook/read/env layer but no unsupported
    /// sandbox key.
    fn default_sandbox_args(
        &self,
        sandbox: &crate::commands::ctx::config::SandboxConfig,
        safety: &crate::commands::ctx::safety::SafetyPolicy,
        mode: super::LaunchMode,
    ) -> Vec<String> {
        // The non-`Bash(...)` surface is pre-approved in BOTH modes: file
        // scope, the harness dirs, WebFetch/WebSearch. These are outside
        // `[safety]`'s command-only domain (see `safety::
        // command_pattern_from_bash_rule`), so the safety hook -- registered
        // for the `Bash` tool alone -- cannot speak for them, and leaving
        // them off the list would prompt on every file read.
        let mut allow_entries: Vec<String> = super::SHIPPED_POSTURE_ALLOW
            .iter()
            .filter(|(rule, _)| !rule.starts_with("Bash("))
            .map(|(rule, _)| rule.to_string())
            .collect();

        if !mode.is_interactive() {
            allow_entries.extend(
                safety
                    .allow
                    .iter()
                    .map(|rule| format!("Bash({})", rule.pattern)),
            );
        }
        allow_entries.extend(super::scratchpad_rules(&std::env::temp_dir()));
        allow_entries.extend(sandbox.extra_allow.iter().cloned());
        let allow = allow_entries.join(",");

        let mut deny_entries: Vec<String> = super::SHIPPED_POSTURE_DENY
            .iter()
            .filter(|(rule, _)| !rule.starts_with("Bash("))
            .map(|(rule, _)| rule.to_string())
            .collect();
        deny_entries.extend(
            safety
                .deny
                .iter()
                .map(|rule| format!("Bash({})", rule.pattern)),
        );
        // The ask set is a hard rule ONLY headlessly. Interactively it must
        // reach a prompt, which means it belongs on neither list: the hook's
        // own "ask" decision is what stops it. Headlessly there is nobody to
        // answer, so folding it into the deny list turns what `dontAsk`
        // would refuse by omission into an explicit, named refusal.
        if !mode.is_interactive() {
            deny_entries.extend(
                safety
                    .ask
                    .iter()
                    .map(|rule| format!("Bash({})", rule.pattern)),
            );
        }
        deny_entries.extend(sandbox.extra_deny.iter().cloned());
        let deny = deny_entries.join(",");

        // `dontAsk` is "don't prompt, deny if not pre-approved" (the
        // installed CLI's own `--help` text, quoted in this method's doc
        // comment) -- correct with no human present, and exactly wrong with
        // one. `default` prompts for anything not pre-approved, which is what
        // lets the safety hook's own decisions be the whole story. Never
        // `acceptEdits`/`bypassPermissions`: both were probed live and both
        // auto-run unapproved destructive actions.
        let permission_mode = if mode.is_interactive() {
            "default"
        } else {
            "dontAsk"
        };

        let mut args = vec![
            "--permission-mode".to_string(),
            permission_mode.to_string(),
            format!("--allowedTools={allow}"),
            format!("--disallowedTools={deny}"),
        ];
        if let Some(path) = self.launch_settings_path(safety) {
            args.push("--settings".to_string());
            args.push(path.display().to_string());
        }
        #[cfg(not(test))]
        args.extend(
            std::env::current_dir()
                .ok()
                .map_or_else(Vec::new, |repo| linked_worktree_args(&repo)),
        );
        args
    }

    /// A delegated headless worker (`zirv ctx agent`, and the dashboard's
    /// own spawn-request pane variant) used to silently inherit whatever the
    /// operator's own interactive default model happened to be -- often a
    /// far pricier model than the delegated task actually needs. `"sonnet"`
    /// is the user-approved hard default that stops that, used only when
    /// the operator has not set `worker.claude` explicitly (see
    /// `adapters::resolve_worker_model`).
    fn default_worker_model(&self) -> Option<&'static str> {
        Some("sonnet")
    }

    /// Claude's own model ladder, top to bottom: `fable`/`mythos` (the
    /// orchestrator-tier aliases), `opus`, `sonnet`, `haiku`. Matched by
    /// substring on `seat`, lowercased first so `"claude-Opus-4-5"` and a
    /// bare `"opus"` both hit the same rung regardless of case (so
    /// `"claude-fable-5"` and a bare `"fable"` both hit the fable rung)
    /// rather than exact equality, since a seat string can carry a full id
    /// (`claude-opus-4-1`) or a bare alias. `haiku` is already the floor, so
    /// it maps to itself instead of falling off the ladder; an absent or
    /// unrecognised seat assumes the top tier, same as
    /// `AgentAdapter::review_model_below`'s own doc comment requires -- the
    /// deliberate consequence is that the computed default can then resolve
    /// to a model *more expensive* than the seat actually in use (an
    /// accepted spend-up default; the operator can override it with
    /// `[review]` or by setting `chat.model`).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let seat = seat.map(str::to_lowercase);
        match seat.as_deref() {
            Some(s) if s.contains("fable") || s.contains("mythos") => "opus",
            Some(s) if s.contains("opus") => "sonnet",
            Some(s) if s.contains("sonnet") => "haiku",
            Some(s) if s.contains("haiku") => "haiku",
            _ => "opus",
        }
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        let model = model.to_lowercase();
        if model.contains("fable") || model.contains("mythos") {
            return Some(4);
        }
        if model.contains("opus") {
            return Some(3);
        }
        if model.contains("sonnet") {
            return Some(2);
        }
        if model.contains("haiku") {
            return Some(1);
        }
        None
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let projects = self.home_dir().join(".claude").join("projects");
        let computed = projects
            .join(project_slug(&session.cwd))
            .join(format!("{}.jsonl", session.id));
        if computed.exists() {
            return computed;
        }

        // The slug rule is verified for `/` and `.` but not every character,
        // so fall back to finding the session file wherever it landed.
        let wanted = format!("{}.jsonl", session.id);
        if let Ok(entries) = std::fs::read_dir(&projects) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&wanted);
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        computed
    }

    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        parse_events(jsonl)
    }

    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        structural_context(jsonl, last_n)
    }

    fn model_hint(&self, jsonl: &str) -> Option<String> {
        model_hint(jsonl)
    }

    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        transcript_usage(jsonl)
    }

    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    fn quit_sequence(&self) -> &'static str {
        "/exit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
            events: true,
            // claude's own composer submits a same-burst trailing `\r`
            // correctly -- issue #118 is codex-specific.
            defer_injection_submit: false,
            context_window_tokens: self.context_window_tokens(None),
        }
    }

    /// Claude reports a per-model capacity (issue #155): the long-window
    /// `[1m]` / `-1m` marker reports `LONG_CONTEXT_WINDOW_TOKENS`, and every
    /// other id -- including an unstated model -- gets the conservative
    /// `DEFAULT_CONTEXT_WINDOW_TOKENS`. See that constant's own doc comment
    /// for why an overstated capacity is the worse failure mode.
    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let model = model.map(str::to_lowercase);
        match model.as_deref() {
            Some(m) if m.contains("[1m]") || m.contains("-1m") => Some(LONG_CONTEXT_WINDOW_TOKENS),
            _ => Some(DEFAULT_CONTEXT_WINDOW_TOKENS),
        }
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): `--model
    /// <MODEL>` is a real flag.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    /// `--resume <SESSION_ID>` is already a fact this codebase relies on
    /// elsewhere -- `wrap::extra_launch_flags` strips it (among the flags
    /// that pin a launch to an existing conversation) on every restart, and
    /// `exec.rs`'s own `RESUME_FLAGS_WITH_VALUE` treats it as a two-token
    /// flag -- so this is wiring up an already-verified flag for the
    /// dashboard's own restore path, not a fresh claim.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["--resume".to_string(), session_id.to_string()])
    }

    /// The same `--session-id <uuid>` flag `headless_cmd` already pins every
    /// headless run with (verified against the real CLI), offered here so an
    /// *interactive* dashboard pane can be pinned too. That is what makes the
    /// roster's stored uuid the claude conversation id, and therefore what
    /// makes `resume_args` above resolve to a real conversation after a quit.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        vec!["--session-id".to_string(), session.to_string()]
    }

    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: vec![
                (super::SOCKET_ENV.to_string(), socket.display().to_string()),
                (super::SESSION_ENV.to_string(), session.id.to_string()),
            ],
            instructions: "register a Stop hook running `zirv ctx hook stop` in \
                           ~/.claude/settings.json so turn boundaries reach the supervisor"
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::{NormalizedEvent, input_hash};

    fn test_launch_settings() -> Value {
        launch_settings_value(
            &Default::default(),
            Path::new("zirv-test-safety-policy.json"),
            &LaunchEnvironment::default(),
        )
        .expect("settings")
    }

    pub(crate) fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// SECURITY (FINDING 1): `detect_help_flag` forwards `bin_args` (repo-
    /// controlled on the interactive path) into the `--help` probe argv. When
    /// the program resolves to the Windows `cmd.exe /c <shim>` form, cmd.exe
    /// would reparse a metacharacter in `bin_args` as a command -- so the probe
    /// must run the fail-closed `guard_cmd_shim_reparse` check *before* it
    /// spawns, and report "unsupported" (`false`) on rejection without ever
    /// executing anything. Proven by a shim that writes a sentinel next to
    /// itself if it ever runs: a metachar `bin_arg` leaves the sentinel absent,
    /// while a clean one spawns and creates it.
    #[cfg(windows)]
    #[test]
    fn detect_help_flag_refuses_to_spawn_when_a_bin_arg_would_be_reparsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        // `%~dp0` is the shim's own directory, so the sentinel lands there
        // regardless of the probe's cwd. If this batch file ever runs, the
        // sentinel exists.
        std::fs::write(&shim, "@echo off\r\necho ran> \"%~dp0ran.marker\"\r\n")
            .expect("write shim");
        let sentinel = dir.path().join("ran.marker");

        // A metacharacter-bearing bin_arg: the guard must refuse before spawn.
        let detected = detect_help_flag(&shim.display().to_string(), &["foo&calc".to_string()]);
        assert!(!detected, "a rejected probe reports unsupported");
        assert!(!sentinel.exists(), "the shim must never have been spawned");

        // Control: a clean bin_arg passes the guard and does spawn the shim
        // (which then creates the sentinel), confirming the guard -- not some
        // unrelated failure -- is what stopped the metachar case above.
        let _ = detect_help_flag(&shim.display().to_string(), &["--model".to_string()]);
        assert!(
            sentinel.exists(),
            "a clean bin_arg is allowed through and the shim runs"
        );
    }

    /// The needles track `scripts/record-claude-fixture.py`'s SECRET pattern.
    /// A scrub rule with no guard behind it is a rule that can silently stop
    /// working, and the cost of that is a credential in a public repository.
    #[test]
    fn recorded_fixture_carries_no_personal_data() {
        let text = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture must be committed");
        for needle in [
            "jonathansolskov",
            "/Users/",
            "sk-ant",
            "sk-proj",
            "ghp_",
            "gho_",
            "ghu_",
            "ghs_",
            "ghr_",
            "AKIA",
            "-----BEGIN",
            "ApiKey ",
            "Bearer ",
            "eyJ",
        ] {
            assert!(
                !text.contains(needle),
                "fixture leaks '{needle}'; re-run scripts/record-claude-fixture.py"
            );
        }
        assert_eq!(
            credential_shape(&text),
            None,
            "fixture leaks a credential-shaped string; re-run scripts/record-claude-fixture.py"
        );
        assert!(
            text.contains("compact_boundary"),
            "fixture must include a compaction"
        );
        assert!(
            text.lines().count() >= 50,
            "fixture is too small to be representative"
        );
    }

    /// The two scrub rules a literal needle cannot express: the fixture
    /// legitimately contains `?key=REDACTED` and `checkout@v2`, so only the
    /// credential-shaped forms of each count as a leak.
    fn credential_shape(text: &str) -> Option<String> {
        for (index, _) in text.match_indices("key=") {
            let hex: String = text[index + 4..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            if hex.len() >= 8 {
                return Some(format!("key={hex}"));
            }
        }

        for (index, _) in text.match_indices('@') {
            let has_local = text[..index]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            let domain: String = text[index + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            let tld = domain.rsplit('.').next().unwrap_or_default();
            let is_email = has_local
                && domain.contains('.')
                && tld.len() >= 2
                && tld.chars().all(|c| c.is_ascii_alphabetic());
            if is_email {
                return Some(format!("@{domain}"));
            }
        }
        None
    }

    #[test]
    fn the_credential_shape_check_separates_secrets_from_ordinary_text() {
        assert_eq!(
            credential_shape("http://localhost:1/?key=deadbeefcafe"),
            Some("key=deadbeefcafe".to_string())
        );
        assert_eq!(
            credential_shape("mail someone@example.com now"),
            Some("@example.com".to_string())
        );
        assert_eq!(credential_shape("?key=REDACTED"), None);
        assert_eq!(credential_shape("uses: actions/checkout@v2"), None);
        assert_eq!(credential_shape("#[cfg(test)] @testable import"), None);
    }

    #[test]
    fn context_tokens_sum_the_cache_fields() {
        // Verified against a real transcript: input_tokens alone is 2 in a
        // 110k-token session, so the cache fields carry the real size.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(context_tokens_of(&usage), 108_886);
    }

    #[test]
    fn transcript_usage_sums_actual_main_session_usage() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":5,"output_tokens":7}}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":100,"output_tokens":100}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":11,"output_tokens":13}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(
            usage,
            TranscriptUsage {
                input_tokens: 13,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 5,
                output_tokens: 20,
            }
        );
        assert_eq!(usage.context_total(), 21, "the pre-2.34.0 combined number");
        assert_eq!(transcript_usage("not json"), None);
    }

    /// The adapter stops pre-summing. `context_tokens_of` keeps its exact old
    /// meaning and value, because rot's context gate and every display path
    /// still want one combined "real context size" number -- it is just no
    /// longer the ONLY thing that survives the boundary.
    #[test]
    fn transcript_usage_reports_each_token_class_separately() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"#,
            r#""cache_creation_input_tokens":200,"cache_read_input_tokens":3000,"#,
            r#""output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":5,"#,
            r#""cache_creation_input_tokens":0,"cache_read_input_tokens":3100,"#,
            r#""output_tokens":7}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.cache_creation_input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 6_100);
        assert_eq!(usage.output_tokens, 47);
        assert_eq!(
            usage.context_total(),
            6_315,
            "context_total must equal what the old pre-summed input_tokens was"
        );
    }

    /// A sidechain row still does not reach the MAIN-session usage total:
    /// Task 2.2 gives subagent spend its own bucket rather than folding it
    /// into a number whose meaning is "this session's own context".
    #[test]
    fn transcript_usage_still_excludes_sidechain_rows_from_the_main_total() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":900,"#,
            r#""cache_read_input_tokens":900,"output_tokens":900}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.output_tokens, 2);
    }

    /// Subagent turns live in `isSidechain` rows. They are charged to the
    /// account (`window::sum_transcripts` walks `subagents/` too) but were
    /// dropped from workflow accounting entirely. Counted separately here, so
    /// the main-session number keeps meaning "this session's own context"
    /// while the child spend stops being invisible.
    #[test]
    fn sidechain_usage_is_counted_separately_rather_than_dropped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":900,"#,
            r#""cache_read_input_tokens":12000,"output_tokens":90}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#,
        );
        let side = sidechain_transcript_usage(jsonl).expect("sidechain usage");
        assert_eq!(side.input_tokens, 900);
        assert_eq!(side.cache_read_input_tokens, 12_000);
        assert_eq!(side.output_tokens, 90);

        assert_eq!(
            sidechain_transcript_usage(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":1}}}"#
            ),
            None,
            "no sidechain rows means None, not a zeroed reading"
        );
    }

    #[test]
    fn a_real_prompt_starts_a_turn_but_a_tool_result_does_not() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n"
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::TurnStart { at_ms: None },
                NormalizedEvent::ToolResult { is_error: false },
            ]
        );
    }

    #[test]
    fn missing_is_error_counts_as_success() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![NormalizedEvent::ToolResult { is_error: false }]
        );
    }

    /// Same-error repetition (`rot::Signals::same_error_repeats`): an
    /// erroring tool result with extractable text emits a `ToolErrorText`
    /// carrying its normalized hash right after the `ToolResult` -- never in
    /// place of it.
    #[test]
    fn an_erroring_tool_result_emits_its_normalized_error_hash() {
        let jsonl = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"boom: file missing","is_error":true}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![
                NormalizedEvent::ToolResult { is_error: true },
                NormalizedEvent::ToolErrorText {
                    hash: error_text_hash("boom: file missing"),
                },
            ]
        );
    }

    /// A successful tool result never gets a `ToolErrorText` sibling, even
    /// when it carries text.
    #[test]
    fn a_successful_tool_result_never_emits_an_error_hash() {
        let jsonl = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"all good"}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![NormalizedEvent::ToolResult { is_error: false }]
        );
    }

    #[test]
    fn assistant_yields_text_tokens_and_tool_calls() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":["#,
            r#"{"type":"thinking","thinking":"hmm"},"#,
            r#"{"type":"text","text":"[zirv] on it"},"#,
            r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#,
            r#"],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::AssistantFirstText { at_ms: None },
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] on it".to_string(),
                    input_tokens: 100,
                    at_ms: None,
                },
                NormalizedEvent::ToolCall {
                    name: "Bash".to_string(),
                    input_hash: input_hash("{\"command\":\"ls\"}"),
                    at_ms: None,
                },
            ]
        );
    }

    /// Issue #293: `at_ms` comes from each row's own top-level `timestamp`
    /// field, and a non-empty assistant text gets an `AssistantFirstText`
    /// pushed right before the `AssistantFinal` that carries it.
    #[test]
    fn timestamps_are_read_from_each_rows_own_timestamp_field() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"go"},"timestamp":"2026-08-20T10:00:00.000Z"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":["#,
            r#"{"type":"text","text":"[zirv] hi"},"#,
            r#"{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}"#,
            r#"],"usage":{"input_tokens":5}},"timestamp":"2026-08-20T10:00:00.500Z"}"#
        );
        let turn_at = parse_iso8601_utc_ms("2026-08-20T10:00:00.000Z");
        let msg_at = parse_iso8601_utc_ms("2026-08-20T10:00:00.500Z");
        assert!(
            turn_at.is_some() && msg_at.is_some(),
            "fixture timestamps must parse"
        );
        assert_eq!(
            parse_events(jsonl),
            vec![
                NormalizedEvent::TurnStart { at_ms: turn_at },
                NormalizedEvent::AssistantFirstText { at_ms: msg_at },
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] hi".to_string(),
                    input_tokens: 5,
                    at_ms: msg_at,
                },
                NormalizedEvent::ToolCall {
                    name: "Bash".to_string(),
                    input_hash: input_hash("{\"command\":\"ls\"}"),
                    at_ms: msg_at,
                },
            ]
        );
    }

    /// Issue #293: a timestamped tool result gets a `ToolResultTimestamp`
    /// sibling right after it, mirroring `ToolErrorText`'s own placement.
    #[test]
    fn a_timestamped_tool_result_gets_a_timestamp_sibling_event() {
        let jsonl = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]},"timestamp":"2026-08-20T10:00:00.000Z"}"#;
        let at = parse_iso8601_utc_ms("2026-08-20T10:00:00.000Z");
        assert!(at.is_some(), "fixture timestamp must parse");
        assert_eq!(
            parse_events(jsonl),
            vec![
                NormalizedEvent::ToolResult { is_error: false },
                NormalizedEvent::ToolResultTimestamp { at_ms: at },
            ]
        );
    }

    #[test]
    fn tool_only_assistant_messages_still_report_tokens() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":5}}}"#;
        let events = parse_events(jsonl);
        assert_eq!(
            events[0],
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 5,
                at_ms: None,
            }
        );
    }

    #[test]
    fn compact_boundary_becomes_a_compaction_event() {
        let jsonl = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#;
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::Compaction]);
    }

    #[test]
    fn api_error_classification_checks_exclusions_before_overflow_patterns() {
        use crate::commands::ctx::event::ProviderErrorClass;

        let cases = [
            (
                "API Error: prompt is too long: 213462 tokens > 200000 maximum",
                ProviderErrorClass::Overflow,
            ),
            (
                "API Error: 413 {\"error\":{\"type\":\"request_too_large\"}}",
                ProviderErrorClass::Overflow,
            ),
            (
                "Your input exceeds the context window of this model",
                ProviderErrorClass::Overflow,
            ),
            (
                "Requested token count exceeds the model's maximum context length of 131072 tokens",
                ProviderErrorClass::Overflow,
            ),
            (
                "Throttling error: Too many tokens, please wait before trying again",
                ProviderErrorClass::RateLimit,
            ),
            (
                "rate limit: prompt is too long",
                ProviderErrorClass::RateLimit,
            ),
            (
                "too many requests: request_too_large",
                ProviderErrorClass::RateLimit,
            ),
            (
                "Service unavailable: request_too_large",
                ProviderErrorClass::Other,
            ),
            ("API Error: connection reset", ProviderErrorClass::Other),
        ];

        for (message, expected) in cases {
            assert_eq!(
                super::super::classify_provider_error(message),
                expected,
                "{message}"
            );
        }
    }

    #[test]
    fn provider_error_and_model_events_are_parsed_from_the_recorded_fixture() {
        use crate::commands::ctx::event::ProviderErrorClass;

        let jsonl =
            std::fs::read_to_string(fixture_path("claude-provider-errors-model-drift.jsonl"))
                .expect("fixture");
        let events = parse_events(&jsonl);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    NormalizedEvent::ProviderError {
                        class: ProviderErrorClass::Overflow
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    NormalizedEvent::ProviderError {
                        class: ProviderErrorClass::RateLimit
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    NormalizedEvent::ModelId { id } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["claude-opus-5", "claude-sonnet-5"]
        );
    }

    #[test]
    fn sidechain_meta_and_garbage_lines_are_skipped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"content":"hook noise"}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"type":"pr-link","prNumber":7}"#,
            "\n",
            r#"{"type":"user","message":{"content":"real prompt"}}"#,
            "\n"
        );
        assert_eq!(
            parse_events(jsonl),
            vec![NormalizedEvent::TurnStart { at_ms: None }]
        );
    }

    /// The invariant the incremental scoring path rests on (see the
    /// `parse_events` contract on `AgentAdapter`): this parser is line-local,
    /// so a transcript cut at newlines and parsed piecewise yields exactly the
    /// events one parse of the whole file yields.
    #[test]
    fn parsing_a_transcript_in_pieces_yields_the_same_events() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let whole = parse_events(&jsonl);
        let lines: Vec<&str> = jsonl.lines().collect();

        for chunk in [1, 3, 17] {
            let pieced: Vec<NormalizedEvent> = lines
                .chunks(chunk)
                .flat_map(|piece| parse_events(&format!("{}\n", piece.join("\n"))))
                .collect();
            assert_eq!(pieced, whole, "in pieces of {chunk} lines");
        }
    }

    #[test]
    fn real_fixture_matches_recorded_expectations() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");

        let events = parse_events(&jsonl);
        let count = |pred: &dyn Fn(&NormalizedEvent) -> bool| {
            events.iter().filter(|e| pred(e)).count() as u64
        };
        let want = |key: &str| {
            expected[key]
                .as_u64()
                .unwrap_or_else(|| panic!("{key} missing"))
        };

        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::TurnStart { .. })),
            want("turn_start")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::AssistantFinal { .. })),
            want("assistant")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolCall { .. })),
            want("tool_call")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: true })),
            want("tool_result_error")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: false })),
            want("tool_result_ok")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::Compaction)),
            want("compaction")
        );

        let last_tokens = events
            .iter()
            .rev()
            .find_map(|e| match e {
                NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
                _ => None,
            })
            .expect("fixture has assistant events");
        assert_eq!(last_tokens, want("last_context_tokens"));
        assert!(
            want("tool_result_error") >= 1,
            "fixture must contain a tool error"
        );
    }

    use crate::commands::ctx::adapters::{AgentAdapter, SESSION_ENV, SOCKET_ENV};
    use crate::commands::ctx::event::{SessionId, SessionRef};

    /// I: `super::super::built_args` (`adapters/mod.rs`) takes the program
    /// string rather than the whole adapter, since `program` is private to
    /// this module -- this thin wrapper is what lets every call site below
    /// keep passing `&adapter` unchanged.
    fn built_args(adapter: &ClaudeAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(&adapter.program, cmd)
    }

    #[test]
    fn project_slug_matches_on_disk_evidence() {
        assert_eq!(
            project_slug(std::path::Path::new(
                "/Users/x/Documents/Privat/zirv-fitness-tracking"
            )),
            "-Users-x-Documents-Privat-zirv-fitness-tracking"
        );
        // A dot becomes a dash, which is why worktrees show up as `--claude-worktrees`.
        assert_eq!(
            project_slug(std::path::Path::new("/Users/x/repo/.claude-worktrees/b")),
            "-Users-x-repo--claude-worktrees-b"
        );
    }

    #[test]
    fn transcript_path_is_derived_from_home_and_cwd() {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(
            adapter.transcript_path(&session),
            home.path()
                .join(".claude/projects/-work-repo/11111111-2222-4333-8444-555555555555.jsonl")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_scanning_when_the_slug_misses() {
        let home = tempfile::tempdir().expect("tempdir");
        let real = home.path().join(".claude/projects/some-other-slug");
        std::fs::create_dir_all(&real).expect("mkdir");
        let actual = real.join("11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&actual, "").expect("write");

        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), actual);
    }

    #[test]
    fn headless_cmd_pins_the_session_id() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "sonnet".to_string()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-claude");
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );
    }

    /// FIX B: the stdin headless form keeps `-p` and the session pin but never
    /// puts the prompt on argv -- claude reads it from stdin instead, so a
    /// prompt bearing a cmd.exe metacharacter is never reparsed on the shim
    /// form. The extra flags (the file-based system prompt, the operator's own)
    /// still ride on argv, exactly as `headless_cmd` places them.
    #[test]
    fn headless_cmd_stdin_omits_the_prompt_and_reads_it_from_stdin() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter
            .headless_cmd_stdin(
                &SessionId::parse("abc"),
                &[
                    "--append-system-prompt-file".to_string(),
                    "/s/p.md".to_string(),
                ],
            )
            .expect("claude has a verified stdin form");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--append-system-prompt-file".to_string(),
                "/s/p.md".to_string(),
            ],
            "no positional prompt token: the prompt travels on stdin"
        );
        assert!(
            !args.iter().any(|a| a.contains("foo") || a.contains('&')),
            "the prompt text is nowhere in argv: {args:?}"
        );
    }

    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally() {
        let adapter = ClaudeAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(built_args(&adapter, &with), vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--continue".to_string()]);
        assert_eq!(
            built_args(&adapter, &without),
            vec!["--continue".to_string()]
        );
    }

    #[test]
    fn distiller_cmd_uses_a_cheap_model_and_reads_stdin() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
            ]
        );
    }

    /// I6: the judgment/distiller child embeds untrusted repo CLAUDE.md text
    /// in its prompt and otherwise runs with the operator's full tool
    /// permissions. Verified against the real CLI (see
    /// docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md):
    /// this is the one flag, in the one argv shape, that provably blocks
    /// tool use, including an adversarial attempt to route around it via
    /// Bash or Task delegation.
    #[test]
    fn the_distiller_denies_the_tools_verified_to_matter() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        let args = built_args(&adapter, &cmd);

        let deny = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("the distiller must restrict its own tools");
        assert_eq!(
            deny, "--disallowedTools=Write,Edit,Bash,NotebookEdit",
            "must be one argv token (a two-token --flag value form was \
             verified to swallow the next argv entry instead): {args:?}"
        );
        assert!(
            deny.contains("Bash"),
            "Bash alone bypasses a Write/Edit-only deny list via a shell \
             redirect, verified against the real CLI: {deny}"
        );
    }

    /// Bug B (harness parity): the shipped default `[policy]` (all `Allow`)
    /// must leave a real launch byte-for-byte unaffected -- `policy_args` is
    /// new, but an operator who never touched `[policy]` must see no argv
    /// change at all.
    #[test]
    fn policy_args_is_empty_under_the_default_all_allow_policy() {
        let adapter = ClaudeAdapter::new(None);
        assert!(
            adapter
                .policy_args(
                    &crate::commands::ctx::policy::EffectivePolicy::default(),
                    super::super::LaunchMode::Interactive
                )
                .is_empty()
        );
    }

    /// A `[policy] shell_exec = "deny"` (or `repo_fs_write = "deny"`) must
    /// reach a real launch as the exact same `--disallowedTools=...` pin the
    /// distiller already relies on -- reusing `read_only_args()` rather than
    /// a second literal is what guarantees the two can never drift.
    #[test]
    fn policy_args_pins_the_verified_tool_deny_when_shell_exec_is_denied() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = ClaudeAdapter::new(None);
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(
            adapter.policy_args(&policy, super::super::LaunchMode::Interactive),
            adapter.read_only_args(),
            "policy_args must reuse read_only_args verbatim, never a second literal"
        );
    }

    #[test]
    fn policy_args_pins_the_verified_tool_deny_when_repo_fs_write_is_denied() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = ClaudeAdapter::new(None);
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(
            adapter.policy_args(&policy, super::super::LaunchMode::Interactive),
            adapter.read_only_args()
        );
    }

    /// `Ask` stays `OperatorControlled` (see `policy_support`): claude has no
    /// verified per-run mechanism for it, so `policy_args` must not invent
    /// one.
    #[test]
    fn policy_args_leaves_ask_untouched() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = ClaudeAdapter::new(None);
        let policy = EffectivePolicy {
            shell_exec: Stance::Ask,
            ..EffectivePolicy::default()
        };
        assert!(
            adapter
                .policy_args(&policy, super::super::LaunchMode::Interactive)
                .is_empty()
        );
    }

    /// The shipped-default posture (2026-08-22): verified against the real
    /// installed binary (see this method's own doc comment) as the one
    /// claude mode that both suppresses prompts and never auto-runs an
    /// unapproved action -- `bypassPermissions`/`acceptEdits` were probed
    /// and rejected for doing the opposite.
    #[test]
    fn default_sandbox_args_uses_the_verified_dont_ask_mode_when_headless() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        assert_eq!(
            &args[0..2],
            &["--permission-mode".to_string(), "dontAsk".to_string()]
        );
    }

    /// Every supervised Claude launch carries a Zirv-owned, per-run settings
    /// layer. This is the attestation that the command classifier is really
    /// installed for this process; relying on a one-time global setup leaves
    /// upgraded, reset, and deliberately minimal profiles unguarded.
    ///
    /// Issue #147: the native `Bash(dangerouslyDisableSandbox:true)` ask
    /// rule this test used to pin here is now ABSENT -- documented behavior
    /// (code.claude.com/docs/en/permissions) is that a native settings rule
    /// is evaluated independently of a PreToolUse hook's own decision, so
    /// that rule silently defeated every hook-side allow for an escape
    /// retry (the gh carve-out, and the new `[safety] escape_allow` gate).
    /// The attested safety hook is now the sole zirv-side decision point.
    #[test]
    fn launch_settings_attest_the_safety_hook_and_no_longer_inject_the_native_sandbox_escape_ask_rule()
     {
        let settings = test_launch_settings();
        assert_eq!(settings["disableAllHooks"], false);
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/0/matcher"),
            Some(&serde_json::json!("Bash|PowerShell"))
        );
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/0/hooks/0/command"),
            Some(&serde_json::json!("zirv ctx safety check"))
        );
        assert!(
            !settings["permissions"]["ask"]
                .as_array()
                .is_some_and(|rules| rules
                    .contains(&serde_json::json!("Bash(dangerouslyDisableSandbox:true)"))),
            "the native ask rule must be gone -- it silently defeated every hook-side allow: {settings}"
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"], "1");
    }

    #[test]
    fn launch_settings_observe_permission_events_without_changing_pretooluse() {
        let settings = test_launch_settings();
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            serde_json::json!([{
                "matcher": "Bash|PowerShell",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx safety check"
                }]
            }])
        );
        let observer = serde_json::json!([{
            "hooks": [{
                "type": "command",
                "command": "zirv ctx hook permission"
            }]
        }]);
        assert_eq!(settings["hooks"]["PermissionRequest"], observer);
        assert_eq!(settings["hooks"]["PermissionDenied"], observer);
    }

    #[test]
    fn launch_settings_additional_directories_exactly_match_the_scratchpad_roots() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("zirv-test-safety-policy.json");
        let scratchpad_roots = super::super::scratchpad_roots(&std::env::temp_dir());
        let settings = launch_settings_value(
            &policy,
            policy_path,
            &LaunchEnvironment {
                scratchpad_roots: scratchpad_roots.clone(),
                ..LaunchEnvironment::default()
            },
        )
        .expect("settings");
        assert_eq!(
            settings["permissions"]["additionalDirectories"],
            serde_json::json!(scratchpad_roots)
        );

        let settings = launch_settings_value(&policy, policy_path, &LaunchEnvironment::default())
            .expect("settings");
        assert!(settings["permissions"]["additionalDirectories"].is_null());
    }

    #[test]
    fn launch_settings_export_ssh_auth_sock_only_when_resolved() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("zirv-test-safety-policy.json");
        let settings = launch_settings_value(
            &policy,
            policy_path,
            &LaunchEnvironment {
                ssh_auth_sock: Some("/tmp/ssh-agent.sock".to_string()),
                ..LaunchEnvironment::default()
            },
        )
        .expect("settings");
        assert_eq!(settings["env"]["SSH_AUTH_SOCK"], "/tmp/ssh-agent.sock");
        assert_eq!(settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"], "1");
        assert!(settings["env"][super::super::super::safety::POLICY_FINGERPRINT_ENV].is_string());
        assert_eq!(
            settings["env"][super::super::super::safety::POLICY_SNAPSHOT_ENV],
            policy_path.display().to_string()
        );

        let settings = launch_settings_value(&policy, policy_path, &LaunchEnvironment::default())
            .expect("settings");
        assert!(settings["env"]["SSH_AUTH_SOCK"].is_null());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launch_settings_allow_the_resolved_unix_sockets_on_macos() {
        let settings = launch_settings_value(
            &Default::default(),
            Path::new("zirv-test-safety-policy.json"),
            &LaunchEnvironment {
                unix_sockets: vec![
                    "/var/run/docker.sock".to_string(),
                    "/tmp/ssh-agent.sock".to_string(),
                ],
                ..LaunchEnvironment::default()
            },
        )
        .expect("settings");
        assert_eq!(
            settings["sandbox"]["network"]["allowUnixSockets"],
            serde_json::json!(["/var/run/docker.sock", "/tmp/ssh-agent.sock"])
        );

        let settings = launch_settings_value(
            &Default::default(),
            Path::new("zirv-test-safety-policy.json"),
            &LaunchEnvironment::default(),
        )
        .expect("settings");
        assert!(settings["sandbox"]["network"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn launch_environment_deduplicates_existing_docker_socket_paths() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("docker-target.sock");
        std::fs::write(&target, "").expect("fake socket target");
        let socket = dir.path().join("docker.sock");
        symlink(&target, &socket).expect("socket symlink");
        let canonical_target = target.canonicalize().expect("canonical target");
        let docker_host = format!("unix://{}", canonical_target.display());

        assert_eq!(
            resolve_docker_socket_paths(&socket, Some(&docker_host)),
            vec![
                socket.display().to_string(),
                canonical_target.display().to_string(),
            ]
        );
        assert!(resolve_docker_socket_paths(&dir.path().join("missing"), None).is_empty());
    }

    #[test]
    fn launch_settings_bind_the_hook_to_an_immutable_policy_snapshot() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("C:/safe/policies/policy.json");
        let settings = launch_settings_value(&policy, policy_path, &LaunchEnvironment::default())
            .expect("settings");
        let expected =
            super::super::super::safety::policy_fingerprint(&policy).expect("fingerprint");

        assert_eq!(
            settings["env"][super::super::super::safety::POLICY_FINGERPRINT_ENV],
            expected
        );
        assert_eq!(
            settings["env"][super::super::super::safety::POLICY_SNAPSHOT_ENV],
            policy_path.display().to_string()
        );
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/0/matcher"),
            Some(&serde_json::json!("Bash|PowerShell")),
            "the identical hook must guard both native Windows and Unix shell tools"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_settings_enable_containment_and_common_credential_denials() {
        let settings = test_launch_settings();
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["autoAllowBashIfSandboxed"], true);
        let files = settings["sandbox"]["filesystem"]["denyRead"]
            .as_array()
            .expect("credential file rules");
        assert!(files.iter().any(|entry| entry == "~/.ssh"));
        let read_denies = settings["permissions"]["deny"]
            .as_array()
            .expect("built-in Read credential denials");
        assert!(read_denies.iter().any(|entry| entry == "Read(~/.ssh/**)"));
    }

    /// Issue #222: Claude's native permission and sandbox layers project the
    /// three-way allow/sandbox/ask partition without widening repo scripts or
    /// subprocess-launching ctx verbs.
    ///
    /// Code review fix (CRITICAL, issue #224 follow-up): `ctx` must NOT get
    /// a name-level `Bash(zirv ctx *)`/`zirv ctx *` entry -- several of its
    /// verbs (`exec`, `wrap`, ...) spawn a subprocess with caller-controlled
    /// argv, and a blanket entry handed those an unattended, unsandboxed
    /// escape. `ctx` instead gets one verb-scoped entry per `safety::
    /// reserved_zirv_command_patterns`'s own generated list; every OTHER
    /// reserved name keeps its name-level entry, since its payload is a
    /// prompt or a path, not arbitrary argv.
    ///
    #[test]
    fn launch_settings_project_the_prompt_free_command_family_partition() {
        let settings = test_launch_settings();
        let permission_allow = settings["permissions"]["allow"]
            .as_array()
            .expect("reserved built-in permission rules");
        for name in crate::utils::RESERVED_COMMANDS {
            if matches!(*name, "ctx" | "setup") {
                continue;
            }
            let expected = serde_json::json!(format!("Bash(zirv {name} *)"));
            assert!(
                permission_allow.contains(&expected),
                "reserved permission rule {expected} missing from {settings}"
            );
        }
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv *)")));
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv ctx *)")));
        for family in PROMPT_FREE_COMMAND_FAMILIES {
            let rule = format!("Bash({})", family.pattern);
            assert!(
                permission_allow.contains(&serde_json::json!(rule)),
                "native allow rule {rule} missing from {settings}"
            );
        }
        assert!(permission_allow.contains(&serde_json::json!("Bash(zirv ctx status *)")));
        assert!(permission_allow.contains(&serde_json::json!("Bash(zirv ctx inbox *)")));
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv ctx exec *)")));
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv ctx wrap *)")));
        // `usage`'s own `tee` subcommand launches an arbitrary trailing
        // statusline command, so a wildcard `usage *` entry would also cover
        // `usage tee -- <cmd>` -- unlike the escape-safe retry path, a
        // native permission/sandbox glob cannot see the fourth token.
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv ctx usage *)")));
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv setup *)")));
        assert!(permission_allow.contains(&serde_json::json!("Bash(zirv test *)")));
        assert!(permission_allow.contains(&serde_json::json!("Bash(zirv verify *)")));
        assert!(permission_allow.contains(&serde_json::json!("Bash(zirv frontend *)")));
        assert!(!permission_allow.contains(&serde_json::json!("Bash(zirv somescript *)")));

        #[cfg(not(windows))]
        {
            let exclusions = settings["sandbox"]["excludedCommands"]
                .as_array()
                .expect("reserved built-in sandbox exclusions");
            for name in crate::utils::RESERVED_COMMANDS {
                if matches!(*name, "ctx" | "setup" | "test" | "verify" | "frontend") {
                    continue;
                }
                let expected = serde_json::json!(format!("zirv {name} *"));
                assert!(
                    exclusions.contains(&expected),
                    "reserved sandbox exclusion {expected} missing from {settings}"
                );
            }
            assert!(!exclusions.contains(&serde_json::json!("zirv *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv ctx *")));
            for family in PROMPT_FREE_COMMAND_FAMILIES {
                assert_eq!(
                    exclusions.contains(&serde_json::json!(family.pattern)),
                    family.sandbox_excluded,
                    "sandbox projection for {} is wrong in {settings}",
                    family.pattern
                );
            }
            assert!(exclusions.contains(&serde_json::json!("zirv ctx status *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv ctx exec *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv ctx wrap *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv ctx usage *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv setup *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv test *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv verify *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv frontend *")));
            assert!(!exclusions.contains(&serde_json::json!("zirv somescript *")));
            assert!(
                settings["sandbox"]["filesystem"]["denyRead"]
                    .as_array()
                    .is_some_and(|rules| rules.iter().any(|rule| rule == "~/.config/gh/hosts.yml"))
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_settings_allow_write_to_the_state_root_but_not_the_policy_snapshot() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("/operator/.zirv/runtime/policies/abc123.json");
        let state_root = Path::new("/state");
        let settings = launch_settings_value(
            &policy,
            policy_path,
            &LaunchEnvironment {
                state_write_root: Some(state_root.to_path_buf()),
                ..LaunchEnvironment::default()
            },
        )
        .expect("settings");
        let allow_write = settings["sandbox"]["filesystem"]["allowWrite"]
            .as_array()
            .expect("allowWrite must be present when a state root is given");
        assert!(
            allow_write.iter().any(|entry| entry == "/state"),
            "the state root must be allow-listed for write: {settings}"
        );
        assert!(
            !allow_write.contains(&serde_json::json!(policy_path.display().to_string())),
            "the policy snapshot must not be separately allow-listed: {settings}"
        );

        // No state root resolved (best-effort failure): no allowWrite key at
        // all, never an empty-but-present one that could mask a future bug.
        let settings_without_state =
            launch_settings_value(&policy, policy_path, &LaunchEnvironment::default())
                .expect("settings");
        assert!(settings_without_state["sandbox"]["filesystem"]["allowWrite"].is_null());
    }

    #[test]
    fn worktree_porcelain_projects_only_external_paths_and_honours_the_cap() {
        let mut porcelain = String::from(
            "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
             worktree /repo/nested\nHEAD def\ndetached\n\n\
             worktree /repo-other\nHEAD ghi\nbare\n\n",
        );
        for i in 0..20 {
            porcelain.push_str(&format!("worktree /outside-{i}\nHEAD {i}\n\n"));
        }

        let paths = parse_worktree_porcelain(&porcelain);
        let roots = additional_worktree_roots(Path::new("/repo"), paths);
        assert_eq!(roots.len(), 16);
        assert_eq!(
            &roots[..2],
            &[PathBuf::from("/repo-other"), PathBuf::from("/outside-0")]
        );
        assert_eq!(roots[roots.len() - 1], PathBuf::from("/outside-14"));
        assert!(
            roots
                .iter()
                .all(|root| root != Path::new("/repo") && root != Path::new("/repo/nested"))
        );
    }

    /// Issue #329: ssh needs `known_hosts`/`config` to verify a host, but
    /// every private key under `~/.ssh` must stay denied.
    #[test]
    fn launch_settings_reopen_only_the_non_secret_ssh_files_inside_the_denied_home() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("zirv-test-safety-policy.json");
        let settings = launch_settings_value(&policy, policy_path, &LaunchEnvironment::default())
            .expect("settings");

        let allow_read = settings["sandbox"]["filesystem"]["allowRead"]
            .as_array()
            .expect("allowRead must be present")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(allow_read, vec!["~/.ssh/known_hosts", "~/.ssh/config"]);

        // The broad deny stays, so anything else under ~/.ssh (every private
        // key) remains blocked by the less specific rule.
        let deny_read = settings["sandbox"]["filesystem"]["denyRead"]
            .as_array()
            .expect("denyRead must be present")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert!(deny_read.contains(&"~/.ssh".to_string()));
        assert!(
            !allow_read.iter().any(|p| p == "~/.ssh" || p.contains('*')),
            "no wildcard may re-open the whole key directory: {allow_read:?}"
        );
    }

    /// Issue #329: a linked worktree Claude may `cd` into must also be
    /// writable, or ordinary gates there fail with `Operation not permitted`
    /// and force an unsandboxed retry. The roots land in BOTH grants, as
    /// literal paths (neither key supports globs portably).
    #[test]
    fn launch_settings_grant_writes_and_working_directories_to_linked_worktrees() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("zirv-test-safety-policy.json");
        let worktrees = vec!["/work/wt-a".to_string(), "/work/wt-b".to_string()];
        let settings = launch_settings_value(
            &policy,
            policy_path,
            &LaunchEnvironment {
                state_write_root: Some(PathBuf::from("/state/zirv")),
                scratchpad_roots: vec!["/tmp/claude-501".to_string()],
                workspace_write_roots: worktrees.clone(),
                ..LaunchEnvironment::default()
            },
        )
        .expect("settings");

        assert_eq!(
            settings["permissions"]["additionalDirectories"],
            serde_json::json!(["/tmp/claude-501", "/work/wt-a", "/work/wt-b"])
        );
        assert_eq!(
            settings["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/state/zirv", "/work/wt-a", "/work/wt-b"])
        );
        for root in &worktrees {
            assert!(
                !root.contains('*'),
                "worktree grants must be literal paths: {root}"
            );
        }
    }

    #[test]
    fn every_projected_launch_names_the_attested_settings_file() {
        let path = PathBuf::from("C:/safe/zirv-claude-launch-settings.json");
        let adapter = ClaudeAdapter::new(None).with_launch_settings_forced(Some(path.clone()));
        for mode in [
            super::super::LaunchMode::Interactive,
            super::super::LaunchMode::Headless,
        ] {
            let args = adapter.default_sandbox_args(&Default::default(), &Default::default(), mode);
            let index = args
                .iter()
                .position(|arg| arg == "--settings")
                .expect("the launch must carry its hook settings");
            assert_eq!(args.get(index + 1), Some(&path.display().to_string()));
        }
    }

    #[test]
    fn launch_settings_are_materialized_atomically_under_the_zirv_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("state");
        let _state = super::super::super::testenv::VarGuard::set(&[(
            super::super::super::state::STATE_ENV,
            Some(state.path().to_str().expect("utf8 state path")),
        )]);
        let adapter = ClaudeAdapter::new(None)
            .with_home(home.path().to_path_buf())
            .with_live_launch_settings();
        let policy = super::super::super::safety::SafetyPolicy::default();
        let fingerprint =
            super::super::super::safety::policy_fingerprint(&policy).expect("fingerprint");
        let path = adapter
            .launch_settings_path(&policy)
            .expect("settings materialized");
        assert_eq!(
            path,
            home.path()
                .join(".zirv")
                .join("runtime")
                .join(format!("claude-launch-settings-{fingerprint}.json"))
        );
        let policy_path = home
            .path()
            .join(".zirv")
            .join("runtime")
            .join("policies")
            .join(format!("{fingerprint}.json"));
        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("read materialized settings"),
        )
        .expect("valid settings JSON");
        assert_eq!(
            written["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!([state.path().display().to_string()]),
            "the materialized settings must allow the exact resolved state root"
        );
        assert!(
            !written["sandbox"]["filesystem"]["allowWrite"]
                .as_array()
                .is_some_and(|entries| entries
                    .contains(&serde_json::json!(policy_path.display().to_string()))),
            "the immutable policy snapshot must not be separately allow-listed"
        );
        assert_eq!(
            written,
            launch_settings_value(&policy, &policy_path, &LaunchEnvironment::resolve())
                .expect("settings")
        );
        let snapshotted: super::super::super::safety::SafetyPolicy = serde_json::from_str(
            &std::fs::read_to_string(policy_path).expect("read policy snapshot"),
        )
        .expect("valid policy JSON");
        assert_eq!(snapshotted, policy);
    }

    /// If the private settings file cannot be materialized, the projection
    /// falls back to Design B: no broad Bash allow. Claude's native flow may
    /// prompt, but a missing guard can never turn into silent full access.
    #[test]
    fn an_unattested_launch_falls_back_without_widening_bash() {
        let adapter = ClaudeAdapter::new(None).with_launch_settings_forced(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Interactive,
        );
        assert!(!args.iter().any(|arg| arg == "--settings"));
        let allow = args
            .iter()
            .find(|arg| arg.starts_with("--allowedTools="))
            .expect("allowed tools");
        assert!(!allow.contains("Bash(*)"), "unattested widening: {allow}");
    }

    /// THE requirement, at the argv level: an interactive launch must not
    /// carry a finite Bash allow-list under a prompting permission mode,
    /// because everything off the end of that list is a prompt. Design A
    /// blanket-allows Bash and lets the safety hook gate; Design B (see the
    /// plan's Task 3 Step 1) drops the blanket entry and lets the hook's own
    /// explicit `"allow"` carry it. This test pins what BOTH designs share:
    /// the mode is `default`, and no per-command Bash allow-list is emitted.
    #[test]
    fn the_interactive_projection_never_emits_a_finite_bash_allow_list() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Interactive,
        );
        assert_eq!(
            &args[0..2],
            &["--permission-mode".to_string(), "default".to_string()]
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        for family in ["Bash(cargo *)", "Bash(git *)", "Bash(npm *)"] {
            assert!(
                !allow_arg.contains(family),
                "a per-family Bash allow-list means every OTHER command prompts: {allow_arg}"
            );
        }
        // The non-Bash surface is still pre-approved: those tools are outside
        // `[safety]`'s command-only domain, so the hook cannot speak for them.
        assert!(allow_arg.contains("Edit(./**)"), "got {allow_arg}");
        assert!(allow_arg.contains("Read(./**)"), "got {allow_arg}");
        assert!(allow_arg.contains("WebFetch"), "got {allow_arg}");
    }

    /// The ask set must never be pre-approved and never hard-denied on an
    /// interactive launch: pre-approving it would skip the prompt this whole
    /// change exists to produce, and denying it would be the silent death it
    /// exists to remove.
    #[test]
    fn the_interactive_projection_leaves_the_ask_set_to_the_hook() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Interactive,
        );
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        for (rule, _) in super::super::SHIPPED_POSTURE_ASK {
            assert!(
                !deny_arg.contains(rule),
                "interactive must let '{rule}' reach a prompt, not die: {deny_arg}"
            );
        }
        for (rule, _) in super::super::SHIPPED_POSTURE_DENY {
            assert!(
                deny_arg.contains(rule),
                "the deny set must still be a hard rule: {deny_arg}"
            );
        }
    }

    /// Headless is untouched by all of the above.
    #[test]
    fn the_headless_projection_is_unchanged_by_the_interactive_inversion() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        assert_eq!(
            &args[0..2],
            &["--permission-mode".to_string(), "dontAsk".to_string()]
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        assert!(allow_arg.contains("Bash(cargo *)"), "got {allow_arg}");
        assert!(
            !allow_arg.contains("Bash(*)"),
            "no blanket allow headlessly: {allow_arg}"
        );
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        for (rule, _) in super::super::SHIPPED_POSTURE_ASK {
            assert!(
                deny_arg.contains(rule),
                "headless has nobody to prompt, so ask folds into deny: {deny_arg}"
            );
        }
    }

    /// Fix round 2 (2026-08-22): `dontAsk` alone denies every unapproved
    /// action, including a legitimate in-repo write -- inert, not "runs
    /// freely inside the workspace". `SHIPPED_POSTURE_ALLOW`/`_DENY` is
    /// what makes it usable; this pins the generated argv shape and that
    /// every entry from the shared list actually landed, so the two lists
    /// (source of truth and generated argv) can never silently drift.
    #[test]
    fn default_sandbox_args_generates_the_allow_and_deny_lists_from_the_shared_source() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        assert_eq!(args.len(), 6, "got {args:?}");
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        for (rule, _) in super::super::SHIPPED_POSTURE_ALLOW {
            assert!(
                allow_arg.contains(rule),
                "allow rule '{rule}' missing from {allow_arg}"
            );
        }
        for (rule, _) in super::super::SHIPPED_POSTURE_DENY {
            assert!(
                deny_arg.contains(rule),
                "deny rule '{rule}' missing from {deny_arg}"
            );
        }
        // Both single `=`-bound tokens, the same discipline `read_only_args`
        // already holds `--disallowedTools` to (the "I6 fix round": a
        // two-token form was verified to swallow the next argv entry).
        assert!(allow_arg.starts_with("--allowedTools="));
        assert!(deny_arg.starts_with("--disallowedTools="));
    }

    /// Issue #224: the generated headless argv allows every reserved zirv
    /// built-in, but never the old blanket `zirv *` family that also covered
    /// untrusted repo scripts. The `cargo *` family remains unchanged.
    ///
    /// Code review fix (CRITICAL, issue #224 follow-up): `ctx` gets verb-
    /// scoped entries only, never a blanket `Bash(zirv ctx *)` -- see
    /// `launch_settings_project_the_prompt_free_command_family_partition`'s
    /// own doc comment for why.
    ///
    /// Issue #222: payload-selecting `test`/`verify`/`frontend` are native-
    /// allowed here but remain absent from sandbox exclusions; `setup` stays
    /// outside every unattended allow surface.
    #[test]
    fn default_sandbox_args_allow_reserved_zirv_builtins_but_not_scripts() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        for name in crate::utils::RESERVED_COMMANDS {
            if matches!(*name, "ctx" | "setup") {
                continue;
            }
            let rule = format!("Bash(zirv {name} *)");
            assert!(
                allow_arg.contains(&rule),
                "allow rule '{rule}' missing from {allow_arg}"
            );
        }
        assert!(allow_arg.contains("Bash(cargo *)"));
        assert!(allow_arg.contains("Bash(zirv ctx status *)"));
        assert!(!allow_arg.contains("Bash(zirv *)"));
        assert!(!allow_arg.contains("Bash(zirv ctx *)"));
        assert!(!allow_arg.contains("Bash(zirv ctx exec *)"));
        assert!(!allow_arg.contains("Bash(zirv ctx wrap *)"));
        assert!(!allow_arg.contains("Bash(zirv ctx usage *)"));
        assert!(!allow_arg.contains("Bash(zirv setup *)"));
        assert!(allow_arg.contains("Bash(zirv test *)"));
        assert!(allow_arg.contains("Bash(zirv verify *)"));
        assert!(allow_arg.contains("Bash(zirv frontend *)"));
        assert!(!allow_arg.contains("Bash(zirv somescript *)"));
    }

    /// Issue #83: `default_sandbox_args` now projects `safety::SafetyPolicy`
    /// (derived from `SHIPPED_POSTURE_ALLOW`/`_DENY`) instead of iterating
    /// those constants directly. Under the shipped default -- no
    /// `[safety]`/`sandbox.extra_*` configured, i.e. exactly the two
    /// `Default::default()` values every other test in this file already
    /// passes -- the generated argv must stay byte-for-byte identical to
    /// what a hand-built projection straight from `SHIPPED_POSTURE_ALLOW`/
    /// `_DENY` (plus the scratchpad rules, issue #104) would produce, so
    /// this refactor could not have silently changed a live-verified
    /// permission set.
    #[test]
    fn the_headless_projection_is_byte_exact_against_the_shipped_constants() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );

        // `SHIPPED_POSTURE_ALLOW`'s entries remain first. Issue #224's
        // source-derived reserved rules follow the command entries because
        // `safety::builtin_allow` appends them; scratchpad rules come last.
        let mut expected_allow: Vec<String> = super::super::SHIPPED_POSTURE_ALLOW
            .iter()
            .map(|(rule, _)| rule.to_string())
            .collect();
        expected_allow.extend(
            super::super::super::safety::reserved_zirv_command_patterns()
                .into_iter()
                .map(|pattern| format!("Bash({pattern})")),
        );
        expected_allow.extend(super::super::scratchpad_rules(&std::env::temp_dir()));
        let mut expected_deny: Vec<String> = super::super::SHIPPED_POSTURE_DENY
            .iter()
            .map(|(rule, _)| rule.to_string())
            .collect();
        expected_deny.extend(
            super::super::SHIPPED_POSTURE_ASK
                .iter()
                .map(|(rule, _)| rule.to_string()),
        );

        assert_eq!(
            args,
            vec![
                "--permission-mode".to_string(),
                "dontAsk".to_string(),
                format!("--allowedTools={}", expected_allow.join(",")),
                format!("--disallowedTools={}", expected_deny.join(",")),
                "--settings".to_string(),
                "zirv-test-claude-launch-settings.json".to_string(),
            ]
        );
    }

    /// Fix round 3 (2026-08-22): an operator's own `sandbox.extra_allow`/
    /// `extra_deny` (`SandboxConfig`, `config.rs`) are appended after the
    /// shipped lists, never replacing them -- the shipped entries must
    /// still be present alongside the operator's own addition.
    #[test]
    fn default_sandbox_args_appends_the_operators_own_extra_allow_and_deny() {
        let adapter = ClaudeAdapter::new(None);
        let sandbox = crate::commands::ctx::config::SandboxConfig {
            enabled: true,
            extra_allow: vec!["Bash(just test *)".to_string()],
            extra_deny: vec!["Bash(terraform apply *)".to_string()],
        };
        let args = adapter.default_sandbox_args(
            &sandbox,
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        assert!(allow_arg.contains("Bash(just test *)"), "got {allow_arg}");
        assert!(
            allow_arg.contains("Read(./**)"),
            "the shipped entries must still be present, not replaced: {allow_arg}"
        );
        assert!(
            deny_arg.contains("Bash(terraform apply *)"),
            "got {deny_arg}"
        );
        assert!(
            deny_arg.contains("Bash(sudo *)"),
            "the shipped deny entries must still be present, not replaced: {deny_arg}"
        );
    }

    /// The scoping rule verified live to actually confine a write to the
    /// workspace is `Edit(./**)`, not a bare `Write` -- see `SHIPPED_
    /// POSTURE_ALLOW`'s own doc comment for the exact CLI error that
    /// disqualified `Write(./**)`. A bare, unscoped `Write`/`Edit` must
    /// never appear: it was verified live to let a write reach the
    /// directory *above* the workspace with no denial at all.
    #[test]
    fn default_sandbox_args_scopes_file_edits_to_the_workspace_not_bare_write() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        assert!(allow_arg.contains("Edit(./**)"), "got {allow_arg}");
        assert!(
            !allow_arg.contains("Write(") && !allow_arg.split(',').any(|t| t == "Write"),
            "a bare/unscoped Write rule was verified live to leak outside the workspace: \
             {allow_arg}"
        );
    }

    /// Issue #104: the scratchpad is a real per-machine directory (`Claude
    /// Code`'s own temp-file scratchpad), and the harness's own memory dir
    /// is writable so a session can update its own auto-memory under
    /// `~/.claude/projects/<slug>/memory/` (see `adapters::scratchpad_rules`
    /// and `SHIPPED_POSTURE_ALLOW`'s own doc comment).
    #[test]
    fn default_sandbox_args_allows_the_scratchpad_and_claude_memory_dir() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        for rule in super::super::scratchpad_rules(&std::env::temp_dir()) {
            assert!(
                allow_arg.contains(&rule),
                "missing scratchpad rule {rule} from {allow_arg}"
            );
        }
        assert!(allow_arg.contains("WebFetch"), "got {allow_arg}");
        assert!(
            allow_arg.contains("Edit(~/.claude/projects/**)"),
            "got {allow_arg}"
        );
    }

    /// Issue #104: a session must never widen its own posture -- the
    /// operator layer is readable (allowed above) but never editable.
    #[test]
    fn default_sandbox_args_denies_editing_the_operator_zirv_layer() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        assert!(deny_arg.contains("Edit(~/.zirv/**)"), "got {deny_arg}");
    }

    /// Must never be the dangerous bypass, under any circumstance.
    #[test]
    fn default_sandbox_args_never_emits_the_dangerous_bypass_flag() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        assert!(
            !args
                .iter()
                .any(|a| a.contains("dangerously-skip-permissions")
                    || a.contains("bypassPermissions")),
            "must never widen: {args:?}"
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) has to work
    /// for all three invocation kinds: exec restarts build headless commands,
    /// handoff distillation builds a distiller command, and wrap restarts build an
    /// interactive one.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &[]);
        assert_eq!(headless.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = headless
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
            ],
            "the bin arguments come before the agent flags"
        );

        let interactive = adapter.interactive_cmd(Some("resume"), &[]);
        assert_eq!(interactive.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = interactive
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["/tmp/stub.sh".to_string(), "resume".to_string()]);

        let distiller = adapter.distiller_cmd("haiku");
        assert_eq!(distiller.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = distiller
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
            ]
        );
    }

    #[test]
    fn a_single_word_bin_and_extra_whitespace_still_work() {
        let adapter = ClaudeAdapter::new(Some("  /opt/homebrew/bin/claude  "));
        let cmd = adapter.interactive_cmd(None, &[]);
        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "/opt/homebrew/bin/claude"
        );
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn turn_signal_setup_exports_socket_and_session() {
        let adapter = ClaudeAdapter::new(None);
        let session = SessionRef {
            id: SessionId::parse("sess-1"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        let setup = adapter.register_turn_signal(&session, std::path::Path::new("/tmp/s/ab.sock"));
        assert!(
            setup
                .env
                .contains(&(SOCKET_ENV.to_string(), "/tmp/s/ab.sock".to_string()))
        );
        assert!(
            setup
                .env
                .contains(&(SESSION_ENV.to_string(), "sess-1".to_string()))
        );
        assert!(
            setup.instructions.contains("zirv ctx hook stop"),
            "instructions should name the hook command: {}",
            setup.instructions
        );
    }

    #[test]
    fn structural_context_extracts_prompts_files_and_errors() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"boom: file missing","is_error":true}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"[zirv] fixed it"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            "\n"
        );
        let ctx = structural_context(jsonl, 5);
        assert_eq!(ctx.user_messages, vec!["first prompt", "second prompt"]);
        assert_eq!(ctx.assistant_texts, vec!["[zirv] fixed it"]);
        assert_eq!(ctx.files_read, vec!["/work/src/lib.rs"]);
        assert!(ctx.files_modified.is_empty(), "a Read is never a write");
        assert_eq!(ctx.tool_errors.len(), 1);
        assert!(ctx.tool_errors[0].contains("boom"));
    }

    /// Issue #280: `Edit`/`Write`/`MultiEdit`/`NotebookEdit` land in
    /// `files_modified`; `Read`/`Grep`/`Glob` and an unrecognised tool with a
    /// file key land in `files_read` -- the conservative direction.
    #[test]
    fn structural_context_classifies_files_read_vs_modified_by_tool_name() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/a.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b","name":"Grep","input":{"path":"/b.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"c","name":"Edit","input":{"file_path":"/c.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"d","name":"Write","input":{"file_path":"/d.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"e","name":"MultiEdit","input":{"file_path":"/e.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"f","name":"NotebookEdit","input":{"notebook_path":"/f.ipynb"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"g","name":"SomeThirdPartyTool","input":{"file_path":"/g.rs"}}],"usage":{}}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert_eq!(ctx.files_read, vec!["/a.rs", "/b.rs", "/g.rs"]);
        assert_eq!(
            ctx.files_modified,
            vec!["/c.rs", "/d.rs", "/e.rs", "/f.ipynb"]
        );
    }

    /// T2: `last_verification` reflects the LAST Bash invocation whose
    /// command looks like a build/test/lint run, correlated by
    /// `id`/`tool_use_id` -- here the second `cargo test` succeeds after
    /// the first one failed, so the handoff must report green.
    #[test]
    fn structural_context_reports_the_last_verification_run() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cargo test"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b1","is_error":true,"content":"assertion failed: left 1, right 2"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b2","name":"Bash","input":{"command":"cargo test"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b2","is_error":false,"content":"test result: ok"}]}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 5);
        let outcome = ctx
            .last_verification
            .expect("a verification run was recorded");
        assert_eq!(outcome.command, "cargo test");
        assert_eq!(
            outcome.status,
            crate::commands::ctx::event::VerificationStatus::Passed,
            "the second run passed"
        );
        assert!(outcome.error_excerpt.is_empty());
    }

    /// An unrelated tool result (a `Read`) landing between a `Bash` call and
    /// its own result must not be mistaken for that `Bash` call's result:
    /// correlation is by `id`/`tool_use_id`, not by call order.
    #[test]
    fn structural_context_correlates_bash_results_by_id_not_by_order() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/a.rs"}},{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cargo test"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"r1","is_error":false,"content":"file contents"},{"type":"tool_result","tool_use_id":"b1","is_error":true,"content":"boom: it failed"}]}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 5);
        let outcome = ctx
            .last_verification
            .expect("a verification run was recorded");
        assert_eq!(outcome.command, "cargo test");
        assert_eq!(
            outcome.status,
            crate::commands::ctx::event::VerificationStatus::Failed
        );
        assert_eq!(outcome.error_excerpt, vec!["boom: it failed".to_string()]);
    }

    #[test]
    fn structural_context_keeps_only_the_last_n_and_dedupes_files() {
        let mut jsonl = String::new();
        for i in 0..6 {
            jsonl.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"p{i}\"}}}}\n"
            ));
            jsonl.push_str(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{\"file_path\":\"/same.rs\"}}],\"usage\":{}}}\n",
            );
        }
        let ctx = structural_context(&jsonl, 2);
        assert_eq!(ctx.user_messages, vec!["p4", "p5"]);
        assert_eq!(ctx.files_read, vec!["/same.rs"]);
    }

    #[test]
    fn the_system_prompt_becomes_the_verified_flag_pair() {
        // Exactly the mechanism recorded in
        // docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md.
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be consistent"),
            vec![
                "--append-system-prompt".to_string(),
                "be consistent".to_string()
            ]
        );
    }

    #[test]
    fn an_empty_prompt_injects_nothing() {
        let adapter = ClaudeAdapter::new(None);
        assert!(adapter.system_prompt_args("").is_empty());
        assert!(adapter.system_prompt_args("   \n").is_empty());
    }

    /// I2: this must name the exact flag `system_prompt_args` emits, so a
    /// caller can find a user's own use of it and merge rather than override.
    #[test]
    fn the_user_facing_flag_name_matches_what_system_prompt_args_emits() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.user_system_prompt_flag(),
            Some("--append-system-prompt")
        );
    }

    #[test]
    fn claude_advertises_the_capability() {
        assert!(ClaudeAdapter::new(None).capabilities().system_prompt);
    }

    /// Claude reports a per-model capacity, with a CONSERVATIVE default for a
    /// model id it does not recognise. Conservative on purpose: an
    /// overstated capacity raises the restart ceiling past what the seat can
    /// actually hold, and a session that overruns its window is a far worse
    /// outcome than one rotated slightly early.
    #[test]
    fn claude_reports_a_conservative_context_window_for_an_unknown_model() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.context_window_tokens(Some("some-model-zirv-has-never-seen")),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            adapter.context_window_tokens(None),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS),
            "an unstated model is the same conservative answer"
        );
        assert_eq!(
            adapter.capabilities().context_window_tokens,
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS),
            "every existing capabilities() caller gets a capacity with no new plumbing"
        );
    }

    /// A recognised long-window model id reports its own capacity, and the
    /// `[1m]` suffix form is recognised too -- that is how a long-window seat
    /// is actually spelled in this environment.
    #[test]
    fn claude_recognises_a_long_window_model_id() {
        let adapter = ClaudeAdapter::new(None);
        let long = adapter
            .context_window_tokens(Some("claude-opus-5[1m]"))
            .expect("a capacity");
        assert!(
            long > DEFAULT_CONTEXT_WINDOW_TOKENS,
            "a 1M seat must not be capped at the conservative default"
        );
        assert_eq!(
            adapter
                .capabilities_for_model(Some("claude-opus-5[1m]"))
                .context_window_tokens,
            Some(long)
        );
    }

    /// Issue #155 D1: `model_hint` reads the same `message.model` field every
    /// assistant row already carries, and reports the LAST one seen -- a
    /// live `/model` switch mid-session must be reflected, not the session's
    /// original model.
    #[test]
    fn model_hint_reports_the_most_recent_assistant_model() {
        let jsonl = format!(
            "{}\n{}\n{}",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5"}}"#,
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-5[1m]"}}"#,
        );
        assert_eq!(
            model_hint(&jsonl),
            Some("claude-opus-5[1m]".to_string()),
            "the LAST assistant model wins, not the first"
        );
    }

    #[test]
    fn model_hint_is_none_for_a_transcript_with_no_assistant_model_field() {
        let jsonl = r#"{"type":"user","message":{"content":"hi"}}"#;
        assert_eq!(model_hint(jsonl), None);
    }

    #[test]
    fn model_args_uses_the_verified_flag() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.model_args("opus"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// C: claude keeps a real default so `resolve_distiller_model` never has
    /// to fall back to an empty model for it, unlike codex.
    #[test]
    fn claude_defaults_the_distiller_model_to_haiku() {
        assert_eq!(
            ClaudeAdapter::new(None).default_distiller_model(),
            Some("haiku")
        );
    }

    /// A delegated headless worker with no operator `worker.claude` override
    /// gets claude's own hard default, not the operator's interactive seat
    /// model -- see `adapters::resolve_worker_model`.
    #[test]
    fn claude_defaults_the_worker_model_to_sonnet() {
        assert_eq!(
            ClaudeAdapter::new(None).default_worker_model(),
            Some("sonnet")
        );
    }

    /// Claude's two role layers are distinct texts, and the worker one carries
    /// none of the orchestrator's own delegate-everything coaching: a session
    /// that was itself delegated to must not be told its job is to delegate.
    #[test]
    fn claude_has_its_own_worker_layer_distinct_from_the_orchestrator_layer() {
        let layer = ClaudeAdapter::new(None)
            .worker_system_prompt()
            .expect("claude has a worker layer");
        assert_eq!(layer, WORKER_PROMPT);
        assert!(layer.starts_with("zirv worker conventions"));
        assert!(
            !layer.contains("spend it on judgment"),
            "the worker layer must not carry the orchestrator's own coaching: {layer}"
        );
        for claim in ["never run `zirv agent`", "fork-type subagents"] {
            assert!(
                layer.contains(claim),
                "the worker layer must say '{claim}': {layer}"
            );
        }
    }

    /// The trimmed coordination layer is real text, materially shorter than
    /// the orchestrator layer -- the whole point is that a coordinator seat
    /// costs less than the seat that spawned it -- and it must never coach
    /// onward coordinator spawning.
    #[test]
    fn the_sub_orchestrator_layer_is_short_and_forbids_spawning_coordinators() {
        assert!(SUB_ORCHESTRATOR_PROMPT.len() < ORCHESTRATOR_PROMPT.len());
        assert!(SUB_ORCHESTRATOR_PROMPT.contains("zirv agent"));
        assert!(
            SUB_ORCHESTRATOR_PROMPT.contains("sub-orchestrator"),
            "must name what it must not spawn"
        );
    }

    /// Claude actually wires `SUB_ORCHESTRATOR_PROMPT` into the adapter
    /// trait rather than leaving the const unused and falling back to the
    /// default (`worker_system_prompt`) -- the three role layers must all be
    /// distinct texts.
    #[test]
    fn claude_has_its_own_sub_orchestrator_layer_distinct_from_the_other_two() {
        let layer = ClaudeAdapter::new(None)
            .sub_orchestrator_system_prompt()
            .expect("claude has a sub-orchestrator layer");
        assert_eq!(layer, SUB_ORCHESTRATOR_PROMPT);
        assert_ne!(layer, WORKER_PROMPT);
        assert_ne!(layer, ORCHESTRATOR_PROMPT);
    }

    /// The claude ladder, top to bottom: fable/mythos, opus, sonnet, haiku.
    /// `review_model_below` returns the tier one below `seat`; an unknown or
    /// absent seat assumes the top tier, and haiku (already the floor) maps
    /// to itself rather than falling off the ladder.
    #[test]
    fn review_model_below_walks_the_claude_ladder() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(adapter.review_model_below(Some("claude-fable-5")), "opus");
        assert_eq!(adapter.review_model_below(Some("mythos")), "opus");
        assert_eq!(adapter.review_model_below(Some("opus")), "sonnet");
        assert_eq!(adapter.review_model_below(Some("sonnet")), "haiku");
        assert_eq!(adapter.review_model_below(Some("haiku")), "haiku");
        assert_eq!(
            adapter.review_model_below(None),
            "opus",
            "no seat configured: assume the top tier"
        );
        assert_eq!(
            adapter.review_model_below(Some("some-unreleased-model")),
            "opus",
            "unrecognised seat: assume the top tier"
        );
    }

    /// Seat matching must be case-insensitive: a mixed-case seat like
    /// "Opus" (or a full id with mixed-case segments) must land on the same
    /// ladder rung as its lowercase form, not fall through to the unknown
    /// arm and assume the top tier.
    #[test]
    fn review_model_below_matches_the_seat_case_insensitively() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(adapter.review_model_below(Some("Opus")), "sonnet");
        assert_eq!(
            adapter.review_model_below(Some("claude-Opus-4-5")),
            "sonnet"
        );
        assert_eq!(adapter.review_model_below(Some("SONNET")), "haiku");
        assert_eq!(adapter.review_model_below(Some("Haiku")), "haiku");
        assert_eq!(adapter.review_model_below(Some("Fable")), "opus");
        assert_eq!(adapter.review_model_below(Some("MYTHOS")), "opus");
    }

    #[test]
    fn resume_args_uses_the_verified_flag() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.resume_args("sess-1"),
            Some(vec!["--resume".to_string(), "sess-1".to_string()])
        );
    }

    #[test]
    fn the_file_flag_name_is_the_documented_one() {
        assert_eq!(
            ClaudeAdapter::new(None).system_prompt_file_flag(),
            Some("--append-system-prompt-file")
        );
    }

    /// Writes a throwaway `--help` stub so the probe can be exercised without
    /// depending on the machine's actual installed binary. The heredoc keeps
    /// `help_text` free of shell-escaping concerns. Every caller spawns it via
    /// `sh`, so it is unix-only like they are.
    #[cfg(unix)]
    fn help_stub(dir: &std::path::Path, name: &str, help_text: &str) -> String {
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat <<'EOF'\n{help_text}\nEOF\n"),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        format!("sh {}", script.display())
    }

    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_detects_the_flag_from_help_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(
            dir.path(),
            "probe-yes.sh",
            "Options:\n  --append-system-prompt-file <path>",
        );
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(adapter.supports_system_prompt_file(&[]));
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): the flag is
    /// never spelled out on its own; it only appears folded into this exact
    /// shorthand, inside the `--bare` option's own description. A probe that
    /// only looked for the plain flag text would report "unsupported" on the
    /// very machine this was verified on.
    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_detects_the_real_clis_bracket_shorthand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(
            dir.path(),
            "probe-bracket.sh",
            "Explicitly provide context via: --system-prompt[-file],\n  \
             --append-system-prompt[-file], --add-dir (CLAUDE.md dirs)",
        );
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(
            adapter.supports_system_prompt_file(&[]),
            "must recognize the bracket-shorthand form the real CLI actually uses"
        );
    }

    #[test]
    fn normalizes_to_advertise_the_file_flag_matches_both_spellings() {
        assert!(normalizes_to_advertise_the_file_flag(
            "--append-system-prompt[-file] <path>"
        ));
        assert!(normalizes_to_advertise_the_file_flag(
            "--append-system-prompt-file <path>"
        ));
        assert!(!normalizes_to_advertise_the_file_flag(
            "--append-system-prompt <prompt>"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_is_false_when_help_omits_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(dir.path(), "probe-no.sh", "nothing relevant here");
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(!adapter.supports_system_prompt_file(&[]));
    }

    #[test]
    fn supports_system_prompt_file_fails_open_when_the_binary_is_missing() {
        let adapter = ClaudeAdapter::new(Some("/nonexistent/definitely-not-a-binary"));
        assert!(
            !adapter.supports_system_prompt_file(&[]),
            "a probe failure must never block a launch"
        );
    }

    /// M7: "probe... once per launch (cache the result in-process across
    /// restarts)". Rewriting the stub after the first probe must not change
    /// the cached answer: a restart inside the same run must never re-spawn
    /// `--help`.
    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_is_cached_after_the_first_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(dir.path(), "probe-cache.sh", "--append-system-prompt-file");
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(adapter.supports_system_prompt_file(&[]));

        let script = dir.path().join("probe-cache.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'nothing now\\n'\n").expect("rewrite stub");
        assert!(
            adapter.supports_system_prompt_file(&[]),
            "the first probe's answer is cached for the life of the process"
        );
    }

    /// Regression: the cache used to be keyed by joining `program` and
    /// `bin_args` into one string, which made two distinct commands collide
    /// on the same key (e.g. `("sh /a", ["--help"])` and `("sh", ["/a",
    /// "--help"])` both joined to `"sh /a --help"`). Same `program` ("sh"),
    /// different `bin_args` (two different scripts, one supporting the flag
    /// and one not) must be cached independently, not share one answer.
    #[cfg(unix)]
    #[test]
    fn the_cache_key_distinguishes_different_bin_args_for_the_same_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supports = dir.path().join("supports.sh");
        std::fs::write(
            &supports,
            "#!/bin/sh\ncat <<'EOF'\n--append-system-prompt-file\nEOF\n",
        )
        .expect("write");
        let unsupported = dir.path().join("unsupported.sh");
        std::fs::write(&unsupported, "#!/bin/sh\ncat <<'EOF'\nnothing here\nEOF\n").expect("write");
        for script in [&supports, &unsupported] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        assert!(
            probe_system_prompt_file_support("sh", &[supports.display().to_string()]),
            "the supporting script's own answer must not be shadowed by the other's"
        );
        assert!(
            !probe_system_prompt_file_support("sh", &[unsupported.display().to_string()]),
            "the non-supporting script must get its own answer, not the cached true from above"
        );
    }

    #[test]
    fn the_prompt_args_compose_with_the_existing_command_builders() {
        let adapter = ClaudeAdapter::new(None);
        let mut extra = adapter.system_prompt_args("be consistent");
        extra.push("--model".to_string());
        extra.push("sonnet".to_string());

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &extra);
        assert_eq!(
            built_args(&adapter, &headless),
            vec![
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--append-system-prompt".to_string(),
                "be consistent".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );

        let interactive = adapter.interactive_cmd(None, &extra);
        assert_eq!(
            built_args(&adapter, &interactive)[0],
            "--append-system-prompt"
        );
    }

    #[test]
    fn structural_context_survives_the_real_fixture() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");
        let recorded = expected["files_touched_min"].as_u64().unwrap_or(0);
        let touched = structural_context(&jsonl, 1_000);
        assert!(
            (touched.files_read.len() + touched.files_modified.len()) as u64 >= recorded,
            "files_read + files_modified should find at least the recorded count"
        );

        let ctx = structural_context(&jsonl, 5);
        assert!(ctx.user_messages.len() <= 5);
        assert!(
            ctx.files_read.len() <= 5 && ctx.files_modified.len() <= 5,
            "and then keep only the tail, like every other field: {} read, {} modified",
            ctx.files_read.len(),
            ctx.files_modified.len()
        );
    }

    /// A handoff leaves as a single argv token, and Windows caps a command
    /// line at 32,767 characters. `files_read`/`files_modified` accumulated
    /// every unique path of the whole session while its neighbours were
    /// capped, so a long enough session could no longer relaunch at all.
    #[test]
    fn structural_context_caps_files_read_like_every_other_field() {
        let mut jsonl = String::new();
        for index in 0..40 {
            jsonl.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{{\"file_path\":\"/src/file-{index}.rs\"}}}}],\"usage\":{{}}}}}}\n"
            ));
        }

        let ctx = structural_context(&jsonl, 5);
        assert_eq!(
            ctx.files_read,
            vec![
                "/src/file-35.rs",
                "/src/file-36.rs",
                "/src/file-37.rs",
                "/src/file-38.rs",
                "/src/file-39.rs"
            ],
            "the newest paths are the ones a handoff is worth carrying"
        );
    }

    #[test]
    fn claude_contributes_the_orchestrator_layer_and_it_names_claudes_own_tools() {
        let layer = ClaudeAdapter::new(None)
            .base_system_prompt()
            .expect("claude has a base layer of its own");
        assert_eq!(layer, ORCHESTRATOR_PROMPT);
        for claude_specific in ["Agent tool", ".claude/agents", "/code-review"] {
            assert!(
                layer.contains(claude_specific),
                "the layer is claude-specific by construction: '{claude_specific}'"
            );
        }
    }

    /// The review bullet must route review to the harness roster's own
    /// configured review model rather than let it silently run on this
    /// seat's own model, must cap fan-out at a single-reviewer effort level,
    /// and the model-routing bullet's "pin" clause must carve out that one
    /// exception rather than blanket-forbid every override.
    #[test]
    fn the_orchestrator_prompt_routes_review_to_the_rosters_configured_model() {
        assert!(
            ORCHESTRATOR_PROMPT.contains("roster's review model"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("runs at low or medium effort"),
            "never a high-or-above fan-out from this seat: {ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains(
                "Agents in .claude/agents that pin their own model keep it, except that reviews \
                 always run on the roster's review model"
            ),
            "the model-routing bullet's pin clause must carve out the review-model exception: \
             {ORCHESTRATOR_PROMPT}"
        );
    }

    /// The specific sentence being corrected: the orchestrator layer used to
    /// say a session carrying the zirv meta-harness layer follows that
    /// layer's cross-harness review round "on top" of its own /code-review.
    /// That instruction is what turned one change into three review rounds.
    #[test]
    fn the_orchestrator_layer_no_longer_stacks_a_review_round_on_top() {
        assert!(
            !ORCHESTRATOR_PROMPT.contains("on top"),
            "the stacking instruction must be gone"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("zirv workflow"),
            "and must instead defer to the workflow gate when one is active"
        );
    }

    /// TASK 1: every Agent-tool dispatch must set the model explicitly, the
    /// seat's own model and fork-type subagents are both off limits, and
    /// token economy applies to both this seat's own replies and every
    /// subagent brief it writes.
    #[test]
    fn the_orchestrator_prompt_encodes_model_routing_and_token_economy() {
        assert!(
            ORCHESTRATOR_PROMPT.contains("Every Agent dispatch sets `model` explicitly"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        for tier in [
            "haiku for mechanical",
            "sonnet for ordinary",
            "opus only for hard",
        ] {
            assert!(
                ORCHESTRATOR_PROMPT.contains(tier),
                "missing tier guidance '{tier}': {ORCHESTRATOR_PROMPT}"
            );
        }
        assert!(
            ORCHESTRATOR_PROMPT.contains("an omitted model inherits this seat"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("Never use `subagent_type: \"fork\"` here"),
            "forks always inherit the seat model and ignore overrides: {ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains(
                "tell the worker to run tests in the FOREGROUND and reply with compact \
                 structured findings, never raw file dumps"
            ),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
    }

    /// Issue #175: the orchestrator layer must default this seat to native
    /// Agent-tool subagents for any bounded task, however substantial, and
    /// reserve a sub-orchestrator for work that genuinely splits into
    /// multiple coherently-scoped areas or must run under zirv's own
    /// supervision independently of this seat -- so a seat stops minting
    /// sub-orchestrators for ordinary tasks a worker could finish.
    #[test]
    fn the_orchestrator_prompt_sizes_delegation_between_subagents_and_sub_orchestrators() {
        assert!(
            ORCHESTRATOR_PROMPT.contains("Trivial and bounded changes stay on this seat"),
            "trivial and bounded work must stay on this seat instead of delegating: \
             {ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("Delegate via the Agent tool"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("zirv ctx agent --role sub-orchestrator --scope"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("several coherently-scoped areas"),
            "sub-orchestrators are reserved for multi-area work: {ORCHESTRATOR_PROMPT}"
        );
    }

    /// The sizing rule is orchestrator-only vocabulary: a Worker never
    /// decides delegation shape at all, and a sub-orchestrator's own
    /// delegation is already capped to Workers by `SUB_ORCHESTRATOR_PROMPT`
    /// itself, so neither layer needs or gets this bullet.
    #[test]
    fn the_worker_and_sub_orchestrator_layers_do_not_gain_the_sizing_rule() {
        for layer in [WORKER_PROMPT, SUB_ORCHESTRATOR_PROMPT] {
            assert!(
                !layer.contains("Size delegation to the job"),
                "only the orchestrator layer decides delegation shape: {layer}"
            );
        }
    }

    /// `exec` strips this many leading tokens off the argv the operator
    /// wrote before carrying the rest into a restart, so a rewrite that
    /// happens inside `base()` must not be counted here: the argv it applies
    /// to is the one the adapter builds, not the one it was handed.
    #[test]
    fn the_launch_prefix_length_counts_the_operators_argv_not_the_rewritten_one() {
        assert_eq!(ClaudeAdapter::new(None).launch_prefix_len(), 1);
        assert_eq!(
            ClaudeAdapter::new(Some("claude.cmd")).launch_prefix_len(),
            1,
            "a .cmd shim is still one program token in the operator's argv"
        );
        assert_eq!(
            ClaudeAdapter::new(Some("sh /tmp/stub.sh")).launch_prefix_len(),
            2
        );
    }

    /// An npm-installed `claude` on Windows is `claude.cmd`, which
    /// `CreateProcessW` rejects with `ERROR_BAD_EXE_FORMAT` (193). The
    /// adapter has to hand it to `cmd.exe` instead, and the tokens it adds
    /// have to lead the ones it was already going to pass.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_launched_through_cmd_exe_with_its_arguments_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let cmd = adapter.interactive_cmd(Some("resume this"), &["--continue".to_string()]);

        assert!(
            cmd.get_program()
                .to_string_lossy()
                .to_lowercase()
                .contains("cmd"),
            "got {:?}",
            cmd.get_program()
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/c".to_string(),
                shim.display().to_string(),
                "resume this".to_string(),
                "--continue".to_string(),
            ]
        );
        assert_eq!(
            adapter.launch_prefix_len(),
            1,
            "and the rewrite never changes what exec strips off the operator's argv"
        );
    }

    /// The rewrite is a Windows concern only: everywhere else the program is
    /// spawned exactly as written, shebang and all.
    #[cfg(not(windows))]
    #[test]
    fn a_program_is_spawned_exactly_as_written_off_windows() {
        let adapter = ClaudeAdapter::new(Some("/opt/claude.cmd"));
        let cmd = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(cmd.get_program().to_string_lossy(), "/opt/claude.cmd");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["resume this".to_string()]);
    }
}
