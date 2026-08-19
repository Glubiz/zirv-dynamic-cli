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
use super::adapters::{self, AgentAdapter};
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

/// Whether `flags` already pins an explicit model choice -- any form
/// `adapters::classify_model_flag` recognises: `--model`/`-m` bare, the
/// `--model=value`/`-m=value` joined form, or the attached `-mvalue` short
/// form. The operator's own choice always wins, so `worker_launch_flags`
/// below never overrides it.
///
/// `-m` is codex's own verified short alias for `--model` (`CodexAdapter`'s
/// doc comments around `distiller_cmd`/`model_args` in `adapters/codex.rs`,
/// citing `codex exec --help`/top-level `codex --help`), so without it
/// `zirv ctx agent codex "<prompt>" -- -m opus` (or the attached
/// `-mopus`) with `worker.codex` configured got a conflicting `--model`
/// prepended ahead of the operator's own flag. Recognising `-m` is harmless
/// for claude, which has no such alias: treating it as pinning there only
/// ever skips a prepend that would otherwise have happened, never breaks a
/// launch. Shared with `adapters::last_model_flag` via `classify_model_flag`
/// so the two can never drift on what counts as a model flag.
fn flags_pin_model(flags: &[String]) -> bool {
    flags
        .iter()
        .any(|f| adapters::classify_model_flag(f).is_some())
}

