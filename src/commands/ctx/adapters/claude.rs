use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::input_hash;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, ToolInvocation,
    TranscriptUsage, UNRESOLVED_TOOL_CALL_CAP, UnresolvedToolCall, error_text_hash,
    last_verification_run,
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
///
/// Issues #328/#334 (2026-09-04): the wrapper-behaviour redesign's own
/// "trivial and bounded changes stay on this seat" carve-out turned out to be
/// the wrong fix -- it let an orchestrator seat write tests and
/// implementations itself, which is exactly what this role must never do.
/// That size-based carve-out is gone: an orchestrator seat never implements,
/// a PreToolUse hook enforces it (`hook::run_pretool`,
/// `safety::run_check_hook_mode_with_env`) by denying this seat's own
/// repository writes, so a denial is the cue to dispatch rather than retry
/// another way. Same-harness delegation is the native Agent tool, not `zirv
/// agent`: `zirv agent <name>` now reaches only a DIFFERENT harness
/// (`agent::run_with` refuses a same-harness target from an orchestrator
/// seat) or a work group / sub-orchestrator. Task size now only decides how
/// many workers and how large a brief, never whether this seat implements.
pub const ORCHESTRATOR_PROMPT: &str = "\
zirv orchestrator conventions (claude)

This seat runs the most capable model; spend it on judgment -- sizing, design choices, \
integration, the final call -- never on implementation.

- This seat coordinates; it does not implement. Every repository change -- code, tests, docs, \
manifests, a one-line fix included -- is made by a delegated worker, never by this seat's own \
Edit/Write or a shell write: a PreToolUse hook denies repository writes from this seat, and \
that denial is the cue to dispatch, not to retry another way. Size the task only to decide how \
many workers and how large a brief.
- Routing rule, which outranks any operator or repository layer that says otherwise: \
same-harness delegation uses this harness's native Agent tool (visible in this session, result \
returned directly); `zirv agent <name>` is for reaching a different harness or a work group, never \
for spawning another claude worker from a claude seat -- zirv refuses it from this seat. `zirv ctx \
agent --role sub-orchestrator --scope \"<area>\"` creates a work group for work that splits into \
several coherently-scoped areas each needing its own coordination. Delegated work stays \
observable: a worker attaches as a pane to any live dashboard, otherwise it runs inline in the \
caller's terminal with its result on stdout. Bundle small related items into one checklist brief with a per-item \
output format, dispatch independent work together in the background, and continue a worker you \
already briefed for follow-ups in its area instead of spawning a fresh one.
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
by ONE designated integrator worker; a writer touching one says so in its report. Git \
integration -- branching, merging worker results, committing, opening the PR -- stays on this \
seat.";

/// Everything in [`ORCHESTRATOR_PROMPT`] AFTER its own first (write-guard)
/// bullet -- shared verbatim by [`orchestrator_prompt_for`]'s `Advise`/
/// `Allow` arms, which splice a different first bullet in front of it.
const ORCHESTRATOR_PROMPT_TAIL_AFTER_WRITE_GUARD_BULLET: &str = "\n\
- Routing rule, which outranks any operator or repository layer that says otherwise: \
same-harness delegation uses this harness's native Agent tool (visible in this session, result \
returned directly); `zirv agent <name>` is for reaching a different harness or a work group, never \
for spawning another claude worker from a claude seat -- zirv refuses it from this seat. `zirv ctx \
agent --role sub-orchestrator --scope \"<area>\"` creates a work group for work that splits into \
several coherently-scoped areas each needing its own coordination. Delegated work stays \
observable: a worker attaches as a pane to any live dashboard, otherwise it runs inline in the \
caller's terminal with its result on stdout. Bundle small related items into one checklist brief with a per-item \
output format, dispatch independent work together in the background, and continue a worker you \
already briefed for follow-ups in its area instead of spawning a fresh one.
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
by ONE designated integrator worker; a writer touching one says so in its report. Git \
integration -- branching, merging worker results, committing, opening the PR -- stays on this \
seat.";

/// The same layer as [`ORCHESTRATOR_PROMPT`], but with the write-guard
/// bullet (its own first bullet, above) posture-dependent (issue #358 T8)
/// instead of hardcoded to `deny`'s wording. `Deny` returns
/// [`ORCHESTRATOR_PROMPT`] itself, unchanged -- the exact, already-shipped,
/// already-tested text -- rather than a reconstruction that could drift
/// from it; `Advise`/`Allow` splice `prompt::orchestrator_write_lines`'s
/// shared, adapter-neutral text in front of
/// [`ORCHESTRATOR_PROMPT_TAIL_AFTER_WRITE_GUARD_BULLET`], the same tail
/// [`ORCHESTRATOR_PROMPT`] carries either way. Claude's own PreToolUse hook
/// (`hook::run_pretool`) is what makes `orchestrator_write_lines`'s
/// `Advise` sentence about writes being "recorded" true here (issue #358
/// review, finding #6) -- passed `true`, unlike codex's own splice.
fn orchestrator_prompt_for(posture: super::super::config::OrchestratorWrites) -> String {
    use super::super::config::OrchestratorWrites;
    if posture == OrchestratorWrites::Deny {
        return ORCHESTRATOR_PROMPT.to_string();
    }
    format!(
        "zirv orchestrator conventions (claude)\n\n\
         This seat runs the most capable model; spend it on judgment -- sizing, design \
         choices, integration, the final call -- never on implementation.\n\n\
         - {}{ORCHESTRATOR_PROMPT_TAIL_AFTER_WRITE_GUARD_BULLET}",
        super::super::prompt::orchestrator_write_lines(posture, true)
    )
}

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
raw file contents into it.
- For test, build and log commands, run `zirv ctx run --compact -- <cmd>`: it keeps the full output \
on disk and gives you a summary plus the id to retrieve it.";

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

/// The literal human-typed text of a `user` row's `message.content` (issue
/// #312): the plain-string shape Claude Code writes for an ordinary prompt,
/// or the `text`-block shape a content array carries otherwise. Mirrors
/// `structural_context`'s own user-text extraction (below), factored out
/// here so `parse_events` can size it for `NormalizedEvent::UserText`
/// without a second full parse of the row.
fn user_message_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
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

/// This codebase's own rough token estimate, shared with `compile.rs` and
/// `context_status.rs`: four bytes to a token.
const THINKING_BYTES_PER_TOKEN: u64 = 4;

/// The literal size of an assistant message's own thinking text, or `None`
/// when the row carries no thinking block with any text left in it -- the
/// shape every live transcript now has, which [`reported_thinking_bytes`]
/// answers instead.
fn thinking_text_bytes(message: &Value) -> Option<u64> {
    let total: u64 = message
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
        .filter_map(|b| b.get("thinking").and_then(Value::as_str))
        .map(|t| t.len() as u64)
        .sum();
    (total > 0).then_some(total)
}

