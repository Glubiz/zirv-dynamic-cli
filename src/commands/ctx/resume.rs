use std::io::Write;
use std::path::Path;

use super::adapters::SESSION_ENV;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::SessionId;
use super::handoff::{Handoff, latest_for_repo};
use super::state::StateDir;
use super::{CtxResult, adapters};

#[derive(Debug, clap::Args)]
pub struct ResumeArgs {
    /// Adapter name: claude or codex.
    #[arg(long)]
    pub agent: Option<String>,
    /// Print the composed initial prompt instead of launching the agent.
    #[arg(long, default_value_t = false)]
    pub print_prompt: bool,
    /// Extra arguments passed through to the agent.
    //
    // `allow_hyphen_values`, because what gets passed through here is the
    // agent's own flags: `--extra --continue`.
    #[arg(long, allow_hyphen_values = true)]
    pub extra: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// Start even though this process looks like it is already inside an
    /// agent session. Off by default: a nested interactive session can take
    /// the outer session down.
    #[arg(long, default_value_t = false)]
    pub allow_nested: bool,
}

/// The launch `resume` hands the terminal to.
///
/// Split out of `run_with` so the environment it gives the child is testable:
/// `run_with` itself `exec`s on unix, which would replace the test binary.
///
/// N1: the scrub comes first and is unconditional. `resume` sets a fresh
/// `SESSION_ENV` but used to leave `ZIRV_CTX_SOCKET` and
/// `ZIRV_CTX_TRANSCRIPT` inherited from whatever session launched it, so the
/// resumed agent's hooks reported their turn boundaries onto the *outer*
/// supervisor's socket -- under a session id that supervisor had never heard
/// of. Same rule the supervisors follow: a child speaks with its own
/// identity or with none.
fn launch_command(
    adapter: &dyn adapters::AgentAdapter,
    prompt: &str,
    extra: &[String],
    repo: &Path,
    session: &str,
    agent_name: &str,
) -> std::process::Command {
    let mut command = adapter.interactive_cmd(Some(prompt), extra);
    command.current_dir(repo);
    super::sessions::scrub_supervision_env_cmd(&mut command);
    command.env(SESSION_ENV, session);
    // Names the same fact `ctx.toml`'s own `agent` config key would, so a
    // nested `zirv ctx ...` call inside this session's own children defaults
    // to this session's own harness rather than re-resolving from scratch.
    command.env(super::adapters::AGENT_ENV, agent_name);
    command
}

pub fn resume_prompt(handoff: &Handoff) -> String {
    format!(
        "You are picking up work from a previous session that ran out of usable context. \
Continue from the handoff below. Re-read the listed files before changing them, and do not \
redo work marked as done.\n\n{}",
        handoff.to_markdown()
    )
}

/// Composes the system prompt and merges the operator's own command-line
/// prompt flag for a resumed session, as `PromptRole::Orchestrator`.
///
/// A resumed run is interactive: `run_with` `exec`s over itself into an agent
/// the operator sits in front of, exactly like `chat` and the bare `wrap`
/// verb, so it takes the same role those two do -- the harness-teaching
/// layer, the adapter's orchestrator layer, and `~/.zirv/system-prompt.md`
/// rather than `~/.zirv/system-prompt.worker.md`. This used to pass
/// `PromptRole::Worker`, which silently coached an operator's own interactive
/// session as a delegated worker and dropped their user layer.
///
/// Split out of `run_with` for the same reason `launch_command` is: `run_with`
/// `exec`s over itself on unix, so composition needs its own seam a test can
/// call without launching an agent.
#[allow(clippy::too_many_arguments)]
fn compose_prompt(
    adapter: &dyn adapters::AgentAdapter,
    home: Option<&Path>,
    repo: &Path,
    simple: bool,
    cfg: &super::config::PromptConfig,
    memory_entries: &[super::prompt::MemoryLine],
    memory_cap: usize,
    extra: &[String],
) -> (Vec<String>, Option<super::prompt::ComposedPrompt>) {
    let composed = super::prompt::compose(
        home,
        repo,
        simple,
        cfg,
        super::prompt::PromptRole::Orchestrator,
        memory_entries,
        memory_cap,
        &[],
    );
    super::prompt::merge_command_line_prompt(
        adapter,
        extra,
        composed,
        None,
        super::prompt::PromptRole::Orchestrator,
    )
}

