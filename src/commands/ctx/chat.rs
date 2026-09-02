//! `zirv ctx chat`: an interactive session launched through the same `wrap`
//! supervision every other interactive verb uses, but the launch is built
//! from the resolved adapter rather than from a user-supplied argv (there is
//! nothing on the command line for `wrap`'s own detection to guess at), and
//! the session is flagged `PromptRole::Orchestrator` rather than `Worker`:
//! this is the session a human is talking to directly, so it is the one
//! allowed to hear about delegating to other harnesses (`zirv ctx send`,
//! `zirv ctx inbox`, `zirv ctx agent`).

use std::io::{IsTerminal, Write};
use std::path::Path;

use super::adapters::{self, AgentAdapter, DefaultOrigin};
use super::chrome::{self, BannerFacts, ChromeCaps, HarnessRule};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::dash;
use super::dash::pane::PaneSpec;
use super::event::SessionId;
use super::prompt::PromptRole;
use super::state::StateDir;
use super::term;
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
    /// Suppress the `zirv ▸` announcement channel. Errors and warnings are
    /// never suppressed; the launch banner and status bar have their own
    /// `[chrome]` toggles.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// Start even though this process looks like it is already inside an
    /// agent session. Off by default: a nested interactive supervisor can
    /// take the outer session down.
    #[arg(long, default_value_t = false)]
    pub allow_nested: bool,
    /// T10: see `WrapArgs::force_pace` -- threaded straight through, since a
    /// `chat` launch becomes a `wrap` launch (`wrap_args_for`).
    #[arg(long, default_value_t = false)]
    pub force_pace: bool,
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
    /// Always `Verb::Chat`: a chat session's registry record must say so
    /// rather than falling back to `wrap`'s own default, the same way
    /// `role` is always `Orchestrator` regardless of resuming or extra
    /// flags.
    pub verb: super::sessions::Verb,
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
        verb: super::sessions::Verb::Chat,
    }
}

/// When `adapter` has no verified system-prompt injection mechanism for the
/// launch shape it is about to use, folds the composed session context onto
/// the positional initial-prompt slot as a fallback -- the same task-prompt-
/// text channel `exec.rs`/`run_loop.rs`/`dash::compose_worker_prompt` already
/// use for exactly this adapter shape (see `prompt::task_prompt_with_
/// composed_fallback`'s own doc comment for why the task prompt is the one
/// channel such an adapter has). A no-op (returns `initial_prompt`
/// unchanged) whenever the adapter *is* supported, so a claude launch --
/// which always reports supported, see `ClaudeAdapter`'s lack of a `system_
/// prompt_supported` override -- is byte-for-byte unaffected.
///
/// `adapter.system_prompt_supported(&[])` (an empty launch) is deliberately
/// probed here rather than against the eventual full launch argv: this runs
/// before `build_launch` exists, and `CodexAdapter::system_prompt_supported`'s
/// own contract is to probe `self.interactive_cmd(None, &[])` when handed an
/// empty launch, answering the same question (does *this adapter's own
/// program* resolve to a reparsing shell shim) without needing the real argv
/// in hand -- `compose_worker_prompt` makes the identical `adapter.system_
/// prompt_supported(&[])` call for the same reason, a launch that also does
/// not exist yet at that point.
///
/// `simple`/`cfg.prompt.enabled` are not checked directly here: `compile::
/// compile` already returns `composed: None` for either (mirroring `prompt::
/// compose`'s own gate), and `task_prompt_with_composed_fallback` is a no-op
/// when handed `None`, so both degrade to returning `initial_prompt`
/// unchanged -- the correct answer either way.
fn orchestrator_initial_prompt(
    adapter: &dyn AgentAdapter,
    initial_prompt: Option<String>,
    cfg: &CtxConfig,
    home: Option<&Path>,
    repo: &Path,
    simple: bool,
    state: &StateDir,
) -> Option<String> {
    if adapter.system_prompt_supported(&[]) {
        return initial_prompt;
    }
    let compiled = super::compile::compile(
        home,
        repo,
        simple,
        cfg,
        adapter,
        PromptRole::Orchestrator,
        state,
        super::state::now_secs(),
        super::adapters::LaunchMode::Interactive,
        false,
    );
    let base = initial_prompt.unwrap_or_default();
    let text =
        super::prompt::task_prompt_with_composed_fallback(&base, false, compiled.composed.as_ref());
    if text.is_empty() { None } else { Some(text) }
}

/// The explicit `--agent`, else the configured default, else the registry's
/// own fallback rule -- whose aggregated error (naming every disabled or
/// unready candidate, and why) is the message a chat session with nothing
/// available shows. Refusing here, before `wrap` is ever reached, is what
/// keeps an explicitly named disabled agent from touching the terminal at
/// all: `wrap::run_with` performs the identical `adapters::select` check of
/// its own before opening a pty, so the same refusal holds even if this
/// function's own check were ever bypassed.
///
/// Also returns the `HarnessRule` that picked the adapter, for the launch
/// banner: an explicit `--agent` never reaches `resolve_default`, so that
/// rule cannot come from `DefaultOrigin` alone.
fn resolve_adapter(
    cfg: &CtxConfig,
    requested: Option<&str>,
) -> CtxResult<(Box<dyn AgentAdapter>, HarnessRule)> {
    if requested.is_some() {
        let adapter = adapters::select(requested, &[], cfg)?;
        return Ok((adapter, HarnessRule::Explicit));
    }
    match cfg.agent.as_deref() {
        Some(name) => Ok((
            adapters::select(Some(name), &[], cfg)?,
            HarnessRule::Configured,
        )),
        None => adapters::resolve_default(cfg).map(|(adapter, origin)| {
            let rule = match origin {
                DefaultOrigin::Configured => HarnessRule::Configured,
                DefaultOrigin::FirstEnabledReady => HarnessRule::FirstEnabledReady,
            };
            (adapter, rule)
        }),
    }
}

