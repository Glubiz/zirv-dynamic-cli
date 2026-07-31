use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::rot::Verdict;
use super::signal::{self, TurnSignal};
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick, Watcher};
use super::{CtxResult, adapters, handoff, log, score};

/// The restart budget is spent and the session is still rotting. Callers apply
/// their own policy from here.
pub const EXIT_ROT_EXHAUSTED: i32 = 75;
/// Wall-clock timeout with no restarts left.
pub const EXIT_TIMEOUT: i32 = 76;

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id of the supervised run, used to locate its transcript.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Transcript path, when the agent writes somewhere the adapter cannot derive.
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    /// Prompt to reuse on restart. Extracted from the command when omitted.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// The headless agent command, after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
}

/// Finds the prompt in a headless agent command. Returns `None` rather than
/// guessing: a restart with the wrong prompt is worse than no restart.
pub fn extract_prompt(command: &[String]) -> Option<String> {
    for (index, arg) in command.iter().enumerate().skip(1) {
        let is_prompt_flag = arg == "-p" || arg == "--print";
        let is_subcommand = arg == "exec";
        if !is_prompt_flag && !is_subcommand {
            continue;
        }
        let next = command.get(index + 1)?;
        if next.starts_with('-') {
            return None;
        }
        return Some(next.clone());
    }
    None
}

/// Compaction of a headless run is pointless (there is no TUI to type into), so
/// only a restart verdict acts.
pub fn should_stop_for_signal(signal: &TurnSignal) -> bool {
    signal.verdict == Verdict::Restart
}

fn build_command(command: &[String], repo: &Path) -> CtxResult<Command> {
    let (program, rest) = command
        .split_first()
        .ok_or("no command to supervise; pass it after --")?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(repo);
    Ok(cmd)
}

