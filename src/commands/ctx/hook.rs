use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::adapters::{self, SESSION_ENV, SOCKET_ENV};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::rot::{Score, Verdict};
use super::state::{StateDir, now_secs};
use super::{CtxResult, log, score, signal};

#[derive(Debug, clap::Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub event: HookEvent,
}

#[derive(Debug, clap::Subcommand)]
pub enum HookEvent {
    /// Claude Stop hook: score the turn and forward or advise.
    Stop,
    /// Claude UserPromptSubmit hook: install the reply marker instruction.
    Prompt,
    /// Claude PreCompact hook: record that a compaction is starting.
    PreCompact,
    /// Claude PreToolUse hook: refuse a subagent dispatch that would inherit
    /// this seat's expensive model.
    Pretool,
    /// Codex notify program: same role as Stop.
    Notify {
        /// Payload, when the agent passes it as an argument instead of stdin.
        payload: Option<String>,
    },
}

/// `stop_hook_active` is absent from the published field table but is delivered
/// in practice, so every field is optional with a zero default. `Serialize` is
/// needed because Task A16 maps a codex notify payload into this shape and hands
/// it back to `run_stop`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HookPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub stop_hook_active: bool,
}

impl HookPayload {
    pub fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }

    fn repo(&self) -> std::path::PathBuf {
        if self.cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(&self.cwd)
        }
    }
}

/// The optimize hint sentence, worded from the signal that actually fired
/// rather than always blaming the tools.
fn optimize_hint(reason: super::optimize::RecommendReason) -> &'static str {
    use super::optimize::RecommendReason;
    match reason {
        RecommendReason::ToolFailures => {
            "This session hit tools hard: `zirv ctx optimize` reviews the instruction files for \
             gaps behind repeated failures."
        }
        RecommendReason::Corrections => {
            "This session needed repeated corrections: `zirv ctx optimize` reviews the \
             instruction files for gaps behind that."
        }
    }
}

/// Decides what the Stop hook prints. `None` means print nothing, which is also
/// what every failure path does.
pub fn stop_output(
    payload: &HookPayload,
    score: &Score,
    socket: Option<&Path>,
    optimize_recommended: Option<super::optimize::RecommendReason>,
) -> Option<String> {
    if payload.stop_hook_active {
        return None;
    }
    if socket.is_some() {
        return None;
    }
    if score.verdict == Verdict::Healthy && optimize_recommended.is_none() {
        return None;
    }

    // A healthy session is never told to /compact or resume: the only thing
    // worth saying is the optimize hint that got it here in the first place.
    if score.verdict == Verdict::Healthy {
        let hint = optimize_recommended.map(optimize_hint).unwrap_or_default();
        return serde_json::to_string(&serde_json::json!({ "systemMessage": hint })).ok();
    }

    let mut advisory = format!(
        "zirv ctx: verdict {} (score {}, context {} tokens). Consider /compact, or run `zirv ctx resume` for a clean session with a handoff.",
        score.verdict.as_str(),
        score.score,
        score.context_tokens
    );
    if let Some(reason) = optimize_recommended {
        advisory.push(' ');
        advisory.push_str(optimize_hint(reason));
    }
    serde_json::to_string(&serde_json::json!({ "systemMessage": advisory })).ok()
}

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

/// Corrections in `transcript`, read through the same adapter selection
/// `score_transcript` uses to score it (`cfg.agent`/`cfg.agent_bin`), not a
/// hardcoded claude parser (item 1). `adapters::select` failing (an unready
/// adapter, e.g. codex today) degrades to zero corrections rather than
/// panicking: an optimize recommendation is advisory, and a hook may never
/// fail loudly.
fn corrections_in(transcript: &Path, cfg: &CtxConfig) -> usize {
    let Ok(adapter) = adapters::select(cfg.agent.as_deref(), &[], cfg) else {
        return 0;
    };
    std::fs::read_to_string(transcript)
        .map(|jsonl| super::optimize::count_corrections(adapter.as_ref(), &jsonl))
        .unwrap_or(0)
}

/// `CtxConfig::load`'s degrade-on-error fallback used by the Stop hook's
/// optimize-recommendation path: a hook must never fail outright on a bad
/// config, but degrading all the way to `CtxConfig::default()` would hand
/// `corrections_in` a fully permissive `AgentGate`, which is exactly the
/// same trust hole `optimize.rs`'s config-load fallback had (review finding
/// 1): a malformed *repo* `.settings.toml` would silently revive an agent
/// the *operator* disabled. It would also, since issue #44 made `cfg.policy`
/// load-bearing, hand back the widest possible policy from a config that
/// could not even be read. `config::degrade_to_operator_only` substitutes
/// `AgentGate::load_operator_only`/`EffectivePolicy::fail_closed` for those
/// two fields, keeping both the operator's disable and the operator's policy
/// in force even when the rest of the config (or the repo settings layer
/// specifically) cannot be read.
fn cfg_or_operator_only_gate(repo: &Path, env: EnvLookup<'_>) -> CtxConfig {
    match CtxConfig::load(repo, env) {
        Ok(cfg) => cfg,
        Err(_) => super::config::degrade_to_operator_only(env),
    }
}

pub fn run_stop<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    // Every early return is deliberate: a hook that errors must still exit 0.
    let Ok(payload) = HookPayload::parse(stdin) else {
        return Ok(0);
    };
    if payload.stop_hook_active || payload.transcript_path.is_empty() {
        return Ok(0);
    }
    let transcript = Path::new(&payload.transcript_path);
    if !transcript.is_file() {
        return Ok(0);
    }
    let repo = payload.repo();
    // Cached: this hook is a fresh process after every single turn, so scoring
    // the whole transcript each time is quadratic over a session's length.
    let Ok(score) = score::score_transcript_cached(transcript, None, &repo, env) else {
        return Ok(0);
    };

    let socket = env(SOCKET_ENV).map(std::path::PathBuf::from);
    let session = env(SESSION_ENV).unwrap_or_else(|| payload.session_id.clone());

    if let Some(path) = socket.as_deref() {
        let turn = score.signals.turns as u64;
        let _ = signal::send(
            path,
            &signal::TurnSignal {
                session_id: session.clone(),
                turn,
                score: score.score,
                verdict: score.verdict,
                // The supervisor spawned the agent but does not know which
                // session file it chose, so the hook has to say.
                transcript_path: Some(payload.transcript_path.clone()),
            },
        );
    }

    let mut optimize_recommended = None;
    if let Ok(state) = StateDir::resolve(env) {
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: &session,
                verb: "hook",
                verdict: score.verdict.as_str(),
                score: score.score,
                action: if socket.is_some() {
                    "forward"
                } else {
                    "advise"
                },
                detail: &payload.transcript_path,
            },
        );

        // The analysis itself is far too heavy for a hook, so this only queues
        // the recommendation for a human to act on. Counting corrections is
        // the one expensive part -- a full re-read and re-parse of the
        // transcript -- so it is paid for only once the free gates say it
        // could matter. Without that ordering every turn re-parses the whole
        // session, which is precisely what the cached score above removes.
        let cfg = cfg_or_operator_only_gate(&repo, env);
        let now = now_secs();
        if super::optimize::recommendation_possible(&state, &score, &cfg.optimize, now) {
            let corrections = corrections_in(transcript, &cfg);
            optimize_recommended = super::optimize::queue_recommendation(
                &state,
                &session,
                &score,
                corrections,
                &cfg.optimize,
                now,
            );
        }
    }

    if let Some(line) = stop_output(&payload, &score, socket.as_deref(), optimize_recommended) {
        let _ = writeln!(w, "{line}");
    }
    Ok(0)
}