/// The response's reported thinking size on the byte scale, for a row whose
/// thinking text was stripped to a bare `signature` (or redacted outright).
/// `None` unless the row actually carries such a block, so a row that never
/// thought is never credited with the response's thinking tokens.
fn reported_thinking_bytes(message: &Value) -> Option<u64> {
    let stripped = message
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .any(|b| match b.get("type").and_then(Value::as_str) {
            Some("thinking") => b.get("signature").is_some(),
            Some("redacted_thinking") => true,
            _ => false,
        });
    if !stripped {
        return None;
    }
    let tokens = message
        .get("usage")?
        .get("output_tokens_details")?
        .get("thinking_tokens")
        .and_then(Value::as_u64)?;
    Some(tokens.saturating_mul(THINKING_BYTES_PER_TOKEN))
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

        // Issue #455: the two structured fields these rows carry alongside
        // the text (`error: "server_error"`, `apiErrorStatus: 429`) are read
        // here and handed to the classifier as hints. The gate stays
        // `isApiErrorMessage` alone: an ordinary assistant row whose prose
        // happens to mention "API Error: 503" must never produce a
        // `ProviderError`, or route health would be poisoned by a session
        // merely talking about an outage.
        if row.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let message = row.get("message").cloned().unwrap_or(Value::Null);
            let hints = super::ProviderErrorHints {
                kind: row.get("error").and_then(Value::as_str),
                status: row.get("apiErrorStatus").and_then(Value::as_u64),
            };
            let text = text_of(&message);
            // Issue #455 (review round 1, finding 2): the row's OWN time,
            // not the clock -- `at_ms` above is already parsed from this
            // row's `timestamp`. `uuid` is claude's own row identity
            // (verified on every row of `claude-real-session.jsonl`);
            // `provider_error_id` falls back to a time-plus-content
            // fingerprint so consecutive retries of one failing turn stay
            // DISTINCT observations while the same row seen twice does not.
            let at = at_ms.map(|ms| ms / 1000);
            let id = row
                .get("uuid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| super::provider_error_id(at_ms, &text));
            events.push(NormalizedEvent::ProviderError {
                class: super::classify_provider_error(&text, hints),
                at,
                id,
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
                    // Issue #312: the literal human-typed text of this turn,
                    // for `breakdown::attribute_window`'s `user_text` bucket
                    // -- a sibling of `TurnStart`, never a field on it, for
                    // the same reason `ToolErrorText` is one (see that
                    // variant's own doc comment).
                    let byte_len = user_message_text(message.get("content")).len() as u64;
                    if byte_len > 0 {
                        events.push(NormalizedEvent::UserText { byte_len });
                    }
                    continue;
                }
                for block in results {
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    events.push(NormalizedEvent::ToolResult { is_error });
                    // Issue #312: the result's own raw content, sized and
                    // hashed for `breakdown::attribute_window`'s dedup and
                    // live/stale accounting -- computed unconditionally
                    // (unlike the error-only `detail` read this replaces)
                    // since every result needs a byte length regardless of
                    // whether it errored.
                    let detail = tool_result_text(block);
                    events.push(NormalizedEvent::ToolResultSize {
                        byte_len: detail.len() as u64,
                        content_hash: input_hash(&detail),
                    });
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
                    if is_error && !detail.is_empty() {
                        events.push(NormalizedEvent::ToolErrorText {
                            hash: error_text_hash(&detail),
                        });
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
                // Issue #312: `text_of` above already drops `thinking`
                // blocks entirely when building `AssistantFinal::text`, so
                // without this sibling event that content is invisible to
                // `breakdown::attribute_window`'s `thinking` bucket.
                //
                // Current Claude Code writes every thinking block with its
                // text stripped to `""` and only a `signature` (or as a
                // `redacted_thinking` block), so that sum is now zero for a
                // live session no matter how much the model thought. The
                // response's own `usage.output_tokens_details.thinking_tokens`
                // still reports the real count; scaled here to the BYTE unit
                // every other `breakdown::attribute_window` weight is in, at
                // this codebase's own 4-bytes-per-token estimate (see
                // `context_status::BYTES_PER_TOKEN`).
                let thinking_bytes = thinking_text_bytes(&message)
                    .or_else(|| reported_thinking_bytes(&message))
                    .unwrap_or(0);
                if thinking_bytes > 0 {
                    events.push(NormalizedEvent::AssistantThinking {
                        byte_len: thinking_bytes,
                    });
                }

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
                        let is_modification = MODIFICATION_TOOLS
                            .iter()
                            .any(|t| name.eq_ignore_ascii_case(t));
                        let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                        events.push(NormalizedEvent::ToolCall {
                            name: name.clone(),
                            input_hash: input_hash(&raw),
                            at_ms,
                        });
                        // Issue #312: the call's file-shaped argument, when
                        // its transcript shape exposes one, so
                        // `breakdown::attribute_window` can mark an earlier
                        // live result STALE once a modifying call names the
                        // same path. A sibling of `ToolCall`, never a field
                        // on it, for the same reason `ToolErrorText` is one.
                        if let Some(path) = block.get("input").and_then(|input| {
                            FILE_KEYS
                                .iter()
                                .find_map(|key| input.get(*key).and_then(Value::as_str))
                        }) {
                            events.push(NormalizedEvent::ToolCallPath {
                                path: path.to_string(),
                                is_modification,
                            });
                        }
                        // Issue #294 (`zirv ctx measure`): a sibling of
                        // `ToolCall`, never a field on it, for the same
                        // reason `ToolCallPath` is one -- see
                        // `NormalizedEvent::ToolCallRead`/`ToolCallEdit`'s
                        // own doc comments.
                        if name.eq_ignore_ascii_case("Read") {
                            let ranged = block.get("input").is_some_and(|input| {
                                input.get("offset").is_some() || input.get("limit").is_some()
                            });
                            events.push(NormalizedEvent::ToolCallRead { ranged });
                        } else if name.eq_ignore_ascii_case("Edit") {
                            if let Some(input) = block.get("input") {
                                let old = input
                                    .get("old_string")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let new = input
                                    .get("new_string")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                events.push(NormalizedEvent::ToolCallEdit {
                                    old_bytes: old.len() as u64,
                                    new_bytes: new.len() as u64,
                                    core_bytes: super::super::measure::core_change_bytes(old, new),
                                });
                            }
                        } else if name.eq_ignore_ascii_case("MultiEdit")
                            && let Some(edits) = block
                                .get("input")
                                .and_then(|input| input.get("edits"))
                                .and_then(Value::as_array)
                        {
                            let mut old_bytes = 0u64;
                            let mut new_bytes = 0u64;
                            let mut core_bytes = 0u64;
                            for edit in edits {
                                let old =
                                    edit.get("old_string").and_then(Value::as_str).unwrap_or("");
                                let new =
                                    edit.get("new_string").and_then(Value::as_str).unwrap_or("");
                                old_bytes += old.len() as u64;
                                new_bytes += new.len() as u64;
                                core_bytes += super::super::measure::core_change_bytes(old, new);
                            }
                            events.push(NormalizedEvent::ToolCallEdit {
                                old_bytes,
                                new_bytes,
                                core_bytes,
                            });
                        }
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

/// The identity of the API response an assistant row belongs to, for
/// [`fold_assistant_usage`]'s dedup. Claude Code >= 2.1.209 splits one
/// response across one transcript row per content block (thinking, text,
/// tool_use), each repeating the response's identical `usage` object under
/// the same `message.id`. `requestId` is the fallback for a row carrying no
/// message id; `None` means this row has no response identity at all and is
/// counted on its own, which is exactly the pre-split behaviour.
pub fn response_identity(row: &Value) -> Option<&str> {
    row.get("message")
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .or_else(|| row.get("requestId").and_then(Value::as_str))
}

/// The shared fold behind [`transcript_usage`] and [`sidechain_transcript_usage`]:
/// every assistant row whose `isSidechain` flag matches `want_sidechain`,
/// summed into the four raw classes. One fold, two filters, so the main and
/// sidechain readers can never drift on what counts as an assistant usage
/// row.
///
/// Usage is folded once per API RESPONSE, not once per row: consecutive rows
/// sharing a [`response_identity`] repeat one response's own usage object, so
/// only the first of a run contributes. Tracking the last-seen id rather than
/// a set keeps this streaming-safe over an append-only transcript.
fn fold_assistant_usage(jsonl: &str, want_sidechain: bool) -> Option<TranscriptUsage> {
    fold_usage_rows(jsonl, |row| {
        (row.get("isSidechain").and_then(Value::as_bool) == Some(true)) == want_sidechain
    })
}

/// The fold itself, over every `assistant` row `keep` accepts.
fn fold_usage_rows(jsonl: &str, keep: impl Fn(&Value) -> bool) -> Option<TranscriptUsage> {
    let mut usage = TranscriptUsage::default();
    let mut observed = false;
    let mut last_id: Option<String> = None;
    for line in jsonl.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") || !keep(&row) {
            continue;
        }
        let Some(current) = row.get("message").and_then(|message| message.get("usage")) else {
            continue;
        };
        observed = true;
        let id = response_identity(&row).map(str::to_string);
        if id.is_some() && id == last_id {
            continue;
        }
        last_id = id;
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

/// How many subagent transcripts one call will open, and how many of their
/// bytes it will read. Bounds a directory that accumulates one file per
/// dispatch for the life of a session; a phase that overruns either bound
/// reports what it read rather than stalling the caller.
/// `pub(crate)`: `session_spend::session_transcript_usage` (issue #457)
/// applies the identical newest-first cap to the same `subagents/`
/// directory when folding a session's own total spend, and reuses these
/// exact bounds rather than picking its own.
pub(crate) const MAX_SUBAGENT_TRANSCRIPTS: usize = 256;
pub(crate) const MAX_SUBAGENT_BYTES: u64 = 32 * 1024 * 1024;

/// The modern home of subagent spend (2026-09-06). Current Claude Code writes
/// NO `isSidechain` rows into the main transcript at all -- 0 of 15,510 rows
/// across twelve recorded real sessions -- so [`sidechain_transcript_usage`]'s
/// in-file fold is now a legacy branch that answers `None` for every live
/// session. Subagent turns live in sibling files instead:
/// `<transcript-dir>/<session-id>/subagents/agent-<id>.jsonl`, whose rows do
/// carry `isSidechain: true`.
///
/// `main_range` is the caller's own phase slice of the MAIN transcript; its
/// first parseable `timestamp` is the phase boundary this fold floors at, so
/// the answer keeps the "since the checkpoint" meaning the byte-range read
/// gave the legacy branch. A range with no parseable timestamp yields `None`
/// -- an honest "cannot place this window", never the whole session's subagent
/// spend attributed to one phase. A subagent row with no timestamp of its own
/// cannot be placed either and is skipped, the same convention
/// `window::sum_file` already applies.
pub fn subagent_transcript_usage(transcript: &Path, main_range: &str) -> Option<TranscriptUsage> {
    let since_ms = first_timestamp_ms(main_range)?;
    let dir = subagents_dir(transcript)?;
    let mut usage = TranscriptUsage::default();
    let mut observed = false;
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            Some((
                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                meta.len(),
                path,
            ))
        })
        .collect();
    // Newest first, so a session whose directory outgrows the caps keeps the
    // files a recent phase can actually have written to. Ordering only: which
    // rows count is decided by `since_ms` against each row's own timestamp,
    // never by a file's mtime.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    for (_, len, path) in entries {
        if files >= MAX_SUBAGENT_TRANSCRIPTS || bytes >= MAX_SUBAGENT_BYTES {
            break;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        files += 1;
        bytes = bytes.saturating_add(len);
        let Some(file_usage) = fold_usage_rows(&body, |row| {
            row.get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_iso8601_utc_ms)
                .is_some_and(|at| at >= since_ms)
        }) else {
            continue;
        };
        observed = true;
        usage.input_tokens = usage.input_tokens.saturating_add(file_usage.input_tokens);
        usage.cache_creation_input_tokens = usage
            .cache_creation_input_tokens
            .saturating_add(file_usage.cache_creation_input_tokens);
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(file_usage.cache_read_input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(file_usage.output_tokens);
    }
    observed.then_some(usage)
}

/// `<transcript-dir>/<session-id>/subagents`, derived from the main
/// transcript's own path rather than recomputed from a `SessionRef`, so the
/// scan-fallback path `transcript_path` may have resolved is honoured.
///
/// `pub(crate)`: `session_spend::session_transcript_usage` (issue #457)
/// reuses this exact derivation rather than recomputing it, so the two can
/// never disagree about where a session's native-subagent transcripts live.
pub(crate) fn subagents_dir(transcript: &Path) -> Option<PathBuf> {
    let stem = transcript.file_stem()?;
    Some(transcript.parent()?.join(stem).join("subagents"))
}

/// The first parseable row `timestamp` in `jsonl`, in unix milliseconds.
fn first_timestamp_ms(jsonl: &str) -> Option<u64> {
    jsonl.lines().find_map(|line| {
        serde_json::from_str::<Value>(line.trim())
            .ok()?
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc_ms)
    })
}

const FILE_KEYS: &[&str] = &["file_path", "notebook_path", "path"];
/// Tool names whose file-key argument is a modification, not a read (issue
/// #280). Anything else -- `Read`/`Grep`/`Glob`, or a tool this codebase does
/// not recognise -- lands in `files_read` instead, the conservative
/// direction: claiming a file was edited when it was not is the damaging
/// error, never the reverse.
const MODIFICATION_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
const ERROR_SNIPPET: usize = 200;

/// A single line describing one `tool_use` block's own input, for
/// [`UnresolvedToolCall::summary`] (issue #455): the command for `Bash`, the
/// path for a file-shaped tool, its `description` otherwise, and -- when
/// none of those keys is present -- the first string value in the input
/// object (`serde_json`'s default, non-`preserve_order` `Map` sorts by key,
/// so this is deterministic even though it is not encounter order), falling
/// back to the bare tool name when the input carries no string at all.
/// Redacted and capped by the caller (`adapters::redacted_tool_summary`),
/// never rendered raw.
fn describe_tool_input(tool_name: &str, input: &Value) -> String {
    if tool_name.eq_ignore_ascii_case("Bash")
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        return command.to_string();
    }
    for key in FILE_KEYS {
        if let Some(path) = input.get(*key).and_then(Value::as_str) {
            return path.to_string();
        }
    }
    if let Some(description) = input.get("description").and_then(Value::as_str) {
        return description.to_string();
    }
    input
        .as_object()
        .and_then(|obj| obj.values().find_map(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| tool_name.to_string())
}

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

/// This adapter's own vendor slug in `catalogue`'s registry (issue #381):
/// claude's ladder, strengths, windows and prices all now live there rather
/// than as literals in this file.
const CATALOGUE_VENDOR: &str = "anthropic";

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
    // Issue #455: every `tool_use` id seen, keyed by its own id, removed the
    // moment a `tool_result` for it arrives -- whatever is left at the end
    // never resolved within the scanned range. `seq` records encounter
    // order (a `HashMap` does not) so the final list can still be rendered
    // oldest-first, newest-last, like every other capped field.
    let mut pending_calls: HashMap<String, (usize, String, String)> = HashMap::new();
    let mut call_seq: usize = 0;
    // Every modification-shaped tool_use call's id that ever named a given
    // path, by that path -- ALL of them, not just the first: a path is
    // unconfirmed if ANY call that touched it is still unresolved at the
    // end of the scan, regardless of order (review finding: a path first
    // touched by a call that later resolved, then touched again by one that
    // never did, must still end up unconfirmed -- the unsafe direction is
    // claiming a write landed when the LAST attempt on it never reported
    // back).
    let mut path_source_calls: HashMap<String, Vec<String>> = HashMap::new();
    // The most recent tool_use's own name, for the "after tool call X"
    // clause in a `tail_cut` reason -- rolling state because the row that
    // reveals the cut (a provider-error row) carries no tool reference of
    // its own.
    let mut last_tool_name: Option<String> = None;
    // Issue #455 review round 2: text pushed by an assistant row whose own
    // turn has not yet reached a boundary (a `tool_result`/user row, or a
    // successful `end_turn`) -- the ONLY text a cut can legitimately mark
    // partial. Flushed into `assistant_texts` the moment a boundary is
    // reached (a prior reply demonstrably was not the one cut), and moved
    // into `out.partial_text` instead when the cut itself arrives, so an
    // unrelated, already-closed reply from earlier in the transcript is
    // never the one withheld.
    let mut pending_open_text: Option<String> = None;

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

        // Issue #455: gated on `isApiErrorMessage` alone, exactly like
        // `parse_events` above -- never on the row's own text, so an
        // ordinary assistant reply that merely QUOTES "API Error: 503"
        // can never be mistaken for one. Handled before the `type` match
        // below (an API-error row's own `type` is `"assistant"`) so its
        // placeholder text ("API Error: ...") never reaches
        // `assistant_texts` and gets shown as though it were a finished
        // reply -- exactly the bug this issue reports.
        if row.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let kind = row
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            out.tail_cut = Some(match last_tool_name.as_deref() {
                Some(name) => format!("API error ({kind}) after tool call {name}"),
                None => format!("API error ({kind})"),
            });
            // Issue #455 review round 2: only text still OPEN at this exact
            // moment is the cut turn's own -- `None` when the cut turn
            // carried no text of its own (e.g. a bare tool_use), which must
            // never be confused with "nothing was cut".
            out.partial_text = pending_open_text
                .take()
                .map(|raw| super::redacted_tool_summary(&raw));
            continue;
        }

        let message = row.get("message").cloned().unwrap_or(Value::Null);

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                // Issue #455 review round 2: a user-type row (a fresh
                // prompt, or a tool_result) is a turn boundary -- its mere
                // presence proves the assistant text still open before it
                // was not cut, so it settles into `assistant_texts` rather
                // than staying eligible to be marked partial later.
                if let Some(prev) = pending_open_text.take() {
                    out.assistant_texts.push(prev);
                }
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
                            let tool_use_id = block.get("tool_use_id").and_then(Value::as_str);
                            if let Some(command) =
                                tool_use_id.and_then(|id| pending_bash.remove(id))
                            {
                                invocations.push(ToolInvocation {
                                    command,
                                    is_error,
                                    error_text: if is_error { detail } else { String::new() },
                                });
                            }
                            if let Some(id) = tool_use_id {
                                pending_calls.remove(id);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                // Issue #455 review round 2: this row is itself further
                // activity, so whatever was still open from an EARLIER row
                // demonstrably was not the one cut -- settle it before
                // deciding what this row's own text does.
                if let Some(prev) = pending_open_text.take() {
                    out.assistant_texts.push(prev);
                }
                // A later, genuine assistant row supersedes any earlier
                // cut: whatever `tail_cut`/`partial_text` read after the
                // whole scan is the state as of the END of the range, which
                // is exactly what `handoff::structural` needs.
                out.tail_cut = None;
                out.partial_text = None;
                let text = text_of(&message);
                // `end_turn` is this row's own boundary -- a complete reply
                // on its own, never held open even for a single row.
                let is_end_turn =
                    message.get("stop_reason").and_then(Value::as_str) == Some("end_turn");
                if !text.trim().is_empty() {
                    if is_end_turn {
                        out.assistant_texts.push(text);
                    } else {
                        pending_open_text = Some(text);
                    }
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
                    last_tool_name = Some(tool_name.to_string());
                    let id = block.get("id").and_then(Value::as_str);

                    let is_modification = MODIFICATION_TOOLS
                        .iter()
                        .any(|t| tool_name.eq_ignore_ascii_case(t));
                    let target = if is_modification {
                        &mut out.files_modified
                    } else {
                        &mut out.files_read
                    };
                    for key in FILE_KEYS {
                        if let Some(path) = input.get(*key).and_then(Value::as_str) {
                            // Recorded for EVERY modification call that
                            // names this path, not only the one that ends
                            // up pushed below -- a later call on an
                            // already-listed path must still be able to
                            // mark it unconfirmed.
                            if is_modification && let Some(id) = id {
                                path_source_calls
                                    .entry(path.to_string())
                                    .or_default()
                                    .push(id.to_string());
                            }
                            if !target.iter().any(|p| p == path) {
                                target.push(path.to_string());
                            }
                        }
                    }
                    let is_bash = tool_name.eq_ignore_ascii_case("Bash");
                    if is_bash
                        && let (Some(id), Some(command)) =
                            (id, input.get("command").and_then(Value::as_str))
                    {
                        pending_bash.insert(id.to_string(), command.to_string());
                    }

                    if let Some(id) = id {
                        let summary =
                            super::redacted_tool_summary(&describe_tool_input(tool_name, input));
                        call_seq += 1;
                        pending_calls
                            .insert(id.to_string(), (call_seq, tool_name.to_string(), summary));
                    }
                }
            }
            _ => {}
        }
    }

    // Issue #455 review round 2: whatever is still open at the end of the
    // scanned range was never claimed by a cut (that path already moved it
    // into `partial_text` and cleared this), so it is a normal, uncut reply
    // -- the pre-#455 behaviour for a session that simply ends there.
    if let Some(text) = pending_open_text.take() {
        out.assistant_texts.push(text);
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

    // Issue #455: `pending_calls` left over is every call whose result never
    // arrived in the scanned range. `unresolved_ids` (the FULL set, before
    // the display cap below) drives the `files_modified` unconfirmed check,
    // so a call old enough to be dropped from the rendered list still marks
    // its file -- the file is no less unconfirmed for not being individually
    // listed.
    let mut unresolved: Vec<(usize, String, String, String)> = pending_calls
        .into_iter()
        .map(|(id, (seq, name, summary))| (seq, id, name, summary))
        .collect();
    unresolved.sort_by_key(|(seq, ..)| *seq);
    let unresolved_ids: std::collections::HashSet<&str> =
        unresolved.iter().map(|(_, id, ..)| id.as_str()).collect();
    out.unconfirmed_files_modified = out
        .files_modified
        .iter()
        .filter(|path| {
            path_source_calls
                .get(*path)
                .is_some_and(|ids| ids.iter().any(|id| unresolved_ids.contains(id.as_str())))
        })
        .cloned()
        .collect();
    if unresolved.len() > UNRESOLVED_TOOL_CALL_CAP {
        unresolved.drain(..unresolved.len() - UNRESOLVED_TOOL_CALL_CAP);
    }
    out.unresolved_tool_calls = unresolved
        .into_iter()
        .map(|(_, id, name, summary)| UnresolvedToolCall { name, id, summary })
        .collect();

    out
}

