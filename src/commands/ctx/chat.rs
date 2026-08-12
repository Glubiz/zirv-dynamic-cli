//! `zirv ctx chat`: an interactive session launched through the same `wrap`
//! supervision every other interactive verb uses, but the launch is built
//! from the resolved adapter rather than from a user-supplied argv (there is
//! nothing on the command line for `wrap`'s own detection to guess at), and
//! the session is flagged `PromptRole::Orchestrator` rather than `Worker`:
//! this is the session a human is talking to directly, so it is the one
//! allowed to hear about delegating to other harnesses (`zirv ctx send`,
//! `zirv ctx inbox`, `zirv ctx agent`).

use std::io::Write;
use std::path::Path;

use super::adapters::{self, AgentAdapter};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::prompt::PromptRole;
use super::state::StateDir;
use super::wrap::{self, WrapArgs};
use super::{CtxResult, handoff, resume};

#[derive(Debug, clap::Args)]
pub struct ChatArgs {
    /// Adapter name: claude or codex. Falls back to the configured default,
    /// then to the registry's own fallback rule.
    #[arg(long)]
    pub agent: Option<String>,
    /// Fold the latest stored handoff into the first prompt.
    #[arg(long, default_value_t = false)]
    pub resume: bool,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// Extra arguments passed through to the agent, after `--`.
    //
    // `allow_hyphen_values`, because what gets passed through here is the
    // agent's own flags.
    #[arg(allow_hyphen_values = true, last = true)]
    pub extra: Vec<String>,
}

/// What `run_with` hands to `wrap::run_with`: the resolved agent's own name
/// (so wrap never has to guess), the argv `interactive_cmd` built, and the
/// role every chat session carries.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatLaunch {
    pub agent_name: String,
    pub argv: Vec<String>,
    pub role: PromptRole,
}

/// Builds the launch from the adapter rather than from any user-supplied
/// argv: a chat session names no wrapped command on the command line, so
/// there is nothing for `wrap`'s own detection to guess at. Always
/// `PromptRole::Orchestrator`: a `chat` session is the one a human is
/// talking to directly, so it is the one allowed to hear about delegating to
/// other harnesses.
///
/// A pure function of the adapter and the two pieces of caller-supplied
/// state (the initial prompt, the extra flags), so it is testable without
/// spawning a pty.
pub fn build_launch(
    adapter: &dyn AgentAdapter,
    initial_prompt: Option<&str>,
    extra: &[String],
) -> ChatLaunch {
    let command = adapter.interactive_cmd(initial_prompt, extra);
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    ChatLaunch {
        agent_name: adapter.name().to_string(),
        argv,
        role: PromptRole::Orchestrator,
    }
}

/// The explicit `--agent`, else the configured default, else the registry's
/// own fallback rule -- whose aggregated error (naming every disabled or
/// unready candidate, and why) is the message a chat session with nothing
/// available shows. Refusing here, before `wrap` is ever reached, is what
/// keeps an explicitly named disabled agent from touching the terminal at
/// all: `wrap::run_with` performs the identical `adapters::select` check of
/// its own before opening a pty, so the same refusal holds even if this
/// function's own check were ever bypassed.
fn resolve_adapter(cfg: &CtxConfig, requested: Option<&str>) -> CtxResult<Box<dyn AgentAdapter>> {
    match requested.or(cfg.agent.as_deref()) {
        Some(name) => adapters::select(Some(name), &[], cfg),
        None => adapters::resolve_default(cfg).map(|(adapter, _origin)| adapter),
    }
}

/// `--resume`'s initial prompt: the latest stored handoff, folded in the same
/// words `zirv ctx resume` uses. When nothing is stored, prints a note and
/// starts fresh rather than failing the session outright: `--resume` is a
/// request to continue if there is something to continue from, not a
/// precondition for starting at all.
pub fn resolve_initial_prompt<W: Write>(
    resume_requested: bool,
    state: &StateDir,
    repo: &Path,
    w: &mut W,
) -> CtxResult<Option<String>> {
    if !resume_requested {
        return Ok(None);
    }
    match handoff::latest_for_repo(state, repo)? {
        Some((_path, found)) => Ok(Some(resume::resume_prompt(&found))),
        None => {
            writeln!(
                w,
                "zirv ctx chat: --resume requested but no handoff is stored for this repo; \
                 starting a fresh session"
            )?;
            Ok(None)
        }
    }
}