/// Every known harness in registry order, alongside whether the gate
/// currently enables it -- the banner's own harness list.
fn harness_list(cfg: &CtxConfig) -> Vec<(String, bool)> {
    adapters::ADAPTERS
        .iter()
        .map(|(name, _)| ((*name).to_string(), cfg.agents.is_enabled(name)))
        .collect()
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

/// Probes stdout for the launch banner: whether it is a terminal at all, its
/// current size, and whether VT output could be enabled. The returned guard
/// must outlive the whole session -- it is what keeps VT processing on for
/// `wrap`'s own raw-mode session that follows -- so callers hold it rather
/// than letting it drop immediately.
///
/// `stdout_is_tty` comes from `IsTerminal` on stdout specifically, not from
/// whether `window_size` succeeded: on unix that call reads `STDIN_FD`'s own
/// terminal-ness, so `zirv chat > log` -- stdout redirected, stdin still a
/// real terminal -- used to still print the banner straight into the log
/// file. The size itself still has to come from `window_size`: it is the
/// only source for it either way.
///
/// `stdin_is_tty` is a second, independent probe (not derived from
/// `window_size` succeeding): `ChromeCaps::probe` never needed it -- the
/// banner and status bar only ever write to stdout -- but `dash_eligible`
/// does, since the dashboard reads keystrokes from stdin to drive pane
/// selection and overlays, and a piped stdin can never make a usable session
/// even when stdout happens to be a terminal.
fn probe_terminal() -> (bool, bool, bool, (u16, u16), Option<term::VtGuard>) {
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let size = term::window_size(term::STDIN_FD).unwrap_or((0, 0));
    let vt_guard = term::enable_vt_output().ok();
    let vt_ok = vt_guard.is_some();
    (stdout_is_tty, stdin_is_tty, vt_ok, size, vt_guard)
}

/// `stderr` is a second, explicit writer -- not `std::io::stderr()` reached
/// for directly -- so the one diagnostic this function ever prints on its
/// own (the no-adapter/config error below) stays testable the same way
/// every message on `w` already is, without resorting to capturing the real
/// process stream.
pub fn run_with<W: Write, E: Write>(
    args: &ChatArgs,
    w: &mut W,
    stderr: &mut E,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // F2, first of all: before any config load, adapter resolution, terminal
    // probe or VT mode change. A `chat` started inside an existing agent
    // session can take that outer session down (see
    // `sessions::nested_session_evidence`), so it must refuse without having
    // touched the shared console at all.
    //
    // Printed on `stderr` and reported as exit code 1 rather than returned
    // as an `Err`, matching every other refusal in this function: a returned
    // `Err` would be printed a second time, unstyled, by `ctx`'s own
    // dispatch. `wrap::run_with` re-checks this independently (it has no
    // writer of its own, so it returns the `Err` there), which is what keeps
    // the guard holding even if this path were bypassed -- and why
    // `allow_nested` has to be threaded into `WrapArgs` below.
    if let Some(refusal) = super::sessions::nesting_refusal("chat", env, args.allow_nested) {
        writeln!(stderr, "{refusal}")?;
        return Ok(1);
    }

    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Held for the rest of this function: dropping it early would restore
    // the console's original VT mode before `wrap`'s own raw-mode session
    // (which relies on VT already being on) even opens.
    let (stdout_is_tty, stdin_is_tty, vt_ok, size, _vt_guard) = probe_terminal();
    let chrome = ChromeCaps::probe(stdout_is_tty, vt_ok, size, &cfg.chrome, args.simple, false);

    let (adapter, rule) = match resolve_adapter(&cfg, args.agent.as_deref()) {
        Ok(found) => found,
        Err(err) => {
            // Printed once, here, rather than propagated as `Err`: `zirv
            // ctx`'s own top-level dispatch prints any returned `Err` a
            // second time, unstyled, through `output::error`. Styling only
            // when `chrome.colour` (not gating whether this prints at all
            // on `chrome.banner`, an old bug -- a piped or redirected run
            // still needs to see why it refused to start) and returning
            // `Ok(1)` instead is what keeps this to one printed copy;
            // main.rs's own early-exit branches use the same shape,
            // printing their own message and choosing the exit code
            // directly rather than bubbling an error up to be printed
            // again.
            //
            // On `stderr`, not `w`: `w` is stdout, and `zirv chat > log`
            // must still show the operator *something* on the terminal
            // when it refuses to start, exactly like `output::error`
            // elsewhere in this codebase -- an error silently landing only
            // in a redirected stdout file is indistinguishable from a
            // session that hung or was killed.
            writeln!(
                stderr,
                "{}",
                chrome::style_no_adapter_error(&err.to_string(), chrome.colour)
            )?;
            return Ok(1);
        }
    };
    let state = StateDir::resolve(env)?;
    let initial_prompt = resolve_initial_prompt(args.resume, &state, repo, w)?;
    let resuming = args.resume && initial_prompt.is_some();
    let session = SessionId::new_v4();

    // Bug B (harness parity): an adapter with no verified system-prompt
    // injection mechanism for this launch shape (codex's own `system_prompt_
    // supported` narrows to `false` on a Windows shell-shim launch) never
    // reaches `injection_args_for_session`'s output at all. `dash_
    // orchestrator_pane` and `wrap::run_with`'s own fallback below both
    // correctly skip that call for such an adapter, but neither has anywhere
    // left to deliver the composed context, because the positional
    // initial-prompt slot `build_launch` bakes into `launch.argv` is already
    // fixed by the time either of them runs. Every *other* Zirv launch path
    // (`exec`, `loop`, and the dashboard's own worker panes via `dash::
    // compose_worker_prompt`) already folds the composed context onto its
    // task-prompt text as a fallback for exactly this adapter shape; this
    // Orchestrator launch was the one path that never got the same
    // treatment, so a codex orchestrator (a standalone `wrap` fallback, or
    // the dashboard's own orchestrator pane) started with no zirv context at
    // all -- not even the shipped default layer -- while a claude
    // orchestrator always gets one. Folded in here, once, before `build_
    // launch` bakes the positional prompt slot: both branches below reuse
    // this same `launch`.
    let initial_prompt = orchestrator_initial_prompt(
        adapter.as_ref(),
        initial_prompt,
        &cfg,
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &state,
    );

    // Applies to both branches below (the dashboard's orchestrator pane and
    // the wrap fallback): `chat.model` shapes `zirv chat` generally, not only
    // the dashboard, so the model flags are folded into the launch's own
    // extra arguments once, here, before either path reads `launch.argv`.
    let extra = extra_with_model(&cfg, adapter.as_ref(), &args.extra);
    let launch = build_launch(adapter.as_ref(), initial_prompt.as_deref(), &extra);

    if chrome.banner {
        let facts = BannerFacts {
            harness: adapter.name().to_string(),
            rule,
            session: session.as_str().to_string(),
            harnesses: harness_list(&cfg),
            resuming: resuming.then(|| "the last stored handoff for this repo".to_string()),
            model: cfg.chat.model.clone(),
        };
        // `size.0 == 0` only ever means the terminal-size probe itself
        // failed (`probe_terminal`'s own `unwrap_or((0, 0))`), not a real
        // zero-width terminal -- treated as "unknown" so the banner falls
        // back to its compact tier instead of rendering a zero-width box.
        let banner_cols = (size.0 > 0).then_some(size.0);
        writeln!(
            w,
            "{}",
            chrome::banner(&facts, chrome.colour, vt_ok, banner_cols)
        )?;
    }

    let env = quiet_env(env, args.quiet);

    // Emitted here -- after `quiet_env`, before either launch path -- so the
    // dashboard branch and the `wrap` fallback disclose identically, and
    // independently of whether a banner was printed at all.
    announce_model_choice(stderr, &cfg, args.quiet);

    if chrome::dash_eligible(
        stdout_is_tty,
        stdin_is_tty,
        vt_ok,
        size,
        &cfg.dash,
        args.simple,
    ) {
        let pane = dash_orchestrator_pane(
            adapter.as_ref(),
            launch,
            &cfg,
            &state,
            repo,
            session.as_str(),
            args.simple,
        )?;
        return dash::run_dashboard(&cfg, repo, &env, &state, pane, args.force_pace);
    }

    // Ineligible because the dashboard is on but the terminal is too small
    // (every other axis -- both streams a terminal, VT available, not
    // `--simple` -- already passed): the operator gets one line naming the
    // floor and how to silence the notice, then the same wrap passthrough
    // every other ineligible terminal already reaches. `--simple` itself
    // never reaches here (it is excluded from `dash_eligible`'s failure by
    // construction: it is checked first there, and it is checked here too so
    // an explicit `--simple` never prints a notice about a size it was never
    // going to use anyway).
    if cfg.dash.enabled
        && !args.simple
        && stdout_is_tty
        && stdin_is_tty
        && vt_ok
        && (size.0 < chrome::MIN_DASH_COLS || size.1 < chrome::MIN_DASH_ROWS)
    {
        crate::output::error(format!(
            "the terminal is too small for the dashboard (need at least {}x{}, got {}x{}); \
             falling back to a plain session. Pass --simple to silence this.",
            chrome::MIN_DASH_COLS,
            chrome::MIN_DASH_ROWS,
            size.0,
            size.1
        ));
    }

    let wrap_args = wrap_args_for(args, launch.clone());
    wrap::run_with(
        &wrap_args,
        repo,
        &env,
        launch.role,
        Some(session),
        launch.verb,
    )
}

/// Discloses a configured model on the `zirv \u{25b8}` announcement channel, once,
/// before the launch path is chosen. A no-op when no model is configured.
///
/// `chat.model` is one of the few keys a **repo** `ctx.toml` may set, and that
/// exemption was granted on the strength of the choice being visible on
/// screen. It was not: the only disclosure was `chrome::banner`, and
/// `chrome.banner` is **not** `REPO_FORBIDDEN` -- so a checked-out repo could
/// pair `[chrome] banner = false` with `[chat] model = "..."` and select the
/// model for every session with nothing shown anywhere at all (the `wrap`
/// fallback carries no other model surface; the dashboard header carries one,
/// but only the dashboard has one).
///
/// `chrome.events` **is** `REPO_FORBIDDEN`, so this line survives any repo
/// configuration. The operator's own `--quiet`/`ZIRV_CTX_QUIET` still silences
/// it -- operator over repo, the same asymmetry every other trust decision in
/// this codebase makes.
///
/// `quiet` is passed separately because `cfg` was loaded before `--quiet` was
/// folded into the environment (`quiet_env`), so `cfg.chrome.events` alone
/// knows about `ZIRV_CTX_QUIET` and `[chrome] events` but not about the flag.
fn announce_model_choice<E: Write>(stderr: &mut E, cfg: &CtxConfig, quiet: bool) {
    let Some(model) = &cfg.chat.model else {
        return;
    };
    super::announce::Announcer::new(
        cfg.chrome.events && !quiet,
        console::colors_enabled_stderr(),
    )
    .emit_to(
        stderr,
        &super::announce::Event::ChatModel {
            model: model.clone(),
        },
    );
}

/// The dashboard's orchestrator pane, composed prompt and all.
///
/// F3: the dashboard branch used to hand `dash::run_dashboard` the bare
/// `build_launch` argv, so the one session a human actually talks to was the
/// only session in the whole codebase that got **no** zirv prompt at all --
/// no shipped default layer, no harness meta-teaching, no user/repo/memory
/// layers, and no injection log line -- while the `wrap` fallback below and
/// every worker pane the dashboard spawns all get the full recipe. An
/// operator could not tell the two launch paths apart from the outside, and
/// the orchestrator is precisely the session that is supposed to know how to
/// delegate.
///
/// This is `wrap::run_with`'s own recipe, in its order and with its
/// arguments: `compile::compile` (memory, the derived harness roster --
/// `adapters::harness_prompt_lines`, only for an `Orchestrator` launch --
/// `prompt::compose` as an `Orchestrator`, and the canonical `.zirv/context/`
/// layer on top, issue #44), `merge_command_line_prompt` so an operator's own
/// `--append-system-prompt` in `--` extras is folded in rather than silently
/// duplicated, `injection_args_for_session`, then `log_injection`.
///
/// Deliberately **no** `prompt::with_mail_layer`, exactly like the `wrap`
/// path it mirrors: an interactive Orchestrator session is never given mail
/// bodies, only the one-line unread-count advisory the dashboard header
/// already carries. Only a headless Worker session (`exec`/`loop`, and the
/// worker panes `dash::fulfill_spawn_request` builds) is body-delivered.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dash_orchestrator_pane(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    session: &str,
    simple: bool,
) -> CtxResult<PaneSpec> {
    // Issue #44: gathers memory, the derived harness roster and the
    // canonical `.zirv/context/` layer, and attaches the policy report --
    // see `compile::compile`'s own doc comment.
    let compiled = super::compile::compile(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        simple,
        cfg,
        adapter,
        launch.role,
        state,
        super::state::now_secs(),
        super::adapters::LaunchMode::Interactive,
        true,
    );
    let (mut argv, composed) = super::prompt::merge_command_line_prompt(
        adapter,
        &launch.argv,
        compiled.composed,
        None,
        launch.role,
        &cfg.prompt,
    );
    let prompt_args = super::prompt::injection_args_for_session(
        adapter,
        &argv,
        composed.as_ref(),
        state,
        session,
    )?;
    super::prompt::log_injection(
        state,
        "chat",
        session,
        composed.as_ref(),
        adapter.system_prompt_supported(&argv),
    );
    // Bug B (harness/model parity, 2026-08-22): the same seam every real
    // launch now calls (`adapters::policy_launch_args`) -- the shipped-
    // default "sandboxed, no prompts" posture plus any explicit `[policy]`
    // restriction. This is the dashboard's own orchestrator pane, the
    // interactive session a human is actually watching, so an approval
    // prompt here is at least answerable -- but the posture still applies:
    // "sandboxed, no prompts" is the shipped default for every launch, not
    // only the unattended ones. `flags_pin_policy` (inside `policy_launch_
    // args`) scans the argv built so far, so an operator's own explicit
    // `--sandbox`/`--ask-for-approval`/`--permission-mode`/
    // `--disallowedTools` (passed after `--` on `zirv chat`) still wins.
    let sandbox_extra =
        adapters::policy_launch_args(cfg, adapter, &argv, adapters::LaunchMode::Interactive);
    // Visible, not silent: the one interactive pane a human is actually
    // watching gets the same announcement every headless seam does. `Chrome
    // events`/`--quiet` govern it identically (`cfg.chrome.events`); no
    // `quiet` parameter reaches this function, so a caller that silenced the
    // banner (`--quiet` folded into the environment before `CtxConfig::load`
    // ran) already has `cfg.chrome.events == false` here too.
    super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr()).emit(
        &super::announce::Event::SandboxPosture {
            detail: if sandbox_extra.is_empty() {
                "not applied (operator flags or [sandbox] enabled = false)".to_string()
            } else {
                sandbox_extra.join(" ")
            },
        },
    );
    argv.extend(sandbox_extra);
    argv.extend(prompt_args);
    // R1: a dashboard pane -- and only a dashboard pane -- pins the harness's
    // own conversation to zirv's session uuid, so the quit roster's stored id
    // is the id `AgentAdapter::resume_args` is later asked to resume. The
    // `wrap` fallback below deliberately does not: its relaunches expect the
    // harness to mint a fresh conversation each time. Empty for any adapter
    // with no verified pin flag (codex).
    //
    // D3: unless the operator already pinned it themselves. `zirv chat --
    // --resume <id>` is an explicit instruction about which conversation this
    // seat is, and appending a fresh `--session-id` on top of it hands the
    // harness two contradictory ids and gets the launch refused. The
    // operator's own flag wins.
    //
    // F6: the roster then does **not** record the conversation the operator
    // named. `PaneSpec::session_id` below is zirv's own `session` uuid either
    // way -- nothing reads the operator's `--resume` value back out of the
    // argv -- so for a pin-suppressed launch the id in the roster and the
    // harness's actual conversation id genuinely differ. That is inert only
    // because this pane is the orchestrator: `dash::on_quit` stamps it
    // `roster::ROLE_ORCHESTRATOR` and `dash::restorable_candidates` filters
    // that role out before `roster::restore_argv` is ever called, so no
    // `--resume <uuid zirv invented>` is ever issued from this entry. A worker
    // pane has no such escape hatch, which is why `dash::fulfill_spawn_request`
    // pins unconditionally.
    if !super::exec::pins_an_existing_conversation(&argv, adapter.name()) {
        argv.extend(adapter.session_pin_args(session));
    }

    Ok(PaneSpec {
        agent_name: launch.agent_name,
        argv,
        role: launch.role,
        verb: launch.verb,
        session_id: session.to_string(),
        title: "orch".to_string(),
    })
}