fn keep_last<T>(items: &mut Vec<T>, last_n: usize) {
    if items.len() > last_n {
        items.drain(..items.len() - last_n);
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    /// Issue #395: an operator-only `[endpoint.claude]` override, attached
    /// post-construction via `AgentAdapter::apply_endpoint` (production) or
    /// `with_endpoint` (tests/direct construction) -- never set from a repo
    /// layer, see `config.rs`'s `REPO_FORBIDDEN` entry for `endpoint`.
    endpoint: Option<super::super::config::EndpointTarget>,
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
            endpoint: None,
            #[cfg(test)]
            forced_file_support: None,
            #[cfg(test)]
            forced_launch_settings: Some(Some(PathBuf::from(
                "zirv-test-claude-launch-settings.json",
            ))),
        }
    }

    /// Issue #395: attaches an operator `[endpoint.claude]` override.
    /// Production code reaches this through `AgentAdapter::apply_endpoint`
    /// (see `adapters::apply_endpoint_override`), mirroring `with_home`/
    /// `with_ignore_flags_forced`-style test seams below; this builder is
    /// only the direct-construction path tests use.
    #[cfg(test)]
    pub fn with_endpoint(mut self, endpoint: super::super::config::EndpointTarget) -> Self {
        self.endpoint = Some(endpoint);
        self
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
        // Issue #395: an operator `[endpoint.claude]` override retargets
        // this launch at an Anthropic-compatible vendor endpoint instead of
        // claude's own native account. `AgentAdapter::ready()` (called
        // before any command built from `base()` is ever spawned -- see
        // `adapters::select`/`resolve_default`) already refused the launch
        // if `credential_env` is unset/empty, so reading it here is safe;
        // this is nonetheless a fail-soft read (never a panic) for any
        // caller that reaches `base()` without going through `ready()`
        // first (a unit test building a `Command` directly, say).
        if let Some(ep) = &self.endpoint {
            cmd.env("ANTHROPIC_BASE_URL", &ep.base_url);
            if let Ok(value) = std::env::var(&ep.credential_env) {
                cmd.env("ANTHROPIC_AUTH_TOKEN", value);
            }
        }
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
    fn launch_settings_path(
        &self,
        sandbox: &super::super::config::SandboxConfig,
        safety: &super::super::safety::SafetyPolicy,
    ) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(forced) = &self.forced_launch_settings {
            return forced.clone();
        }

        let dir = self.home_dir().join(".zirv").join("runtime");
        let fingerprint = super::super::safety::policy_fingerprint(safety).ok()?;
        let policy_dir = dir.join("policies");
        let policy_path = policy_dir.join(format!("{fingerprint}.json"));
        let path = dir.join(format!("claude-launch-settings-{fingerprint}.json"));
        let mut launch_environment = LaunchEnvironment::resolve();
        launch_environment.scrub_subprocess_env = sandbox.scrub_subprocess_env;
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
///
/// Issue #334: `launch_settings_value`'s own `PreToolUse` array also carries
/// an `Edit|Write|MultiEdit|NotebookEdit` matcher running `zirv ctx hook
/// pretool` -- the orchestrator-write guard that makes an orchestrator seat
/// technically unable to edit repository files itself. It sits alongside,
/// not instead of, an `Agent|Task` matcher running the same command: that
/// entry attests the existing expensive-seat-inheritance guard on every
/// launch, rather than depending on a one-time `zirv setup apply` having
/// installed it into the operator's own global settings first.
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
    /// Issue #329: the launch repository's own linked worktrees, then its
    /// sibling checkouts and theirs ([`sibling_repo_roots`]). Worktrees were
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
    /// `[sandbox] scrub_subprocess_env` -- see `SandboxConfig`'s doc comment
    /// for what the upstream switch does and why it is off by default.
    scrub_subprocess_env: bool,
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
            .and_then(|repo| std::fs::canonicalize(repo).ok())
            .map(|repo| {
                let mut roots = linked_worktree_roots(&repo);
                let home = crate::utils::home_dir().ok();
                for root in sibling_repo_roots(&repo, home.as_deref()) {
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                roots
            })
            .unwrap_or_default()
            .iter()
            .map(|path| grant_path(path))
            .collect();
        #[cfg(test)]
        let workspace_write_roots = Vec::new();

        Self {
            state_write_root,
            scratchpad_roots,
            unix_sockets,
            ssh_auth_sock,
            workspace_write_roots,
            scrub_subprocess_env: false,
        }
    }
}