/// The effective trailing flags a delegated headless spawn launches with.
///
/// Unchanged when the operator's own `flags` already pin `--model`
/// themselves -- the operator's own choice always wins. Otherwise
/// `worker.<name>`'s configured model, or `adapter`'s own hard default
/// (`AgentAdapter::default_worker_model`), is prepended ahead of `flags` via
/// `adapters::worker_model_args`, so a delegated headless worker stops
/// silently inheriting the operator's own (often far pricier) interactive
/// default model.
///
/// Mirrors `chat.rs`'s own `extra_with_model`: the model flags are prepended
/// as trailing extras rather than spliced into an already-built argv, which
/// is what keeps them from ever landing inside a launcher prefix.
///
/// This resolution runs only on the delegation spawn path -- `zirv ctx
/// agent`'s own headless fallback (this function's caller, `run_with`,
/// below) and the dashboard's own spawn-request pane variant
/// (`dash::mod::fulfill_spawn_request`). It deliberately never touches `zirv
/// ctx exec`/`zirv ctx loop`, whose trailing command the operator typed
/// verbatim, nor `chat`/`wrap`, whose model comes from the orchestrator seat
/// (`chat.model`) instead.
fn worker_launch_flags(
    cfg: &CtxConfig,
    name: &str,
    adapter: &dyn AgentAdapter,
    flags: &[String],
) -> Vec<String> {
    if flags_pin_model(flags) {
        return flags.to_vec();
    }
    let mut out = adapters::worker_model_args(cfg, name, adapter);
    out.extend_from_slice(flags);
    out
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

/// How much longer a *claimed* request is waited out past [`DASH_ACK_TIMEOUT`]
/// before the delegation is called a failure.
///
/// O3: a claim used to be reported as success on the spot -- exit 0, "the
/// dashboard accepted this request" -- for a task that might never run at all
/// (a dashboard that crashed between claiming and spawning leaves exactly that
/// state behind). A claim is good evidence the answer is merely slow, so it
/// buys real extra time; but when the extra time runs out too, the honest
/// answer is a failure, not a success.
const DASH_CLAIM_EXTENSION: Duration = Duration::from_secs(10);

/// The requester's own reading of one [`spawnreq::SpawnAck`].
///
/// O2: `ok: false` is two different answers. A policy refusal ends the
/// delegation -- falling back to headless would run a task this operator's own
/// configuration just refused. A `retryable` refusal is the channel saying it
/// could not carry the request, which the headless path was never subject to,
/// so the caller falls through to it with the reason printed. `None` here is
/// exactly that fall-through.
fn answer_for_ack<W: Write>(ack: spawnreq::SpawnAck, w: &mut W) -> Option<CtxResult<i32>> {
    if ack.ok {
        let short = ack.short.unwrap_or_default();
        return Some(
            writeln!(w, "spawned in dashboard as {short}")
                .map(|_| 0)
                .map_err(|e| e.into()),
        );
    }
    let reason = ack
        .reason
        .unwrap_or_else(|| "the dashboard refused this request".to_string());
    if ack.retryable {
        eprintln!("zirv ctx agent: {reason}; running headless");
        return None;
    }
    Some(writeln!(w, "{reason}").map(|_| 1).map_err(|e| e.into()))
}

/// O3: a request that was claimed but not acked within [`DASH_ACK_TIMEOUT`]
/// gets [`DASH_CLAIM_EXTENSION`] more, and then an honest answer either way.
///
/// Deliberately **no** headless fallback on the timeout, whatever the outcome:
/// the dashboard holds the claim and may still be spawning the pane, so a
/// second run of the same prompt is the one failure worse than a clear error.
/// A retryable refusal that arrives inside the extension is the one exception,
/// and the ack itself authorises it: the dashboard has answered, and its answer
/// is that it spawned nothing. `None` is that fall-through.
fn wait_out_a_claimed_request<W: Write>(
    dir: &Path,
    stem: &str,
    extension: Duration,
    w: &mut W,
) -> Option<CtxResult<i32>> {
    match spawnreq::wait_for_ack(dir, stem, extension) {
        Some(ack) => answer_for_ack(ack, w),
        None => Some(
            writeln!(
                w,
                "dashboard claimed the request but never confirmed; check zirv ctx status / the \
                 dashboard"
            )
            .map(|_| EXIT_DASH_UNCONFIRMED)
            .map_err(|e| e.into()),
        ),
    }
}

/// The exit code a delegation that could not be confirmed reports. A plain
/// `1`: the task may or may not be running, which for the caller is a failure
/// like any other -- the message on stdout is what says which kind.
const EXIT_DASH_UNCONFIRMED: i32 = 1;

/// When this process is itself a dashboard pane's own child
/// (`spawnreq::DASH_REQUESTS_ENV` set, and the directory it names still
/// exists -- the dashboard deletes it on quit, see `dash::on_quit`), asks
/// the dashboard to spawn `name` as a fresh pane instead of running headless
/// in this process's own subshell: writes a `spawnreq::SpawnRequest`
/// carrying `prompt` as data (never argv, the same discipline every other
/// delegation path in this codebase already holds), then waits up to
/// `DASH_ACK_TIMEOUT` for the matching ack.
///
/// `Some(result)` means the dashboard gave a definitive answer -- a pane was
/// spawned, the request was refused on policy grounds, or it was *claimed* and
/// then never confirmed even after `DASH_CLAIM_EXTENSION` (O3) -- and the
/// caller's own headless path must NOT run.
///
/// `None` means the caller falls through to today's headless behavior
/// unchanged, which covers: no dashboard to ask (env unset, or the directory
/// is gone -- both silent, byte-for-byte the pre-Task-11 behavior); options a
/// pane cannot honour (`--max-restarts`/`--timeout-secs`/`-- flags` other than
/// a lone `--model` pin, notice printed); a prompt that would be misread as a
/// flag (notice printed); a request that could not even be written (notice
/// printed); an unclaimed ack
/// timeout (notice printed, since that is a live channel that simply did not
/// respond); and a `retryable` refusal, where the dashboard has answered that
/// it spawned nothing for a reason that says nothing about whether the task
/// may run (O2).
fn try_join_dashboard<W: Write>(
    args: &AgentArgs,
    prompt: &str,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    ack_timeout: Duration,
    claim_extension: Duration,
) -> Option<CtxResult<i32>> {
    let dir = env(spawnreq::DASH_REQUESTS_ENV).map(std::path::PathBuf::from)?;
    if !dir.is_dir() {
        return None;
    }
    // A pane is not a supervised headless run: the restart budget, the
    // wall-clock limit and the trailing `-- flags` all belong to
    // `exec::run_with`, and a `SpawnRequest` carries none of them. Silently
    // dropping an operator's `--timeout-secs` would be worse than not using
    // the dashboard at all, so this falls back to the path that honours them.
    // `--quiet` is deliberately still allowed: it only shapes the
    // announcement channel of a run that is not happening in this process
    // anyway.
    // A model pin is the one trailing flag a pane *can* honour -- it travels
    // in the request and the pane builds it into its own argv -- and the
    // harness layer now teaches orchestrators to write one on every
    // delegation, so declining the pane for it would cost a dashboard session
    // a visible pane per delegated task. Anything else in `flags` still
    // declines: honouring some of what the operator typed and dropping the
    // rest would be worse than not using the dashboard at all.
    let pinned_model = super::adapters::model_only_flags(&args.flags);
    if args.max_restarts.is_some()
        || args.timeout_secs.is_some()
        || (!args.flags.is_empty() && pinned_model.is_none())
    {
        eprintln!(
            "zirv ctx agent: dashboard panes don't support --max-restarts/--timeout-secs/-- flags \
             other than a --model pin; running headless"
        );
        return None;
    }
    // Defense in depth for the same rule `dash::fulfill_spawn_request`
    // enforces at the authority side: the request's prompt is encoded
    // positionally into the pane's argv, so a prompt shaped like a flag
    // would arrive at the real harness child as one. The headless path this
    // falls back to is safe by construction -- there the prompt travels as
    // the `-p <value>` data it is.
    if super::dash::argv_unsafe_prompt(prompt) {
        eprintln!(
            "zirv ctx agent: a prompt beginning with '-' cannot be spawned as a dashboard pane; \
             running headless"
        );
        return None;
    }
    let requested_by = env(super::adapters::SESSION_ENV)
        .map(|s| super::sessions::short_id(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let req = spawnreq::SpawnRequest {
        agent: args.name.clone(),
        prompt: prompt.to_string(),
        cwd: repo.to_path_buf(),
        requested_by,
        model: pinned_model.map(str::to_string),
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
    match spawnreq::wait_for_ack(&dir, &stem, ack_timeout) {
        Some(ack) => answer_for_ack(ack, w),
        // F10: `take_requests` takes the request the moment the dashboard
        // picks it up, so a timeout here is ambiguous -- nobody was listening,
        // or somebody took it and is still spawning. Both ends acting on that
        // ambiguity is how one `zirv ctx agent` became two live sessions
        // working the same prompt.
        //
        // F2: the **removal is the decision**, not a check followed by one.
        // This used to ask `is_claimed` and then remove the request, which is
        // check-then-act against a dashboard doing exactly one thing: renaming
        // this very file into its claim (`spawnreq::take_requests`). A claim
        // landing between the check and the remove sent this side headless
        // while the dashboard was already spawning the same prompt. Removing
        // first collapses the two into one atomic operation that only one side
        // can win:
        //
        // * `Ok` -- this process took its own request back off disk before
        //   anybody claimed it, and a dashboard's later rename now finds
        //   nothing, so the headless fallback cannot double-run it;
        // * `Err`, for any reason -- the file is no longer where this process
        //   left it (or cannot be removed), and the thing that moves it is a
        //   claim. Waiting the claim out is the safe reading: the worst case
        //   is an honest "claimed but never confirmed" failure for a request
        //   whose directory vanished with a quitting dashboard, against a
        //   double-run of the operator's task if this guessed the other way.
        None => {
            if std::fs::remove_file(&path).is_ok() {
                eprintln!("zirv ctx agent: dashboard did not answer; running headless");
                return None;
            }
            wait_out_a_claimed_request(&dir, &stem, claim_extension, w)
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

    if let Some(result) = try_join_dashboard(
        args,
        &prompt,
        w,
        repo,
        env,
        DASH_ACK_TIMEOUT,
        DASH_CLAIM_EXTENSION,
    ) {
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

    // Resolved here, ahead of `exec::run_with`'s own (identical) selection
    // further down, purely to compute the default worker model this spawn
    // launches with -- see `worker_launch_flags`. `&[]` for the command: it
    // only matters to `select` when `name` is `None`, and this call always
    // passes the delegation target explicitly.
    let adapter = adapters::select(Some(&args.name), &[], &cfg)?;
    let command = worker_launch_flags(&cfg, &args.name, adapter.as_ref(), &args.flags);

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
        command,
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

    // `worker_launch_flags`/`flags_pin_model`: pure, so these are testable
    // against a plain adapter without spawning anything.

    #[test]
    fn flag_passthrough_wins_over_the_configured_or_default_worker_model() {
        let adapter = super::super::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let flags = vec!["--model".to_string(), "opus".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &adapter, &flags),
            flags,
            "the operator's own --model must reach argv unchanged"
        );

        let joined = vec!["--model=opus".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &adapter, &joined),
            joined,
            "the --model=value joined form must also be recognised as already pinned"
        );
    }

    /// FIX 2: codex's own `-m` short alias must pin exactly like `--model`,
    /// or a configured `worker.codex` gets a conflicting `--model` prepended
    /// ahead of an operator's own `-m <value>`.
    #[test]
    fn codexs_short_m_alias_also_pins_the_model_for_worker_launches() {
        let adapter = super::super::adapters::codex::CodexAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: None,
                codex: Some("gpt-5.6-terra".to_string()),
            },
            ..CtxConfig::default()
        };

        let bare = vec!["-m".to_string(), "opus".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &bare),
            bare,
            "the operator's own -m must reach argv unchanged, not gain a conflicting --model"
        );

        let joined = vec!["-m=opus".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &joined),
            joined,
            "the -m=value joined form must also be recognised as already pinned"
        );

        // FIX 2 (round 2): the attached short form, `-mopus` with no
        // separator at all, must be recognised too, or `zirv ctx agent
        // codex "p" -- -mopus` with `worker.codex` configured gets a
        // conflicting `--model` prepended ahead of the operator's own flag.
        let attached = vec!["-mopus".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &attached),
            attached,
            "the attached -mvalue short form must also be recognised as already pinned"
        );
    }

    /// A long flag that merely starts with `-m` once its leading `-` is
    /// peeled (`--model-foo`) must not be misread as the attached short
    /// form: `worker.codex`'s configured model still gets prepended ahead
    /// of it, exactly as for any other unrelated flag.
    #[test]
    fn a_long_flag_starting_with_m_does_not_false_positive_as_pinning() {
        let adapter = super::super::adapters::codex::CodexAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: None,
                codex: Some("gpt-5.6-terra".to_string()),
            },
            ..CtxConfig::default()
        };
        let flags = vec!["--model-foo".to_string(), "opus".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &flags),
            vec![
                "--model".to_string(),
                "gpt-5.6-terra".to_string(),
                "--model-foo".to_string(),
                "opus".to_string(),
            ],
            "an unrelated flag must not suppress the configured-model prepend"
        );
    }

    #[test]
    fn a_configured_worker_model_is_prepended_ahead_of_the_operators_own_flags() {
        let adapter = super::super::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: Some("opus".to_string()),
                codex: None,
            },
            ..CtxConfig::default()
        };
        let flags = vec!["--allowedTools".to_string(), "Bash".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &adapter, &flags),
            vec![
                "--model".to_string(),
                "opus".to_string(),
                "--allowedTools".to_string(),
                "Bash".to_string(),
            ]
        );
    }

    #[test]
    fn claude_gets_the_sonnet_default_when_nothing_is_configured_or_passed() {
        let adapter = super::super::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &adapter, &[]),
            vec!["--model".to_string(), "sonnet".to_string()]
        );
    }

    #[test]
    fn codex_gets_no_model_flag_when_nothing_is_configured_or_passed() {
        let adapter = super::super::adapters::codex::CodexAdapter::new(None);
        let cfg = CtxConfig::default();
        assert!(
            worker_launch_flags(&cfg, "codex", &adapter, &[]).is_empty(),
            "codex has no adapter-owned default, so its own config default applies untouched"
        );
    }

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

    /// Codex is a supported delegation target now that its own `ready()`
    /// only checks that its program resolves (`CodexAdapter::ready`, mirrors
    /// `ClaudeAdapter::ready`), so a disabled-by-settings codex is refused
    /// for the gate reason, not "not implemented yet" -- the same shape as
    /// `the_delegation_verb_refuses_an_agent_the_settings_file_disabled`
    /// above, with the disabled name swapped.
    #[test]
    fn the_delegation_verb_refuses_an_agent_the_settings_file_disabled_for_codex() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let env = base_env(&tmp.path().join("state"));
        let args = args_for("codex", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// H4: distinct from the gate-disabled tests above -- an *enabled* named
    /// agent whose own `ready()` fails must still have that error propagate
    /// all the way out of `agent::run_with`. Not reachable with `ZIRV_CTX_
    /// AGENT_BIN` (an explicit path is never a `ready()` failure -- only a
    /// bare name resolved via `PATH` to an unlaunchable extension is, see
    /// `adapters::mod::tests::an_unlaunchable_program_on_path_is_named_
    /// rather_than_left_to_error_193`), so this deliberately omits `ZIRV_
    /// CTX_AGENT_BIN` and rigs `PATH`/`PATHEXT` instead, the same seam
    /// `readiness_note_and_the_fallback_skip_both_stay_covered_when_an_
    /// adapter_is_genuinely_unready` uses.
    #[cfg(windows)]
    #[test]
    fn the_delegation_verb_propagates_a_genuine_ready_failure() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();
        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("claude's own ready() must fail under this rig");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(
            !msg.to_lowercase().contains("disabled"),
            "a ready() failure is not a gate refusal, must not say 'disabled': {msg}"
        );
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
        // Match the layer's version header, not the bare name: the adapter
        // layer legitimately references "the zirv meta-harness layer" by name,
        // and only the header marks the layer itself being present.
        assert!(
            !argv.contains("zirv meta-harness (v"),
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

    /// Polls `dir` for a `req-*.json` file and hands its file stem to
    /// `respond` (which writes whatever answer the test wants), returning the
    /// request's own raw contents -- the same "responder" shape every
    /// dashboard-join test below needs, since `write_request` mints a random
    /// uuid filename the test cannot know in advance. Never touches a real
    /// agent: this only ever races against `try_join_dashboard`'s own polling
    /// loop, both confined to a tempdir.
    fn intercept_next_request(
        dir: std::path::PathBuf,
        respond: impl Fn(&std::path::Path, &str),
    ) -> String {
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
                        respond(&dir, &stem);
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

    fn respond_to_next_request(dir: std::path::PathBuf, ack_body: &'static str) -> String {
        intercept_next_request(dir, move |dir, stem| {
            std::fs::write(dir.join(format!("ack-{stem}.json")), ack_body).expect("write ack");
        })
    }

    /// The `AgentArgs` shape a dashboard join actually accepts: no restart
    /// budget, no wall-clock limit, no trailing flags -- a pane carries none
    /// of those, so `try_join_dashboard` deliberately falls back to headless
    /// when any is set (F9).
    fn joinable_args(name: &str, prompt: &str) -> AgentArgs {
        let mut args = args_for(name, prompt);
        args.max_restarts = None;
        args.timeout_secs = None;
        args
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

        let args = joinable_args("claude", "a specific delegated task");
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

    /// A lone `--model` pin is the one trailing flag a pane can honour, so it
    /// joins the dashboard instead of declining to the headless path -- the
    /// harness layer now teaches orchestrators to write one on every
    /// delegation, and declining would cost a dashboard session its pane every
    /// time. The pinned model travels in the request, for the fulfilment side
    /// to build into the pane's own argv.
    #[test]
    fn a_lone_model_pin_still_joins_the_dashboard_and_travels_in_the_request() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "go");
        args.flags = vec!["--model".to_string(), "haiku".to_string()];
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(
            req.model.as_deref(),
            Some("haiku"),
            "the pinned model reaches the fulfilment side: {request_body}"
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

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        responder.join().expect("responder thread");

        assert_eq!(code, 1);
        let output = String::from_utf8_lossy(&out);
        assert!(output.contains("disabled"), "got {output}");
    }

    /// A live requests directory, and the `AgentArgs`/env pair that reaches
    /// it: the shape every `try_join_dashboard`-level test below shares.
    fn live_dashboard_dir(root: &Path) -> (PathBuf, HashMap<String, String>) {
        let requests_dir = root.join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");
        let mut env = base_env(&root.join("state"));
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            requests_dir.display().to_string(),
        );
        (requests_dir, env)
    }

    /// F2 (defense in depth): the request's prompt is encoded positionally
    /// into the pane's argv, so a prompt shaped like a flag would reach the
    /// real harness child as one. The dashboard refuses such a request at the
    /// authority side; this end refuses to even write it, and falls back to
    /// the headless path, where the prompt travels as the `-p <value>` data
    /// it is.
    #[test]
    fn a_prompt_that_begins_with_a_dash_is_never_written_as_a_spawn_request() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let args = joinable_args("claude", "--dangerously-skip-permissions");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
        );

        assert!(
            joined.is_none(),
            "must fall through to the safe headless path"
        );
        assert!(out.is_empty(), "nothing is reported as spawned");
        let written: Vec<_> = std::fs::read_dir(&requests_dir)
            .expect("read requests dir")
            .flatten()
            .collect();
        assert!(
            written.is_empty(),
            "no request may be written at all: {written:?}"
        );
    }

    /// F9: `--max-restarts`, `--timeout-secs` and trailing `-- flags` are all
    /// honoured by `exec::run_with` and carried by nothing in a
    /// `SpawnRequest`. Silently dropping them would be worse than not using
    /// the dashboard, so the join declines and the headless path runs.
    /// `--quiet` stays allowed: it only shapes an announcement channel.
    #[test]
    fn options_a_pane_cannot_honour_decline_the_dashboard_join() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        for mutate in [
            (|a: &mut AgentArgs| a.max_restarts = Some(2)) as fn(&mut AgentArgs),
            |a: &mut AgentArgs| a.timeout_secs = Some(90),
            |a: &mut AgentArgs| a.flags = vec!["--verbose".to_string()],
            // A model pin plus anything else is still a decline: honouring
            // half of what the operator typed is worse than not using the
            // dashboard at all.
            |a: &mut AgentArgs| {
                a.flags = vec![
                    "--model".to_string(),
                    "haiku".to_string(),
                    "--verbose".to_string(),
                ]
            },
        ] {
            let mut args = joinable_args("claude", "go");
            mutate(&mut args);
            let mut out = Vec::new();
            let joined = try_join_dashboard(
                &args,
                &args.prompt,
                &mut out,
                tmp.path(),
                &|k| env.get(k).cloned(),
                Duration::from_millis(200),
                Duration::from_millis(200),
            );
            assert!(joined.is_none(), "must fall back to the headless path");
            assert!(
                std::fs::read_dir(&requests_dir)
                    .expect("read requests dir")
                    .flatten()
                    .next()
                    .is_none(),
                "and must not have written a request either"
            );
        }

        // The control: the same call with none of them set does reach the
        // channel (it writes a request, then times out unanswered).
        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        assert!(joined.is_none(), "an unanswered request still falls back");
    }

    /// R5: an unanswered, unclaimed request is taken back off disk before the
    /// headless fallback starts. `take_requests` runs on the dashboard's own
    /// tick, so a request left behind could still be picked up afterwards --
    /// and then the headless run and that pane would both work the same
    /// prompt, which is exactly the double-run the claim protocol exists to
    /// prevent.
    #[test]
    fn an_unclaimed_timeout_takes_its_own_request_back_off_disk() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        assert!(joined.is_none(), "nobody answered, so this runs headless");

        let leftover: Vec<PathBuf> = std::fs::read_dir(&requests_dir)
            .expect("read requests dir")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(
            leftover.is_empty(),
            "the request must not be left for a later tick to pick up: {leftover:?}"
        );
    }

    /// F2: the request's removal is what decides which way an unanswered
    /// timeout goes, so a request that is no longer where this process left it
    /// is waited out as claimed -- even with no `claim-*` file to be seen. The
    /// old `is_claimed` pre-check read the claim a moment before acting on it,
    /// and a claim landing inside that window sent this side headless while the
    /// dashboard was already spawning the same prompt.
    #[test]
    fn a_request_that_vanished_without_a_claim_file_is_waited_out_not_double_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        // Moves the request aside to a name that is neither a request nor a
        // claim, and never acks: exactly what `is_claimed` would have read as
        // "nobody has this", and what `remove_file` reads as "not mine any
        // more".
        let taker = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let found = std::fs::read_dir(&dir)
                        .ok()
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .find(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("req-") && n.ends_with(".json"))
                        });
                    if let Some(path) = found {
                        std::fs::rename(&path, dir.join("taken-elsewhere")).expect("rename");
                        return;
                    }
                    if std::time::Instant::now() > deadline {
                        panic!("no spawn request appeared within the deadline");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(300),
            Duration::from_millis(300),
        );
        taker.join().expect("taker thread");

        let code = joined
            .expect("a request this process could not take back must not fall back to headless")
            .expect("writes its line");
        assert_eq!(code, 1, "an unconfirmed spawn is a failure, not a success");
        assert!(
            String::from_utf8_lossy(&out).contains("claimed the request but never confirmed"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Claims the next request exactly the way the dashboard's own tick does
    /// -- `take_requests`, which claims by renaming (O6) -- and then runs
    /// `respond` with its stem. Returns the request's raw contents.
    fn claim_next_request(dir: std::path::PathBuf, respond: impl Fn(&std::path::Path, &str)) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let taken = crate::commands::ctx::dash::spawnreq::take_requests(&dir);
            if let Some((path, _)) = taken.first() {
                let stem = crate::commands::ctx::dash::spawnreq::request_stem(path).expect("stem");
                respond(&dir, &stem);
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("no spawn request appeared within the deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// O3: a claim buys the dashboard extra time, not a free pass. When the
    /// extension runs out with no ack, the delegation fails honestly -- and
    /// still does not double-run headless, because the dashboard holds the
    /// claim and may yet spawn the pane.
    #[test]
    fn a_claimed_but_unconfirmed_request_fails_instead_of_reporting_success() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        // Takes (and so claims) the request, then deliberately never acks.
        let claimer = std::thread::spawn({
            let dir = requests_dir.clone();
            move || claim_next_request(dir, |_dir, _stem| {})
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(300),
            Duration::from_millis(300),
        );
        claimer.join().expect("claimer thread");

        let code = joined
            .expect("a claimed request must not fall back to headless")
            .expect("writes its line");
        assert_eq!(code, 1, "an unconfirmed spawn is a failure, not a success");
        let printed = String::from_utf8_lossy(&out);
        assert!(
            printed.contains("claimed the request but never confirmed")
                && printed.contains("zirv ctx status"),
            "got {printed}"
        );
    }

    /// O3, the other half: an ack that arrives late -- after the first
    /// timeout, inside the extension the claim bought -- is a success.
    #[test]
    fn a_late_ack_inside_the_claim_extension_still_succeeds() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                claim_next_request(dir, |dir, stem| {
                    // Past the requester's own first timeout, well inside the
                    // extension: a slow spawn, not a dead dashboard.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    std::fs::write(
                        dir.join(format!("ack-{stem}.json")),
                        r#"{"ok":true,"short":"bbbb2222","reason":null}"#,
                    )
                    .expect("write ack");
                })
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_secs(5),
        );
        responder.join().expect("responder thread");

        let code = joined
            .expect("claimed, then acked")
            .expect("writes its line");
        assert_eq!(code, 0);
        assert!(
            String::from_utf8_lossy(&out).contains("bbbb2222"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// O2: a refusal the dashboard itself marks `retryable` -- a repo
    /// mismatch, a pty that would not open -- says nothing about whether the
    /// task may run, and the headless path was never subject to it. The join
    /// declines rather than killing the delegation outright.
    #[test]
    fn a_retryable_refusal_falls_back_to_the_headless_path() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"this dashboard only spawns panes in its own repo","retryable":true}"#,
                )
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_millis(200),
        );
        responder.join().expect("responder thread");

        assert!(
            joined.is_none(),
            "a channel-level failure must not suppress the headless path"
        );
        assert!(out.is_empty(), "nothing is reported as spawned");
    }

    /// O2, the other class: a policy refusal ends the delegation. Falling back
    /// to headless would run a task this operator's own configuration just
    /// refused.
    #[test]
    fn a_policy_refusal_ends_the_delegation() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"claude is disabled by .zirv/.settings.toml","retryable":false}"#,
                )
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_millis(200),
        );
        responder.join().expect("responder thread");

        let code = joined.expect("a refusal is definitive").expect("writes");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8_lossy(&out).contains("disabled"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// The other half of F10: nothing claimed it, so the timeout still falls
    /// back to headless exactly as before.
    #[test]
    fn an_unclaimed_timeout_still_falls_back_to_headless() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (_requests_dir, env) = live_dashboard_dir(tmp.path());

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        assert!(joined.is_none());
        assert!(out.is_empty());
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
            // Pacing off: with the claude exemption gone from
            // `has_no_usage_source`, this empty state dir would otherwise
            // make the gate write its one no-source skip line into `out`,
            // and this test's whole proof is that `out` stayed empty.
            ("ZIRV_CTX_PACE".to_string(), "false".to_string()),
        ]
        .into();
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            tmp.path()
                .join("no-such-requests-dir")
                .display()
                .to_string(),
        );

        let args = joinable_args("claude", "go");
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