/// The extra arguments a chat launch is built with: the configured model's
/// own flags (`AgentAdapter::model_args`) ahead of whatever the operator
/// passed after `--`. Handing these to `build_launch`/`interactive_cmd` puts
/// them *after* the positional initial prompt, which is where CLI flags are
/// still perfectly valid and -- unlike splicing them into an already-built
/// argv -- is the one placement that cannot land inside a launcher prefix.
///
/// R1: this used to splice `model_args` in at `launch_prefix_len()`, which
/// deliberately counts only the argv the *operator* wrote (program plus
/// `bin_args`) and explicitly does not count the tokens `ClaudeAdapter::base`
/// prepends when it has to route an npm-installed `claude.cmd` through
/// `cmd.exe /c` (see `claude.rs`'s own `launch_prefix_len` doc comment). On
/// such a launch the real argv prefix is three tokens, not one, so the splice
/// produced `["cmd.exe", "--model", "fable", "/c", "claude.cmd", ...]` --
/// `cmd.exe` was handed the model flags and never started the agent at all.
/// Appending as trailing extras removes the prefix arithmetic entirely, the
/// same way `wrap::restart_launch_flags`'s output is carried into
/// `relaunch_command`'s `extra` rather than spliced anywhere.
///
/// An adapter with no verified model flag, or no configured model, yields the
/// operator's own extras unchanged.
fn extra_with_model(cfg: &CtxConfig, adapter: &dyn AgentAdapter, extra: &[String]) -> Vec<String> {
    let Some(model) = cfg.chat.model.as_deref() else {
        return extra.to_vec();
    };
    let mut out = adapter.model_args(model);
    out.extend_from_slice(extra);
    out
}