pub fn run_with<W: Write>(
    args: &ResumeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // N1, before any work: `resume` hands the terminal to an interactive
    // agent (it `exec`s over itself on unix), so it is subject to the same
    // nesting guard `wrap` and `chat` are -- it had no guard at all.
    //
    // `--print-prompt` is exempt: it launches nothing, prints the composed
    // prompt and returns, so it is the read-only half of this verb (like
    // `zirv ctx status`) and refusing it from inside a session would cost
    // usability for no safety. Only the branch that actually takes the
    // terminal over is gated.
    if !args.print_prompt
        && let Some(refusal) = super::sessions::nesting_refusal("resume", env, args.allow_nested)
    {
        return Err(refusal.into());
    }

    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;

    let (path, handoff) = latest_for_repo(&state, repo)?.ok_or_else(|| {
        format!(
            "no handoff stored for {}; run `zirv ctx handoff --transcript <path>` first",
            repo.display()
        )
    })?;

    let prompt = resume_prompt(&handoff);
    if args.print_prompt {
        writeln!(w, "{prompt}")?;
        return Ok(0);
    }

    let adapter = adapters::select(args.agent.as_deref().or(cfg.agent.as_deref()), &[], &cfg)?;

    let memory_slug = super::state::repo_slug(repo);
    let memory_entries = super::memory::render_for_prompt(&state, repo, &memory_slug, &cfg);
    let (user_extra, composed) = compose_prompt(
        adapter.as_ref(),
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &cfg.prompt,
        &memory_entries,
        cfg.memory.core_max_bytes,
        &args.extra,
    );
    // M2: attribution is logged per session, not once per verb. A resumed run
    // is interactive, so the agent mints its own transcript id and this one is
    // zirv's; exporting it is what makes the two meet, because the hook inside
    // the session reports under it (the same env var the supervisors set).
    let session = SessionId::new_v4();
    // M7: the composed prompt goes through a private file rather than argv,
    // where `ps` would show it to every other user on the machine. The adapter
    // builds this launch itself, so the probe gets an empty argv.
    let prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        &[],
        composed.as_ref(),
        &state,
        session.as_str(),
    )?;
    super::prompt::log_injection(
        &state,
        "resume",
        session.as_str(),
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );
    let extra: Vec<String> = user_extra
        .into_iter()
        .chain(prompt_args.iter().cloned())
        .collect();

    let mut command = launch_command(
        adapter.as_ref(),
        &prompt,
        &extra,
        repo,
        session.as_str(),
        adapter.name(),
    );
    writeln!(w, "resuming from {}", path.display())?;
    w.flush()?;

    // Replace this process so the TUI owns the terminal directly: resume hands
    // over, it does not supervise.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        Err(format!("could not start {}: {err}", adapter.name()).into())
    }
    #[cfg(not(unix))]
    {
        // FINDING-1 fix: `resume` is the one launch seam that spawns directly
        // rather than through `supervise::spawn_tapped` or a pty
        // `CommandBuilder`, so it never passed through the cmd.exe-reparse
        // backstop. Apply it here too. FIX A already keeps the composed
        // system prompt off this argv (file form on Windows), so the only free
        // text still on it is the interactive positional handoff prompt, which
        // this guards fail-closed exactly like every other interactive seam.
        {
            let program = command.get_program().to_string_lossy().to_string();
            let args: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect();
            adapters::guard_cmd_shim_reparse(&program, &args)?;
        }
        let status = command.status()?;
        Ok(status.code().unwrap_or(1))
    }
}