const MAX_SIBLING_REPOS: usize = 32;

/// Issue #329 item 2 (the biggest single source of prompts in that report,
/// 9 of 21): the launch repository's sibling checkouts -- every direct child
/// of the repo's parent directory that is itself a git checkout (a `.git`
/// directory, or the `.git` file of a linked worktree) -- followed by each
/// sibling's own linked worktrees. A cross-repo change (`crm` plus the
/// `marketing-automation-client` library it calls, plus a worktree of the
/// service that serves it) is ordinary work in a services directory, and
/// every gate in the other two checkouts failed `Operation not permitted`
/// under the sandbox until the operator retried it unsandboxed.
///
/// Scope, deliberately narrow: only git checkouts, never the parent itself
/// (a non-repo directory next to the launch repo stays untouched); nothing
/// when the parent is a filesystem root (`/workspace/repo`, `C:\repo` -- the
/// container/CI layout, where "siblings" would mean every path on the
/// machine) or the home directory (`~/repo` -- "never the whole home", and
/// its children are not a workspace). Capped at [`MAX_SIBLING_REPOS`] in
/// sorted order, [`MAX_LINKED_WORKTREES`] per sibling.
///
/// A sibling's worktrees come from `<sibling>/.git/worktrees/*/gitdir` --
/// the file git itself keeps, naming `<worktree>/.git` -- rather than one
/// `git worktree list` per sibling: a services directory can hold dozens of
/// checkouts, and this runs on every launch. A stale entry whose worktree
/// is gone no longer canonicalises and is skipped, the same way `git
/// worktree prune` would drop it.
///
/// Trust boundary (codex review on #329): a sibling checkout is repo-owned,
/// so nothing it can write may widen the grant beyond itself. A `gitdir`
/// file is only believed when the named worktree's own `.git` file points
/// BACK at that exact `.git/worktrees/<name>` entry -- the mutual link git
/// maintains -- so a sibling cannot nominate `~` or `/` as its "worktree".
/// A sibling reached through a symlink is skipped (its canonical path must
/// be a direct child of the parent), so a `link -> ~` entry with a `.git`
/// inside cannot grant the whole home. Every root additionally passes
/// [`is_grantable_root`]: never a filesystem root, never the home directory
/// or an ancestor of it, never the launch repo or an ancestor of it. Without
/// a resolvable home directory nothing is granted at all (fail closed).
///
/// Siblings are write grants and `additionalDirectories` only, never
/// `--add-dir`: `--add-dir` also loads that directory's own `.claude/`
/// hooks and skills, and another checkout's repo-owned hooks must not run
/// in this session (the launch repo's own worktrees keep `--add-dir`, since
/// they share its `.claude/`).
fn sibling_repo_roots(canonical_repo: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Some(parent) = canonical_repo.parent() else {
        return Vec::new();
    };
    let Some(home) = home else {
        return Vec::new();
    };
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if parent.parent().is_none() || home == parent {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut siblings: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").exists())
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter(|path| path.parent() == Some(parent))
        .filter(|path| is_grantable_root(path, canonical_repo, &home))
        .collect();
    siblings.sort();
    siblings.dedup();
    siblings.truncate(MAX_SIBLING_REPOS);

    let mut roots: Vec<PathBuf> = Vec::new();
    for sibling in siblings {
        let worktrees = linked_worktrees_from_git_dir(&sibling);
        if !roots.contains(&sibling) {
            roots.push(sibling);
        }
        for worktree in worktrees {
            if is_grantable_root(&worktree, canonical_repo, &home) && !roots.contains(&worktree) {
                roots.push(worktree);
            }
        }
    }
    roots
}

/// Whether `candidate` (canonical) may become a sandbox write root on the
/// launch repo's behalf: not a filesystem root, not the home directory or
/// any ancestor of it, and neither the launch repo, one of its ancestors,
/// nor a path inside it (those are covered -- or deliberately not -- by the
/// repo's own grant).
fn is_grantable_root(candidate: &Path, canonical_repo: &Path, canonical_home: &Path) -> bool {
    candidate.parent().is_some()
        && !canonical_home.starts_with(candidate)
        && !canonical_repo.starts_with(candidate)
        && !candidate.starts_with(canonical_repo)
}

/// The linked worktrees of `repo` as git records them, without a process:
/// each `<repo>/.git/worktrees/<name>/gitdir` holds the absolute path of
/// that worktree's `.git` file, whose parent is the worktree root. A checkout
/// that is itself a linked worktree (`.git` is a file) has no such directory
/// and yields nothing; its main checkout, if it is a sibling too, carries
/// the list. An entry counts only when the worktree's `.git` file names
/// this same `<repo>/.git/worktrees/<name>` directory back (see
/// [`sibling_repo_roots`]).
fn linked_worktrees_from_git_dir(repo: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(repo.join(".git").join("worktrees")) else {
        return Vec::new();
    };
    let mut worktrees: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let entry_dir = std::fs::canonicalize(entry.path()).ok()?;
            let gitdir = std::fs::read_to_string(entry_dir.join("gitdir")).ok()?;
            let root = std::fs::canonicalize(Path::new(gitdir.trim()).parent()?).ok()?;
            let back_link = std::fs::read_to_string(root.join(".git")).ok()?;
            let back_link = back_link.trim().strip_prefix("gitdir:")?.trim();
            let back_link = std::fs::canonicalize(root.join(back_link)).ok()?;
            (back_link == entry_dir).then_some(root)
        })
        .filter(|root| !root.starts_with(repo))
        .collect();
    worktrees.sort();
    worktrees.dedup();
    worktrees.truncate(MAX_LINKED_WORKTREES);
    worktrees
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
            }, {
                "matcher": "Edit|Write|MultiEdit|NotebookEdit",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook pretool"
                }]
            }, {
                "matcher": "Agent|Task",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook pretool"
                }]
            }],
            // Issue #326: compact output. Runs AFTER the tool, on the tool's
            // own result, and replaces it via `updatedToolOutput` -- the full
            // output is stored verbatim under the state dir first, so this is
            // compression, never loss. Deliberately a separate event from the
            // `Bash|PowerShell` PreToolUse entry above: nothing here can
            // touch a permission decision.
            "PostToolUse": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook posttool"
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
            super::super::safety::POLICY_FINGERPRINT_ENV: fingerprint,
            super::super::safety::POLICY_SNAPSHOT_ENV: policy_path.display().to_string()
        }
    });
    // Operator opt-in only (`[sandbox] scrub_subprocess_env`): the upstream
    // switch strips `SSH_AUTH_SOCK` and its kin from every subprocess and
    // forces the permission mode to `default` -- see `SandboxConfig`.
    if launch_environment.scrub_subprocess_env {
        settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"] = serde_json::json!("1");
    }

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

    // Request Claude's OS sandbox when Claude Code can provide it (macOS,
    // Linux and WSL2, not native Windows). Linux needs bubblewrap (`bwrap`)
    // and socat; if missing, Claude Code warns and runs without OS sandboxing.
    // Zirv's `--permission-mode default`, allowed/disallowed tools and
    // `zirv ctx safety check` PreToolUse hook still apply.
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
            "failIfUnavailable": false,
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
/// working-directory set. Other repositories stay out of `--add-dir` (it
/// would load their own `.claude/` hooks); sibling checkouts get sandbox
/// write grants and `additionalDirectories` instead, via
/// [`sibling_repo_roots`] (issue #329). Discovery is best-effort, bounded,
/// and never invokes a shell.
#[cfg(not(test))]
fn linked_worktree_args(repo: &Path) -> Vec<String> {
    linked_worktree_roots(repo)
        .into_iter()
        .flat_map(|path| ["--add-dir".to_string(), grant_path(&path)])
        .collect()
}