/// The `wrap` invocation a resolved chat launch becomes. Pure, so what does
/// and does not survive the hand-off is testable without a pty.
///
/// `allow_nested` is threaded through rather than re-derived: `wrap::run_with`
/// runs the same nesting guard again against the same environment, so an
/// override honored here but dropped here would simply be refused one layer
/// down.
pub fn wrap_args_for(args: &ChatArgs, launch: ChatLaunch) -> WrapArgs {
    WrapArgs {
        agent: Some(launch.agent_name),
        no_supervise: false,
        command: launch.argv,
        simple: args.simple,
        allow_nested: args.allow_nested,
        force_pace: args.force_pace,
    }
}

/// `--quiet` on the `chat` and `agent` verbs is a CLI flag, not an
/// environment variable, but `CtxConfig::load` (inside `wrap::run_with`)
/// only ever reads `chrome.events` from config layers and `ZIRV_CTX_QUIET`.
/// Folding the flag into the same env lookup both already share is simpler
/// than adding a second, parallel "quiet" parameter to every downstream
/// signature: it reuses the one config key that already means "silence the
/// announcement channel", and honors the same operator-overrides-repo
/// precedence `ZIRV_CTX_QUIET` always has.
pub(crate) fn quiet_env<'a>(
    env: EnvLookup<'a>,
    quiet: bool,
) -> impl Fn(&str) -> Option<String> + 'a {
    move |key: &str| {
        if quiet && key == "ZIRV_CTX_QUIET" {
            Some("true".to_string())
        } else {
            env(key)
        }
    }
}

