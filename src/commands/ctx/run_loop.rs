use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::pace;
use super::rot::Verdict;
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick};
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
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
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
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    let adapter = adapters::select(args.agent.as_deref().or(cfg.agent.as_deref()), &[], &cfg)?;
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
    // One registry record for the whole run, refreshed (not re-registered)
    // each cycle since every cycle mints a fresh session id -- see
    // `SessionGuard::refresh_session`. `None` until the first cycle actually
    // has a session to register. Released explicitly at every arm that
    // leaves this loop, matching the explicit-arm discipline `RawGuard`
    // follows under this binary's `panic = "abort"` release profile.
    let mut session_guard: Option<super::sessions::SessionGuard> = None;
    loop {
        if let Some(limit) = args.cycles
            && cycle >= limit
        {
            if let Some(guard) = session_guard.as_mut() {
                guard.release();
            }
            return Ok(0);
        }
        cycle += 1;

        pace::wait_for_window(
            w, &state, &cfg.pace, "loop", "loop", &now_fn, &sleep_fn, None,
        );

        let mail_slug = super::state::repo_slug(repo);
        // N5: re-read every cycle, same as mail below -- each cycle is a
        // fresh, stateless session, so a fact remembered or verified since
        // the previous cycle must be picked up too, not a snapshot taken
        // once before the loop started.
        let memory_entries = super::memory::render_for_prompt(&state, &mail_slug, &cfg, now_fn());
        // Recomposed every cycle -- the same seam as `injection_args_for_
        // session` a few lines down -- because each cycle is a fresh,
        // stateless session: it must pick up whatever mail has arrived since
        // the previous cycle, not a snapshot taken once before the loop
        // started.
        let composed = super::prompt::compose(
            crate::utils::home_dir().ok().as_deref(),
            repo,
            args.simple,
            &cfg.prompt,
            super::prompt::PromptRole::Worker,
            &memory_entries,
            cfg.memory.max_injected_bytes,
        );
        // A fresh session id per cycle is the whole point: the orchestrator
        // never accumulates context across cycles. Minted here, ahead of
        // mail listing and the nudge-marker check below, both of which need
        // it.
        let session = SessionId::new_v4();
        let session_short = super::sessions::short_id(session.as_str());
        // N3 scopes delivery to the live session's own short id everywhere
        // else (`exec`, `wrap`'s advisory), but `loop` deliberately passes
        // `None` here rather than `Some(&session_short)`: unlike those,
        // `loop` has no stable session identity across a run -- every cycle
        // mints a brand new one -- so a message addressed to *this* cycle's
        // short id (a `zirv ctx nudge` sent while it is live, say) would
        // otherwise become permanently unaddressable the moment the next
        // cycle mints a different short id. `None` is what lets "the mail
        // arrives at the natural next cycle" (this module's own nudge
        // handling, a few lines down) actually hold.
        //
        // `mut`: drained right after this cycle's own spawn actually
        // succeeds (Item 3), not here -- a launch that fails to spawn, or a
        // pacing park ahead of it, must not move mail to `read/` before any
        // session has actually started to see it.
        let mut mail_entries: Vec<(PathBuf, super::mail::Message)> = if composed.is_some()
            && cfg.mail.enabled
        {
            super::mail::list(&state, &mail_slug, Some(adapter.name()), None).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mail_messages: Vec<super::mail::Message> =
            mail_entries.iter().map(|(_, msg)| msg.clone()).collect();
        if !mail_messages.is_empty() {
            announcer.emit(&super::announce::Event::MailDelivered {
                count: mail_messages.len(),
            });
        }
        let composed =
            super::prompt::with_mail_layer(composed, &mail_messages, cfg.mail.max_delivered_bytes);
        let (user_extra, composed) =
            super::prompt::merge_command_line_prompt(adapter.as_ref(), &args.extra, composed, None);

        match session_guard.as_mut() {
            Some(guard) => guard.refresh_session(session.as_str()),
            None => {
                session_guard = Some(super::sessions::SessionGuard::register(
                    &state,
                    super::sessions::Record::new(
                        session.as_str(),
                        adapter.name(),
                        repo,
                        super::sessions::Verb::Loop,
                    ),
                ));
            }
        }
        // M7: rebuilt per cycle because the private prompt file is named after
        // the session it belongs to, and every cycle is a new session. The
        // adapter builds this launch itself, so the probe gets an empty argv.
        let extra: Vec<String> = user_extra
            .iter()
            .cloned()
            .chain(super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            ))
            .collect();
        // M2: README promises injection attribution "at every session start",
        // and every cycle is a new session, so the entry is written here under
        // that cycle's own id rather than once under a literal "loop".
        super::prompt::log_injection(
            &state,
            "loop",
            session.as_str(),
            composed.as_ref(),
            adapter.capabilities().system_prompt,
        );
        announcer.emit(&super::prompt::injection_event(
            composed.as_ref(),
            adapter.capabilities().system_prompt,
        ));
        let transcript = adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        });

        let mut command = adapter.headless_cmd(&prompt, &session, &extra);
        command.current_dir(repo);
        // Names the same fact `ctx.toml`'s own `agent` key would, so a nested
        // `zirv ctx ...` call inside this cycle's own child processes
        // defaults to this cycle's harness rather than re-resolving from
        // scratch.
        command.env(super::adapters::AGENT_ENV, adapter.name());

        writeln!(w, "zirv ctx loop: cycle {cycle} session {session}")?;
        let (mut child, tap) = supervise::spawn_tapped(command)?;
        // Item 3: consumed right after this cycle's own spawn has actually
        // succeeded, so the next cycle's fresh `mail::list` does not pick
        // the same message up again -- but a launch that never got this far
        // (spawn_tapped failed, or `?` above already returned) leaves it
        // unread. A failed consume must not stop the cycle: the mail has
        // already reached the prompt either way, and housekeeping failures
        // are best-effort throughout the state dir.
        for (path, _) in mail_entries.drain(..) {
            let _ = super::mail::consume(&state, &mail_slug, &path);
        }
        let mut scorer = score::IncrementalScorer::new(transcript.clone());
        let mut rotted = false;
        let mut limit_hit = false;

        let outcome = {
            let mut tick = || {
                if pace::scan_for_limit(
                    &tap.try_lines(),
                    &state,
                    session.as_str(),
                    "loop",
                    &mut std::io::stderr(),
                ) {
                    limit_hit = true;
                    return Tick::Stop("limit");
                }
                // N4: `loop` never restarts for a nudge -- each cycle is
                // already a fresh, stateless session that re-lists mail on
                // its own natural boundary (see the mail block above), so
                // the nudge's payload arrives there regardless. Claiming the
                // marker here only stops it from re-firing and lets the
                // operator see that it arrived.
                if super::sessions::claim_nudge_marker(&state, &session_short) {
                    announcer.emit(&super::announce::Event::Nudge {
                        from: session_short.clone(),
                        restarted: false,
                    });
                }
                match scorer.poll(adapter.as_ref(), &cfg.score) {
                    Ok(Some(score)) if score.verdict == Verdict::Restart => {
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
        if !limit_hit {
            limit_hit = pace::scan_for_limit(
                &tap.try_lines(),
                &state,
                session.as_str(),
                "loop",
                &mut std::io::stderr(),
            );
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
                None,
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
            if let Some(guard) = session_guard.as_mut() {
                guard.release();
            }
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
            simple: false,
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
    fn a_cycle_launches_with_the_system_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "1");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut out = Vec::new();
        let mut args = args_for(1);
        args.simple = false;
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(argv.contains("--append-system-prompt"), "got {argv}");
        assert!(argv.contains("zirv session conventions"), "got {argv}");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"prompt-injected\""), "got {log}");
    }

    /// M2: README promises injection attribution "at every session start", and
    /// every cycle is its own session. One entry per cycle, each under that
    /// cycle's own id, not one entry under a literal "loop".
    #[test]
    fn injection_is_logged_once_per_cycle_under_that_cycles_session_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "1");
        }
        let mut out = Vec::new();
        let code = run_with(&args_for(2), &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        let attributed: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("\"action\":\"prompt-injected\""))
            .filter_map(|l| {
                let key = "\"session\":\"";
                let start = l.find(key)? + key.len();
                Some(&l[start..start + l[start..].find('"')?])
            })
            .collect();
        assert_eq!(attributed.len(), 2, "one entry per cycle: {log}");
        assert_ne!(
            attributed[0], attributed[1],
            "each cycle is a new session and must be logged under its own id: {log}"
        );
        assert!(
            !attributed.contains(&"loop"),
            "the verb is not a session id: {log}"
        );
    }

    /// I2: the caller's own --append-system-prompt (passed via --extra) must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[test]
    fn a_users_own_append_system_prompt_survives_alongside_zirvs_own() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "1");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut out = Vec::new();
        let mut args = args_for(1);
        args.simple = false;
        args.extra = vec![
            "--append-system-prompt".to_string(),
            "always answer in Danish".to_string(),
        ];
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
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
    fn simple_launches_with_no_zirv_text_at_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "1");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut out = Vec::new();
        let mut args = args_for(1);
        args.simple = true;
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(
            code.expect("runs"),
            0,
            "supervision is unaffected by --simple"
        );

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(!argv.contains("--append-system-prompt"), "got {argv}");
        assert!(!argv.contains("zirv session conventions"), "got {argv}");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"prompt-skipped\""), "got {log}");
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

    /// B3: `mail.enabled = false` must gate delivery at the `loop` seam too,
    /// not just `exec`'s (and not just `send`/`inbox`).
    #[test]
    fn disabled_mail_is_not_delivered_into_a_loop_cycle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "a disabled notice".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_MAIL".to_string(), "false".to_string());
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }

        let args = args_for(1);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            !argv.contains("a disabled notice"),
            "mail.enabled = false must gate delivery at the loop seam too: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a delivery that never happened must not consume the message either"
        );
    }

    /// T7: `run_with` recomposes the system prompt (and re-lists mail) inside
    /// the per-cycle loop, not once before it starts, so a message sent
    /// *during* one cycle reaches the next one within the very same `run_
    /// with` call. A custom one-shot agent script touches a marker file the
    /// instant its first invocation starts, then pauses briefly; a
    /// background thread races to write the mail as soon as it sees that
    /// marker, landing it before cycle 2's own compose-and-list runs.
    #[test]
    fn each_loop_cycle_picks_up_mail_that_arrived_since_the_previous_one() {
        // A plain tempdir, not `testenv::repo()`: this test never matches a
        // transcript path against `HOME` (the only reason that helper
        // canonicalizes), and on Windows a canonicalized `\\?\`-prefixed path
        // embedded in an argv string sh(1) parses loses its backslashes.
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let marker = tmp.path().join("cycle-marker");
        let script = tmp.path().join("mid-loop-agent.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             set -eu\n\
             [ -z \"${FAKE_AGENT_ARGV_LOG:-}\" ] || printf '%s\\n' \"$*\" >> \"$FAKE_AGENT_ARGV_LOG\"\n\
             if [ ! -f \"$CYCLE_MARKER\" ]; then\n\
             \ttouch \"$CYCLE_MARKER\"\n\
             \tsleep 0.3\n\
             fi\n\
             exit 0\n",
        )
        .expect("write script");

        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        // `adapter.headless_cmd` spawns `ZIRV_CTX_AGENT_BIN` directly, unlike
        // the raw `sh <script>` commands `fake_agent_command`-style helpers
        // use elsewhere; routing it through `sh` explicitly keeps this
        // test's spawn portable, the same way the wrap.rs relaunch tests
        // already do (`("ZIRV_CTX_AGENT_BIN", format!("sh {script}"))`).
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", script.display()),
        );
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // The spawned script inherits the real process environment (not the
        // `env` closure above, which only feeds `CtxConfig::load`), so these
        // two have to be real process env vars.
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
            std::env::set_var("CYCLE_MARKER", &marker);
        }

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let marker_for_writer = marker.clone();
        let writer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker_for_writer.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let state = crate::commands::ctx::state::StateDir::from_root(state_for_writer);
            let slug = crate::commands::ctx::state::repo_slug(&repo_for_writer);
            let _ = crate::commands::ctx::mail::store(
                &state,
                &slug,
                &crate::commands::ctx::mail::Message {
                    from_session: "other-session".to_string(),
                    from_agent: "claude".to_string(),
                    to: "any".to_string(),
                    to_session: None,
                    sent: 1,
                    body: "a fresh notice".to_string(),
                },
                &CtxConfig::default(),
            );
        });

        // Three cycles, not two: the third is what proves S3 -- mail
        // delivered into cycle 2's prompt is consumed right after, so cycle
        // 3's own fresh `mail::list` must not pick the same message up
        // again.
        let mut args = args_for(3);
        args.interval_secs = Some(0);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
            std::env::remove_var("CYCLE_MARKER");
        }
        assert_eq!(code.expect("all three cycles run"), 0);

        // Each cycle's own session id, from `zirv ctx loop`'s own "cycle N
        // session <id>" announcement on `out` -- the anchor used below,
        // since the composed prompt text itself contains embedded newlines
        // and would make a naive `argv.lines()` count meaningless.
        let printed = String::from_utf8(out).expect("utf8");
        let sessions: Vec<&str> = printed
            .lines()
            .filter_map(|line| line.rsplit(' ').next())
            .collect();
        assert_eq!(sessions.len(), 3, "three cycles ran: {printed:?}");

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        let cycle1_at = argv.find(sessions[0]).expect("cycle 1's own invocation");
        let cycle2_at = argv.find(sessions[1]).expect("cycle 2's own invocation");
        assert!(cycle1_at < cycle2_at, "cycle 1 launches before cycle 2");

        let mail_at = argv
            .find("a fresh notice")
            .expect("the mail must reach a launch's composed prompt");
        assert!(
            mail_at > cycle2_at,
            "mail sent during cycle 1 must land in cycle 2's own launch, not cycle 1's: {argv}"
        );
        assert_eq!(
            argv.matches("a fresh notice").count(),
            1,
            "delivered once in cycle 2, consumed, and must not reappear in cycle 3: {argv}"
        );
    }

    /// N4: a nudge sent while cycle 1 is live must not restart or kill it
    /// (`loop` never restarts for a nudge -- only `exec` does), and its
    /// payload -- ordinary session-addressed mail -- must reach cycle 2, the
    /// natural next boundary, the same way `each_loop_cycle_picks_up_mail_
    /// that_arrived_since_the_previous_one` proves for an undirected
    /// message.
    #[test]
    fn a_loop_cycle_reports_the_nudge_and_picks_it_up_at_its_own_boundary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let marker = tmp.path().join("cycle-marker");
        let script = tmp.path().join("mid-loop-agent.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             set -eu\n\
             [ -z \"${FAKE_AGENT_ARGV_LOG:-}\" ] || printf '%s\\n' \"$*\" >> \"$FAKE_AGENT_ARGV_LOG\"\n\
             if [ ! -f \"$CYCLE_MARKER\" ]; then\n\
             \ttouch \"$CYCLE_MARKER\"\n\
             \tsleep 0.3\n\
             fi\n\
             exit 0\n",
        )
        .expect("write script");

        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", script.display()),
        );
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
            std::env::set_var("CYCLE_MARKER", &marker);
        }

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let marker_for_writer = marker.clone();
        let writer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker_for_writer.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let state = crate::commands::ctx::state::StateDir::from_root(state_for_writer.clone());
            // The empty prefix matches whichever session is currently live --
            // `loop` keeps exactly one registry record at a time, refreshed
            // each cycle, so this is cycle 1's own short id while it is
            // still running.
            let Ok(live) = crate::commands::ctx::sessions::resolve_prefix(&state, "") else {
                return;
            };
            let env = [(
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_for_writer.display().to_string(),
            )]
            .into_iter()
            .collect::<HashMap<_, _>>();
            let args = crate::commands::ctx::sessions::NudgeArgs {
                prefix: live.short,
                message: Some("switch focus".to_string()),
                message_file: None,
            };
            let mut out = Vec::new();
            let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
            let _ = crate::commands::ctx::sessions::run_nudge_with(
                &args,
                &mut out,
                &repo_for_writer,
                &|k| env.get(k).cloned(),
                &mut stdin,
            );
        });

        let mut args = args_for(3);
        args.interval_secs = Some(0);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
            std::env::remove_var("CYCLE_MARKER");
        }
        assert_eq!(
            code.expect("all three cycles run, none killed by the nudge"),
            0
        );

        let printed = String::from_utf8(out).expect("utf8");
        let sessions: Vec<&str> = printed
            .lines()
            .filter_map(|line| line.rsplit(' ').next())
            .collect();
        assert_eq!(
            sessions.len(),
            3,
            "the nudge must not shorten the run: {printed:?}"
        );

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        let cycle2_at = argv.find(sessions[1]).expect("cycle 2's own invocation");
        let mail_at = argv
            .find("switch focus")
            .expect("the nudge's payload must reach a launch's composed prompt");
        assert!(
            mail_at > cycle2_at,
            "the nudge landed during cycle 1 and must reach cycle 2, its own next boundary: {argv}"
        );
    }
}