/// UserPromptSubmit is the only hook that can add context to the model, which
/// is how the marker signal gets installed.
///
/// Issue #225 (steady-state token reduction): this sentence is paid,
/// uncached, on EVERY user turn -- unlike the once-per-session prompt layers
/// in `prompt.rs`, it cannot ride the provider's cache. It used to be 170
/// bytes; the shorter wording below keeps the exact same contract (start
/// every FINAL answer with the marker on line 1, mid-turn notes are exempt,
/// it is a context-health marker) in <= 90 bytes for the default `[zirv]`
/// marker. `score.rs`/`rot.rs`'s `marker_miss_rate` only checks for the
/// marker prefix at the start of a line, never this sentence's wording, so
/// rewording it here changes no detection logic.
///
/// Split out of [`prompt_output`] so `zirv ctx compile --measure` (issue
/// #225) can report this sentence's own byte cost without re-deriving its
/// wording a second way -- the measurement and the actual injected text can
/// never drift apart on what "the hook context" means.
pub fn per_turn_context_text(marker: &str) -> String {
    format!(
        "Prefix each final answer with {marker} on line 1 (mid-turn exempt): zirv ctx health marker."
    )
}

pub fn prompt_output(marker: &str) -> String {
    let context = per_turn_context_text(marker);
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context
        }
    })
    .to_string()
}

/// PreCompact cannot add instructions to a compaction (verified against the
/// hook reference), so all this can do is say so. Focus instructions ride
/// along with wrap's injected `/compact <focus>` command instead.
pub fn pre_compact_output() -> String {
    serde_json::json!({
        "systemMessage": "zirv ctx: compaction starting. Preserve the current task, file paths and unresolved errors."
    })
    .to_string()
}

/// A compaction is the largest single context event a session has, so it is
/// recorded even though the hook cannot influence it. Without this entry the
/// decision log shows scores stepping down with no visible cause.
pub fn run_pre_compact<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    // Same rule as every other hook path: nothing here may keep the advisory
    // from being printed or turn into a non-zero exit.
    let payload = HookPayload::parse(stdin).unwrap_or_default();
    let session = env(SESSION_ENV)
        .or_else(|| Some(payload.session_id.clone()))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    if let Ok(state) = StateDir::resolve(env) {
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: &session,
                verb: "hook",
                verdict: "n/a",
                score: 0,
                action: "pre-compact",
                detail: &payload.transcript_path,
            },
        );
    }

    let _ = writeln!(w, "{}", pre_compact_output());
    Ok(0)
}

// -- PreToolUse: the expensive-seat inheritance guard ----------------------

/// Model-name fragments that mark a seat too expensive to inherit silently.
/// Matched case-insensitively as substrings, so a vendor-qualified id
/// (`us.anthropic.mythos-...`) or a suffixed one (`fable[1m]`) still lands.
const EXPENSIVE_TIERS: [&str; 2] = ["fable", "mythos"];

/// The tool names that dispatch a subagent. Both spellings are covered
/// because the tool has been presented under either name and the guard must
/// not turn itself off on a rename.
const SUBAGENT_TOOLS: [&str; 2] = ["Agent", "Task"];

/// Subagent types that pin no model of their own, so an omitted `model`
/// parameter means "inherit the caller's". Matched exactly and
/// case-sensitively: these are literal values of the tool's own
/// `subagent_type` parameter, not free text. Any other name is a
/// `.claude/agents/<name>.md` definition, which carries its own `model`
/// frontmatter and is therefore none of zirv's business.
const GENERIC_SUBAGENT_TYPES: [&str; 5] = ["fork", "claude", "general-purpose", "Explore", "Plan"];

/// The PreToolUse stdin payload, narrowed to what the guard reads. Every
/// field is optional with a zero default, the same rule the Stop payload
/// follows: a hook that fails to parse is a hook that silently stops
/// guarding, so nothing here may be mandatory.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreToolPayload {
    pub tool_name: String,
    pub tool_input: PreToolInput,
}

/// `tool_input` is tool-specific, so only the subagent tool's own parameters
/// are modelled and every other tool's arguments are ignored rather than
/// rejected. `deny_unknown_fields` here would turn an ordinary `Bash` payload
/// into a parse failure.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreToolInput {
    pub subagent_type: String,
    pub model: String,
    /// The subagent's own task text. Every real `Agent`/`Task` dispatch
    /// carries a non-empty one; `pretool_decision` reads its absence as
    /// schema drift -- a missing `tool_input`, an empty `{}`, or one that
    /// simply does not name a `prompt` -- rather than an actual dispatch,
    /// and fails open on it instead of denying on the zero values `#[serde(
    /// default)]` invented for fields the payload never carried at all.
    pub prompt: String,
}

impl PreToolPayload {
    pub fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }
}

fn names_expensive_tier(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    EXPENSIVE_TIERS.iter().any(|tier| model.contains(tier))
}

/// What the model is told when a dispatch is refused. The reason is the only
/// thing it sees, so it has to carry the whole remedy: naming the seat, the
/// cheaper models that are accepted, and the one option (a fork) that no
/// model parameter can rescue.
fn pretool_deny_reason(seat: &str) -> String {
    format!(
        "zirv guard: this seat runs {seat}; re-dispatch with an explicit cheaper model \
         parameter (haiku for mechanical work, sonnet for standard work, opus for hard \
         work), or use an agent type that pins its own model. Forks are not allowed from \
         this seat: a fork always inherits the seat model and ignores a model override."
    )
}