pub fn run<W: Write>(args: &ResumeArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::state::{STATE_ENV, StateDir};

    fn handoff() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec!["Added the route".to_string()],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec![],
        }
    }

    #[test]
    fn the_prompt_frames_the_handoff_as_continuation_work() {
        let prompt = resume_prompt(&handoff());
        assert!(prompt.contains("Wire the payments webhook"));
        assert!(prompt.contains("Add a failing test"));
        assert!(prompt.contains("src/routes/webhook.rs"));
        assert!(
            prompt.to_lowercase().contains("previous session"),
            "say where this came from: {prompt}"
        );
        assert!(!prompt.contains('\u{2014}'));
    }

    #[test]
    fn print_prompt_shows_the_prompt_without_launching_anything() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff())
            .expect("store");

        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent".to_string(),
            ),
        ]
        .into();

        let args = ResumeArgs {
            agent: None,
            print_prompt: true,
            extra: Vec::new(),
            simple: false,
            allow_nested: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("Wire the payments webhook"), "got {text}");
    }

    /// M2: README promises injection attribution "at every session start", and
    /// a resume starts one. It must be attributed to a real session id that the
    /// agent's own hooks will report under, not to the literal verb name.
    ///
    /// Run as a subprocess because `run_with` hands the terminal over with
    /// `exec`, which would replace the test binary itself.
    #[cfg(unix)]
    #[test]
    fn a_resume_logs_injection_under_the_session_id_it_exports() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "old-session", &handoff())
            .expect("store");

        // cargo test builds the bin target, so it sits next to the test
        // binary's grandparent directory (target/debug/deps/<test>).
        let exe = std::env::current_exe().expect("current_exe");
        let zirv = exe
            .parent()
            .and_then(|p| p.parent())
            .expect("target dir")
            .join("zirv");
        // A stub agent that only records what session it was told it is: the
        // shared fake agent wants a --session-id, which an interactive launch
        // (this one) deliberately does not pass.
        let session_log = tmp.path().join("session-env.log");
        let stub = tmp.path().join("stub-agent.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{ZIRV_CTX_SESSION:-}}\" > {}\nexit 0\n",
                session_log.display()
            ),
        )
        .expect("write stub");

        let mut command = std::process::Command::new(&zirv);
        command
            .args(["ctx", "resume", "--agent", "claude"])
            .current_dir(tmp.path())
            .env("HOME", tmp.path().join("home"))
            .env("USERPROFILE", tmp.path().join("home"))
            .env(STATE_ENV, state.root())
            .env("ZIRV_CTX_AGENT_BIN", format!("sh {}", stub.display()));
        // NEW-4: hermetic against the developer's own environment. This
        // spawns the real `zirv` binary, which reads the process environment,
        // so a suite run from inside a supervised session would trip
        // `resume`'s own nesting guard and fail on the refusal instead of
        // testing attribution. The same scrub `wrap`'s pty harness does, for
        // the same reason -- and the stub `ZIRV_CTX_AGENT_BIN` above means
        // even a fully regressed guard can only ever reach the stub.
        for key in crate::commands::ctx::sessions::SUPERVISION_ENV {
            command.env_remove(key);
        }
        command.env_remove("CLAUDE_PID");
        command.env_remove("CLAUDECODE");
        let status = command.status().expect("resume runs");
        assert!(status.success(), "the launched agent exited cleanly");

        let exported = std::fs::read_to_string(&session_log)
            .expect("the launched agent recorded ZIRV_CTX_SESSION");
        let exported = exported.trim();
        assert!(!exported.is_empty(), "a session id must be exported");

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        let attribution = log
            .lines()
            .find(|l| l.contains("\"verb\":\"resume\"") && l.contains("\"action\":\"prompt-"))
            .unwrap_or_else(|| panic!("no attribution entry: {log}"));
        assert!(
            attribution.contains(&format!("\"session\":\"{exported}\"")),
            "attribution must name the session the agent was told to report as: {attribution}"
        );
    }

    // N1: `resume` hands the terminal to an interactive agent exactly like
    // `wrap`/`chat` do (it `exec`s over itself on unix), so it needs both
    // halves of the console fix -- it had neither.

    #[test]
    fn resume_refuses_inside_a_supervised_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff())
            .expect("store");

        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            // Safety belt, not a fixture detail: `adapters::select` calls
            // `ready()`, so an agent_bin that cannot exist makes a launch
            // structurally impossible. If the guard under test ever
            // regresses, this test fails on a missing binary instead of
            // spawning a real nested agent into the developer's session.
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        let args = ResumeArgs {
            agent: None,
            print_prompt: false,
            extra: Vec::new(),
            simple: false,
            allow_nested: false,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("a resume inside a supervised session must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to start inside an existing agent session"),
            "got {msg}"
        );
        assert!(msg.starts_with("zirv ctx resume:"), "names the verb: {msg}");
        assert!(msg.contains("abcdef12"), "names the outer session: {msg}");
        assert!(
            msg.contains("--allow-nested"),
            "says how to override: {msg}"
        );
        assert!(
            out.is_empty(),
            "it refuses before printing or launching anything"
        );
    }

    #[test]
    fn allow_nested_lets_a_resume_past_the_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                STATE_ENV.to_string(),
                tmp.path().join("state").display().to_string(),
            ),
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            // Safety belt, not a fixture detail: `adapters::select` calls
            // `ready()`, so an agent_bin that cannot exist makes a launch
            // structurally impossible. If the guard under test ever
            // regresses, this test fails on a missing binary instead of
            // spawning a real nested agent into the developer's session.
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        // No handoff stored, so getting past the guard surfaces as the
        // *next* refusal -- which is the evidence the guard was passed,
        // without this test ever launching an agent.
        let args = ResumeArgs {
            agent: None,
            print_prompt: true,
            extra: Vec::new(),
            simple: false,
            allow_nested: true,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to resume");
        assert!(!err.to_string().contains("refusing to start inside"));
        assert!(err.to_string().contains("no handoff"));
    }

    /// `--print-prompt` launches nothing -- it is the read-only half of the
    /// verb, like `zirv ctx status` -- so it stays usable from inside a
    /// session. Only the branch that actually hands the terminal over is
    /// gated.
    #[test]
    fn print_prompt_is_not_gated_by_the_nesting_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff())
            .expect("store");

        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            // Safety belt, not a fixture detail: `adapters::select` calls
            // `ready()`, so an agent_bin that cannot exist makes a launch
            // structurally impossible. If the guard under test ever
            // regresses, this test fails on a missing binary instead of
            // spawning a real nested agent into the developer's session.
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        let args = ResumeArgs {
            agent: None,
            print_prompt: true,
            extra: Vec::new(),
            simple: false,
            allow_nested: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("printing a prompt launches nothing, so it is not gated");
        assert_eq!(code, 0);
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("Wire the payments webhook")
        );
    }

    /// The launch `resume` `exec`s into must carry its own fresh session id
    /// and nothing else: it used to set `ZIRV_CTX_SESSION` but leave the
    /// outer session's `ZIRV_CTX_SOCKET`/`ZIRV_CTX_TRANSCRIPT` inherited, so
    /// the resumed agent's hooks reported turn boundaries onto the *outer*
    /// supervisor's socket.
    #[test]
    fn resume_never_leaks_the_outer_sessions_socket() {
        let adapter =
            crate::commands::ctx::adapters::claude::ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let command = launch_command(
            &adapter,
            "carry on",
            &[],
            Path::new("/tmp/repo"),
            "inner999-2222-4333-8444-555555555555",
            "claude",
        );

        let mut removed: Vec<&str> = Vec::new();
        let mut set: Vec<(&str, String)> = Vec::new();
        for (key, value) in command.get_envs() {
            let Some(key) = key.to_str() else { continue };
            match value {
                None => removed.push(key),
                Some(v) => set.push((key, v.to_string_lossy().to_string())),
            }
        }

        for key in [
            crate::commands::ctx::adapters::SOCKET_ENV,
            crate::commands::ctx::wrap::TRANSCRIPT_ENV,
        ] {
            assert!(
                removed.contains(&key),
                "{key} must be scrubbed, not inherited: removed={removed:?} set={set:?}"
            );
        }
        assert!(
            set.iter()
                .any(|(k, v)| *k == crate::commands::ctx::adapters::SESSION_ENV
                    && v == "inner999-2222-4333-8444-555555555555"),
            "its own fresh session id still reaches the child: {set:?}"
        );
    }

    /// A resumed session is interactive, so it must be composed on the
    /// Orchestrator side of the split: it reads the operator's own
    /// `system-prompt.md`, never `system-prompt.worker.md`, and gets claude's
    /// orchestrator layer rather than the worker one. Pins `compose_prompt`
    /// directly (the seam `run_with` itself cannot be called from a test,
    /// since it `exec`s over the process on unix) so a future regression back
    /// to `PromptRole::Worker` fails here instead of silently shipping.
    #[test]
    fn a_resumed_session_is_composed_for_the_orchestrator_role() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(
            home.join(".zirv/system-prompt.md"),
            "orchestrator user text",
        )
        .expect("write");
        std::fs::write(
            home.join(".zirv")
                .join(crate::commands::ctx::prompt::WORKER_PROMPT_FILE),
            "worker user text",
        )
        .expect("write");

        let adapter =
            crate::commands::ctx::adapters::claude::ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cfg = crate::commands::ctx::config::PromptConfig::default();
        let (_, composed) = compose_prompt(&adapter, Some(&home), &repo, false, &cfg, &[], 0, &[]);
        let composed = composed.expect("composed");

        assert!(
            composed.text.contains("orchestrator user text"),
            "reads the operator's own system-prompt.md: {}",
            composed.text
        );
        assert!(
            !composed.text.contains("worker user text"),
            "must never read the worker-only user layer file: {}",
            composed.text
        );
        assert!(
            composed.text.contains("zirv meta-harness"),
            "an interactive resumed session gets the harness-teaching layer: {}",
            composed.text
        );
        assert!(
            composed.text.contains("zirv orchestrator conventions"),
            "gets claude's orchestrator layer: {}",
            composed.text
        );
        assert!(
            !composed.text.contains("zirv worker conventions"),
            "a resumed session must never be coached as a delegated worker: {}",
            composed.text
        );
    }

    #[test]
    fn a_repo_with_no_handoff_reports_that_clearly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [(
            STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let args = ResumeArgs {
            agent: None,
            print_prompt: true,
            extra: Vec::new(),
            simple: false,
            allow_nested: false,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to resume");
        let msg = err.to_string();
        assert!(msg.contains("no handoff"), "got {msg}");
        assert!(msg.contains("zirv ctx handoff"), "point at the fix: {msg}");
    }
}
