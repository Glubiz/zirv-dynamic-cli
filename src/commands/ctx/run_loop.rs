use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::pace;
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
    //
    // `allow_hyphen_values`, because what gets passed through here is the
    // agent's own flags: `--extra --model --extra opus`.
    #[arg(long, allow_hyphen_values = true)]
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
    let mut failures = 0u32;
    let now_fn = now_secs;
    let sleep_fn = |d: Duration| std::thread::sleep(d);
    loop {
        if let Some(limit) = args.cycles
            && cycle >= limit
        {
            return Ok(0);
        }
        cycle += 1;

        pace::wait_for_window(w, &state, &cfg.pace, "loop", "loop", &now_fn, &sleep_fn);

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
        let (mut child, tap) = supervise::spawn_tapped(command)?;
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;
        let mut limit_hit = false;

        let outcome = {
            let agent = args.agent.as_deref().or(cfg.agent.as_deref());
            let mut tick = || {
                if tap.try_lines().iter().any(|line| pace::is_limit_hit(line)) {
                    limit_hit = true;
                    return Tick::Stop("limit");
                }
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

        // See the matching comment in exec.rs: supervise_child checks the
        // child's exit status before calling the tick, so a fast limit-hit
        // exit can race past the last tick that would have caught it.
        if !limit_hit && tap.try_lines().iter().any(|line| pace::is_limit_hit(line)) {
            limit_hit = true;
        }

        let (action, failed) = match outcome {
            // A usage limit is the window's fault, not the cycle's: park and
            // let the next cycle do the work.
            Outcome::StoppedByTick(_) if limit_hit => ("limit-park", false),
            // Rot is hygiene, not failure: the next cycle is the restart.
            Outcome::StoppedByTick(_) if rotted => ("rot-kill", false),
            Outcome::StoppedByTick(reason) => (reason, true),
            Outcome::TimedOut => ("timeout-kill", true),
            Outcome::Exited(0) if !limit_hit => ("ok", false),
            Outcome::Exited(0) => ("limit-park", false),
            Outcome::Exited(_) if limit_hit => ("limit-park", false),
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

        if limit_hit {
            pace::wait_for_window(
                w,
                &state,
                &cfg.pace,
                "loop",
                session.as_str(),
                &now_fn,
                &sleep_fn,
            );
        }

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
            &mut failures,
        )? {
            return Ok(code);
        }
    }
}

/// Exponential backoff on consecutive failures, capped at four intervals so a
/// broken loop still checks in occasionally.
pub fn backoff_for(failures: u32, base: Duration, interval: Duration) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let cap = interval.saturating_mul(4);
    let shift = (failures - 1).min(16);
    let scaled = base.saturating_mul(1u32 << shift);
    if scaled > cap { cap } else { scaled }
}