pub fn run<W: Write>(args: &ChatArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &mut std::io::stderr(), &repo, &env)
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
            files_modified: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec![],
            ..Handoff::default()
        }
    }

    #[test]
    fn chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv() {
        // Deliberately does not recompute `expected_argv` by calling
        // `adapter.interactive_cmd` a second time and extracting it the same
        // way `build_launch` does: that would just be `build_launch`'s own
        // extraction logic compared against itself, so a bug in it would
        // never show up here. Asserting on fixed, independently-known
        // content (the adapter binary is the given constant; the prompt is
        // the given constant; extra flags land after both) is what actually
        // pins the behavior.
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let launch = build_launch(
            &adapter,
            Some("hello"),
            &["--model".to_string(), "opus".to_string()],
        );

        assert_eq!(
            launch.argv.first().map(String::as_str),
            Some("/tmp/fake-claude"),
            "the argv's own program is the adapter's own binary: {:?}",
            launch.argv
        );
        assert!(
            launch.argv.contains(&"hello".to_string()),
            "the initial prompt reaches argv: {:?}",
            launch.argv
        );
        assert_eq!(
            &launch.argv[launch.argv.len() - 2..],
            &["--model".to_string(), "opus".to_string()],
            "extra flags land last: {:?}",
            launch.argv
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

    /// Bug B: claude always reports `system_prompt_supported`, so this
    /// function must be a true no-op for it -- byte-for-byte the same
    /// `initial_prompt` in and out, whether or not one was given.
    #[test]
    fn orchestrator_initial_prompt_is_a_no_op_for_a_supported_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                None,
                &cfg,
                Some(&home),
                tmp.path(),
                false,
                &state,
            ),
            None
        );
        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                Some("resume this".to_string()),
                &cfg,
                Some(&home),
                tmp.path(),
                false,
                &state,
            ),
            Some("resume this".to_string())
        );
    }

    /// Bug B, the actual fix: on a Windows npm-installed `codex.cmd` shim --
    /// the shape `CodexAdapter::system_prompt_supported` narrows to
    /// unsupported -- the composed session context (the shipped default
    /// layer and, because this is an Orchestrator launch, the harness
    /// meta-teaching layer) must land on the positional initial-prompt slot,
    /// since `injection_args_for_session` never reaches this adapter at all.
    /// Before this fix a codex orchestrator on this launch shape started
    /// with no zirv context whatsoever, while a claude orchestrator always
    /// got one (see the previous test).
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_folds_composed_context_for_an_unsupported_codex_shim() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));
        assert!(
            !adapter.system_prompt_supported(&[]),
            "a .cmd shim must be the unsupported shape this test exercises"
        );

        let text = orchestrator_initial_prompt(
            &adapter,
            None,
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
        )
        .expect("an unsupported adapter still gets a fallback prompt");
        assert!(
            text.contains("zirv engineering standard"),
            "the shipped default layer must reach the fallback: {text}"
        );
        assert!(
            text.contains("zirv meta-harness"),
            "an Orchestrator launch must still get the harness delegation layer: {text}"
        );

        // A real resume prompt is preserved as the leading text, with the
        // composed context appended after it -- the same order `exec.rs`'s
        // own `task_prompt_with_composed_fallback` call keeps for a headless
        // worker's own task text.
        let with_resume = orchestrator_initial_prompt(
            &adapter,
            Some("continue the payments webhook".to_string()),
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
        )
        .expect("still folds a fallback in on top of a real prompt");
        assert!(
            with_resume.starts_with("continue the payments webhook"),
            "the caller's own prompt text must lead: {with_resume}"
        );
        assert!(
            with_resume.contains("zirv engineering standard"),
            "and the composed context must still follow it: {with_resume}"
        );
    }

    /// `--simple` disables prompt composition entirely (`compile::compile`
    /// returns `composed: None`, mirroring `prompt::compose`'s own gate), so
    /// even an unsupported adapter must fall back to the caller's own
    /// `initial_prompt` unchanged rather than injecting anything.
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_is_a_no_op_under_simple_even_for_an_unsupported_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));

        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                None,
                &cfg,
                Some(&home),
                tmp.path(),
                true,
                &state
            ),
            None,
            "--simple must still suppress every zirv-injected layer, fallback included"
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

    /// N1: a chat session's registry record must say "chat", not fall back to
    /// `wrap`'s own default verb -- `wrap::run_with` takes a `verb` parameter
    /// precisely so this can be threaded through explicitly rather than
    /// guessed from `role` (the two are independent: role governs prompt
    /// injection permissions, verb only names the calling verb for the
    /// registry). Unit-tested here, on the same pure `build_launch` the role
    /// assertions above already exercise, rather than through a real pty.
    #[test]
    fn chat_registers_as_chat_rather_than_wrap() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            build_launch(&adapter, None, &[]).verb,
            crate::commands::ctx::sessions::Verb::Chat,
        );
        // Resuming or adding extra flags must not change that either.
        assert_eq!(
            build_launch(&adapter, Some("resume this"), &["--model".to_string()]).verb,
            crate::commands::ctx::sessions::Verb::Chat,
        );
    }

    /// F3: the dashboard's orchestrator pane must carry the same composed
    /// prompt the `wrap` fallback builds -- the shipped default layer proves
    /// injection happened at all, and the harness meta-teaching layer proves
    /// it happened as an *Orchestrator*. Before this fix the dashboard
    /// branch handed `run_dashboard` the bare adapter argv, so the one
    /// session a human talks to was the only unprompted one in the codebase.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_composed_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        // A binary that does not exist and is not a cmd shim: the file-flag
        // capability probe fails and the launch is not the reparsed `cmd.exe /c`
        // form, so `injection_args_for_session` uses the inline
        // `system_prompt_args` form and the prompt text is visible in argv --
        // which is what makes this assertable without a real agent. (The shim
        // form, which forces the file form, is covered in `prompt.rs`.)
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("zirv engineering standard"),
            "the shipped default layer proves injection happened: {argv}"
        );
        assert!(
            argv.contains("zirv meta-harness"),
            "an orchestrator session gets the harness delegation layer: {argv}"
        );
        assert_eq!(pane.role, PromptRole::Orchestrator);
        assert_eq!(pane.verb, crate::commands::ctx::sessions::Verb::Chat);
        assert_eq!(pane.title, "orch");
        assert_eq!(
            pane.argv.first().map(String::as_str),
            Some("/nonexistent/fake-claude"),
            "the launch program is still the adapter's own binary: {argv}"
        );
    }

    /// Bug B (harness/model parity, 2026-08-22): the dashboard's own
    /// orchestrator pane is the interactive session a human actually
    /// watches -- previously the one path a codex operator saw zero
    /// zirv-applied argv restriction on. It now carries the shipped-default
    /// sandbox posture too, and an operator's own explicit pin still wins.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_shipped_sandbox_posture_by_default() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
        )
        .expect("pane");
        assert!(
            pane.argv
                .windows(2)
                .any(|w| w == ["--sandbox", "workspace-write"]),
            "got {:?}",
            pane.argv
        );
        assert!(
            pane.argv
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"]),
            "got {:?}",
            pane.argv
        );
    }

    /// An operator's own explicit `--sandbox`/`--ask-for-approval` (passed
    /// after `--` on `zirv chat`) suppresses the zirv-computed prefix
    /// entirely, the same `flags_pin_policy` contract every other seam
    /// honours.
    #[test]
    fn the_dash_orchestrator_pane_lets_an_operators_own_sandbox_flag_win() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let extra = vec!["--sandbox".to_string(), "danger-full-access".to_string()];
        let launch = build_launch(&adapter, None, &extra);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
        )
        .expect("pane");
        assert_eq!(
            pane.argv
                .iter()
                .filter(|a| a.as_str() == "--sandbox")
                .count(),
            1,
            "the operator's own --sandbox must appear exactly once, not augmented: {:?}",
            pane.argv
        );
        assert!(pane.argv.contains(&"danger-full-access".to_string()));
    }

    /// `[sandbox] enabled = false` restores the pre-2026-08-22 behaviour: no
    /// posture argv from this seam at all.
    #[test]
    fn the_dash_orchestrator_pane_carries_nothing_when_the_sandbox_posture_is_opted_out() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig {
            sandbox: crate::commands::ctx::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..CtxConfig::default()
        };

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
        )
        .expect("pane");
        assert!(
            !pane.argv.contains(&"--sandbox".to_string()),
            "got {:?}",
            pane.argv
        );
    }

    /// Issue #34 seam coverage (memory review, fix round): the dashboard
    /// orchestrator pane's composed prompt must actually carry the memory
    /// core layer, bounded by the CONFIGURED `cfg.memory.core_max_bytes` --
    /// not a hardcoded default. A tiny cap forces `prompt::with_memory_layer`
    /// to truncate, which only happens if the seam really threads the
    /// configured value through (see `with_memory_layer`'s own truncation
    /// note).
    #[test]
    fn the_dash_orchestrator_pane_carries_the_memory_layer_under_its_configured_cap() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.core_max_bytes = 40;
        // Issue #155: the merged memory layer is capped by the SUM of the two
        // budgets now, not `core_max_bytes` alone -- zero the retrieval half
        // out so this test's tiny budget still actually bounds what gets
        // delivered.
        cfg.memory.retrieval_max_bytes = 0;
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());

        crate::commands::ctx::memory::remember(
            &state,
            &slug,
            &crate::commands::ctx::memory::Entry {
                key: "seam-fact".to_string(),
                written_by: "test".to_string(),
                written: 1,
                verified: 1,
                source: "explicit".to_string(),
                body: format!("{}TAIL_MARKER_NOT_TRUNCATED", "z".repeat(200)),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("seam-fact"),
            "the memory core layer must reach the composed prompt: {argv}"
        );
        assert!(
            !argv.contains("TAIL_MARKER_NOT_TRUNCATED"),
            "a tiny core_max_bytes must actually bound the delivered memory layer: {argv}"
        );
        assert!(
            argv.contains("[memory truncated:"),
            "the truncation must be visible, not silent: {argv}"
        );
    }

    /// The orchestrator is never body-delivered mail -- it gets the header's
    /// one-line unread-count advisory instead. Same trust split `wrap`'s own
    /// orchestrator path holds (it never calls `with_mail_layer` either);
    /// only a headless Worker session is handed message bodies.
    #[test]
    fn the_dash_orchestrator_pane_is_never_given_mail_bodies() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "SECRET-MAIL-BODY-MARKER".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            !argv.contains("SECRET-MAIL-BODY-MARKER"),
            "an interactive orchestrator never receives message bodies: {argv}"
        );
        assert!(
            crate::commands::ctx::mail::list(&state, &slug, None, None)
                .expect("list")
                .len()
                == 1,
            "and nothing was consumed on its behalf either"
        );
    }

    /// `--simple` promises no zirv-*injected instruction* -- the composed
    /// prompt layer -- at all. It also makes the terminal dashboard-
    /// ineligible, so this path is unreachable in practice today -- pinned
    /// anyway, because the flag's meaning must not depend on which launch
    /// path happens to be taken.
    ///
    /// 2026-08-22 revision: the shipped-default sandbox posture
    /// (`adapters::policy_launch_args`) is a *safety* flag layer, not
    /// injected instruction text, so `--simple` does not withhold it --
    /// otherwise `--simple` would double as an accidental way to disable
    /// the default sandboxing, which is not what "skip zirv's injected
    /// text" asks for. This test now pins that the session pin *and* the
    /// sandbox prefix survive `--simple`, and nothing else does.
    #[test]
    fn a_simple_dash_orchestrator_pane_still_carries_the_sandbox_posture_but_no_injected_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let mut expected = launch.argv.clone();
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            true,
        )
        .expect("pane");
        // R1: the session pin is launch plumbing, not injected instruction --
        // `--simple` promises the agent no zirv-authored text, and a pane that
        // cannot be resumed after a quit is not what it is asking for.
        expected.extend(adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::adapters::LaunchMode::Interactive,
        ));
        expected.extend(adapter.session_pin_args("11111111-2222-4333-8444-555555555555"));
        assert_eq!(
            pane.argv, expected,
            "--simple leaves the adapter's own argv untouched apart from the sandbox posture \
             and the session pin"
        );
    }

    /// R1: the roster stores zirv's own uuid, so a dashboard pane has to make
    /// the harness adopt it as the conversation id -- otherwise the next
    /// launch's restore runs `claude --resume <uuid zirv invented>` and the
    /// restored pane dies with "no conversation found" before it draws a
    /// frame.
    #[test]
    fn the_dash_orchestrator_pane_pins_the_harness_session_to_zirvs_own_uuid() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane =
            dash_orchestrator_pane(&adapter, launch, &cfg, &state, tmp.path(), session, false)
                .expect("pane");

        let pin = pane
            .argv
            .iter()
            .position(|a| a == "--session-id")
            .unwrap_or_else(|| panic!("no --session-id in {:?}", pane.argv));
        assert_eq!(
            pane.argv.get(pin + 1).map(String::as_str),
            Some(session),
            "the pinned id is the pane's own registry session id: {:?}",
            pane.argv
        );
    }

    /// D3: an operator who passed their own resume flag has already said which
    /// conversation this seat is. Appending a fresh `--session-id` on top of it
    /// hands the harness two contradictory ids and gets the launch refused
    /// outright -- and inside a dashboard the pane then died on the spot and was
    /// reaped, so the failure was invisible.
    #[test]
    fn an_operators_own_resume_flag_suppresses_the_session_pin() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let existing = "99999999-8888-4777-8666-555555555555";
        for extra in [
            vec!["--resume".to_string(), existing.to_string()],
            vec![format!("--resume={existing}")],
            vec!["--session-id".to_string(), existing.to_string()],
            vec![format!("--session-id={existing}")],
            vec!["-c".to_string()],
            vec!["--continue".to_string()],
            vec!["--fork-session".to_string()],
        ] {
            let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
            let launch = build_launch(&adapter, None, &extra);
            let pane =
                dash_orchestrator_pane(&adapter, launch, &cfg, &state, tmp.path(), session, false)
                    .expect("pane");

            assert!(
                !pane.argv.iter().any(|a| a == session),
                "no fresh pin may be appended alongside {extra:?}: {:?}",
                pane.argv
            );
            assert!(
                pane.argv.iter().any(|a| a.contains(existing)
                    || a == "-c"
                    || a == "--continue"
                    || a == "--fork-session"),
                "and the operator's own flag still reaches the harness: {:?}",
                pane.argv
            );
        }
    }

    /// F6: what the roster actually records when the pin is suppressed. The
    /// `PaneSpec` keeps zirv's own uuid whatever the operator pinned, so the
    /// stored id and the harness's real conversation id differ -- and the only
    /// thing that makes that inert is the orchestrator being excluded from
    /// restore (`dash::restorable_candidates`, pinned by its own test). This
    /// test is the other end of that pair: it states the mismatch plainly, so
    /// a future change that starts restoring orchestrators has to face it.
    #[test]
    fn a_pin_suppressed_orchestrator_pane_still_carries_zirvs_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let existing = "99999999-8888-4777-8666-555555555555";
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(
            &adapter,
            None,
            &["--resume".to_string(), existing.to_string()],
        );
        let pane =
            dash_orchestrator_pane(&adapter, launch, &cfg, &state, tmp.path(), session, false)
                .expect("pane");

        assert_eq!(
            pane.session_id, session,
            "the pane -- and so the roster entry built from it -- keeps zirv's own uuid"
        );
        assert!(
            pane.argv.iter().any(|a| a == existing),
            "while the harness is actually resuming the operator's conversation: {:?}",
            pane.argv
        );
        assert_eq!(
            pane.verb,
            crate::commands::ctx::sessions::Verb::Chat,
            "and it is the verb `on_quit` stamps ROLE_ORCHESTRATOR from, which is what keeps \
             the mismatch out of any restore"
        );
    }

    // The `chat.model` disclosure. `chat.model` is repo-settable because the
    // choice is supposed to be visible; `chrome.banner` is not
    // `REPO_FORBIDDEN`, so the banner alone could be turned off by the same
    // repo that chose the model. `chrome.events` is.

    // `cfg_with_model` is the shared helper defined further down with the
    // Task 6 model-splice tests.

    #[test]
    fn a_configured_chat_model_is_disclosed_on_the_events_channel() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(Some("fable")), false);
        let text = String::from_utf8(err).expect("utf8");
        assert!(
            text.contains("chat model 'fable' (from config)"),
            "got {text:?}"
        );
    }

    #[test]
    fn no_configured_model_discloses_nothing() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(None), false);
        assert!(err.is_empty(), "got {err:?}");
    }

    /// The operator may silence it; a repo may not. `--quiet` reaches this as
    /// the flag (config was loaded before it was folded into the environment),
    /// `ZIRV_CTX_QUIET`/`[chrome] events = false` reach it as
    /// `cfg.chrome.events` -- both are operator-controlled surfaces.
    #[test]
    fn the_operator_can_silence_the_model_disclosure_but_a_repo_cannot() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(Some("fable")), true);
        assert!(err.is_empty(), "--quiet silences it: {err:?}");

        let mut quiet_cfg = cfg_with_model(Some("fable"));
        quiet_cfg.chrome.events = false;
        let mut err = Vec::new();
        announce_model_choice(&mut err, &quiet_cfg, false);
        assert!(
            err.is_empty(),
            "ZIRV_CTX_QUIET / [chrome] events = false silences it too: {err:?}"
        );

        // And the repo's own lever does not: `chrome.banner` is not
        // `REPO_FORBIDDEN`, so a repo can turn the banner off -- the events
        // line is emitted regardless of it.
        let mut bannerless = cfg_with_model(Some("fable"));
        bannerless.chrome.banner = false;
        let mut err = Vec::new();
        announce_model_choice(&mut err, &bannerless, false);
        assert!(
            String::from_utf8(err).expect("utf8").contains("fable"),
            "a repo-disabled banner must not take the disclosure with it"
        );
    }

    /// The exact scenario the finding describes, end to end through the real
    /// config loader: a repo turns its banner off and picks a model. It may do
    /// both -- neither key is repo-forbidden -- and the events line is what
    /// discloses the choice anyway. A repo that tries to silence *that* channel
    /// does not get a quiet session, it gets a refusal.
    #[test]
    fn a_repo_can_hide_the_banner_and_pick_a_model_but_cannot_hide_the_disclosure() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv");
        std::fs::create_dir_all(&dir).expect("mkdir .zirv");
        std::fs::write(
            dir.join("ctx.toml"),
            "[chrome]\nbanner = false\n\n[chat]\nmodel = \"sneaky\"\n",
        )
        .expect("write repo ctx.toml");
        let env: std::collections::HashMap<String, String> = Default::default();
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");

        assert!(!cfg.chrome.banner, "a repo may turn the banner off");
        assert_eq!(
            cfg.chat.model.as_deref(),
            Some("sneaky"),
            "and it may still choose the model"
        );
        assert!(
            cfg.chrome.events,
            "but the announcement channel is still on"
        );

        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg, false);
        assert!(
            String::from_utf8(err).expect("utf8").contains("sneaky"),
            "so the choice is disclosed anyway"
        );

        // And the channel itself is `REPO_FORBIDDEN`: a repo reaching for it
        // fails the load outright rather than quietly winning.
        std::fs::write(
            dir.join("ctx.toml"),
            "[chrome]\nevents = false\n\n[chat]\nmodel = \"sneaky\"\n",
        )
        .expect("write repo ctx.toml");
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a repo may not set chrome.events");
        assert!(
            err.to_string().contains("chrome.events"),
            "the refusal names the key: {err}"
        );
    }

    /// End to end through `run_with`'s own stderr writer, on the `wrap`
    /// fallback path (the only one reachable under `cargo test`'s piped
    /// stdio). The dashboard branch cannot be driven from a test, which is
    /// precisely why the emit sits *before* the branch: one call site, both
    /// paths.
    #[test]
    fn run_with_discloses_the_model_before_it_picks_a_launch_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            ("ZIRV_CTX_CHAT_MODEL".to_string(), "fable".to_string()),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        // The launch itself fails (the configured binary does not exist),
        // which is fine and is what the neighbouring tests already pin: the
        // disclosure happens before the launch either way.
        let _ = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        );

        let text = String::from_utf8(err_out).expect("utf8");
        assert!(
            text.contains("chat model 'fable' (from config)"),
            "the disclosure must reach stderr on the wrap fallback path: {text:?}"
        );
    }

    /// R1, the other half: `wrap` is untouched. Its relaunch path expects the
    /// harness to mint a fresh conversation on every restart, so the pin lives
    /// at the dashboard-pane seam and never inside `interactive_cmd`/
    /// `build_launch`.
    #[test]
    fn the_wrap_fallback_launch_is_never_session_pinned() {
        let adapter = ClaudeAdapter::new(None);
        let launch = build_launch(&adapter, Some("do the thing"), &["--model".to_string()]);
        assert!(
            !launch.argv.iter().any(|a| a == "--session-id"),
            "the plain chat/wrap launch carries no pin: {:?}",
            launch.argv
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
    /// Printed to `stderr` (not `w`/stdout: `zirv chat > log` must still show
    /// the operator something on the terminal, matching `output::error`'s own
    /// stream) and reported via exit code 1 rather than a returned `Err`:
    /// propagating it would have `zirv ctx`'s own dispatch print the same
    /// text a second time, unstyled, through `output::error`.
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
            quiet: false,
            allow_nested: false,
            force_pace: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1, "nothing is both enabled and ready");
        assert!(out.is_empty(), "nothing prints to stdout on this path");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(msg.contains("disabled"), "must say why: {msg}");
    }

    /// The gate is checked, and refuses, before any terminal or pty work:
    /// this runs synchronously, with no pty ever opened, to a printed
    /// message and exit code 1 (not a returned `Err` -- see the comment on
    /// `chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_
    /// reason` for why). `wrap` performs the identical gate check on its
    /// own before touching a terminal, so the refusal holds even by that
    /// second, independent path.
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
            quiet: false,
            allow_nested: false,
            force_pace: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1");
        assert_eq!(code, 1, "claude is disabled");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    #[test]
    fn codex_is_a_valid_launch_target_regardless_of_readiness() {
        // Sanity: build_launch itself does not care about readiness, only
        // resolve_adapter (exercised above) does -- true whether or not
        // codex's own ready() happens to succeed on the machine running this.
        let adapter = CodexAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "codex");
        assert_eq!(launch.role, PromptRole::Orchestrator);
    }

    /// The chrome probe (`std::io::stdout().is_terminal()`, `term::window_
    /// size`, `term::enable_vt_output`) runs unconditionally at the top of
    /// `run_with`, before adapter resolution -- under cargo test's own piped
    /// stdio (never a terminal) it must degrade cleanly rather than panic,
    /// and a disabled agent must still be refused, with no banner printed
    /// (there is nothing to show a banner for once resolution has failed).
    #[test]
    fn resolving_a_disabled_agent_under_non_terminal_stdio_does_not_panic_and_prints_no_banner() {
        // The agent is disabled explicitly (the same setup `chat_with_no_
        // enabled_and_ready_adapter_names_each_candidate_and_its_reason`
        // uses) so this test never depends on whatever agent binaries
        // happen to be on this machine's PATH: `resolve_adapter` fails
        // deterministically before `wrap::run_with` -- and therefore before
        // any pty or subprocess -- is ever reached.
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
            quiet: false,
            allow_nested: false,
            force_pace: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        // Reaching this line at all -- rather than a panic from the probe --
        // is the main thing this test pins.
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1, no panic");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out).expect("utf8").is_empty(),
            "no terminal means no banner, and resolution failed before the banner code anyway"
        );
        let printed = String::from_utf8(err_out).expect("utf8");
        assert!(printed.contains("disabled"), "got {printed}");
    }

    // F2: the nesting guard, checked before anything touches the terminal.

    fn chat_args(allow_nested: bool) -> ChatArgs {
        ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested,
            force_pace: false,
            extra: Vec::new(),
        }
    }

    /// The refusal comes out on `stderr` as exit code 1, the same shape every
    /// other `chat` refusal uses (a returned `Err` would be printed a second
    /// time by `ctx`'s own dispatch), and it names the outer session so the
    /// operator can see *which* one they were about to endanger.
    #[test]
    fn chat_refuses_to_start_inside_a_supervised_session_and_names_the_evidence() {
        let repo = crate::commands::ctx::testenv::repo();
        // The `ZIRV_CTX_AGENT_BIN` entry is a safety belt, not a fixture
        // detail: `adapters::select`/`resolve_default` call `ready()`, so an
        // agent_bin that cannot exist makes a launch structurally
        // impossible. If the guard under test ever regresses, this fails on
        // a missing binary rather than spawning a real nested agent.
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("refuses by printing and exiting 1, not by propagating an Err");

        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out).expect("utf8").is_empty(),
            "nothing goes to stdout on this path -- not even a banner"
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            msg.contains("refusing to start inside an existing agent session"),
            "got {msg}"
        );
        assert!(msg.contains("abcdef12"), "names the outer session: {msg}");
        assert!(
            msg.contains("--allow-nested"),
            "says how to override: {msg}"
        );
    }

    /// With the override on, the guard is out of the way and resolution
    /// proceeds -- reaching the disabled-agent refusal instead. That specific
    /// later message is the evidence the guard was passed, without this test
    /// ever launching an agent.
    #[test]
    fn allow_nested_overrides_the_guard() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "abcdef12-3456-4789-8abc-def012345678".to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(
            &chat_args(true),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("past the guard, onto adapter resolution");
        assert_eq!(code, 1);
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            !msg.contains("refusing to start inside"),
            "the guard was overridden: {msg}"
        );
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// `--allow-nested` has to reach `wrap` too: `wrap::run_with` runs the
    /// identical guard against the identical environment, so an override that
    /// stopped here would simply be refused one layer down.
    #[test]
    fn the_override_is_threaded_through_to_the_wrap_arguments() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        for allow_nested in [false, true] {
            let wrap_args = wrap_args_for(&chat_args(allow_nested), launch.clone());
            assert_eq!(
                wrap_args.allow_nested, allow_nested,
                "chat's own override has to reach wrap's identical guard"
            );
            assert_eq!(wrap_args.agent.as_deref(), Some("claude"));
            assert!(
                !wrap_args.no_supervise,
                "a chat session is always supervised"
            );
        }
    }

    #[test]
    fn quiet_folds_into_the_env_lookup_as_zirv_ctx_quiet() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let base = |k: &str| empty.get(k).cloned();
        let looked_up = quiet_env(&base, true);
        assert_eq!(looked_up("ZIRV_CTX_QUIET"), Some("true".to_string()));
        assert_eq!(looked_up("ZIRV_CTX_AGENT"), None, "other keys pass through");

        let not_quiet = quiet_env(&base, false);
        assert_eq!(
            not_quiet("ZIRV_CTX_QUIET"),
            None,
            "without --quiet the underlying lookup is untouched"
        );
    }

    #[test]
    fn an_interactive_quiet_flag_overrides_the_operators_stored_zirv_ctx_quiet_false() {
        // An operator who explicitly set ZIRV_CTX_QUIET=false is still
        // overridden by an interactive --quiet flag: the flag is this
        // invocation's own request, layered on top like any other override.
        let set: std::collections::HashMap<String, String> =
            [("ZIRV_CTX_QUIET".to_string(), "false".to_string())].into();
        let base = |k: &str| set.get(k).cloned();
        let looked_up = quiet_env(&base, true);
        assert_eq!(looked_up("ZIRV_CTX_QUIET"), Some("true".to_string()));
    }

    // Task 6: dashboard wiring -- model splice and the wrap fallback.

    fn cfg_with_model(model: Option<&str>) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.chat.model = model.map(str::to_string);
        cfg
    }

    /// The configured model's flags land after the positional prompt and
    /// ahead of the operator's own `--` extras; no configured model leaves the
    /// argv byte-for-byte unchanged.
    #[test]
    fn orchestrator_argv_carries_the_configured_model() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));

        let extra = extra_with_model(
            &cfg_with_model(Some("opus")),
            &adapter,
            &["--continue".to_string()],
        );
        let with_model = build_launch(&adapter, Some("hello"), &extra);
        assert_eq!(
            with_model.argv,
            vec![
                "/tmp/fake-claude".to_string(),
                "hello".to_string(),
                "--model".to_string(),
                "opus".to_string(),
                "--continue".to_string(),
            ],
            "model flags follow the prompt, the operator's extras still land last"
        );

        let plain = extra_with_model(&cfg_with_model(None), &adapter, &["--continue".to_string()]);
        let without_model = build_launch(&adapter, Some("hello"), &plain);
        assert_eq!(
            without_model.argv,
            build_launch(&adapter, Some("hello"), &["--continue".to_string()]).argv,
            "no configured model means the argv is untouched"
        );
    }

    /// R1, the shape the old splice broke on: a program whose real argv
    /// prefix is more than one token. `launch_prefix_len()` counts only what
    /// the operator wrote, so splicing at it dropped the model flags *inside*
    /// the launcher's own arguments. Appending them as trailing extras cannot:
    /// whatever the prefix turns out to be, the flags land after the prompt.
    #[test]
    fn model_flags_never_land_inside_a_multi_token_launch_prefix() {
        // `bin_args`: "sh /tmp/stub.sh" is program + one leading argument.
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));
        let extra = extra_with_model(&cfg_with_model(Some("fable")), &adapter, &[]);
        let launch = build_launch(&adapter, Some("do the work"), &extra);
        assert_eq!(
            launch.argv,
            vec![
                "sh".to_string(),
                "/tmp/stub.sh".to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "fable".to_string(),
            ]
        );
    }

    /// The Windows launcher rewrite specifically: an npm-installed
    /// `claude.cmd` is spawned as `cmd.exe /c <shim> ...`, a three-token
    /// prefix against a `launch_prefix_len()` of 1. The old splice put
    /// `--model fable` between `cmd.exe` and `/c`, so `cmd.exe` was handed the
    /// model flags and the agent never started.
    #[cfg(windows)]
    #[test]
    fn model_flags_land_after_the_prompt_behind_the_windows_cmd_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let extra = extra_with_model(&cfg_with_model(Some("fable")), &adapter, &[]);
        let launch = build_launch(&adapter, Some("do the work"), &extra);

        assert_eq!(
            launch.argv,
            vec![
                launch.argv[0].clone(),
                "/c".to_string(),
                shim.display().to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "fable".to_string(),
            ],
            "the launcher prefix stays intact and the model flags trail the prompt"
        );
        assert!(
            launch.argv[0].to_lowercase().contains("cmd"),
            "the shim is routed through cmd.exe: {:?}",
            launch.argv[0]
        );
    }

    /// The dashboard eligibility gate is real terminal I/O
    /// (`std::io::stdout()`/`stdin().is_terminal()`), and `cargo test`'s own
    /// stdio is never a real terminal either way -- so under test, `zirv
    /// chat` always falls through to the `wrap` path regardless of
    /// `--simple`. This pins that the fallback is actually reached (not
    /// short-circuited by some other refusal) by letting adapter resolution
    /// succeed and following it all the way into `wrap::run_with`'s own
    /// spawn attempt, which fails fast because the binary does not exist --
    /// the fake-agent-bin pattern, never a real agent.
    #[test]
    fn simple_flag_still_reaches_the_wrap_fallback_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let result = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        );
        // The dashboard is never reachable under `cargo test`'s own piped
        // stdio (`dash_eligible` requires a real terminal on both streams),
        // so this pins the wrap path specifically: it got far enough to
        // actually attempt the configured, nonexistent binary -- proof the
        // model splice and the dashboard branch above it did not divert or
        // corrupt the launch -- and failed there rather than anywhere
        // earlier (a disabled agent, the nesting guard, or a config error).
        let failure =
            result.expect_err("the configured binary does not exist, so the spawn must fail");
        let msg = failure.to_string();
        assert!(
            msg.contains("agent-bin") || msg.contains("Z:"),
            "expected the failure to name the configured (nonexistent) binary: {msg}"
        );
    }

    /// `chat_args(false).simple` is `false` above deliberately: the point is
    /// that even the default (non-`--simple`) path reaches `wrap` under
    /// non-terminal stdio. This companion pins that `--simple` explicitly
    /// set behaves the same way -- neither flag value changes which path a
    /// non-terminal `cargo test` run reaches.
    #[test]
    fn explicit_simple_also_reaches_the_wrap_fallback_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let mut simple_args = chat_args(false);
        simple_args.simple = true;

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let result = run_with(&simple_args, &mut out, &mut err_out, repo.path(), &|k| {
            env.get(k).cloned()
        });
        let failure =
            result.expect_err("the configured binary does not exist, so the spawn must fail");
        let msg = failure.to_string();
        assert!(
            msg.contains("agent-bin") || msg.contains("Z:"),
            "expected the failure to name the configured (nonexistent) binary: {msg}"
        );
    }
}