/// The whole decision, pure: `Some(reason)` denies, `None` allows.
///
/// `seat` is `SEAT_MODEL_ENV`'s value, absent for any session zirv did not
/// launch as an expensive orchestrator seat. Every gate below is a reason to
/// allow, so an unrecognised tool, an unset seat, a cheap seat, a payload
/// with no `prompt` (see `PreToolInput::prompt`'s own doc comment -- that is
/// schema drift, not a dispatch), or a payload this function does not
/// understand at all fall through to allow. That is deliberate: this hook
/// runs in front of every tool call in the session, and the cost of a wrong
/// deny is far higher than the cost of a missed one.
pub fn pretool_decision(seat: Option<&str>, payload: &PreToolPayload) -> Option<String> {
    let seat = seat?;
    if !names_expensive_tier(seat) {
        return None;
    }
    if !SUBAGENT_TOOLS.contains(&payload.tool_name.as_str()) {
        return None;
    }
    // A payload with no `tool_input` at all, an empty `{}`, or one that
    // simply omits `prompt` is schema drift, not a subagent dispatch: every
    // genuine `Agent`/`Task` call carries a non-empty `prompt` (the
    // subagent's own task text), so this guard must not deny on `#[serde(
    // default)]`'s own zero values for a call it never actually recognised.
    if payload.tool_input.prompt.trim().is_empty() {
        return None;
    }

    let subagent_type = payload.tool_input.subagent_type.trim();
    let model = payload.tool_input.model.trim();

    // A fork inherits the seat model by construction and ignores `model`
    // outright, so naming a cheap one buys nothing and must not read as
    // though it did.
    let denied = if subagent_type == "fork" {
        true
    } else if !model.is_empty() {
        // An explicit model is honored, unless it asks for the seat tier
        // again by name, which is the exact spend being guarded.
        names_expensive_tier(model)
    } else {
        // No model named: only a subagent type that pins its own inherits.
        subagent_type.is_empty() || GENERIC_SUBAGENT_TYPES.contains(&subagent_type)
    };

    denied.then(|| pretool_deny_reason(seat))
}

/// The documented PreToolUse deny envelope. Printed on stdout with exit 0:
/// exit 2 would block too, but it blocks on stderr text and cannot be
/// overridden, and this hook must never be the reason a session cannot make
/// progress.
pub fn pretool_output(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    })
    .to_string()
}

/// Fails open on every path: no seat env, an unparseable payload, a tool this
/// guard knows nothing about and any internal error all exit 0 with nothing
/// on stdout, which claude reads as "no decision, use the normal permission
/// flow". Nothing here may `unwrap`, `expect` or return `Err` -- the release
/// profile is `panic = "abort"`, and a hook that aborts takes the tool call
/// with it.
pub fn run_pretool<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    let Some(seat) = env(adapters::SEAT_MODEL_ENV) else {
        return Ok(0);
    };
    let Ok(payload) = PreToolPayload::parse(stdin) else {
        return Ok(0);
    };
    if let Some(reason) = pretool_decision(Some(&seat), &payload) {
        let _ = writeln!(w, "{}", pretool_output(&reason));
    }
    Ok(0)
}

/// Field names codex uses for the rollout path, most specific first. Populate
/// from the verified notes file during Task A9/A10; the claude spelling stays
/// last so a hook registered on either agent keeps working.
const NOTIFY_TRANSCRIPT_KEYS: &[&str] = &["rollout_path", "session_file", "transcript_path"];

/// Maps an agent's notify payload onto the shape the scorer needs. Codex does
/// not use claude's field names, so this is a real mapping rather than an alias:
/// aliasing would let a renamed field parse as an empty transcript path and drop
/// every turn signal without a word.
pub fn notify_payload_to_hook(raw: &str) -> CtxResult<HookPayload> {
    let value: serde_json::Value = serde_json::from_str(raw)?;

    let transcript_path = NOTIFY_TRANSCRIPT_KEYS
        .iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .ok_or_else(|| {
            format!(
                "notify payload carries no known transcript field (tried {}); \
                 record the real field name in \
                 docs/superpowers/notes/2026-07-31-codex-cli-facts.md and add it to \
                 NOTIFY_TRANSCRIPT_KEYS",
                NOTIFY_TRANSCRIPT_KEYS.join(", ")
            )
        })?
        .to_string();

    let string_at = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    Ok(HookPayload {
        session_id: string_at("session_id"),
        transcript_path,
        cwd: string_at("cwd"),
        stop_hook_active: false,
    })
}

/// What an unmapped payload is allowed to leave behind. Diagnosing a field
/// mismatch needs the field names, never their values: a notify payload can
/// carry tokens, prompts and file contents, and the decision log is a plain
/// file that outlives the session.
pub fn notify_shape(payload: &str) -> String {
    const MAX_KEYS: usize = 200;
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(payload)
    else {
        return format!("unparseable notify payload, {} bytes", payload.len());
    };

    let mut keys: String = map.keys().cloned().collect::<Vec<_>>().join(", ");
    keys.truncate(MAX_KEYS);
    format!("notify payload fields: {keys}")
}

pub fn run_notify<W: Write>(w: &mut W, payload: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    // Codex passes its notify payload as an argument on some versions and on
    // stdin on others (see docs/superpowers/notes/2026-07-31-codex-cli-facts.md),
    // so both routes land here.
    let Ok(mapped) = notify_payload_to_hook(payload) else {
        // A hook never blocks the agent, so an unmapped payload is recorded
        // rather than surfaced. The decision log is where a silent mismatch
        // becomes visible.
        if let Ok(state) = StateDir::resolve(env) {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: "unknown",
                    verb: "hook",
                    verdict: "n/a",
                    score: 0,
                    action: "notify-unmapped",
                    detail: &notify_shape(payload),
                },
            );
        }
        return Ok(0);
    };

    // Same rule as every other branch here: a hook must exit 0 even if this
    // serialization step somehow failed, so `?` is not an option.
    let Ok(raw) = serde_json::to_string(&mapped) else {
        return Ok(0);
    };
    run_stop(w, &raw, env)
}

