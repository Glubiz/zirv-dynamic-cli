use std::io::Write;
use std::path::Path;

use super::config::{CtxConfig, EnvLookup, env_from_process};
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

    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;

    let composed = super::prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &cfg.prompt,
    );
    let prompt_args = super::prompt::injection_args(adapter.as_ref(), composed.as_ref());
    super::prompt::log_injection(
        &state,
        "resume",
        "resume",
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );
    let extra: Vec<String> = args
        .extra
        .iter()
        .cloned()
        .chain(prompt_args.iter().cloned())
        .collect();

    let mut command = adapter.interactive_cmd(Some(&prompt), &extra);
    command.current_dir(repo);
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
