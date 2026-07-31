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

pub fn run<W: Write>(args: &HookArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    match &args.event {
        HookEvent::Stop => run_stop(w, &read_stdin(), &env),
        _ => Ok(0),
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
}