pub fn run<W: Write>(args: &HookArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    match &args.event {
        HookEvent::Stop => run_stop(w, &read_stdin(), &env),
        HookEvent::Prompt => {
            let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let marker = super::config::CtxConfig::load(&repo, &env)
                .map(|cfg| cfg.score.marker)
                .unwrap_or_else(|_| super::config::DEFAULT_MARKER.to_string());
            if !marker.is_empty() {
                let _ = writeln!(w, "{}", prompt_output(&marker));
            }
            Ok(0)
        }
        HookEvent::PreCompact => run_pre_compact(w, &read_stdin(), &env),
        HookEvent::Pretool => run_pretool(w, &read_stdin(), &env),
        HookEvent::Notify { payload } => {
            let raw = match payload {
                Some(text) => text.clone(),
                None => read_stdin(),
            };
            run_notify(w, &raw, &env)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::rot::{Score, Signals, Verdict};

    fn payload() -> HookPayload {
        HookPayload {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            cwd: "/work/repo".to_string(),
            stop_hook_active: false,
        }
    }

    fn score_of(verdict: Verdict, score: u32) -> Score {
        Score {
            score,
            verdict,
            signals: Signals {
                turns: 12,
                tool_failure_rate: 1.0,
                repetition_hits: 0,
                max_repeat: 1,
                marker_miss_rate: Some(1.0),
            },
            context_tokens: 170_000,
        }
    }

    #[test]
    fn payload_parsing_tolerates_missing_fields() {
        let parsed = HookPayload::parse("{\"session_id\":\"s\"}").expect("parse");
        assert_eq!(parsed.session_id, "s");
        assert_eq!(parsed.transcript_path, "");
        assert!(!parsed.stop_hook_active);

        let full = HookPayload::parse(
            "{\"session_id\":\"s\",\"transcript_path\":\"/t.jsonl\",\"cwd\":\"/c\",\"stop_hook_active\":true}",
        )
        .expect("parse");
        assert!(full.stop_hook_active);
        assert_eq!(full.cwd, "/c");
    }

    #[test]
    fn a_healthy_session_prints_nothing() {
        assert_eq!(
            stop_output(&payload(), &score_of(Verdict::Healthy, 10), None, None),
            None
        );
    }

    #[test]
    fn an_advisory_verdict_prints_a_non_blocking_system_message() {
        let out = stop_output(&payload(), &score_of(Verdict::Advise, 45), None, None)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed["systemMessage"].is_string());
        assert!(
            parsed.get("decision").is_none(),
            "the hook must never block the stop: {out}"
        );
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(text.contains("advise"), "verdict should be named: {text}");
        assert!(
            !text.contains('\u{2014}'),
            "no em dashes in user-facing copy"
        );
    }

    #[test]
    fn a_restart_verdict_still_only_advises() {
        let out = stop_output(&payload(), &score_of(Verdict::Restart, 95), None, None)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed.get("decision").is_none());
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            text.contains("zirv ctx resume"),
            "point at recovery: {text}"
        );
    }

    #[test]
    fn when_a_supervisor_owns_the_session_the_hook_stays_silent() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Restart, 95),
            Some(std::path::Path::new("/tmp/s/ab.sock")),
            None,
        );
        assert_eq!(out, None, "the supervisor intervenes, not the hook");
    }

    /// Ported canary case 7: never fire twice in a row.
    #[test]
    fn stop_hook_active_short_circuits_everything() {
        let mut p = payload();
        p.stop_hook_active = true;
        assert_eq!(
            stop_output(&p, &score_of(Verdict::Restart, 95), None, None),
            None
        );
    }

    fn stop_payload(transcript: &std::path::Path, cwd: &std::path::Path) -> String {
        serde_json::json!({
            "session_id": "s",
            "transcript_path": transcript,
            "cwd": cwd,
        })
        .to_string()
    }

    #[test]
    fn run_exits_zero_even_with_unparseable_stdin() {
        let mut out = Vec::new();
        let code = run_stop(&mut out, "this is not json", &|_| None).expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "nothing on stdout: {out:?}");
    }

    #[test]
    fn run_exits_zero_when_the_transcript_is_gone() {
        let mut out = Vec::new();
        let code = run_stop(
            &mut out,
            "{\"session_id\":\"s\",\"transcript_path\":\"/nope/missing.jsonl\",\"cwd\":\"/tmp\"}",
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn run_scores_a_real_transcript_and_advises() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let transcript = rotting_transcript(dir.path());

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let stdin = stop_payload(&transcript, dir.path());
        let mut out = Vec::new();
        let code = run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("json");
        assert!(
            parsed["systemMessage"]
                .as_str()
                .unwrap_or_default()
                .contains("restart")
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log written");
        assert!(log.contains("\"verb\":\"hook\""), "got {log}");
    }

    /// A supervisor cannot derive the agent's transcript path: the agent mints
    /// its own session id. The Stop hook runs inside that session, so it is the
    /// only party that knows, and the signal is the only channel it has.
    #[cfg(unix)]
    #[test]
    fn the_forwarded_signal_names_the_transcript_the_hook_scored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = rotting_transcript(dir.path());
        let socket = dir.path().join("t.sock");
        let server = signal::SignalServer::bind(&socket).expect("bind");

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SOCKET_ENV.to_string(),
            socket.display().to_string(),
        )]
        .into();
        let stdin = stop_payload(&transcript, dir.path());

        let mut out = Vec::new();
        run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut received = None;
        while received.is_none() && std::time::Instant::now() < deadline {
            received = server.try_recv();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let signal = received.expect("the hook forwards a signal");
        assert_eq!(
            signal.transcript_path.as_deref(),
            Some(transcript.display().to_string().as_str()),
            "the supervisor has no other way to learn this path"
        );
    }

    /// Twelve turns of tool errors and missed markers at 170k tokens: enough
    /// for a non-healthy verdict, which is what makes the hook forward at all.
    fn rotting_transcript(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("t.jsonl");
        let mut text = String::new();
        for i in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n");
            let block = if i < 2 { "[zirv] ok" } else { "sloppy" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{block}\"}}],\"usage\":{{\"input_tokens\":170000}}}}}}\n"
            ));
        }
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn a_failure_heavy_session_queues_an_optimize_recommendation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let transcript = dir.path().join("t.jsonl");
        let mut text = String::new();
        for i in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n");
            let block = if i < 2 { "[zirv] ok" } else { "sloppy" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{block}\"}}],\"usage\":{{\"input_tokens\":170000}}}}}}\n"
            ));
        }
        std::fs::write(&transcript, text).expect("write");

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let stdin = stop_payload(&transcript, dir.path());

        let mut out = Vec::new();
        let code = run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0, "the hook never blocks");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains(crate::commands::ctx::optimize::RECOMMEND_ACTION),
            "got {log}"
        );

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        let message = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            message.contains("zirv ctx optimize"),
            "mention it once: {message}"
        );
        assert!(parsed.get("decision").is_none(), "still never blocking");
    }

    /// Five corrections, no tool failures, low context: enough to recommend
    /// via the corrections signal alone, and healthy enough that the verdict
    /// stays `Healthy`.
    fn correction_heavy_transcript(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("t.jsonl");
        let mut text = String::new();
        for i in 0..12 {
            // Tools never fail here; the user keeps correcting.
            let prompt = if i < 5 {
                "no, not like that"
            } else {
                "carry on"
            };
            text.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"{prompt}\"}}}}\n"
            ));
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":false}]}}\n");
            text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] ok\"}],\"usage\":{\"input_tokens\":1000}}}\n");
        }
        std::fs::write(&path, text).expect("write");
        path
    }

    /// Item 1: `corrections_in` must use whichever adapter `cfg` selects
    /// (mirroring `score_transcript`), not a hardcoded claude call.
    #[test]
    fn corrections_are_computed_through_the_configured_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());
        let cfg = CtxConfig::default();
        assert_eq!(corrections_in(&transcript, &cfg), 5);
    }

    /// An adapter with no event parsing wired up (codex today -- out of
    /// scope, see issue #11) must degrade to zero corrections rather than
    /// panic: the recommendation is advisory, never load-bearing. Codex is
    /// selectable now (`CodexAdapter::ready` mirrors claude's), so this
    /// exercises `structural_context`'s all-empty stub rather than a
    /// selection failure, but the degrade-to-zero guarantee is the same one.
    #[test]
    fn corrections_are_zero_for_an_adapter_with_no_event_parsing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());
        let cfg = CtxConfig {
            agent: Some("codex".to_string()),
            ..CtxConfig::default()
        };
        assert_eq!(
            corrections_in(&transcript, &cfg),
            0,
            "an adapter with no parsing degrades to zero corrections, not a panic"
        );
    }

    #[test]
    fn a_correction_heavy_session_queues_one_even_with_clean_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let transcript = correction_heavy_transcript(dir.path());

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let stdin = stop_payload(&transcript, dir.path());

        let mut out = Vec::new();
        run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains(crate::commands::ctx::optimize::RECOMMEND_ACTION),
            "corrections alone must be enough to queue: {log}"
        );
        assert!(
            log.contains("5 corrections"),
            "and the entry says which signal: {log}"
        );
    }

    /// I1: a healthy, correction-heavy transcript must not be told to
    /// `/compact`, and must not blame tools it never used.
    #[test]
    fn a_healthy_correction_heavy_session_prints_only_the_optimize_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let transcript = correction_heavy_transcript(dir.path());

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let stdin = stop_payload(&transcript, dir.path());

        let mut out = Vec::new();
        run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        let message = parsed["systemMessage"].as_str().unwrap_or_default();

        assert!(
            !message.contains("/compact"),
            "a healthy session must not be told to compact: {message}"
        );
        assert!(
            !message.contains("hit tools hard"),
            "tool failure rate was 0.00, wording must not blame the tools: {message}"
        );
        assert!(
            message.contains("zirv ctx optimize"),
            "the optimize hint must still appear: {message}"
        );
    }

    /// Task A6: `select`'s gate check degrades the same way an adapter with
    /// no event parsing already does -- `corrections_in`'s `Ok(adapter)`
    /// else-branch covers a refused `select`. G (2026-08-15): disabling
    /// claude via a repo-only `.settings.toml` used to fall through to codex
    /// (enabled, and its own `ready()` succeeds) here, exercising `count_
    /// corrections`'s `structural_context` path instead. `resolve_default`
    /// now refuses that silent provider switch outright
    /// (`AgentGate::disabled_only_by_repo`), so this test exercises the
    /// refused-`select` path once more -- the assertion is unchanged (both
    /// paths degrade to zero, not a panic), but for a different reason than
    /// when this test was written.
    #[test]
    fn a_disabled_agent_leaves_the_stop_hook_a_silent_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");

        assert_eq!(
            corrections_in(&transcript, &cfg),
            0,
            "a refused fallback (repo may narrow, not select) still degrades to zero, not a panic"
        );
    }

    /// Review finding 1: `run_stop`'s config-load degradation
    /// (`CtxConfig::load(&repo, env).unwrap_or_default()`) used to fall back
    /// to a fully permissive gate, so a malformed *repo* `.settings.toml`
    /// could silently void an *operator* disable and let the hook compute
    /// corrections through the very adapter the operator turned off. The
    /// fallback must use `AgentGate::load_operator_only` so the operator's
    /// disable survives a broken repo layer.
    ///
    /// This is an end-to-end regression guard, not the primary evidence for
    /// the fix: `run_stop` already bails out earlier, at
    /// `score::score_transcript_cached`'s own `CtxConfig::load(repo, env)?`,
    /// for the exact same `(repo, env)` this test's malformed repo file
    /// breaks -- so the hook was already a silent no-op here before this fix,
    /// for an unrelated reason. `cfg_or_operator_only_gate_denies_what_the_
    /// operator_denied_even_with_a_broken_repo_layer` below is the test that
    /// actually exercises the changed line.
    #[test]
    fn a_malformed_repo_settings_file_does_not_revive_an_operator_disabled_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());

        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir repo");
        std::fs::write(dir.path().join(".zirv/.settings.toml"), "not [ valid toml").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let stdin = stop_payload(&transcript, dir.path());

        let mut out = Vec::new();
        run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(
            !log.contains("5 corrections"),
            "the disabled adapter must never be used to count corrections: {log}"
        );
    }

    /// Direct test of the changed line (review finding 1, hook.rs half): a
    /// malformed repo `.settings.toml` must not make `cfg_or_operator_only_
    /// gate` fall back to a fully permissive gate. Unlike the end-to-end
    /// test above, this reaches the fallback arm directly, independent of
    /// whatever else in `run_stop` might also happen to fail closed first.
    #[test]
    fn cfg_or_operator_only_gate_denies_what_the_operator_denied_even_with_a_broken_repo_layer() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(repo.join(".zirv/.settings.toml"), "not [ valid toml").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        assert!(
            CtxConfig::load(&repo, &|k| empty.get(k).cloned()).is_err(),
            "the malformed repo file must actually make CtxConfig::load fail, or this test \
             proves nothing"
        );

        let cfg = cfg_or_operator_only_gate(&repo, &|k| empty.get(k).cloned());
        assert!(
            !cfg.agents.is_enabled("claude"),
            "the operator's disable must survive a repo layer that could not be read"
        );
        assert_eq!(
            cfg.policy,
            super::super::policy::EffectivePolicy::fail_closed(),
            "issue #44: a failed config load must fail closed on policy too, not default to Allow"
        );
    }

    /// A malformed repo policy must fail closed while preserving the
    /// operator's own narrowing through `degrade_to_operator_only`.
    #[test]
    fn a_malformed_repo_policy_table_preserves_the_operators_policy_narrowing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\n",
        )
        .expect("write");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(
            repo.join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"nope\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        assert!(
            CtxConfig::load(&repo, &|k| empty.get(k).cloned()).is_err(),
            "the malformed repo [policy] table must actually make CtxConfig::load fail, or this \
             test proves nothing"
        );

        let cfg = cfg_or_operator_only_gate(&repo, &|k| empty.get(k).cloned());
        assert_eq!(
            cfg.policy.shell_exec,
            super::super::policy::Stance::Deny,
            "the operator's shell_exec=deny survives a broken repo layer"
        );
    }

    #[test]
    fn a_clean_session_queues_nothing_and_says_nothing_about_optimize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("t.jsonl");
        let mut text = String::new();
        for _ in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":false}]}}\n");
            text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] ok\"}],\"usage\":{\"input_tokens\":1000}}}\n");
        }
        std::fs::write(&transcript, text).expect("write");

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let stdin = stop_payload(&transcript, dir.path());

        let mut out = Vec::new();
        run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(
            !log.contains(crate::commands::ctx::optimize::RECOMMEND_ACTION),
            "got {log}"
        );
        assert!(
            !String::from_utf8_lossy(&out).contains("optimize"),
            "a healthy session hears nothing about it"
        );
    }

    #[test]
    fn prompt_hook_emits_the_documented_injection_shape() {
        let out = prompt_output("[zirv]");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit",
            "exact key casing matters: {out}"
        );
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext");
        assert!(
            context.contains("[zirv]"),
            "the marker must appear: {context}"
        );
        assert!(
            context.contains("final"),
            "only final answers carry the marker: {context}"
        );
        assert!(parsed.get("decision").is_none(), "never block a prompt");
        assert!(!context.contains('\u{2014}'));
    }

    #[test]
    fn prompt_hook_uses_the_configured_marker() {
        let out = prompt_output("[acme]");
        assert!(out.contains("[acme]"));
        assert!(
            !out.contains("[zirv]"),
            "nothing user-specific is hardcoded"
        );
    }

    /// Issue #225: this `additionalContext` is paid, uncached, on every user
    /// turn -- unlike the once-per-session prompt layers in `prompt.rs`, so it
    /// carries a hard byte budget the way `HARNESS_PROMPT`'s own doc comment
    /// tracks a shape budget. Pinned against the raw sentence, not the
    /// wrapping JSON, so growth in `additionalContext`'s own text is caught
    /// even if `hookSpecificOutput`'s envelope grows for an unrelated reason.
    #[test]
    fn prompt_hook_context_stays_under_the_ninety_byte_steady_state_budget() {
        let parsed: serde_json::Value =
            serde_json::from_str(&prompt_output("[zirv]")).expect("valid json");
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext")
            .to_string();
        assert!(
            context.len() <= 90,
            "steady-state per-turn context must stay <= 90 bytes for the default marker, \
             got {} bytes: {context}",
            context.len()
        );
        // Same contract the old 170-byte sentence carried: start every FINAL
        // answer with the marker on line 1, mid-turn notes are exempt, and
        // it is a context-health marker read by zirv ctx -- just shorter.
        for claim in ["final", "[zirv]", "mid-turn", "zirv ctx"] {
            assert!(
                context.contains(claim),
                "the trimmed sentence must still say '{claim}': {context}"
            );
        }
    }

    /// Observational is not the same as silent: a compaction is the single
    /// biggest context event in a session, and the decision log is where a
    /// later "why did quality drop here" gets answered.
    #[test]
    fn pre_compact_records_that_a_compaction_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let code = run_pre_compact(
            &mut out,
            "{\"session_id\":\"s\",\"transcript_path\":\"/tmp/t.jsonl\",\"cwd\":\"/work\"}",
            &|k| env.get(k).cloned(),
        )
        .expect("runs");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("systemMessage"),
            "the advisory still goes out: {printed}"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log written");
        assert!(log.contains("\"action\":\"pre-compact\""), "got {log}");
        assert!(log.contains("\"session\":\"s\""), "name the session: {log}");
        assert!(
            log.contains("/tmp/t.jsonl"),
            "name the transcript it happened in: {log}"
        );
    }

    #[test]
    fn pre_compact_exits_zero_even_with_unusable_stdin() {
        let mut out = Vec::new();
        let code = run_pre_compact(&mut out, "not json at all", &|_| None).expect("never errors");
        assert_eq!(code, 0);
        assert!(
            String::from_utf8_lossy(&out).contains("systemMessage"),
            "the advisory does not depend on the payload"
        );
    }

    #[test]
    fn pre_compact_only_advises_because_injection_is_unsupported() {
        let out = pre_compact_output();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed["systemMessage"].is_string());
        assert!(parsed.get("decision").is_none(), "never block a compaction");
        assert!(
            parsed.get("hookSpecificOutput").is_none(),
            "PreCompact honors no additionalContext"
        );
    }

    /// PLACEHOLDER PAYLOAD, REPLACE DURING A9/A10 EXECUTION. The literal below
    /// must be swapped for the real codex notify payload recorded in
    /// docs/superpowers/notes/2026-07-31-codex-cli-facts.md, and the field names
    /// in `notify_payload_to_hook` updated to match. Until then this test only
    /// proves the shape-mapping seam exists, not that it maps codex correctly.
    const CODEX_NOTIFY_SAMPLE: &str = "{\"type\":\"agent-turn-complete\",\"session_id\":\"s\",\"rollout_path\":\"/tmp/r.jsonl\",\"cwd\":\"/work\"}";

    #[test]
    fn notify_maps_the_codex_payload_onto_the_hook_payload() {
        let mapped = notify_payload_to_hook(CODEX_NOTIFY_SAMPLE).expect("mapping exists");
        assert_eq!(mapped.session_id, "s");
        assert_eq!(
            mapped.transcript_path, "/tmp/r.jsonl",
            "codex names the transcript differently from claude, so it must be mapped, not assumed"
        );
        assert_eq!(mapped.cwd, "/work");
        assert!(!mapped.stop_hook_active);
    }

    #[test]
    fn a_notify_payload_with_no_transcript_field_is_an_explicit_error() {
        // Silently scoring nothing is the failure mode this guards against: a
        // dropped turn signal with no diagnostic is worse than a loud mismatch.
        let err = notify_payload_to_hook("{\"session_id\":\"s\"}")
            .expect_err("an unmapped payload must not look like a healthy session");
        let msg = err.to_string();
        assert!(msg.contains("transcript"), "say what is missing: {msg}");
        assert!(
            msg.contains("codex-cli-facts"),
            "point at the verified notes: {msg}"
        );
    }

    #[test]
    fn notify_accepts_an_argv_payload_and_exits_zero() {
        let mut out = Vec::new();
        let code = run_notify(&mut out, CODEX_NOTIFY_SAMPLE, &|_| None).expect("runs");
        assert_eq!(code, 0);
    }

    /// An unmapped payload is the one case where something unrecognised gets
    /// written down, so it is also the one case that can leak.
    #[test]
    fn an_unmapped_notify_payload_is_logged_by_shape_and_never_by_value() {
        let payload = "{\"kind\":\"turn-done\",\"authorization\":\"Bearer sk-ant-secret-value\",\"prompt\":\"what the user actually typed\"}";
        let shape = notify_shape(payload);

        assert!(shape.contains("authorization"), "keys diagnose it: {shape}");
        assert!(shape.contains("kind"));
        assert!(
            !shape.contains("sk-ant-secret-value"),
            "values never reach the log: {shape}"
        );
        assert!(
            !shape.contains("what the user actually typed"),
            "values never reach the log: {shape}"
        );

        assert!(
            notify_shape("not json at all").contains("unparseable"),
            "an unparseable payload still says something useful"
        );
        assert!(
            !notify_shape("Bearer sk-ant-secret-value").contains("sk-ant"),
            "not even an unparseable one is quoted back"
        );
    }

    #[test]
    fn an_unmapped_payload_reaches_the_decision_log_by_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        run_notify(
            &mut out,
            "{\"kind\":\"turn-done\",\"token\":\"sk-ant-secret-value\"}",
            &|k| env.get(k).cloned(),
        )
        .expect("runs");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("notify-unmapped"), "got {log}");
        assert!(
            log.contains("token"),
            "the field name is the diagnosis: {log}"
        );
        assert!(
            !log.contains("sk-ant-secret-value"),
            "leaked a value: {log}"
        );
    }

    #[test]
    fn notify_survives_a_non_json_payload() {
        let mut out = Vec::new();
        let code = run_notify(&mut out, "agent-turn-complete", &|_| None).expect("runs");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "no output and no panic: {out:?}");
    }

    // -- PreToolUse: the expensive-seat inheritance guard ------------------

    /// Builds the PreToolUse stdin claude actually sends, so every rule below
    /// is exercised through the same parser the hook uses in production
    /// rather than through a hand-built struct.
    fn pretool_stdin(tool_name: &str, tool_input: serde_json::Value) -> String {
        serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/work/repo",
            "permission_mode": "default",
            "hook_event_name": "PreToolUse",
            "tool_name": tool_name,
            "tool_input": tool_input,
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string()
    }

    fn decide(
        seat: Option<&str>,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> Option<String> {
        let payload = PreToolPayload::parse(&pretool_stdin(tool_name, tool_input))
            .expect("the documented payload must parse");
        pretool_decision(seat, &payload)
    }

    const SEAT: Option<&str> = Some("fable");

    #[test]
    fn a_fork_is_denied_because_it_always_inherits_the_seat_model() {
        let reason = decide(
            SEAT,
            "Agent",
            serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"}),
        )
        .expect("a fork from an expensive seat must be blocked");
        assert!(reason.contains("fable"), "name the seat: {reason}");
        assert!(reason.contains("Fork"), "say forks are out: {reason}");
        assert!(!reason.contains('\u{2014}'), "no em dashes in user copy");
    }

    /// A fork ignores the `model` parameter entirely, so naming a cheap one
    /// must not buy a way past the rule.
    #[test]
    fn a_fork_is_denied_even_when_it_names_a_cheap_model() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "fork", "model": "haiku", "prompt": "do the thing"})
            )
            .is_some()
        );
    }

    #[test]
    fn an_explicit_seat_tier_model_is_denied() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "model": "fable", "prompt": "do the thing"})
            )
            .is_some(),
            "re-asking for the seat tier by name is the exact spend being guarded"
        );
    }

    #[test]
    fn an_explicit_cheaper_model_is_allowed() {
        for model in ["haiku", "sonnet", "opus"] {
            assert_eq!(
                decide(
                    SEAT,
                    "Agent",
                    serde_json::json!({"subagent_type": "general-purpose", "model": model})
                ),
                None,
                "{model} is the whole point of the escape hatch"
            );
        }
    }

    #[test]
    fn an_omitted_model_on_a_generic_subagent_type_is_denied() {
        for kind in ["fork", "claude", "general-purpose", "Explore", "Plan"] {
            assert!(
                decide(
                    SEAT,
                    "Agent",
                    serde_json::json!({"subagent_type": kind, "prompt": "do the thing"})
                )
                .is_some(),
                "{kind} pins no model of its own, so it inherits the seat"
            );
        }
    }

    /// A real dispatch (a non-empty `prompt`) with both `model` and
    /// `subagent_type` omitted/blank is denied exactly like an explicit
    /// generic type -- empty is the same as absent, once the payload is
    /// recognised as a real dispatch at all.
    #[test]
    fn an_omitted_model_and_an_omitted_subagent_type_is_denied() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "", "model": "", "prompt": "do the thing"})
            )
            .is_some(),
            "empty is the same as absent"
        );
    }

    /// A named `.claude/agents/<name>.md` definition carries its own `model`
    /// frontmatter, so zirv has no business second-guessing it.
    #[test]
    fn a_named_custom_subagent_type_with_no_model_is_allowed() {
        for kind in [
            "vault-keeper",
            "statusline-setup",
            "claude-security:explore",
        ] {
            assert_eq!(
                decide(SEAT, "Agent", serde_json::json!({"subagent_type": kind})),
                None,
                "{kind} pins its own model"
            );
        }
    }

    #[test]
    fn the_generic_type_set_is_matched_exactly_and_not_by_prefix() {
        assert_eq!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose-reviewer"})
            ),
            None,
            "a custom type that merely starts like a generic one still pins its own model"
        );
    }

    #[test]
    fn both_spellings_of_the_subagent_tool_are_covered() {
        for tool in ["Agent", "Task"] {
            assert!(
                decide(
                    SEAT,
                    tool,
                    serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"})
                )
                .is_some(),
                "{tool} dispatches subagents"
            );
        }
    }

    #[test]
    fn the_seat_tier_test_is_case_insensitive_and_covers_mythos() {
        for seat in ["fable", "Fable", "claude-fable-5", "mythos", "MYTHOS[1m]"] {
            assert!(
                decide(
                    Some(seat),
                    "Agent",
                    serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"})
                )
                .is_some(),
                "{seat} is an expensive seat"
            );
        }
        for model in ["Fable", "us.anthropic.mythos-v1"] {
            assert!(
                decide(
                    SEAT,
                    "Agent",
                    serde_json::json!({"subagent_type": "general-purpose", "model": model, "prompt": "do the thing"})
                )
                .is_some(),
                "{model} names the seat tier"
            );
        }
    }

    // -- fail open ---------------------------------------------------------

    /// A payload naming the subagent tool but carrying no `tool_input` field
    /// at all is schema drift, not a dispatch -- `#[serde(default)]` still
    /// fills in `PreToolInput::default()`, and with no `prompt` in it the
    /// guard must not deny on those defaulted zero values.
    #[test]
    fn agent_tool_with_no_tool_input_field_at_all_is_allowed() {
        let payload = PreToolPayload::parse(r#"{"tool_name":"Agent"}"#)
            .expect("tool_input is optional at the top level");
        assert_eq!(pretool_decision(SEAT, &payload), None);
    }

    /// An explicit empty `tool_input: {}` is the same case as a missing one:
    /// no `prompt`, so nothing recognisable as a real dispatch.
    #[test]
    fn agent_tool_with_an_empty_tool_input_object_is_allowed() {
        assert_eq!(decide(SEAT, "Agent", serde_json::json!({})), None);
    }

    /// `tool_input` present, with fields that would have denied under the
    /// old rule (a fork naming the seat tier itself), but no `prompt` at
    /// all -- still not a recognisable dispatch, so this must fail open.
    /// This is the exact defect the `prompt` gate fixes: before it, this
    /// payload was denied on defaulted zero values alone.
    #[test]
    fn agent_tool_input_lacking_a_prompt_field_is_allowed() {
        assert_eq!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "fork", "model": "fable"})
            ),
            None,
            "no prompt means this payload is not recognised as a real dispatch"
        );
    }

    /// Regression: once a payload actually carries a `prompt`, the existing
    /// deny rule for an omitted model on a generic subagent type still
    /// applies exactly as it did before the `prompt` gate.
    #[test]
    fn a_prompt_carrying_dispatch_with_omitted_model_on_a_generic_type_is_still_denied() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "prompt": "do the thing"})
            )
            .is_some()
        );
    }

    #[test]
    fn with_no_seat_env_nothing_is_ever_denied() {
        assert_eq!(
            decide(None, "Agent", serde_json::json!({"subagent_type": "fork"})),
            None,
            "the guard is scoped to an expensive orchestrator seat and nothing else"
        );
    }

    #[test]
    fn a_cheap_seat_denies_nothing() {
        for seat in ["sonnet", "opus", "haiku", ""] {
            assert_eq!(
                decide(
                    Some(seat),
                    "Agent",
                    serde_json::json!({"subagent_type": "fork"})
                ),
                None,
                "a {seat} seat costs what a fork of it costs"
            );
        }
    }

    #[test]
    fn a_non_subagent_tool_is_never_touched() {
        for tool in ["Bash", "Read", "Edit", "WebFetch", "mcp__memory__create"] {
            assert_eq!(
                decide(SEAT, tool, serde_json::json!({"command": "ls"})),
                None,
                "{tool} spawns no seat-inheriting session"
            );
        }
    }

    #[test]
    fn the_deny_output_matches_the_documented_pretooluse_shape() {
        let out = pretool_output("because");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse",
            "exact key casing matters: {out}"
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecision"], "deny",
            "got {out}"
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"], "because",
            "got {out}"
        );
    }

    #[test]
    fn run_pretool_denies_a_fork_end_to_end_and_still_exits_zero() {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
            "fable".to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0, "exit 2 would block on stderr text instead of json");

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[test]
    fn run_pretool_allows_a_cheap_dispatch_with_no_output_at_all() {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
            "fable".to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "model": "sonnet", "prompt": "do the thing"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "no json means no decision: {out:?}");
    }

    #[test]
    fn run_pretool_exits_zero_and_silent_without_the_seat_env() {
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin("Agent", serde_json::json!({"subagent_type": "fork"})),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "a non-zirv session is never made worse");
    }

    #[test]
    fn run_pretool_exits_zero_and_silent_on_garbage_stdin() {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
            "fable".to_string(),
        )]
        .into();
        for stdin in [
            "",
            "this is not json",
            "{",
            "[]",
            "null",
            "{\"tool_name\":\"Agent\",\"tool_input\":\"not an object\"}",
            "{\"tool_name\":42}",
        ] {
            let mut out = Vec::new();
            let code = run_pretool(&mut out, stdin, &|k| env.get(k).cloned())
                .unwrap_or_else(|_| panic!("must never error on {stdin:?}"));
            assert_eq!(code, 0, "must never block on {stdin:?}");
            assert!(out.is_empty(), "must stay silent on {stdin:?}: {out:?}");
        }
    }

    /// A `tool_input` carrying types this hook does not model (numbers,
    /// nested objects, `run_in_background`) is the ordinary case for every
    /// tool that is not the subagent one, and must not be a parse failure
    /// that quietly turns the guard off for the tools it does model.
    #[test]
    fn an_unmodelled_tool_input_still_parses_and_allows() {
        let payload = PreToolPayload::parse(&pretool_stdin(
            "Bash",
            serde_json::json!({"command": "rm -rf /tmp/build", "timeout": 120000, "run_in_background": false}),
        ))
        .expect("an ordinary Bash payload must parse");
        assert_eq!(payload.tool_name, "Bash");
        assert_eq!(pretool_decision(SEAT, &payload), None);
    }

    // -- the seat env the orchestrator exports ------------------------------

    #[test]
    fn only_an_orchestrator_launch_with_a_configured_model_exports_the_seat() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &[], Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
        assert!(
            seat_model_env(PromptRole::Worker, &[], Some("fable")).is_empty(),
            "a worker is not a seat that spawns subagents"
        );
        assert!(
            seat_model_env(PromptRole::Orchestrator, &[], None).is_empty(),
            "nothing configured, nothing to inherit"
        );
        assert!(
            seat_model_env(PromptRole::Orchestrator, &[], Some("   ")).is_empty(),
            "a blank model names no tier"
        );
    }

    /// FIX 1: an operator passthrough `--model` with no `chat.model`
    /// configured must still disclose the seat it actually launches on --
    /// the guard used to fail open on exactly this shape.
    #[test]
    fn an_operator_passed_model_flag_exports_the_seat_with_no_config_at_all() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model".to_string(), "fable".to_string()];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, None),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
    }

    /// FIX 1: an operator passthrough overriding a configured `chat.model`
    /// must disclose the flag's own value, not the configured one -- the
    /// launch actually runs on the flag.
    #[test]
    fn an_operator_passed_model_flag_wins_over_a_configured_chat_model() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model".to_string(), "sonnet".to_string()];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "sonnet".to_string())]
        );
    }

    /// With no operator passthrough at all, behavior is unchanged from
    /// before FIX 1: the configured `chat.model` alone decides.
    #[test]
    fn with_no_operator_flag_the_configured_model_still_decides() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &[], Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
    }

    /// The `--model=<value>` joined form is recognised too, not just the
    /// two-token spelling.
    #[test]
    fn the_joined_equals_form_of_the_model_flag_is_recognised() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model=opus".to_string()];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, None),
            vec![(SEAT_MODEL_ENV.to_string(), "opus".to_string())]
        );
    }

    /// A repeated `--model` is CLI last-wins: the later occurrence in argv
    /// order is the one the real harness actually launches on.
    #[test]
    fn a_repeated_model_flag_resolves_to_the_last_occurrence() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--model".to_string(),
            "haiku".to_string(),
        ];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, None),
            vec![(SEAT_MODEL_ENV.to_string(), "haiku".to_string())]
        );

        // Mixed spellings: the joined form arriving last still wins over an
        // earlier two-token occurrence.
        let mixed = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--model=haiku".to_string(),
        ];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &mixed, None),
            vec![(SEAT_MODEL_ENV.to_string(), "haiku".to_string())]
        );
    }

    /// FIX 1 still respects the two guards `seat_model_env` already had: a
    /// `Worker` role never exports regardless of what the flags carry, and a
    /// blank resolved model (however it was resolved) exports nothing.
    #[test]
    fn fix_1_still_respects_the_role_gate_and_the_blank_model_suppression() {
        use crate::commands::ctx::adapters::seat_model_env;
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model".to_string(), "fable".to_string()];
        assert!(
            seat_model_env(PromptRole::Worker, &flags, None).is_empty(),
            "a worker pane must never export a seat, flags or not"
        );
        let blank = vec!["--model".to_string(), "   ".to_string()];
        assert!(
            seat_model_env(PromptRole::Orchestrator, &blank, None).is_empty(),
            "a blank flag value names no tier, same as a blank configured model"
        );
    }

    #[test]
    fn notify_falls_back_to_the_claude_shape_when_that_is_what_arrives() {
        // The claude Stop payload already carries `transcript_path`, so a hook
        // registered on either agent keeps working.
        let mapped = notify_payload_to_hook(
            "{\"session_id\":\"s\",\"transcript_path\":\"/tmp/t.jsonl\",\"cwd\":\"/work\"}",
        )
        .expect("claude shape maps straight through");
        assert_eq!(mapped.transcript_path, "/tmp/t.jsonl");
    }
}
