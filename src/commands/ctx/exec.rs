use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::pace;
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
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
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
/// only a restart verdict acts, and only for the session this supervisor owns:
/// the socket is named after eight hex characters of a session id, so a stale
/// hook can reach it and must not be able to kill a healthy child.
pub fn should_stop_for_signal(signal: &TurnSignal, session: &str) -> bool {
    signal.session_id == session && signal.verdict == Verdict::Restart
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

    let composed = super::prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &cfg.prompt,
    );
    // The first spawn's own argv may already carry the adapter's system-prompt
    // flag (e.g. `-- claude --append-system-prompt "..."`); merge it in rather
    // than letting `prompt_args` silently override it below.
    let (launch_command, composed) =
        super::prompt::merge_command_line_prompt(adapter.as_ref(), &args.command, composed);
    let prompt_args = super::prompt::injection_args(adapter.as_ref(), composed.as_ref());

    let session_raw = args
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);
    super::prompt::log_injection(
        &state,
        "exec",
        session.as_str(),
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );

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
    // Rebuilt for every session, because the hook inside a child reports the
    // session id this exports. Pinning the first one makes every restart's
    // signals look like they belong to a session that is already dead.
    let turn_env_for = |session: &SessionId| {
        server
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
            .unwrap_or_default()
    };

    let mut command = build_command(&launch_command, repo)?;
    for arg in &prompt_args {
        command.arg(arg);
    }
    for (key, value) in turn_env_for(&session) {
        command.env(key, value);
    }
    let mut restarts = 0;
    let now_fn = now_secs;
    let sleep_fn = |d: Duration| std::thread::sleep(d);

    loop {
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "exec",
            session.as_str(),
            &now_fn,
            &sleep_fn,
        );

        let (mut child, tap) = supervise::spawn_tapped(command)?;
        // Fresh watcher per iteration, over the current session's transcript.
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;
        let mut limit_hit = false;

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
            session.as_str(),
            &mut rotted,
            &tap,
            &mut limit_hit,
        )?;

        // `supervise_child` checks the child's exit status before calling the
        // tick, so a fast limit-hit exit (print the notice, exit immediately,
        // exactly what a real exhausted-window run looks like) can race past
        // the last tick that would have caught it. A final drain here closes
        // that race without touching supervise_child's general contract.
        if !limit_hit && tap.try_lines().iter().any(|line| pace::is_limit_hit(line)) {
            limit_hit = true;
        }

        match outcome {
            Outcome::Exited(code) if !limit_hit => return Ok(code),
            Outcome::Exited(_) | Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        if limit_hit {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "limit",
                    score: 100,
                    action: "limit-park",
                    detail: "agent reported a usage limit; parking until the window resets",
                },
            );
            writeln!(
                w,
                "zirv ctx exec: the agent reported a usage limit, parking until the window resets"
            )?;

            pace::wait_for_window(
                w,
                &state,
                &cfg.pace,
                "exec",
                session.as_str(),
                &now_fn,
                &sleep_fn,
            );

            let Some(prompt_text) = prompt.clone() else {
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit and the original prompt is unknown, so it cannot relaunch. Pass --prompt to enable parking."
                )?;
                return Ok(EXIT_ROT_EXHAUSTED);
            };

            // A park is not a restart: the budget is for rot, not for waiting.
            session = SessionId::new_v4();
            transcript = derive_transcript(&session);
            command = adapter.headless_cmd(&prompt_text, &session, &prompt_args);
            command.current_dir(repo);
            for (key, value) in turn_env_for(&session) {
                command.env(key, value);
            }
            continue;
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
        let (note, source) = handoff::distill_or_structural(
            adapter.as_ref(),
            &cfg.handoff.model,
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
        );
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
        command = adapter.headless_cmd(&combined, &session, &prompt_args);
        command.current_dir(repo);
        for (key, value) in turn_env_for(&session) {
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
    session: &str,
    rotted: &mut bool,
    tap: &supervise::OutputTap,
    limit_hit: &mut bool,
) -> CtxResult<Outcome> {
    let mut tick = || {
        if tap.try_lines().iter().any(|line| pace::is_limit_hit(line)) {
            *limit_hit = true;
            return Tick::Stop("limit");
        }
        if let Some(server) = server
            && let Some(received) = server.try_recv()
            && should_stop_for_signal(&received, session)
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            timeout_secs: Some(60),
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
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
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "88888888-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // First child rots, second is healthy.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
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
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "44444444-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
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
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            timeout_secs: None,
            simple: false,
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
            transcript_path: None,
        }
    }

    #[test]
    fn only_a_restart_signal_stops_the_run() {
        assert!(should_stop_for_signal(
            &signal_with(Verdict::Restart, 95),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Compact, 65),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Advise, 45),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Healthy, 0),
            "s"
        ));
    }

    /// The socket path is derived from the first eight hex characters of a
    /// session id, so a stale hook or a neighbouring run can reach it. Killing
    /// a healthy child on someone else's verdict is the failure to avoid.
    #[test]
    fn a_verdict_about_another_session_is_ignored() {
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Restart, 95),
            "a-different-session"
        ));
    }

    /// Every restart is a new session, and the hook inside it reports whatever
    /// `ZIRV_CTX_SESSION` says. Leave that pinned to the dead session's id and
    /// the session check above rejects every signal the restart produces.
    #[test]
    fn a_restarted_child_is_told_its_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));
        let seen = tmp.path().join("sessions.txt");

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &seen);
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
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged: Vec<String> = std::fs::read_to_string(&seen)
            .expect("the children recorded their session env")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(logged.len(), 2, "one line per child: {logged:?}");
        assert_eq!(logged[0], session, "the first child owns the given id");

        let first = transcript_for(&home, tmp.path(), session);
        let restarted = transcripts_in(&home)
            .into_iter()
            .find(|path| *path != first)
            .expect("the restarted child wrote its own transcript");
        let restarted_session = restarted
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("session id from the transcript name");
        assert_eq!(
            logged[1], restarted_session,
            "the restart must export the new session id, not the dead one's"
        );
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "55555555-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(1),
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
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

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "77777777-2222-4333-8444-555555555555";
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("x".repeat(120));
        let mut env = base_env(&long_state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0, "polling still supervises the run");
    }

    use crate::commands::ctx::window::{self, UsageWindows, Window};

    fn store_collector(state_dir: &std::path::Path, percent: f64, resets_in: u64) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + resets_in,
                    observed_at: now,
                }),
                seven_day: None,
            },
        )
        .expect("store collector state");
    }

    #[test]
    fn a_limit_hit_parks_and_relaunches_without_spending_the_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // A reset one second out plus no jitter keeps the park short; the point
        // is that it parks and relaunches, not how long it waits.
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());

        // First child hits the limit, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero budget: a limit hit must park even with no restarts allowed,
            // because a park is not a restart.
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the relaunched child finished cleanly, so exec exits with its code"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "a park must not consume the restart budget: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the relaunch is a new session with its own transcript"
        );
    }

    #[test]
    fn an_exhausted_window_delays_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "aaaaaaaa-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0, "a pause is never an exit");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(1),
            "it should have waited before spawning"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    #[test]
    fn a_restart_relaunches_with_the_system_prompt_too() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("--append-system-prompt"),
            "the restarted child must carry the prompt too: {argv}"
        );
    }

    /// I2: a user's own --append-system-prompt inside the `--` command must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[test]
    fn a_users_own_append_system_prompt_is_merged_into_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "dddddddd-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--append-system-prompt".to_string());
        command.push("always answer in Danish".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert_eq!(
            argv.matches("--append-system-prompt").count(),
            1,
            "exactly one flag must reach the agent: {argv}"
        );
        assert!(
            argv.contains("always answer in Danish"),
            "the user's own instruction must survive: {argv}"
        );
        assert!(
            argv.contains("zirv session conventions"),
            "zirv's own layer is still present: {argv}"
        );
    }

    #[test]
    fn a_healthy_window_adds_no_delay() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "bbbbbbbb-2222-4333-8444-555555555555";
        let env = base_env(&state);
        store_collector(&state, 5.0, 3600);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(!log.contains("pace-wait"), "nothing to wait for: {log}");
    }
}