#[allow(clippy::too_many_arguments)]
fn handle_cycle_outcome<W: Write>(
    args: &LoopArgs,
    cfg: &CtxConfig,
    state: &StateDir,
    w: &mut W,
    repo: &Path,
    failed: bool,
    max_failures: u32,
    cycle: u32,
    interval: Duration,
    failures: &mut u32,
) -> CtxResult<Option<i32>> {
    if !failed {
        *failures = 0;
        if !interval.is_zero() {
            std::thread::sleep(interval);
        }
        return Ok(None);
    }

    *failures += 1;
    writeln!(
        w,
        "zirv ctx loop: cycle {cycle} failed ({}/{max_failures} consecutive)",
        *failures
    )?;

    if *failures >= max_failures {
        let on_failure = args
            .on_failure
            .clone()
            .or_else(|| cfg.supervise.on_failure.clone());
        let detail = match &on_failure {
            Some(command) => {
                let code = supervise::run_shell(command, repo)?;
                format!("on_failure exited {code}")
            }
            None => "no on_failure command configured".to_string(),
        };
        let _ = log::append(
            state,
            &log::Decision {
                ts: now_secs(),
                session: "loop",
                verb: "loop",
                verdict: "n/a",
                score: 0,
                action: "give-up",
                detail: &detail,
            },
        );
        writeln!(
            w,
            "zirv ctx loop: giving up after {} consecutive failures, exiting {EXIT_FAILED}",
            *failures
        )?;
        return Ok(Some(EXIT_FAILED));
    }

    let wait = backoff_for(
        *failures,
        Duration::from_secs(cfg.supervise.backoff_base_secs),
        interval,
    );
    if !wait.is_zero() {
        writeln!(w, "zirv ctx loop: backing off {}s", wait.as_secs())?;
        std::thread::sleep(wait);
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
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
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
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
        let tmp = crate::commands::ctx::testenv::repo();
        let env = base_env(&tmp.path().join("state"));
        let mut args = args_for(0);
        args.cycles = Some(0);
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to do");
        assert!(err.to_string().contains("cycles"), "got {err}");
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let base = Duration::from_secs(60);
        let interval = Duration::from_secs(900);
        assert_eq!(backoff_for(0, base, interval), Duration::ZERO);
        assert_eq!(backoff_for(1, base, interval), Duration::from_secs(60));
        assert_eq!(backoff_for(2, base, interval), Duration::from_secs(120));
        assert_eq!(backoff_for(3, base, interval), Duration::from_secs(240));
        assert_eq!(
            backoff_for(20, base, interval),
            Duration::from_secs(3600),
            "capped at four intervals"
        );
    }

    #[test]
    fn backoff_never_overflows_on_absurd_failure_counts() {
        let capped = backoff_for(u32::MAX, Duration::from_secs(60), Duration::from_secs(900));
        assert_eq!(capped, Duration::from_secs(3600));
    }

    #[test]
    fn repeated_failures_run_on_failure_and_exit_nonzero() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let env = base_env(&state);
        let marker = tmp.path().join("on-failure-ran");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
            std::env::set_var("FAKE_AGENT_TURNS", "2");
        }
        let mut args = args_for(5);
        args.max_failures = Some(2);
        args.on_failure = Some(format!("touch {}", marker.display()));
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(code.expect("runs"), EXIT_FAILED);
        assert!(marker.exists(), "the on_failure hook must run");
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "it stopped at the failure cap instead of running all 5 cycles"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"give-up\""), "got {log}");
    }

    #[test]
    fn a_successful_cycle_resets_the_failure_count() {
        let mut failures = 3u32;
        let mut out = Vec::new();
        let tmp = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let mut args = args_for(1);
        args.interval_secs = Some(0);

        let code = handle_cycle_outcome(
            &args,
            &cfg,
            &state,
            &mut out,
            tmp.path(),
            false,
            5,
            1,
            Duration::ZERO,
            &mut failures,
        )
        .expect("handled");
        assert_eq!(code, None, "keep looping");
        assert_eq!(failures, 0, "a green cycle clears the streak");
    }

    #[test]
    fn a_failure_below_the_cap_keeps_looping() {
        let mut failures = 0u32;
        let mut out = Vec::new();
        let tmp = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let mut args = args_for(1);
        args.on_failure = None;

        let code = handle_cycle_outcome(
            &args,
            &cfg,
            &state,
            &mut out,
            tmp.path(),
            true,
            5,
            1,
            Duration::ZERO,
            &mut failures,
        )
        .expect("handled");
        assert_eq!(code, None);
        assert_eq!(failures, 1);
    }

    #[test]
    fn a_zero_interval_disables_backoff_entirely() {
        assert_eq!(
            backoff_for(3, Duration::from_secs(60), Duration::ZERO),
            Duration::ZERO,
            "tests and one-shot loops must not sleep"
        );
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
    fn each_cycle_passes_the_pacing_gate_first() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "2");
        }
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args_for(1), &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(code.expect("runs"), 0, "a pause is never an exit");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(1),
            "it waited"
        );
        assert_eq!(transcripts_in(&home).len(), 1, "the cycle still ran");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    #[test]
    fn a_limit_hit_cycle_is_parked_and_is_not_a_failure() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_TURNS", "2");
        }
        let mut args = args_for(2);
        // One failure would end the loop, so this proves a park is not a failure.
        args.max_failures = Some(1);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "a usage limit is not a cycle failure: the window just needed time"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(!log.contains("\"action\":\"give-up\""), "got {log}");
        assert_eq!(transcripts_in(&home).len(), 2, "both cycles ran");
    }
}