pub fn run_with<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &args.command,
        cfg.agent_bin.as_deref(),
    )?;
    let state = StateDir::resolve(env)?;

    let session_raw = args
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);

    let derive_transcript = |session: &SessionId| {
        adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    };

    // `--transcript` describes the caller's own first child only. Every restart
    // is a new session launched by the adapter, so its transcript path has to be
    // derived again or the watcher would keep polling the dead child's file.
    let mut transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| derive_transcript(&session));

    let prompt = args
        .prompt
        .clone()
        .or_else(|| extract_prompt(&args.command));
    let max_restarts = args.max_restarts.unwrap_or(cfg.supervise.max_restarts);
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);

    let socket_path = state.socket_for(session.as_str());
    let server = match signal::SignalServer::bind(&socket_path) {
        Ok(server) => Some(server),
        Err(e) => {
            // Turn signals only accelerate detection; polling is the floor.
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "no-socket",
                    detail: &e.to_string(),
                },
            );
            None
        }
    };
    let turn_env = server
        .as_ref()
        .map(|server| {
            adapter
                .register_turn_signal(
                    &SessionRef {
                        id: session.clone(),
                        cwd: repo.to_path_buf(),
                    },
                    server.path(),
                )
                .env
        })
        .unwrap_or_default();

    let mut command = build_command(&args.command, repo)?;
    for (key, value) in &turn_env {
        command.env(key, value);
    }
    let mut restarts = 0;

    loop {
        let mut child = supervise::spawn(command)?;
        // Fresh watcher per iteration, over the current session's transcript.
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;

        let outcome = supervise_run(
            &mut child,
            Instant::now() + timeout,
            poll,
            &mut watcher,
            &transcript,
            args.agent.as_deref().or(cfg.agent.as_deref()),
            repo,
            env,
            server.as_ref(),
            &mut rotted,
        )?;

        match outcome {
            Outcome::Exited(code) => return Ok(code),
            Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        let reason = if rotted { "rot" } else { "timeout" };
        let exhausted_code = if rotted {
            EXIT_ROT_EXHAUSTED
        } else {
            EXIT_TIMEOUT
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "kill",
                detail: &transcript.display().to_string(),
            },
        );

        let Some(prompt_text) = prompt.clone() else {
            writeln!(
                w,
                "zirv ctx exec: {reason} detected but the original prompt is unknown, so it cannot restart. Pass --prompt to enable restarts."
            )?;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "stand-down",
                    detail: "no prompt available for restart",
                },
            );
            return Ok(exhausted_code);
        };

        if restarts >= max_restarts {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "give-up",
                    detail: "restart budget exhausted",
                },
            );
            writeln!(
                w,
                "zirv ctx exec: {reason} after {restarts} restarts, giving up with exit {exhausted_code}"
            )?;
            return Ok(exhausted_code);
        }

        let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
        let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
        let (note, source) =
            handoff::distill_or_structural(adapter.as_ref(), &cfg.handoff.model, &ctx);
        let stored = handoff::store(&state, repo, session.as_str(), &note)?;

        restarts += 1;
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "restart",
                detail: &format!("{source} handoff at {}", stored.display()),
            },
        );
        writeln!(
            w,
            "zirv ctx exec: {reason} detected, restarting ({restarts}/{max_restarts}) with a {source} handoff"
        )?;

        session = SessionId::new_v4();
        // The new session writes somewhere new, so the next iteration's watcher
        // must follow it rather than the file the killed child left behind.
        transcript = derive_transcript(&session);
        let combined = format!("{prompt_text}\n\n{}", note.to_markdown());
        command = adapter.headless_cmd(&combined, &session, &[]);
        command.current_dir(repo);
        for (key, value) in &turn_env {
            command.env(key, value);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise_run(
    child: &mut std::process::Child,
    deadline: Instant,
    poll: Duration,
    watcher: &mut Watcher,
    transcript: &Path,
    agent: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
    server: Option<&signal::SignalServer>,
    rotted: &mut bool,
) -> CtxResult<Outcome> {
    let mut tick = || {
        if let Some(server) = server
            && let Some(received) = server.try_recv()
            && should_stop_for_signal(&received)
        {
            *rotted = true;
            return Tick::Stop("rot");
        }
        // A scoring failure must never kill a healthy run.
        match watcher.read_if_changed() {
            Ok(Some(_)) => {}
            _ => return Tick::Continue,
        }
        match score::score_transcript(transcript, agent, repo, env) {
            Ok(score) if score.verdict == Verdict::Restart => {
                *rotted = true;
                Tick::Stop("rot")
            }
            _ => Tick::Continue,
        }
    };
    supervise::supervise_child(child, deadline, poll, &mut tick)
}

pub fn run<W: Write>(args: &ExecArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Runs the fake agent directly, so `exec` supervises a real child whose
    /// transcript path we control through `--transcript`.
    fn fake_agent_command(session: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-agent.sh").display().to_string(),
            "-p".to_string(),
            "do the work".to_string(),
            "--session-id".to_string(),
            session.to_string(),
        ]
    }

    fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                fixture("fake-agent.sh").display().to_string(),
            ),
        ]
        .into()
    }

    fn transcript_for(home: &std::path::Path, repo: &std::path::Path, session: &str) -> PathBuf {
        home.join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(repo))
            .join(format!("{session}.jsonl"))
    }

    #[test]
    fn prompt_extraction_finds_the_dash_p_argument() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert_eq!(extract_prompt(&cmd), Some("fix the bug".to_string()));
    }

    #[test]
    fn prompt_extraction_handles_print_and_positional_forms() {
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--print".to_string(),
                "go".to_string()
            ]),
            Some("go".to_string())
        );
        assert_eq!(
            extract_prompt(&["codex".to_string(), "exec".to_string(), "go".to_string()]),
            Some("go".to_string())
        );
    }

    #[test]
    fn prompt_extraction_gives_up_rather_than_guessing() {
        assert_eq!(
            extract_prompt(&["claude".to_string(), "-p".to_string()]),
            None
        );
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--resume".to_string(),
                "abc".to_string()
            ]),
            None
        );
        assert_eq!(extract_prompt(&[]), None);
    }

    #[test]
    fn a_healthy_run_exits_with_the_childs_own_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    #[test]
    fn a_failing_child_propagates_its_exit_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3);
    }

    /// `FAKE_AGENT_MODE` applies to every invocation, so both the original child
    /// and the restarted one rot and the budget runs out.
    #[test]
    fn a_rotted_run_is_killed_restarted_and_capped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-2222-4333-8444-555555555555";
        let env = base_env(&state);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "the caller applies its own policy after the budget is spent"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(
            log.contains("\"action\":\"restart\""),
            "a restart was attempted: {log}"
        );
        assert!(
            log.contains("\"action\":\"give-up\""),
            "and then it stopped: {log}"
        );

        let handoffs = state.join("handoffs");
        let stored: Vec<_> = walk_md(&handoffs);
        assert!(
            !stored.is_empty(),
            "a handoff is written before each restart"
        );
    }

    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }

    fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(home.join(".claude/projects")) else {
            return found;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                if file.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    found.push(file.path());
                }
            }
        }
        found
    }

    /// The restarted child is a new session writing to a new transcript, so
    /// supervision must follow it there. If the watcher kept polling the killed
    /// child's rotted file, this healthy second child would be killed too and
    /// the run would exit 75 instead of 0.
    #[test]
    fn a_restart_supervises_the_new_sessions_transcript() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "88888888-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // First child rots, second is healthy.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the healthy restarted child must be allowed to finish"
        );

        let found = transcripts_in(&home);
        assert_eq!(found.len(), 2, "one transcript per session: {found:?}");
        let first = transcript_for(&home, tmp.path(), session);
        assert!(
            found.contains(&first),
            "the original session's transcript: {found:?}"
        );
        assert!(
            found.iter().any(|p| *p != first),
            "the restarted session wrote its own transcript: {found:?}"
        );
    }

    #[test]
    fn a_run_with_no_discoverable_prompt_refuses_to_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "44444444-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            timeout_secs: Some(60),
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "rot was detected but no restart was possible"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("cannot restart"),
            "say why supervision stood down: {text}"
        );
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            timeout_secs: None,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to supervise");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    fn signal_with(verdict: Verdict, score: u32) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn: 4,
            score,
            verdict,
        }
    }

    #[test]
    fn only_a_restart_signal_stops_the_run() {
        assert!(should_stop_for_signal(&signal_with(Verdict::Restart, 95)));
        assert!(!should_stop_for_signal(&signal_with(Verdict::Compact, 65)));
        assert!(!should_stop_for_signal(&signal_with(Verdict::Advise, 45)));
        assert!(!should_stop_for_signal(&signal_with(Verdict::Healthy, 0)));
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "55555555-2222-4333-8444-555555555555";
        let env = base_env(&state);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(1),
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_TIMEOUT);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline must not wait for the child"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"timeout\""), "got {log}");
    }

    #[cfg(unix)]
    #[test]
    fn the_child_is_told_where_the_socket_is() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "66666666-2222-4333-8444-555555555555";
        let env = base_env(&state);
        let marker = tmp.path().join("socket-env.txt");

        // A child that records the socket env it inherited, then exits.
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$ZIRV_CTX_SOCKET\" > {}; exit 0",
                marker.display()
            ),
        ];

        unsafe {
            std::env::set_var("HOME", &home);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            command,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let seen = std::fs::read_to_string(&marker).expect("marker written");
        assert!(seen.ends_with(".sock"), "socket path exported: {seen}");
        assert!(seen.contains("66666666"), "per-session socket: {seen}");
    }

    #[test]
    fn an_unbindable_socket_does_not_stop_the_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "77777777-2222-4333-8444-555555555555";
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("x".repeat(120));
        let mut env = base_env(&long_state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0, "polling still supervises the run");
    }
}
