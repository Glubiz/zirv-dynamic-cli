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
}

pub fn resume_prompt(handoff: &Handoff) -> String {
    format!(
        "You are picking up work from a previous session that ran out of usable context. \
Continue from the handoff below. Re-read the listed files before changing them, and do not \
redo work marked as done.\n\n{}",
        handoff.to_markdown()
    )
}

pub fn run_with<W: Write>(
    args: &ResumeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
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

    let composed = super::prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &cfg.prompt,
    );
    let (user_extra, composed) =
        super::prompt::merge_command_line_prompt(adapter.as_ref(), &args.extra, composed, None);
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
    );
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

    let mut command = adapter.interactive_cmd(Some(&prompt), &extra);
    command.current_dir(repo);
    command.env(SESSION_ENV, session.as_str());
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

        let status = std::process::Command::new(&zirv)
            .args(["ctx", "resume", "--agent", "claude"])
            .current_dir(tmp.path())
            .env("HOME", tmp.path().join("home"))
            .env(STATE_ENV, state.root())
            .env("ZIRV_CTX_AGENT_BIN", format!("sh {}", stub.display()))
            .status()
            .expect("resume runs");
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
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to resume");
        let msg = err.to_string();
        assert!(msg.contains("no handoff"), "got {msg}");
        assert!(msg.contains("zirv ctx handoff"), "point at the fix: {msg}");
    }
}
