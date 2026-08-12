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

use super::CtxResult;
use super::announce::{Announcer, Event};
use super::chat::quiet_env;
use super::config::{CtxConfig, EnvLookup, env_from_process};
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

pub fn run_with<W: Write>(
    args: &AgentArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    validate_flags(&args.flags)?;
    let prompt = resolve_prompt(&args.prompt, &mut std::io::stdin())?;

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
}
