use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::adapters::{SESSION_ENV, SOCKET_ENV};
use super::config::{EnvLookup, env_from_process};
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

/// Decides what the Stop hook prints. `None` means print nothing, which is also
/// what every failure path does.
pub fn stop_output(payload: &HookPayload, score: &Score, socket: Option<&Path>) -> Option<String> {
    if payload.stop_hook_active {
        return None;
    }
    if socket.is_some() {
        return None;
    }
    if score.verdict == Verdict::Healthy {
        return None;
    }

    let advisory = format!(
        "zirv ctx: verdict {} (score {}, context {} tokens). Consider /compact, or run `zirv ctx resume` for a clean session with a handoff.",
        score.verdict.as_str(),
        score.score,
        score.context_tokens
    );
    serde_json::to_string(&serde_json::json!({ "systemMessage": advisory })).ok()
}

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
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
    let Ok(score) = score::score_transcript(transcript, None, &repo, env) else {
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
    }

    if let Some(line) = stop_output(&payload, &score, socket.as_deref()) {
        let _ = writeln!(w, "{line}");
    }
    Ok(0)
}

/// UserPromptSubmit is the only hook that can add context to the model, which
/// is how the marker signal gets installed.
pub fn prompt_output(marker: &str) -> String {
    let context = format!(
        "Start every final answer in this session with the prefix {marker} on the first line. \
         Mid-turn status notes do not need it. This is a context-health marker read by zirv ctx."
    );
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
                    detail: payload.chars().take(200).collect::<String>().as_str(),
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
            stop_output(&payload(), &score_of(Verdict::Healthy, 10), None),
            None
        );
    }

    #[test]
    fn an_advisory_verdict_prints_a_non_blocking_system_message() {
        let out = stop_output(&payload(), &score_of(Verdict::Advise, 45), None)
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
        let out = stop_output(&payload(), &score_of(Verdict::Restart, 95), None)
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
        );
        assert_eq!(out, None, "the supervisor intervenes, not the hook");
    }

    /// Ported canary case 7: never fire twice in a row.
    #[test]
    fn stop_hook_active_short_circuits_everything() {
        let mut p = payload();
        p.stop_hook_active = true;
        assert_eq!(stop_output(&p, &score_of(Verdict::Restart, 95), None), None);
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
        let transcript = rotting_transcript(dir.path());

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let stdin = format!(
            "{{\"session_id\":\"s\",\"transcript_path\":\"{}\",\"cwd\":\"{}\"}}",
            transcript.display(),
            dir.path().display()
        );
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
        let stdin = format!(
            "{{\"session_id\":\"s\",\"transcript_path\":\"{}\",\"cwd\":\"{}\"}}",
            transcript.display(),
            dir.path().display()
        );

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

    #[test]
    fn notify_survives_a_non_json_payload() {
        let mut out = Vec::new();
        let code = run_notify(&mut out, "agent-turn-complete", &|_| None).expect("runs");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "no output and no panic: {out:?}");
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
