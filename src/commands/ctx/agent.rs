//! `zirv ctx agent <name> <prompt> [-- flags]`: a one-shot delegation to a
//! supervised headless worker on another enabled harness. Builds the same
//! `ExecArgs` a hand-written `zirv ctx exec --agent <name> --prompt <text> --
//! ...` invocation would, and drives it through `exec::run_with` directly, so
//! a delegated run gets the identical pacing, rot detection and
//! restart-with-handoff behavior as running `zirv ctx exec` from the command
//! line. The prompt always travels as `ExecArgs::prompt` (data), never
//! encoded into the trailing `command` argv: a prompt shaped like a flag must
//! never be misread as one.
//!
//! Always a worker session (`exec::run_with` never takes a `PromptRole`; it
//! is hardcoded to `Worker`), which is what keeps a delegated run from being
//! taught to delegate further.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use super::CtxResult;
use super::announce::{Announcer, Event};
use super::chat::quiet_env;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::dash::spawnreq;
use super::event::SessionId;
use super::exec::{self, ExecArgs};

#[derive(Debug, clap::Args)]
pub struct AgentArgs {
    /// Adapter name to delegate to.
    pub name: String,
    /// The task prompt, or "-" to read it from stdin.
    pub prompt: String,
    /// Extra flags for the agent's own CLI, after `--`.
    //
    // `allow_hyphen_values`, because what gets passed through here is the
    // agent's own flags.
    #[arg(allow_hyphen_values = true, last = true)]
    pub flags: Vec<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// Suppress the `zirv ▸` announcement channel for this delegated run.
    /// Errors and warnings are never suppressed.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

/// Everything wrong with `flags` that can be seen without running anything.
/// Mirrors `AgentCommand::validate`'s rule for a script `agent:` step:
/// `flags` reach the agent's own CLI directly, so a leading bare word would
/// be read as the program rather than as a flag.
pub fn validate_flags(flags: &[String]) -> CtxResult<()> {
    if let Some(first) = flags.first()
        && !first.starts_with('-')
    {
        return Err(format!(
            "'flags' are passed to the agent's own CLI, so they must start with '-'; got '{first}'"
        )
        .into());
    }
    Ok(())
}

/// `"-"` reads the whole of `stdin` (trimmed); anything else is the prompt
/// text itself. `stdin` is a parameter rather than `std::io::stdin()` so this
/// stays testable without touching the real process stream.
pub fn resolve_prompt(raw: &str, stdin: &mut dyn Read) -> CtxResult<String> {
    if raw != "-" {
        return Ok(raw.to_string());
    }
    let mut buffer = String::new();
    stdin.read_to_string(&mut buffer)?;
    Ok(buffer.trim().to_string())
}

/// The supervisor's own two outcomes (rot-exhausted, timed out) get a
/// human-readable line; every other exit code -- including a plain success --
/// gets none, since that is either self-explanatory or the agent's own doing.
pub fn exit_note(code: i32) -> Option<String> {
    matches!(code, exec::EXIT_ROT_EXHAUSTED | exec::EXIT_TIMEOUT).then(|| exec::describe_exit(code))
}

/// How long a delegated run waits for the dashboard's own answer before
/// giving up and running headless instead. Generous enough for a live
/// dashboard's own event loop (50ms poll, plus a once-per-tick request
/// sweep) to notice the request and spawn a pane; short enough that an
/// operator who is not actually running a dashboard right now (a stale
/// `DASH_REQUESTS_ENV` inherited from a shell that used to be a pane, whose
/// directory has not yet been reaped) is not kept waiting for long.
const DASH_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// When this process is itself a dashboard pane's own child
/// (`spawnreq::DASH_REQUESTS_ENV` set, and the directory it names still
/// exists -- the dashboard deletes it on quit, see `dash::on_quit`), asks
/// the dashboard to spawn `name` as a fresh pane instead of running headless
/// in this process's own subshell: writes a `spawnreq::SpawnRequest`
/// carrying `prompt` as data (never argv, the same discipline every other
/// delegation path in this codebase already holds), then waits up to
/// `DASH_ACK_TIMEOUT` for the matching ack.
///
/// `Some(result)` means the dashboard answered (or the request itself could
/// not even be written) and the caller's own headless path must NOT run --
/// a pane was spawned there instead, or the request was refused outright.
/// `None` means either there is no dashboard to ask (env unset, or the
/// directory is gone -- both **byte-for-byte** today's behavior, no notice
/// printed) or a live one did not answer in time (a notice IS printed, since
/// that case is a live channel that just did not respond), and either way
/// the caller falls through to today's headless behavior unchanged.
fn try_join_dashboard<W: Write>(
    name: &str,
    prompt: &str,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> Option<CtxResult<i32>> {
    let dir = env(spawnreq::DASH_REQUESTS_ENV).map(std::path::PathBuf::from)?;
    if !dir.is_dir() {
        return None;
    }
    let requested_by = env(super::adapters::SESSION_ENV)
        .map(|s| super::sessions::short_id(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let req = spawnreq::SpawnRequest {
        agent: name.to_string(),
        prompt: prompt.to_string(),
        cwd: repo.to_path_buf(),
        requested_by,
    };
    let path = match spawnreq::write_request(&dir, &req) {
        Ok(path) => path,
        Err(_) => {
            eprintln!("zirv ctx agent: dashboard did not answer; running headless");
            return None;
        }
    };
    let Some(stem) = spawnreq::request_stem(&path) else {
        eprintln!("zirv ctx agent: dashboard did not answer; running headless");
        return None;
    };
    match spawnreq::wait_for_ack(&dir, &stem, DASH_ACK_TIMEOUT) {
        Some(ack) if ack.ok => {
            let short = ack.short.unwrap_or_default();
            Some(
                writeln!(w, "spawned in dashboard as {short}")
                    .map(|_| 0)
                    .map_err(|e| e.into()),
            )
        }
        Some(ack) => {
            let reason = ack
                .reason
                .unwrap_or_else(|| "the dashboard refused this request".to_string());
            Some(writeln!(w, "{reason}").map(|_| 1).map_err(|e| e.into()))
        }
        None => {
            eprintln!("zirv ctx agent: dashboard did not answer; running headless");
            None
        }
    }
}

pub fn run_with<W: Write>(
    args: &AgentArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    validate_flags(&args.flags)?;
    let prompt = resolve_prompt(&args.prompt, &mut std::io::stdin())?;

    if let Some(result) = try_join_dashboard(&args.name, &prompt, w, repo, env) {
        return result;
    }

    // A second, independent config load just for the announcer: `exec::
    // run_with` below loads its own copy internally (the same pattern
    // `chat.rs` already uses ahead of `wrap::run_with`), so this costs one
    // extra read of the same layered config rather than a new code path.
    let cfg = CtxConfig::load(repo, env)?;
    let announcer = Announcer::new(
        cfg.chrome.events && !args.quiet,
        console::colors_enabled_stderr(),
    );
    let env = quiet_env(env, args.quiet);

    let exec_args = ExecArgs {
        agent: Some(args.name.clone()),
        session_id: Some(SessionId::new_v4().to_string()),
        transcript: None,
        // Data, never argv: `run_with` builds the launch from the adapter
        // itself when the trailing command carries no program name, exactly
        // as every restart already does. Encoding the prompt into `command`
        // here only to have `exec::run_with` parse it back out is what would
        // let a prompt shaped like a flag be misread as one.
        prompt: Some(prompt),
        max_restarts: args.max_restarts,
        timeout_secs: args.timeout_secs,
        command: args.flags.clone(),
        simple: false,
    };

    announcer.emit(&Event::DelegatedStart {
        agent: args.name.clone(),
    });
    let code = exec::run_with(&exec_args, w, repo, &env)?;
    announcer.emit(&Event::DelegatedFinish {
        agent: args.name.clone(),
        meaning: exec::describe_exit(code),
    });
    if let Some(note) = exit_note(code) {
        eprintln!("zirv ctx agent: {note}");
    }
    Ok(code)
}

pub fn run<W: Write>(args: &AgentArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn base_env(state: &Path) -> HashMap<String, String> {
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

    fn args_for(name: &str, prompt: &str) -> AgentArgs {
        AgentArgs {
            name: name.to_string(),
            prompt: prompt.to_string(),
            flags: Vec::new(),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            quiet: false,
        }
    }

    #[test]
    fn flags_after_the_separator_reach_the_agent_and_must_start_with_a_hyphen() {
        assert!(validate_flags(&["--model".to_string(), "opus".to_string()]).is_ok());
        assert!(validate_flags(&[]).is_ok());

        let err = validate_flags(&["opus".to_string()]).expect_err("bare word is not a flag");
        let msg = err.to_string();
        assert!(msg.contains("opus"), "got {msg}");
        assert!(msg.contains('-'), "must say flags need a hyphen: {msg}");
    }

    #[test]
    fn a_prompt_of_a_single_dash_is_read_from_stdin() {
        let mut stdin = std::io::Cursor::new(b"fix the failing tests\n".to_vec());
        let prompt = resolve_prompt("-", &mut stdin).expect("reads stdin");
        assert_eq!(prompt, "fix the failing tests");
    }

    #[test]
    fn an_ordinary_prompt_never_touches_stdin() {
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let prompt = resolve_prompt("fix the bug", &mut stdin).expect("resolves");
        assert_eq!(prompt, "fix the bug");
    }

    #[test]
    fn exit_note_names_the_supervisors_own_outcomes_and_nothing_else() {
        assert!(
            exit_note(exec::EXIT_ROT_EXHAUSTED)
                .expect("rot exhausted has a note")
                .contains("restart budget")
        );
        assert!(
            exit_note(exec::EXIT_TIMEOUT)
                .expect("timeout has a note")
                .contains("wall-clock timeout")
        );
        assert_eq!(exit_note(0), None, "success needs no explanation");
        assert_eq!(exit_note(3), None, "an ordinary agent failure is its own");
    }

    #[test]
    fn the_delegation_verb_refuses_an_agent_the_settings_file_disabled() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");

        let env = base_env(&tmp.path().join("state"));
        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("claude is disabled");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    #[test]
    fn the_delegation_verb_refuses_an_agent_that_is_not_ready_yet() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let env = base_env(&tmp.path().join("state"));
        let args = args_for("codex", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex is not implemented yet");
        assert!(err.to_string().contains("not implemented yet"));
    }

    #[test]
    fn the_prompt_travels_as_data_and_is_never_encoded_into_argv() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let argv_log = tmp.path().join("argv.log");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }

        let mut args = args_for("claude", "--looks-like-a-flag but is not");
        args.flags = vec!["--model".to_string(), "opus".to_string()];
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("-p --looks-like-a-flag but is not"),
            "the prompt must land as the -p value verbatim, not be parsed as flags: {argv}"
        );
        assert!(
            argv.contains("--model opus"),
            "the operator's own flags must still reach the agent: {argv}"
        );
    }

    #[test]
    fn a_rot_exhausted_run_keeps_its_exit_code_and_explains_it_in_words() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }

        let mut args = args_for("claude", "do the work");
        args.max_restarts = Some(0);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            exec::EXIT_ROT_EXHAUSTED,
            "the caller applies its own policy after the budget is spent"
        );
        assert!(
            exit_note(exec::EXIT_ROT_EXHAUSTED)
                .expect("a note exists")
                .contains("restart budget")
        );
    }

    /// A delegated run is a worker session, not an orchestrator one: it must
    /// carry zirv's own shipped default layer (proving injection happened at
    /// all) but never the harness meta-teaching layer, which only an
    /// orchestrator session gets. `exec::run_with` has no `PromptRole`
    /// parameter to get wrong -- it is hardcoded to `Worker` -- so this pins
    /// the observable behavior that fact is supposed to guarantee.
    #[test]
    fn a_delegated_run_is_a_worker_session_not_an_orchestrator_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let argv_log = tmp.path().join("argv.log");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }

        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("zirv session conventions"),
            "the shipped default layer proves injection happened: {argv}"
        );
        assert!(
            !argv.contains("zirv meta-harness"),
            "a worker session must never get the harness delegation layer: {argv}"
        );
    }

    /// `--quiet` folds into `ZIRV_CTX_QUIET` for the delegated `exec::
    /// run_with` call the same way it does for `chat`; this pins that the
    /// flag does not otherwise disturb a delegated run (it still launches,
    /// still succeeds, still exits 0).
    #[test]
    fn quiet_still_lets_the_delegated_run_complete_normally() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let mut args = args_for("claude", "do the work");
        args.quiet = true;
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(
            code.expect("runs"),
            0,
            "--quiet must not change the outcome"
        );
    }

    // Task 11: joining a running dashboard instead of spawning headless.

    /// Polls `dir` for a `req-*.json` file, writes `ack_body` as its matching
    /// ack, and returns the request's own raw contents -- the same
    /// "responder" shape every dashboard-join test below needs, since
    /// `write_request` mints a random uuid filename the test cannot know in
    /// advance. Never touches a real agent: this only ever races against
    /// `try_join_dashboard`'s own polling loop, both confined to a tempdir.
    fn respond_to_next_request(dir: std::path::PathBuf, ack_body: &'static str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                    if name.starts_with("req-") && name.ends_with(".json") {
                        let contents = std::fs::read_to_string(&path).expect("read request");
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .expect("file stem")
                            .to_string();
                        std::fs::write(dir.join(format!("ack-{stem}.json")), ack_body)
                            .expect("write ack");
                        return contents;
                    }
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("no spawn request appeared within the deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A live dashboard (env set, directory present) that acks `ok: true`
    /// short-circuits the headless path entirely: `run_with` reports the
    /// pane's own short id and returns `Ok(0)`, and the request it wrote
    /// carries the prompt as data, not argv.
    #[test]
    fn dashboard_join_spawns_a_pane_and_reports_its_short_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let requests_dir = tmp.path().join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            requests_dir.display().to_string(),
        );

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let args = args_for("claude", "a specific delegated task");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as abcd1234"),
            "got {output}"
        );
        assert!(
            request_body.contains("a specific delegated task"),
            "the prompt must travel as data in the request file: {request_body}"
        );
    }

    /// A refusal ack (the dashboard's own `cfg.agents.refusal` gate, or an
    /// unknown/unready adapter) prints the reason and returns `Ok(1)` --
    /// still short-circuiting the headless path, since the dashboard already
    /// gave a definitive answer.
    #[test]
    fn dashboard_join_prints_the_refusal_reason_and_returns_exit_1() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let requests_dir = tmp.path().join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            requests_dir.display().to_string(),
        );

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"claude is disabled by .zirv/.settings.toml"}"#,
                )
            }
        });

        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        responder.join().expect("responder thread");

        assert_eq!(code, 1);
        let output = String::from_utf8_lossy(&out);
        assert!(output.contains("disabled"), "got {output}");
    }

    /// `DASH_REQUESTS_ENV` set but naming a directory that does not exist --
    /// the dashboard already quit and reaped it, or the value is stale --
    /// must fall straight through to the existing headless path with no
    /// notice printed (byte-for-byte the pre-Task-11 behavior for this
    /// shape), the same fake-agent-bin pattern every other "reached the real
    /// spawn attempt" test in this codebase uses to prove it without ever
    /// launching a real agent.
    #[test]
    fn dashboard_join_falls_through_to_headless_when_the_directory_is_missing() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let mut env: HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                tmp.path().join("state").display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
        ]
        .into();
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            tmp.path()
                .join("no-such-requests-dir")
                .display()
                .to_string(),
        );

        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("the configured binary does not exist, so the headless spawn must fail");
        // Unlike `wrap`'s own pty-based spawn (which names the configured
        // binary in its error text -- see `chat.rs`'s equivalent test), the
        // headless path's plain `std::process::Command::spawn` failure is a
        // bare OS error with no program name in it at all. So the proof here
        // is what did NOT happen: `try_join_dashboard` short-circuits with
        // `Ok(_)` and a message written to `out` on every path it actually
        // takes (spawned, or refused) -- an `Err` with nothing ever written
        // to `out` means neither happened, i.e. this genuinely fell through
        // past the (missing) dashboard directory and into the real headless
        // spawn attempt, which is what failed.
        assert!(
            out.is_empty(),
            "a dashboard short-circuit always writes a line to `out`; nothing was written here"
        );
        let msg = err.to_string();
        assert!(!msg.is_empty(), "got an error with no message at all");
    }
}