/// Renders a discovered worktree or sibling root for Claude's own settings
/// and argv. Discovery canonicalizes, which on Windows yields a `\\?\`
/// verbatim path; Claude Code reads that prefix as a network share and
/// rejects the directory at startup ("is a network path, which cannot be
/// added as a working directory"), one warning per grant, so every sibling
/// grant from issue #329 was refused on Windows. The verbatim prefix is
/// only ever needed for Win32 calls, never for a path handed to another
/// program, so it is stripped here.
fn grant_path(path: &Path) -> String {
    super::super::state::display_path(path)
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
    ///
    /// Issue #395: an operator `[endpoint.claude]` override retargets this
    /// away from claude's own native account -- usage/spend then lands under
    /// the endpoint vendor's own catalogue slug instead, so `StateDir::
    /// usage_for` and the price ledger attribute it correctly. Load-time
    /// validation (`config.rs`'s `validate_endpoint_target`) already proved
    /// `vendor` names a real catalogue vendor, so this lookup is infallible
    /// in practice; a failure still falls back to the native account rather
    /// than panicking, since a stale in-memory config outliving a catalogue
    /// change is cheap insurance, not a real expected path.
    fn provider(&self) -> &'static str {
        if let Some(ep) = &self.endpoint
            && let Some(vendor) = catalogue::vendor(&ep.vendor)
        {
            return vendor.slug;
        }
        "anthropic"
    }

    /// The one thing that can make this adapter unusable before it is asked
    /// to do anything: a program that resolves to a file this OS has no way
    /// to execute. Reported here, by name, rather than left to surface as a
    /// raw `os error 193` out of the spawn. A program that resolves to
    /// nothing at all is not an error here: that is the OS's own
    /// "not found", raised at spawn time where it has always been raised.
    ///
    /// Issue #395: also refuses (naming only the environment variable's
    /// NAME, never a value) when an `[endpoint.claude]` override is
    /// attached and its `credential_env` is unset or empty -- before any
    /// child is ever spawned, exactly like the missing-binary case above.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        if let Some(ep) = &self.endpoint {
            super::require_endpoint_credential(ep)?;
        }
        Ok(())
    }

    /// Issue #395: the production seam (`adapters::apply_endpoint_override`,
    /// called by `select`/`resolve_default`) that attaches a resolved
    /// `[endpoint.claude]` target after construction.
    fn apply_endpoint(&mut self, endpoint: Option<&super::super::config::EndpointTarget>) {
        self.endpoint = endpoint.cloned();
    }

    fn endpoint_vendor(&self) -> Option<&str> {
        self.endpoint.as_ref().map(|ep| ep.vendor.as_str())
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

    fn base_system_prompt(
        &self,
        posture: super::super::config::OrchestratorWrites,
    ) -> Option<String> {
        Some(orchestrator_prompt_for(posture))
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
    /// Review finding (#395 follow-up): the `--model` here now goes through
    /// `model_args`, exactly like every other `--model` emission on this
    /// adapter -- without that, an `[endpoint.claude]` override pinned the
    /// interactive/headless launches to the endpoint vendor's own ladder but
    /// left this one sending claude's native cheap alias (`"haiku"`)
    /// straight to that endpoint, where it is not a valid model at all.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .args(self.model_args(model))
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
    /// per-adapter (see `resolve_distiller_model` in `handoff.rs`). Now the
    /// catalogue's `Cheap` tier (`"haiku"`) rather than a literal here, but
    /// still specific to claude by construction: a hardcoded model name from
    /// one agent's lineup has no business leaking into another adapter's
    /// default.
    fn default_distiller_model(&self) -> Option<&'static str> {
        catalogue::vendor(CATALOGUE_VENDOR)
            .and_then(|v| catalogue::tier_model(v, catalogue::Tier::Cheap))
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
    /// Claude's OS sandbox in auto-allow mode when available; Claude Code
    /// warns and runs without it if unavailable. The sandbox denies common
    /// credential paths to Bash; the launch settings also deny them to the
    /// built-in Read tool and scrub cloud credentials from child environments.
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
        if let Some(path) = self.launch_settings_path(sandbox, safety) {
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
    /// far pricier model than the delegated task actually needs. The
    /// catalogue's `Standard` tier (`"sonnet"`) is the user-approved hard
    /// default that stops that, used only when the operator has not set
    /// `worker.claude` explicitly (see `adapters::resolve_worker_model`).
    fn default_worker_model(&self) -> Option<&'static str> {
        catalogue::vendor(CATALOGUE_VENDOR)
            .and_then(|v| catalogue::tier_model(v, catalogue::Tier::Standard))
    }

    /// Claude's own model ladder, top to bottom: `fable`/`mythos` (the
    /// orchestrator-tier aliases), `opus`, `sonnet`, `haiku` -- now data in
    /// `catalogue` (issue #381) rather than a hand-written ladder here.
    /// Matched by substring on `seat`, lowercased first so
    /// `"claude-Opus-4-5"` and a bare `"opus"` both hit the same rung
    /// regardless of case (so `"claude-fable-5"` and a bare `"fable"` both
    /// hit the fable rung) rather than exact equality, since a seat string
    /// can carry a full id (`claude-opus-4-1`) or a bare alias. `haiku` is
    /// already the floor, so it maps to itself instead of falling off the
    /// ladder; an absent or unrecognised seat assumes the top tier, same as
    /// `AgentAdapter::review_model_below`'s own doc comment requires -- the
    /// deliberate consequence is that the computed default can then resolve
    /// to a model *more expensive* than the seat actually in use (an
    /// accepted spend-up default; the operator can override it with
    /// `[review]` or by setting `chat.model`). See `catalogue::rung_below`'s
    /// own doc comment for why that "assume the top tier" answer is `opus`,
    /// not `fable`.
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        catalogue::vendor(CATALOGUE_VENDOR)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("opus")
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor(CATALOGUE_VENDOR).and_then(|v| catalogue::strength(v, model))
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
            // Issue #418: the ORIGINAL surface these hooks are named for --
            // `hook::run_pretool`/`hook::run_posttool` and `safety::run_check`
            // all parse claude's own PreToolUse/PostToolUse payload shapes
            // directly, no projection involved.
            pre_tool_hook: true,
            post_tool_hook: true,
        }
    }

    /// Claude reports a per-model capacity (issue #155): the long-window
    /// `[1m]` / `-1m` marker reports `LONG_CONTEXT_WINDOW_TOKENS`, and every
    /// other id -- including an unstated model -- gets the conservative
    /// `DEFAULT_CONTEXT_WINDOW_TOKENS` (the catalogue's own `anthropic`
    /// vendor default, which is the same number). The `[1m]`/`-1m` check
    /// stays here rather than in `catalogue` (issue #381): it is a claude-
    /// specific marker convention layered on top of the catalogue's window,
    /// not part of the ladder itself. See `DEFAULT_CONTEXT_WINDOW_TOKENS`'s
    /// own doc comment for why an overstated capacity is the worse failure
    /// mode.
    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let lower = model.map(str::to_lowercase);
        if lower
            .as_deref()
            .is_some_and(|m| m.contains("[1m]") || m.contains("-1m"))
        {
            return Some(LONG_CONTEXT_WINDOW_TOKENS);
        }
        Some(
            catalogue::vendor(CATALOGUE_VENDOR)
                .and_then(|v| catalogue::context_window(v, model))
                .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS),
        )
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): `--model
    /// <MODEL>` is a real flag.
    ///
    /// Issue #395: under an `[endpoint.claude]` override, `model` is pinned
    /// through `EndpointTarget::pin_model` first -- a requested model
    /// (`chat.model`, a handoff tier, an operator `--model`) is honoured
    /// only when it resolves on the ENDPOINT vendor's own ladder, otherwise
    /// it is replaced by that vendor's own default, so a claude alias like
    /// `opus` can never reach a GLM/DeepSeek/etc. endpoint.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), self.pin_model_for_endpoint(model)]
    }

    /// Review finding (#395 follow-up): the shared pinning `model_args`
    /// above and `distiller_cmd` now both route every `--model` through,
    /// and `review_roster_line` routes its advisory text through too, so
    /// the roster's displayed review model can never name a model the
    /// actual launch would replace.
    fn pin_model_for_endpoint(&self, model: &str) -> String {
        match &self.endpoint {
            Some(ep) => ep.pin_model(Some(model)),
            None => model.to_string(),
        }
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

    /// `claude --resume <session-id> "<query>"` is claude's own documented
    /// shape ("Resume session by ID" with a query, CLI reference), which is
    /// what lets a return to a parked conversation carry the interim
    /// harness's handoff packet in the same launch instead of choosing
    /// between continuity and context.
    fn resume_accepts_prompt(&self) -> bool {
        true
    }

    /// Issue #462: claude keeps one JSONL file per conversation, named by
    /// the conversation id, and [`transcript_path`](Self::transcript_path)
    /// resolves exactly that file (computing it from the cwd slug, then
    /// scanning `~/.claude/projects` for the same `<id>.jsonl` name) -- so
    /// its existence IS the proof that `--resume <id>` has something to
    /// resolve. A `false` here is the case that closed an orchestrator:
    /// zirv's own seat uuid, which claude never adopted.
    fn conversation_exists(&self, session: &SessionRef) -> Option<bool> {
        Some(self.transcript_path(session).is_file())
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
    use super::super::super::config::OrchestratorWrites;
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

    /// Claude Code >= 2.1.209 writes one transcript row per CONTENT BLOCK of
    /// one API response (thinking, text, tool_use), each repeating that
    /// response's identical `usage` object under the same `message.id`.
    /// Summing per row therefore multiplies a session's real spend by however
    /// many blocks its responses happened to carry.
    #[test]
    fn usage_is_counted_once_per_api_response_not_once_per_row() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"id":"msg_x","usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_x","usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_x","usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_y","usage":{"input_tokens":1,"output_tokens":2}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(
            usage,
            TranscriptUsage {
                input_tokens: 11,
                cache_creation_input_tokens: 20,
                cache_read_input_tokens: 30,
                output_tokens: 42,
            }
        );
    }

    /// The same rule against the recorded real session: 48 assistant rows,
    /// 20 distinct `message.id`s. Summing per row reports 10_592_616 -- 2.15x
    /// the 4_922_703 the account was actually charged.
    #[test]
    fn real_session_fixture_usage_matches_its_distinct_message_ids() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let usage = transcript_usage(&jsonl).expect("usage");
        let total = usage.input_tokens
            + usage.cache_creation_input_tokens
            + usage.cache_read_input_tokens
            + usage.output_tokens;
        assert_eq!(total, 4_922_703);
    }

    /// Current Claude Code strips a thinking block's text and keeps only its
    /// `signature` (124 of 124 blocks across six recorded real sessions), so
    /// sizing the event by that text reports zero thinking for every session
    /// that thought. The response's own
    /// `usage.output_tokens_details.thinking_tokens` still carries the count.
    #[test]
    fn thinking_bytes_fall_back_to_reported_thinking_tokens_when_the_text_is_stripped() {
        let stripped = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"","signature":"EucMCqgBCBEY"}],"#,
            r#""usage":{"output_tokens":100,"output_tokens_details":{"thinking_tokens":50}}}}"#,
        );
        assert!(
            parse_events(stripped).contains(&NormalizedEvent::AssistantThinking { byte_len: 200 }),
            "50 reported thinking tokens on this repo's own 4-bytes-per-token scale"
        );

        let intact = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"abcde"}],"#,
            r#""usage":{"output_tokens":100,"output_tokens_details":{"thinking_tokens":50}}}}"#,
        );
        assert!(
            parse_events(intact).contains(&NormalizedEvent::AssistantThinking { byte_len: 5 }),
            "real thinking text still sizes itself, never the reported estimate"
        );

        let neither = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"usage":{"output_tokens":1}}}"#;
        assert!(
            !parse_events(neither)
                .iter()
                .any(|e| matches!(e, NormalizedEvent::AssistantThinking { .. })),
            "a row that carries no thinking block emits no thinking event"
        );
    }

    /// A row with neither `message.id` nor `requestId` has no response
    /// identity to dedupe on, so it still counts on its own -- the fallback
    /// preserves every pre-block-split transcript's reading exactly.
    #[test]
    fn rows_without_a_response_id_still_count_individually() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 2);
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
                NormalizedEvent::UserText { byte_len: 12 },
                NormalizedEvent::ToolResult { is_error: false },
                NormalizedEvent::ToolResultSize {
                    byte_len: 2,
                    content_hash: input_hash("ok"),
                },
            ]
        );
    }

    #[test]
    fn missing_is_error_counts_as_success() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![
                NormalizedEvent::ToolResult { is_error: false },
                NormalizedEvent::ToolResultSize {
                    byte_len: 2,
                    content_hash: input_hash("ok"),
                },
            ]
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
                NormalizedEvent::ToolResultSize {
                    byte_len: "boom: file missing".len() as u64,
                    content_hash: input_hash("boom: file missing"),
                },
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
            vec![
                NormalizedEvent::ToolResult { is_error: false },
                NormalizedEvent::ToolResultSize {
                    byte_len: "all good".len() as u64,
                    content_hash: input_hash("all good"),
                },
            ]
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
                NormalizedEvent::AssistantThinking { byte_len: 3 },
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
                NormalizedEvent::UserText { byte_len: 2 },
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
                NormalizedEvent::ToolResultSize {
                    byte_len: 2,
                    content_hash: input_hash("ok"),
                },
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
            // Issue #455: reachability wording no longer lands in the
            // catch-all -- `service unavailable:` is the provider failing,
            // a reset connection never reached it at all.
            (
                "Service unavailable: request_too_large",
                ProviderErrorClass::Server,
            ),
            ("API Error: connection reset", ProviderErrorClass::Transport),
        ];

        for (message, expected) in cases {
            assert_eq!(
                super::super::classify_provider_error(
                    message,
                    super::super::ProviderErrorHints::default()
                ),
                expected,
                "{message}"
            );
        }
    }

    /// Issue #455: the reachability classes, including the exact wording the
    /// observed incident produced, and the structured-hint fallback for a
    /// row whose text says nothing specific.
    #[test]
    fn reachability_errors_are_classified_from_text_then_from_structured_hints() {
        use crate::commands::ctx::adapters::ProviderErrorHints;
        use crate::commands::ctx::event::ProviderErrorClass;

        let text_cases = [
            (
                "API Error: Connection refused - a firewall or proxy may be blocking it \
                 (ConnectionRefused)",
                ProviderErrorClass::Transport,
            ),
            (
                "API Error: 503 Service Unavailable",
                ProviderErrorClass::Server,
            ),
            ("overloaded_error", ProviderErrorClass::Server),
            (
                "API Error: 401 authentication_error",
                ProviderErrorClass::Auth,
            ),
        ];
        for (message, expected) in text_cases {
            assert_eq!(
                super::super::classify_provider_error(message, ProviderErrorHints::default()),
                expected,
                "{message}"
            );
        }

        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints {
                    kind: Some("server_error"),
                    status: None,
                }
            ),
            ProviderErrorClass::Server,
            "a neutral message with error: server_error is a server failure"
        );
        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints {
                    kind: Some("server_error"),
                    status: Some(429),
                }
            ),
            ProviderErrorClass::RateLimit,
            "the status wins over the generic kind"
        );
        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints::default()
            ),
            ProviderErrorClass::Other,
            "no text match and no hints stays unattributed"
        );
    }

    /// Review round 1, finding 3: `Auth` is the one class no cooldown used
    /// to clear, and it was reachable from ordinary English. A sandbox
    /// refusing a file write and a transient proxy 404 are not credential
    /// problems, and codex's `task_complete.error.message` is task text.
    #[test]
    fn ordinary_english_and_bare_status_numbers_never_classify_as_auth() {
        use crate::commands::ctx::adapters::ProviderErrorHints;
        use crate::commands::ctx::event::ProviderErrorClass;

        for message in [
            "permission denied writing /x",
            "404 Not Found",
            "EACCES: permission denied, open '/etc/hosts'",
            "the tool returned 401 lines of output",
        ] {
            assert_eq!(
                super::super::classify_provider_error(message, ProviderErrorHints::default()),
                ProviderErrorClass::Other,
                "{message}"
            );
        }

        // The status still reaches `Auth` -- through the structured field,
        // which is the only place a number is evidence.
        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints {
                    kind: None,
                    status: Some(401),
                }
            ),
            ProviderErrorClass::Auth
        );
        // A bare 5xx in prose is likewise not evidence on its own; the
        // words are.
        assert_eq!(
            super::super::classify_provider_error(
                "upstream said 500",
                ProviderErrorHints::default()
            ),
            ProviderErrorClass::Other
        );
        assert_eq!(
            super::super::classify_provider_error(
                "internal server error",
                ProviderErrorHints::default()
            ),
            ProviderErrorClass::Server
        );
    }

    /// Finding 2: the row's own timestamp and identity travel with the
    /// event, so the window is judged on transcript time and the same row
    /// seen twice is recognisable.
    #[test]
    fn a_provider_error_carries_its_rows_own_time_and_identity() {
        let jsonl = concat!(
            r#"{"type":"assistant","uuid":"row-1","timestamp":"2026-09-10T08:00:00.000Z","#,
            r#""isApiErrorMessage":true,"error":"server_error","apiErrorStatus":503,"#,
            r#""message":{"role":"assistant","content":[{"type":"text","#,
            r#""text":"API Error: 503 Service Unavailable"}]}}"#,
        );
        let events = parse_events(jsonl);
        let NormalizedEvent::ProviderError { class, at, id } = &events[0] else {
            panic!("expected a provider error, got {events:?}");
        };
        assert_eq!(
            *class,
            crate::commands::ctx::event::ProviderErrorClass::Server
        );
        assert_eq!(*at, Some(1_789_027_200));
        assert_eq!(
            id.as_deref(),
            Some("row-1"),
            "claude rows carry their own uuid"
        );
    }

    /// Issue #455: plain assistant prose must never reach route health. The
    /// only rows that produce a `ProviderError` are the structured ones
    /// (`isApiErrorMessage: true`), so a session discussing an outage in
    /// its own answer cannot open a circuit breaker.
    #[test]
    fn only_structured_api_error_rows_produce_a_provider_error() {
        use crate::commands::ctx::event::ProviderErrorClass;

        let jsonl = concat!(
            r#"{"type":"assistant","isApiErrorMessage":true,"error":"server_error","#,
            r#""message":{"role":"assistant","content":[{"type":"text","#,
            r#""text":"API Error: Connection refused (ConnectionRefused)"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","#,
            r#""text":"That looked like API Error: 503 Service Unavailable, so I retried."}]}}"#,
        );
        let errors: Vec<ProviderErrorClass> = parse_events(jsonl)
            .into_iter()
            .filter_map(|event| match event {
                NormalizedEvent::ProviderError { class, .. } => Some(class),
                _ => None,
            })
            .collect();
        assert_eq!(errors, vec![ProviderErrorClass::Transport]);
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
                        class: ProviderErrorClass::Overflow,
                        ..
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
                        class: ProviderErrorClass::RateLimit,
                        ..
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
            vec![
                NormalizedEvent::TurnStart { at_ms: None },
                NormalizedEvent::UserText { byte_len: 11 },
            ]
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

    /// Issue #462: the conversation probe answers from the same file
    /// `transcript_path` resolves -- present means `--resume <id>` has
    /// something to find, absent means it does not, which is exactly the
    /// case (zirv's own seat uuid, never adopted by claude) that killed a
    /// restored orchestrator pane.
    #[test]
    fn conversation_exists_follows_the_transcript_claude_actually_wrote() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join(".claude/projects/-work-repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("49195b07-217f-4401-8681-c857fcea294e.jsonl"), "")
            .expect("write transcript");

        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session_for = |id: &str| SessionRef {
            id: SessionId::parse(id),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(
            adapter.conversation_exists(&session_for("49195b07-217f-4401-8681-c857fcea294e")),
            Some(true),
        );
        assert_eq!(
            adapter.conversation_exists(&session_for("6c967beb-0b72-46e9-9d3e-504a03f741b3")),
            Some(false),
            "zirv's own seat uuid is not a conversation claude ever minted"
        );
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
        // Issue #334: the orchestrator-write guard and the expensive-seat
        // guard are separate `PreToolUse` entries, both running `zirv ctx
        // hook pretool` -- the former attested on every launch instead of
        // depending on a one-time `zirv setup apply`.
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/1/matcher"),
            Some(&serde_json::json!("Edit|Write|MultiEdit|NotebookEdit"))
        );
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/1/hooks/0/command"),
            Some(&serde_json::json!("zirv ctx hook pretool"))
        );
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/2/matcher"),
            Some(&serde_json::json!("Agent|Task"))
        );
        assert_eq!(
            settings.pointer("/hooks/PreToolUse/2/hooks/0/command"),
            Some(&serde_json::json!("zirv ctx hook pretool"))
        );
        assert!(
            !settings["permissions"]["ask"]
                .as_array()
                .is_some_and(|rules| rules
                    .contains(&serde_json::json!("Bash(dangerouslyDisableSandbox:true)"))),
            "the native ask rule must be gone -- it silently defeated every hook-side allow: {settings}"
        );
        assert!(settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"].is_null());
    }

    /// Issue #329: `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` strips `SSH_AUTH_SOCK`
    /// (and every other tool-config variable on Claude's fixed list) from
    /// each Bash subprocess and forces the permission mode to `default`, so
    /// it is emitted only when the operator opts in via `[sandbox]
    /// scrub_subprocess_env`.
    #[test]
    fn launch_settings_emit_the_subprocess_env_scrub_only_on_operator_opt_in() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("zirv-test-safety-policy.json");
        let settings = launch_settings_value(
            &policy,
            policy_path,
            &LaunchEnvironment {
                scrub_subprocess_env: true,
                ..LaunchEnvironment::default()
            },
        )
        .expect("settings");
        assert_eq!(settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"], "1");

        let settings = launch_settings_value(&policy, policy_path, &LaunchEnvironment::default())
            .expect("settings");
        assert!(settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"].is_null());
        assert!(settings["env"][super::super::super::safety::POLICY_FINGERPRINT_ENV].is_string());
    }

    /// Writes the mutual link git keeps between `<repo>/.git/worktrees/
    /// <name>` and `<worktree>/.git`; `back_link` lets a test forge a
    /// worktree whose `.git` file points somewhere else.
    fn link_worktree(repo: &Path, name: &str, worktree: &Path, back_link: Option<&Path>) {
        let entry = repo.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&entry).expect("mkdir");
        std::fs::create_dir_all(worktree).expect("mkdir");
        std::fs::write(
            entry.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .expect("gitdir");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", back_link.unwrap_or(&entry).display()),
        )
        .expect("back link");
    }

    /// Issue #329 item 2: sibling checkouts of the launch repo -- and their
    /// linked worktrees, read from `.git/worktrees/*/gitdir` -- become write
    /// roots; a non-repo neighbour, the launch repo itself, a worktree whose
    /// directory is gone, and a home-directory parent do not. Codex review:
    /// a `gitdir` naming a directory that does not link back (the home
    /// directory, the launch repo's parent, a worktree whose `.git` points
    /// at another entry) grants nothing, a worktree shared by two siblings
    /// appears once, and no home means no siblings at all.
    #[test]
    fn sibling_repo_roots_cover_neighbouring_checkouts_and_their_worktrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let services = home.join("services");
        let repo = services.join("crm");
        let client = services.join("client");
        let service = services.join("service");
        let plain = services.join("notes");
        let worktree = home.join("worktrees").join("markau-107");
        for dir in [repo.join(".git"), client.join(".git"), plain.clone()] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        link_worktree(&service, "markau-107", &worktree, None);
        // The same worktree recorded by a second sibling too: once.
        link_worktree(&client, "shared", &worktree, None);
        // Stale entry: the worktree directory is gone.
        std::fs::create_dir_all(service.join(".git/worktrees/gone")).expect("mkdir");
        std::fs::write(
            service.join(".git/worktrees/gone/gitdir"),
            format!("{}\n", tmp.path().join("missing").join(".git").display()),
        )
        .expect("gitdir");
        // Forged entries: `gitdir` names the home directory (no `.git` file
        // there), a real-looking worktree whose own `.git` points at a
        // different entry, and the launch repo's parent (links back, but is
        // an ancestor of the repo and of nothing else grantable).
        std::fs::create_dir_all(service.join(".git/worktrees/home")).expect("mkdir");
        std::fs::write(
            service.join(".git/worktrees/home/gitdir"),
            format!("{}\n", home.join(".git").display()),
        )
        .expect("gitdir");
        let forged = home.join("forged");
        link_worktree(
            &service,
            "forged",
            &forged,
            Some(&client.join(".git/worktrees/elsewhere")),
        );
        link_worktree(&service, "parent", &services, None);
        // A linked worktree of the launch repo itself, next to it: `.git`
        // is a file, still a checkout.
        let linked = services.join("crm-wt");
        std::fs::create_dir_all(&linked).expect("mkdir");
        std::fs::write(linked.join(".git"), "gitdir: ../crm/.git/worktrees/wt\n").expect("git");

        let canonical_repo = std::fs::canonicalize(&repo).expect("canonical");
        let roots = sibling_repo_roots(&canonical_repo, Some(&home));
        // Discovery order: siblings sorted, each followed by its worktrees
        // (`client` records the shared worktree first).
        let expected: Vec<PathBuf> = [&client, &worktree, &linked, &service]
            .into_iter()
            .map(|path| std::fs::canonicalize(path).expect("canonical"))
            .collect();
        assert_eq!(roots, expected);
        assert!(!roots.contains(&canonical_repo));

        // The parent IS the home directory: never widened to its children.
        assert!(sibling_repo_roots(&canonical_repo, Some(&services)).is_empty());
        // No resolvable home: fail closed.
        assert!(sibling_repo_roots(&canonical_repo, None).is_empty());
    }

    #[test]
    fn grantable_roots_exclude_home_its_ancestors_the_repo_and_filesystem_roots() {
        let home = Path::new("/home/dev");
        let repo = Path::new("/home/dev/services/crm");
        for (candidate, expected) in [
            ("/home/dev/services/client", true),
            ("/home/dev/worktrees/wt", true),
            ("/home/dev", false),
            ("/home", false),
            ("/", false),
            ("/home/dev/services", false),
            ("/home/dev/services/crm", false),
            ("/home/dev/services/crm/vendor", false),
        ] {
            assert_eq!(
                is_grantable_root(Path::new(candidate), repo, home),
                expected,
                "{candidate}"
            );
        }
    }

    #[test]
    fn launch_settings_observe_permission_events_without_changing_pretooluse() {
        let settings = test_launch_settings();
        // Issue #334 added the two guard entries below; wiring the
        // `PermissionRequest`/`PermissionDenied` observers in must not
        // perturb this array further, and neither may issue #326's
        // compact-output hook, which is a `PostToolUse` entry of its own
        // (asserted below) and never touches this array at all.
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            serde_json::json!([{
                "matcher": "Bash|PowerShell",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx safety check"
                }]
            }, {
                "matcher": "Edit|Write|MultiEdit|NotebookEdit",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook pretool"
                }]
            }, {
                "matcher": "Agent|Task",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook pretool"
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

    /// Issue #326: the compact-output hook is wired as its own `PostToolUse`
    /// entry on `Bash`, synchronously (no `"background": true`) -- a
    /// background hook's `updatedToolOutput` would arrive after claude had
    /// already been handed the original result.
    #[test]
    fn launch_settings_wire_the_compact_output_hook_on_post_tool_use() {
        let settings = test_launch_settings();
        assert_eq!(
            settings["hooks"]["PostToolUse"],
            serde_json::json!([{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "zirv ctx hook posttool"
                }]
            }])
        );
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

    /// A canonicalized root (what worktree and sibling discovery produces)
    /// carries the `\\?\` verbatim prefix on Windows; the rendered grant
    /// must not, or Claude Code refuses it as a network path.
    #[test]
    fn grant_paths_never_carry_the_windows_verbatim_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let canonical = std::fs::canonicalize(tmp.path()).expect("canonical");
        #[cfg(windows)]
        assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
        let rendered = grant_path(&canonical);
        assert!(!rendered.starts_with(r"\\?\"), "{rendered}");
        // Still the same directory, just spelled the way another program
        // can consume it.
        assert_eq!(std::fs::canonicalize(&rendered).expect("exists"), canonical);
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
        assert!(
            settings["env"]["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"].is_null(),
            "the scrub would strip the very SSH_AUTH_SOCK exported above (issue #329)"
        );
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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
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
            .launch_settings_path(&Default::default(), &policy)
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
        // The sandbox block is compiled out on native Windows (no OS sandbox).
        #[cfg(not(windows))]
        {
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
        }
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
            scrub_subprocess_env: false,
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

    /// Issue #455, grounded against a scrubbed but shape-faithful fixture: a
    /// `Bash` `git push` never gets a `tool_result` before the stream cuts
    /// with an `isApiErrorMessage` row, so it must surface as an unresolved
    /// call, and the cut itself must be reported rather than letting the
    /// synthetic "API Error: ..." text stand in as a finished reply. The
    /// fixture's own preceding text ("I'll push the branch now.", stop_reason
    /// `tool_use`, never closed by a boundary before the cut) is exactly the
    /// text that WAS open at the cut, so it must surface as `partial_text`,
    /// never in `assistant_texts` (review round 2).
    #[test]
    fn structural_context_flags_an_unresolved_tool_call_and_the_cut_tail() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-partial-stream.jsonl")).expect("fixture");
        let ctx = structural_context(&jsonl, 10);

        assert_eq!(ctx.unresolved_tool_calls.len(), 1);
        let call = &ctx.unresolved_tool_calls[0];
        assert_eq!(call.name, "Bash");
        assert_eq!(call.id, "toolu_partial01");
        assert!(call.summary.contains("git push origin feat/x"));

        let reason = ctx.tail_cut.as_deref().expect("a cut tail");
        assert!(reason.contains("server_error"));
        assert!(reason.contains("Bash"));

        let partial = ctx.partial_text.as_deref().expect("open text at the cut");
        assert!(partial.contains("I'll push the branch now."));

        assert!(
            ctx.assistant_texts.is_empty(),
            "text open at the moment of the cut must never reach assistant_texts: {:?}",
            ctx.assistant_texts
        );
    }

    /// Issue #455 review round 2, the exact regression this round fixes: a
    /// text-only turn completes normally (`end_turn`), then a LATER,
    /// separate turn issues only a `tool_use` (no text of its own) before
    /// the stream cuts. The first turn's text must stay in `assistant_texts`
    /// (and therefore `done`), and `partial_text` must be `None` -- the cut
    /// turn itself had nothing open to withhold.
    #[test]
    fn structural_context_only_withholds_the_text_open_at_the_moment_of_the_cut() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Pushed the branch."}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"git status"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","isApiErrorMessage":true,"error":"server_error","message":{"model":"<synthetic>","content":[{"type":"text","text":"API Error: 500"}],"usage":{}}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert_eq!(ctx.assistant_texts, vec!["Pushed the branch.".to_string()]);
        assert!(ctx.tail_cut.is_some());
        assert!(
            ctx.partial_text.is_none(),
            "the cut turn itself carried no text: {:?}",
            ctx.partial_text
        );
    }

    /// The counterpart to the above: once the `tool_result` arrives, the
    /// call is resolved and neither field fires.
    #[test]
    fn structural_context_does_not_flag_a_tool_call_once_its_result_arrives() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"git push origin feat/x"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b1","is_error":false,"content":"ok"}]}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert!(ctx.unresolved_tool_calls.is_empty());
        assert!(ctx.tail_cut.is_none());
    }

    /// Classifier gating (issue #455): a row that merely QUOTES the words
    /// "API Error"/"503" in ordinary assistant prose, or in a tool result,
    /// must never be mistaken for a real provider-error row -- the gate
    /// stays `isApiErrorMessage` alone.
    #[test]
    fn structural_context_ignores_text_that_merely_quotes_an_api_error() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"grep -r 'API Error' logs/"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b1","is_error":false,"content":"logs/app.log: API Error: 503 last Tuesday"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looks like the logs mention API Error: 503 already."}],"usage":{}}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert!(ctx.tail_cut.is_none());
        assert!(ctx.unresolved_tool_calls.is_empty());
    }

    /// Issue #455: a file an unresolved `Edit`/`Write`-style call claimed to
    /// modify is reported separately so `handoff::structural` can mark it
    /// `(unconfirmed)`; a resolved call's file is never flagged.
    #[test]
    fn structural_context_marks_files_from_an_unresolved_edit_as_unconfirmed() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"e2","name":"Write","input":{"file_path":"/work/src/main.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"e2","is_error":false,"content":"ok"}]}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert_eq!(
            ctx.files_modified,
            vec!["/work/src/lib.rs", "/work/src/main.rs"]
        );
        assert_eq!(ctx.unconfirmed_files_modified, vec!["/work/src/lib.rs"]);
    }

    /// Issue #455 review finding: attribution must not stick to the FIRST
    /// call that ever named a path -- a path first touched by a call that
    /// went on to resolve, then touched AGAIN by a call that never did, must
    /// still end up unconfirmed. The unsafe direction is the reverse:
    /// claiming a write landed when the LAST attempt on it never reported
    /// back.
    #[test]
    fn structural_context_marks_a_path_unconfirmed_even_when_its_first_call_resolved() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"e1","is_error":false,"content":"ok"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"e2","name":"Edit","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert_eq!(ctx.files_modified, vec!["/work/src/lib.rs"]);
        assert_eq!(
            ctx.unconfirmed_files_modified,
            vec!["/work/src/lib.rs"],
            "the second, unresolved edit must still mark the path unconfirmed"
        );
    }

    /// Issue #455: a later, genuine assistant row recovers from an earlier
    /// cut -- `tail_cut` reflects only the state as of the END of the
    /// scanned range.
    #[test]
    fn structural_context_clears_tail_cut_after_a_later_successful_assistant_row() {
        let jsonl = concat!(
            r#"{"type":"assistant","isApiErrorMessage":true,"error":"server_error","message":{"model":"<synthetic>","content":[{"type":"text","text":"API Error: 500"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"recovered and finished"}],"usage":{}}}"#,
            "\n",
        );
        let ctx = structural_context(jsonl, 10);
        assert!(ctx.tail_cut.is_none());
        assert_eq!(ctx.assistant_texts, vec!["recovered and finished"]);
    }

    /// Issue #455: `unresolved_tool_calls` is capped independently of
    /// `last_n`, newest last -- mirrors
    /// `structural_context_caps_files_read_like_every_other_field` below.
    #[test]
    fn structural_context_caps_unresolved_tool_calls_to_the_newest_eight() {
        let mut jsonl = String::new();
        for i in 0..12 {
            jsonl.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"call-{i}\",\"name\":\"Bash\",\"input\":{{\"command\":\"echo {i}\"}}}}],\"usage\":{{}}}}}}\n"
            ));
        }
        let ctx = structural_context(&jsonl, 1_000);
        assert_eq!(ctx.unresolved_tool_calls.len(), 8);
        assert_eq!(ctx.unresolved_tool_calls.first().unwrap().id, "call-4");
        assert_eq!(ctx.unresolved_tool_calls.last().unwrap().id, "call-11");
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

    #[test]
    fn issue_418_claude_advertises_both_native_hook_capabilities() {
        let caps = ClaudeAdapter::new(None).capabilities();
        assert!(caps.pre_tool_hook);
        assert!(caps.post_tool_hook);
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

    /// Issue #395, item 1: an `[endpoint.claude]` override retargets a
    /// launch at a vendor-compatible endpoint end to end -- the launched
    /// `Command` carries the vendor's own base URL and the credential read
    /// fresh from its named environment variable, `model_args` pins a real
    /// vendor rung id, `provider()` reports the vendor slug (so usage/spend
    /// files land under it), and that rung id prices.
    #[test]
    fn an_endpoint_override_retargets_the_launch_at_the_vendor() {
        // SAFETY (test): nextest isolates tests per process, and the serial
        // `cargo test -- --test-threads=1` run never overlaps this variable
        // with another test.
        unsafe {
            std::env::set_var("ZIRV_TEST_ZHIPU_KEY_395", "sekrit-value");
        }
        let target = crate::commands::ctx::config::EndpointTarget {
            vendor: "zhipu".to_string(),
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            credential_env: "ZIRV_TEST_ZHIPU_KEY_395".to_string(),
            model: None,
            wire_api: None,
        };
        let adapter = ClaudeAdapter::new(None).with_endpoint(target);
        assert!(
            adapter.ready().is_ok(),
            "credential is set, so ready() must pass"
        );
        assert_eq!(adapter.provider(), "zhipu");

        let cmd = adapter.headless_cmd("hi", &SessionId::parse("s"), &[]);
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert!(
            envs.iter().any(|(k, v)| k == "ANTHROPIC_BASE_URL"
                && v.as_deref() == Some("https://api.z.ai/api/anthropic")),
            "got {envs:?}"
        );
        assert!(
            envs.iter()
                .any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v.as_deref() == Some("sekrit-value")),
            "got {envs:?}"
        );

        let vendor = catalogue::vendor("zhipu").expect("zhipu is a built-in vendor");
        let model_args = adapter.model_args("opus");
        assert_eq!(model_args[0], "--model");
        assert_eq!(
            model_args[1], vendor.rungs[0].id,
            "no operator model set -> the vendor's own strongest rung"
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        assert!(
            state.usage_for("zhipu").ends_with("usage-zhipu.json"),
            "got {}",
            state.usage_for("zhipu").display()
        );

        let price_table = crate::commands::ctx::price::built_in_table();
        assert!(
            crate::commands::ctx::price::price(
                &model_args[1],
                &crate::commands::ctx::event::TranscriptUsage::default(),
                &price_table
            )
            .is_some(),
            "the pinned rung id must have a built-in price"
        );

        // SAFETY (test): see the matching `set_var` above.
        unsafe {
            std::env::remove_var("ZIRV_TEST_ZHIPU_KEY_395");
        }
    }

    /// Issue #395, item 2: a missing credential fails `ready()` -- the
    /// pre-spawn gate every real launch goes through (`select`/`resolve_
    /// default`) -- naming the environment variable's own NAME, never a
    /// value, and never reaching a spawn at all.
    #[test]
    fn a_missing_endpoint_credential_fails_ready_by_name() {
        // SAFETY (test): see the identical pattern above.
        unsafe {
            std::env::remove_var("ZIRV_TEST_MISSING_KEY_395");
        }
        let target = crate::commands::ctx::config::EndpointTarget {
            vendor: "zhipu".to_string(),
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            credential_env: "ZIRV_TEST_MISSING_KEY_395".to_string(),
            model: None,
            wire_api: None,
        };
        let adapter = ClaudeAdapter::new(None).with_endpoint(target);
        let err = adapter.ready().expect_err("no credential must refuse");
        let message = err.to_string();
        assert!(
            message.contains("ZIRV_TEST_MISSING_KEY_395"),
            "got {message}"
        );
    }

    /// Issue #395, item 4: a requested model that does not resolve on the
    /// endpoint vendor's own ladder (a claude alias, here) is replaced by
    /// the endpoint's own default; a requested model that DOES resolve on
    /// that ladder is honoured verbatim.
    #[test]
    fn model_args_pins_to_the_endpoint_vendors_own_ladder() {
        let target = crate::commands::ctx::config::EndpointTarget {
            vendor: "zhipu".to_string(),
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            credential_env: "ZIRV_TEST_UNUSED_395".to_string(),
            model: None,
            wire_api: None,
        };
        let adapter = ClaudeAdapter::new(None).with_endpoint(target);
        let vendor = catalogue::vendor("zhipu").expect("zhipu is a built-in vendor");

        // A claude-native alias must never reach a GLM endpoint.
        let replaced = adapter.model_args("opus");
        assert_eq!(replaced[1], vendor.rungs[0].id);

        // A zhipu alias/id already on the ladder is honoured verbatim.
        let honoured = adapter.model_args("glm-4.6");
        assert_eq!(honoured[1], "glm-4.6");
    }

    /// Review finding (#395 follow-up): `distiller_cmd` used to hardcode
    /// `--model haiku` regardless of any attached endpoint override, so a
    /// zhipu/deepseek endpoint got claude's native cheap alias -- not a
    /// valid model on that vendor's account -- for the one judgment/
    /// distillation child every rot-scoring pass spawns. It must now carry
    /// a vendor rung, exactly like `model_args_pins_to_the_endpoint_
    /// vendors_own_ladder` already verifies for the interactive/headless
    /// launches.
    #[test]
    fn distiller_cmd_pins_the_model_through_an_endpoint_override() {
        let target = crate::commands::ctx::config::EndpointTarget {
            vendor: "zhipu".to_string(),
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            credential_env: "ZIRV_TEST_UNUSED_395".to_string(),
            model: None,
            wire_api: None,
        };
        let adapter = ClaudeAdapter::new(None).with_endpoint(target);
        let vendor = catalogue::vendor("zhipu").expect("zhipu is a built-in vendor");

        let cmd = adapter.distiller_cmd("haiku");
        let args = built_args(&adapter, &cmd);
        let model_at = args
            .iter()
            .position(|a| a == "--model")
            .expect("distiller_cmd must still emit --model");
        assert_eq!(
            args[model_at + 1],
            vendor.rungs[0].id,
            "the distiller must never send claude's native cheap alias to a zhipu endpoint: \
             got {args:?}"
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

    /// Issue #381: `review_model_below`/`model_strength`/
    /// `default_worker_model`/`default_distiller_model`/
    /// `context_window_tokens` now delegate to `catalogue`. This pins every
    /// answer the pre-catalogue hand-written ladder gave, so the migration
    /// cannot silently change one.
    #[test]
    fn catalogue_backed_answers_match_the_pre_catalogue_ladder() {
        let adapter = ClaudeAdapter::new(None);
        for (seat, expected) in [
            (Some("claude-fable-5"), "opus"),
            (Some("mythos"), "opus"),
            (Some("opus"), "sonnet"),
            (Some("sonnet"), "haiku"),
            (Some("haiku"), "haiku"),
            (None, "opus"),
            (Some("some-unreleased-model"), "opus"),
        ] {
            assert_eq!(adapter.review_model_below(seat), expected, "seat={seat:?}");
        }
        for (model, expected) in [
            ("fable", Some(4)),
            ("mythos", Some(4)),
            ("opus", Some(3)),
            ("sonnet", Some(2)),
            ("haiku", Some(1)),
            ("unknown-model", None),
        ] {
            assert_eq!(adapter.model_strength(model), expected, "model={model}");
        }
        assert_eq!(adapter.default_worker_model(), Some("sonnet"));
        assert_eq!(adapter.default_distiller_model(), Some("haiku"));
        assert_eq!(
            adapter.context_window_tokens(None),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            adapter.context_window_tokens(Some("claude-opus-5[1m]")),
            Some(LONG_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            adapter.context_window_tokens(Some("claude-opus-5-1m")),
            Some(LONG_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            adapter.context_window_tokens(Some("claude-opus-5")),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS)
        );
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
            .base_system_prompt(OrchestratorWrites::Deny)
            .expect("claude has a base layer of its own");
        assert_eq!(layer, ORCHESTRATOR_PROMPT);
        for claude_specific in ["Agent tool", ".claude/agents", "/code-review"] {
            assert!(
                layer.contains(claude_specific),
                "the layer is claude-specific by construction: '{claude_specific}'"
            );
        }
    }

    /// Issue #358 T8: the default posture (`advise`) no longer says "it does
    /// not implement" -- the pinned deny-only assertion moved onto `deny`
    /// itself (`claude_contributes_the_orchestrator_layer_and_it_names_
    /// claudes_own_tools` above, and `ORCHESTRATOR_PROMPT`'s own direct-const
    /// tests elsewhere in this file). One test per posture.
    #[test]
    fn the_orchestrator_layer_follows_this_seats_write_posture() {
        let deny = ClaudeAdapter::new(None)
            .base_system_prompt(OrchestratorWrites::Deny)
            .expect("deny has a base layer");
        assert_eq!(deny, ORCHESTRATOR_PROMPT);
        assert!(deny.contains("it does not implement"));
        assert!(deny.contains("PreToolUse hook denies repository writes"));

        let advise = ClaudeAdapter::new(None)
            .base_system_prompt(OrchestratorWrites::Advise)
            .expect("advise has a base layer");
        assert!(
            !advise.contains("it does not implement"),
            "advise no longer claims writes are technically blocked: {advise}"
        );
        assert!(advise.contains("make trivial edits"), "got:\n{advise}");
        assert!(
            advise.contains("Repository writes from this seat are recorded"),
            "got:\n{advise}"
        );
        // Everything after the write-guard bullet is unchanged.
        assert!(advise.contains("Routing rule, which outranks"));
        assert!(advise.contains("Agent tool"));

        let allow = ClaudeAdapter::new(None)
            .base_system_prompt(OrchestratorWrites::Allow)
            .expect("allow has a base layer");
        assert!(allow.contains("make trivial edits"), "got:\n{allow}");
        assert!(
            !allow.contains("Repository writes from this seat are recorded"),
            "allow drops advise's own last sentence: {allow}"
        );
        assert!(allow.contains("Routing rule, which outranks"));
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
    ///
    /// Issues #328/#334: this seat never implements regardless of task
    /// size -- the old "trivial and bounded changes stay on this seat"
    /// carve-out is gone -- so the sizing question this test now covers is
    /// only how much work is bundled into one worker's brief versus split
    /// into a sub-orchestrator's work group.
    #[test]
    fn the_orchestrator_prompt_sizes_delegation_between_subagents_and_sub_orchestrators() {
        assert!(
            ORCHESTRATOR_PROMPT.contains("it does not implement"),
            "an orchestrator seat never implements, regardless of task size: {ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("native Agent tool"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            !ORCHESTRATOR_PROMPT.contains("stay on this seat"),
            "the old size-based implement-it-yourself carve-out must be gone: \
             {ORCHESTRATOR_PROMPT}"
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

    /// Issue #328: the layer states the routing rule outright -- native Agent
    /// tool for same-harness work, `zirv agent` for another harness or a work
    /// group -- and says it outranks a contradicting operator/repo layer, so a
    /// hand-written "delegate only through `zirv agent`" file can no longer
    /// win by being the more specific text.
    #[test]
    fn the_orchestrator_prompt_routes_same_harness_delegation_to_the_native_agent_tool() {
        assert!(
            ORCHESTRATOR_PROMPT
                .contains("same-harness delegation uses this harness's native Agent tool"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains(
                "`zirv agent <name>` is for reaching a different harness or a work group"
            ),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("outranks any operator or repository layer"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            !ORCHESTRATOR_PROMPT.contains("Delegate only through"),
            "the layer must never instruct routing every delegation through zirv agent: \
             {ORCHESTRATOR_PROMPT}"
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
