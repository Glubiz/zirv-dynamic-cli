// EXIT_FAILED is consumed by the backoff/failure-cap logic added in the next
// task of this plan, so it is dead code module-wide until then, matching
// config.rs/state.rs/log.rs/event.rs/handoff.rs/exec.rs.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::rot::Verdict;
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick, Watcher};
use super::{CtxResult, adapters, log, score};

/// Repeated cycle failures, escalated to the caller.
pub const EXIT_FAILED: i32 = 75;

#[derive(Debug, clap::Args)]
pub struct LoopArgs {
    /// Prompt to run each cycle.
    #[arg(long)]
    pub prompt: Option<String>,
    /// File holding the prompt, when it is long or shared.
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
    /// Adapter name: claude or codex.
    #[arg(long)]
    pub agent: Option<String>,
    /// Seconds to wait between cycles.
    #[arg(long)]
    pub interval_secs: Option<u64>,
    /// Wall-clock limit for one cycle.
    #[arg(long)]
    pub max_cycle_secs: Option<u64>,
    /// Consecutive failures before giving up.
    #[arg(long)]
    pub max_failures: Option<u32>,
    /// Shell command to run when the loop gives up.
    #[arg(long)]
    pub on_failure: Option<String>,
    /// Stop after this many cycles. Runs forever when omitted.
    #[arg(long)]
    pub cycles: Option<u32>,
    /// Extra arguments passed through to the agent.
    #[arg(long)]
    pub extra: Vec<String>,
}

pub fn resolve_prompt(args: &LoopArgs) -> CtxResult<String> {
    if let Some(prompt) = &args.prompt {
        return Ok(prompt.clone());
    }
    if let Some(path) = &args.prompt_file {
        return Ok(std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .trim()
            .to_string());
    }
    Err("no prompt: pass --prompt or --prompt-file".into())
}

pub fn run_with<W: Write>(
    args: &LoopArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    if args.cycles == Some(0) {
        return Err("--cycles must be at least 1".into());
    }

    let prompt = resolve_prompt(args)?;
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;
    let state = StateDir::resolve(env)?;

    let interval = Duration::from_secs(args.interval_secs.unwrap_or(cfg.supervise.interval_secs));
    let max_cycle =
        Duration::from_secs(args.max_cycle_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);
    let max_failures = args.max_failures.unwrap_or(cfg.supervise.max_failures);

    let mut cycle = 0u32;
    loop {
        if let Some(limit) = args.cycles
            && cycle >= limit
        {
            return Ok(0);
        }
        cycle += 1;

        // A fresh session id per cycle is the whole point: the orchestrator
        // never accumulates context across cycles.
        let session = SessionId::new_v4();
        let transcript = adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        });

        let mut command = adapter.headless_cmd(&prompt, &session, &args.extra);
        command.current_dir(repo);

        writeln!(w, "zirv ctx loop: cycle {cycle} session {session}")?;
        let mut child = supervise::spawn(command)?;
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;

        let outcome = {
            let agent = args.agent.as_deref().or(cfg.agent.as_deref());
            let mut tick = || {
                match watcher.read_if_changed() {
                    Ok(Some(_)) => {}
                    _ => return Tick::Continue,
                }
                match score::score_transcript(&transcript, agent, repo, env) {
                    Ok(score) if score.verdict == Verdict::Restart => {
                        rotted = true;
                        Tick::Stop("rot")
                    }
                    _ => Tick::Continue,
                }
            };
            supervise::supervise_child(&mut child, Instant::now() + max_cycle, poll, &mut tick)?
        };

        let (action, failed) = match outcome {
            // Rot is hygiene, not failure: the next cycle is the restart.
            Outcome::StoppedByTick(_) if rotted => ("rot-kill", false),
            Outcome::StoppedByTick(reason) => (reason, true),
            Outcome::TimedOut => ("timeout-kill", true),
            Outcome::Exited(0) => ("ok", false),
            Outcome::Exited(_) => ("nonzero-exit", true),
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "loop",
                verdict: if rotted { "restart" } else { "n/a" },
                score: 0,
                action,
                detail: &transcript.display().to_string(),
            },
        );

        if let Some(code) = handle_cycle_outcome(
            args,
            &cfg,
            &state,
            w,
            repo,
            failed,
            max_failures,
            cycle,
            interval,
        )? {
            return Ok(code);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_cycle_outcome<W: Write>(
    _args: &LoopArgs,
    _cfg: &CtxConfig,
    _state: &StateDir,
    _w: &mut W,
    _repo: &Path,
    _failed: bool,
    _max_failures: u32,
    _cycle: u32,
    interval: Duration,
) -> CtxResult<Option<i32>> {
    if !interval.is_zero() {
        std::thread::sleep(interval);
    }
    Ok(None)
}

pub fn run<W: Write>(args: &LoopArgs, w: &mut W) -> CtxResult<i32> {
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
            ("ZIRV_CTX_POLL_MS".to_string(), "50".to_string()),
        ]
        .into()
    }

    fn args_for(cycles: u32) -> LoopArgs {
        LoopArgs {
            prompt: Some("run the issue loop".to_string()),
            prompt_file: None,
            agent: Some("claude".to_string()),
            interval_secs: Some(0),
            max_cycle_secs: Some(30),
            max_failures: Some(3),
            on_failure: None,
            cycles: Some(cycles),
            extra: Vec::new(),
        }
    }

    fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let projects = home.join(".claude/projects");
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&projects) else {
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

    #[test]
    fn prompt_resolution_prefers_the_flag_then_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("p.txt");
        std::fs::write(&file, "from the file\n").expect("write");

        let mut args = args_for(1);
        assert_eq!(resolve_prompt(&args).expect("prompt"), "run the issue loop");

        args.prompt = None;
        args.prompt_file = Some(file);
        assert_eq!(resolve_prompt(&args).expect("prompt"), "from the file");

        args.prompt_file = None;
        let err = resolve_prompt(&args).expect_err("no prompt at all");
        assert!(err.to_string().contains("--prompt"), "got {err}");
    }

    #[test]
    fn each_cycle_gets_a_fresh_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let env = base_env(&tmp.path().join("state"));

        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "3");
        }
        let mut out = Vec::new();
        let code = run_with(&args_for(3), &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(code.expect("runs"), 0);
        let found = transcripts_in(&home);
        assert_eq!(found.len(), 3, "one transcript per cycle: {found:?}");
    }

    #[test]
    fn a_rotted_cycle_is_killed_without_counting_as_a_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let env = base_env(&state);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut args = args_for(2);
        args.max_failures = Some(1);
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "rot is session hygiene, not a cycle failure: the next cycle is the restart"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(25),
            "both cycles were killed early, not left to sleep 30s each"
        );
        assert_eq!(transcripts_in(&home).len(), 2);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"rot-kill\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "no failure escalation: {log}"
        );
    }

    #[test]
    fn zero_cycles_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = base_env(&tmp.path().join("state"));
        let mut args = args_for(0);
        args.cycles = Some(0);
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to do");
        assert!(err.to_string().contains("cycles"), "got {err}");
    }
}