pub fn run_with<W: Write>(
    args: &ChatArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = resolve_adapter(&cfg, args.agent.as_deref())?;
    let state = StateDir::resolve(env)?;
    let initial_prompt = resolve_initial_prompt(args.resume, &state, repo, w)?;
    let launch = build_launch(adapter.as_ref(), initial_prompt.as_deref(), &args.extra);

    let wrap_args = WrapArgs {
        agent: Some(launch.agent_name),
        no_supervise: false,
        command: launch.argv,
        simple: args.simple,
    };
    wrap::run_with(&wrap_args, w, repo, env, launch.role)
}

pub fn run<W: Write>(args: &ChatArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::state::StateDir;

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
    fn chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let expected = adapter.interactive_cmd(Some("hello"), &[]);
        let mut expected_argv = vec![expected.get_program().to_string_lossy().to_string()];
        expected_argv.extend(expected.get_args().map(|a| a.to_string_lossy().to_string()));

        let launch = build_launch(&adapter, Some("hello"), &[]);

        assert_eq!(
            launch.argv, expected_argv,
            "the launch argv must come straight from the adapter's own interactive_cmd"
        );
    }

    #[test]
    fn chat_passes_the_resolved_agent_explicitly_so_wrap_never_has_to_guess() {
        let adapter = ClaudeAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "claude");
    }

    #[test]
    fn extra_flags_after_the_separator_reach_the_agent_untouched() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let extra = vec!["--model".to_string(), "opus".to_string()];
        let launch = build_launch(&adapter, None, &extra);
        assert_eq!(
            &launch.argv[launch.argv.len() - 2..],
            &extra[..],
            "extra flags must survive to the end of argv untouched: {:?}",
            launch.argv
        );
    }

    #[test]
    fn chat_is_an_orchestrator_session() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            build_launch(&adapter, None, &[]).role,
            PromptRole::Orchestrator,
            "a chat session is the one a human talks to directly"
        );
        // Resuming or adding extra flags must not change that.
        assert_eq!(
            build_launch(&adapter, Some("resume this"), &["--model".to_string()]).role,
            PromptRole::Orchestrator
        );
    }

    #[test]
    fn resume_folds_the_latest_handoff_into_the_first_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff())
            .expect("store");

        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(true, &state, tmp.path(), &mut out)
            .expect("resolves")
            .expect("a handoff was stored");

        assert!(prompt.contains("Wire the payments webhook"), "got {prompt}");
        assert_eq!(
            prompt,
            resume::resume_prompt(&handoff()),
            "chat must fold the handoff the same way `zirv ctx resume` does"
        );
        assert!(
            out.is_empty(),
            "no note needed when a handoff was actually found"
        );
    }

    #[test]
    fn resume_without_a_stored_handoff_starts_a_fresh_session_and_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));

        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(true, &state, tmp.path(), &mut out).expect("resolves");

        assert_eq!(prompt, None, "nothing to fold in, so a fresh session");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("--resume") && printed.to_lowercase().contains("fresh"),
            "must say why it started fresh: {printed}"
        );
    }

    #[test]
    fn no_resume_requested_never_touches_the_handoff_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(false, &state, tmp.path(), &mut out).expect("resolves");
        assert_eq!(prompt, None);
        assert!(out.is_empty());
    }

    /// The registry's own aggregated error (naming every candidate and why it
    /// was skipped) is the message shown when nothing is both enabled and
    /// ready -- the same one `adapters::resolve_default` produces on its own.
    #[test]
    fn chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_reason() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, repo.path(), &|k| empty.get(k).cloned())
            .expect_err("nothing is both enabled and ready");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(msg.contains("disabled"), "must say why: {msg}");
    }

    /// The gate is checked, and refuses, before any terminal or pty work: this
    /// runs synchronously to a returned `Err` with no pty ever opened. `wrap`
    /// performs the identical check on its own before touching a terminal, so
    /// the refusal holds even by that second, independent path.
    #[test]
    fn an_explicitly_named_disabled_agent_is_refused_before_the_terminal_is_touched() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: Some("claude".to_string()),
            resume: false,
            simple: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, repo.path(), &|k| empty.get(k).cloned())
            .expect_err("claude is disabled");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    #[test]
    fn codex_is_a_valid_launch_target_even_though_it_is_not_ready() {
        // Sanity: build_launch itself does not care about readiness, only
        // resolve_adapter (exercised above) does.
        let adapter = CodexAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "codex");
        assert_eq!(launch.role, PromptRole::Orchestrator);
    }
}
