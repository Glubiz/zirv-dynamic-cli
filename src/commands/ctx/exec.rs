//! Supervises one headless run, restarting it on rot with a distilled
//! handoff. Mail (`super::mail`) is delivered into the composed system
//! prompt exactly once, at the very first launch computed in `run_with`: a
//! rot/timeout restart or a usage-limit park reuses that same launch's
//! `prompt_args` (the argv already carrying the composed text, whichever
//! mechanism delivered it), it does not recompute the composed prompt or
//! re-list mail. A message that arrives after the run has started is
//! therefore not retroactively injected into it -- the next `zirv ctx exec`
//! invocation (or a `zirv ctx loop` cycle, which re-lists mail every cycle
//! by design) picks it up instead.
//!
//! N4's `zirv ctx nudge` is the one deliberate exception: a nudge relaunch
//! recomposes the prompt and re-lists mail (scoped to the session that was
//! nudged) precisely because that recompute is the whole point -- see the
//! `nudged` branch in the main loop below.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{NormalizedEvent, SessionId, SessionRef, TranscriptUsage};
use super::pace;
use super::rot::Verdict;
use super::signal::{self, TurnSignal};
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick};
use super::{CtxResult, adapters, agent, handoff, log, score};

/// The restart budget is spent and the session is still rotting. Callers apply
/// their own policy from here.
pub const EXIT_ROT_EXHAUSTED: i32 = 75;
/// Wall-clock timeout with no restarts left.
pub const EXIT_TIMEOUT: i32 = 76;
/// A `--budget-tokens`/`--max-tool-calls` ceiling was reached (issue #155,
/// Phase 5(d)). Unlike the two codes above, this is never followed by a
/// restart: a budget checkpoints the run once and stops it for good.
pub const EXIT_BUDGET_EXHAUSTED: i32 = 77;
/// Issue #227: the restart budget is spent while every restart kept hitting
/// a transient provider capacity/overload error (`pace::CAPACITY_PATTERNS`).
/// Distinct from `EXIT_ROT_EXHAUSTED`: the session itself never rotted, the
/// provider just could not serve it -- retried within budget with a short
/// backoff (`capacity_backoff_secs`) before landing here.
pub const EXIT_CAPACITY_EXHAUSTED: i32 = 78;
/// Issue #227: the provider reported the account itself is out of usable
/// credits/quota (`pace::ACCOUNT_EXHAUSTED_PATTERNS`) -- a hard, non-
/// retryable condition. Never follows a restart: burning the budget cannot
/// fix a billing problem, so this fires on the very first occurrence.
pub const EXIT_ACCOUNT_EXHAUSTED: i32 = 79;

/// The supervisor reports its own outcomes through the same `i32` an agent's
/// exit code arrives on, so "exited with code 75" reads as something the
/// agent did rather than as zirv giving up. Shared by `zirv ctx agent`
/// (agent.rs) and script `agent:` steps (agent_command.rs), which both
/// delegate to this supervisor and want the same wording for the same three
/// outcomes.
pub fn describe_exit(code: i32) -> String {
    match code {
        EXIT_ROT_EXHAUSTED => "the session kept rotting and the restart budget ran out".to_string(),
        EXIT_TIMEOUT => "the supervised run hit its wall-clock timeout".to_string(),
        EXIT_BUDGET_EXHAUSTED => {
            "the token/tool-call budget was spent and the run was stopped".to_string()
        }
        EXIT_CAPACITY_EXHAUSTED => {
            "the provider kept reporting capacity/overload errors and the restart budget ran out"
                .to_string()
        }
        EXIT_ACCOUNT_EXHAUSTED => {
            "the provider account is out of usable credits/quota; restarting cannot fix a \
             billing problem"
                .to_string()
        }
        other => format!("exited with code {other}"),
    }
}

/// Issue #227: backoff before retrying a headless worker that failed on a
/// transient provider capacity error -- 15s/30s/60s, capped at 60s for a
/// fourth or later attempt. `attempt` is 1-based: the restart about to be
/// made (`restarts` after it is incremented). Pure, so the schedule is
/// table-tested without a real clock.
fn capacity_backoff_secs(attempt: u32) -> u64 {
    match attempt {
        0 => 0,
        1 => 15,
        2 => 30,
        _ => 60,
    }
}

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id of the supervised run, used to locate its transcript.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Transcript path, when the agent writes somewhere the adapter cannot derive.
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    /// Prompt to reuse on restart. Extracted from the command when omitted.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// Token ceiling for this run (issue #155, Phase 5(d)). Checkpoints at
    /// `agent::BUDGET_SOFT_FRACTION` of the ceiling and stops -- never
    /// restarted, and never a signal to change models. `None` is unbounded.
    #[arg(long)]
    pub budget_tokens: Option<u64>,
    /// Tool-call ceiling for this run, independent of `budget_tokens`.
    #[arg(long)]
    pub max_tool_calls: Option<u32>,
    /// The headless agent command, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
}

/// One vendor-backed portion of a logical supervised execution. A cross-harness
/// fallback produces more than one segment; a normal run produces exactly one.
#[derive(Debug, Clone)]
pub struct ExecutionSegment {
    pub session: String,
    pub agent: String,
    pub model: Option<String>,
    pub usage: TranscriptUsage,
    pub wall_ms: u64,
}

/// Accounting returned to callers that need to attribute one logical
/// delegation across cross-harness continuations.
#[derive(Debug, Clone, Default)]
pub struct ExecutionReport {
    pub segments: Vec<ExecutionSegment>,
}

/// Flags that pin a launch to a conversation that already exists. A restart is
/// a deliberate escape from the session that rotted, so inheriting any of them
/// would march the fresh child straight back into it and burn the whole
/// restart budget re-entering rot. The first two carry a value; the rest are
/// bare.
///
/// These are claude's own verified flags (`--session-id`/`--resume`,
/// `-c`/`--continue`/`--fork-session`) -- see `ClaudeAdapter::resume_args`/
/// `session_pin_args`. Every matcher below only ever consults this list for
/// an adapter that actually recognises it (issue #143): codex's own `-c` is
/// `-c, --config <key>=<value>`, an unrelated WITH-VALUE flag that happens to
/// share claude's bare resume spelling. Matching it here regardless of
/// adapter used to strip the bare `-c` token as claude's valueless resume
/// flag while leaving codex's own value (e.g. `approval_policy=never`)
/// behind, orphaned on argv -- which real codex-cli then rejects outright.
pub(crate) const RESUME_FLAGS_WITH_VALUE: [&str; 2] = ["--session-id", "--resume"];
pub(crate) const RESUME_FLAGS_BARE: [&str; 3] = ["-c", "--continue", "--fork-session"];

/// Whether `adapter_name` is the one adapter [`RESUME_FLAGS_WITH_VALUE`]/
/// [`RESUME_FLAGS_BARE`] actually describe. Shared by every matcher below so
/// they cannot drift on which adapter this list applies to.
fn adapter_has_resume_flags(adapter_name: &str) -> bool {
    adapter_name.eq_ignore_ascii_case("claude")
}

/// Pure: whether `args` already pins this launch to a conversation that
/// exists -- any of [`RESUME_FLAGS_WITH_VALUE`]/[`RESUME_FLAGS_BARE`], in
/// either the two-token or the `--flag=value` spelling. Always `false` for an
/// adapter that does not recognise these flags at all (see
/// `adapter_has_resume_flags`) -- codex mints its own session id and has no
/// verified pin flag, so nothing in its own argv could mean this.
///
/// D3: a caller that adds a pin of its own (`chat::dash_orchestrator_pane`,
/// via `AgentAdapter::session_pin_args`) has to ask this first. `zirv chat --
/// --resume <id>` produced `--resume <id> --session-id <fresh-uuid>`, which
/// the harness refuses outright: two contradictory conversation ids in one
/// launch. Inside a dashboard the resulting pane died immediately and its
/// corpse was reaped, so the operator saw the session vanish with no error at
/// all.
pub(crate) fn pins_an_existing_conversation(args: &[String], adapter_name: &str) -> bool {
    if !adapter_has_resume_flags(adapter_name) {
        return false;
    }
    args.iter().any(|arg| {
        RESUME_FLAGS_WITH_VALUE.contains(&arg.as_str())
            || RESUME_FLAGS_BARE.contains(&arg.as_str())
            || is_joined_form(arg, &RESUME_FLAGS_WITH_VALUE)
            || is_joined_form(arg, &RESUME_FLAGS_BARE)
    })
}

/// True for `--resume=abc` when `--resume` is in `flags`: the CLIs accept both
/// spellings, so stripping only the two-token form leaves the other behind.
fn is_joined_form(arg: &str, flags: &[&str]) -> bool {
    arg.split_once('=')
        .is_some_and(|(name, _)| flags.contains(&name))
}

/// Locates the token that carries the prompt in a headless agent command, and
/// the prompt itself when that token is followed by one.
///
/// `known` is the prompt zirv already holds for this run (`--prompt`, or an
/// agent step's own text). Given it, the value is recognised by equality
/// instead of by shape, which is the only way to tell a prompt that happens to
/// begin with `-` -- a markdown bullet list, say -- from a genuine second
/// flag. Without it the shape heuristic still applies, because guessing wrong
/// about a restart's prompt is worse than not restarting.
///
/// The value is `None` for a bare flag: `-p` with another flag after it (or
/// nothing at all) means the prompt arrives on stdin. The flag itself still
/// has to be stripped from a restart argv, but the token after it must not be.
fn locate_prompt(
    command: &[String],
    prefix: usize,
    known: Option<&str>,
) -> Option<(usize, Option<String>)> {
    for (index, arg) in command.iter().enumerate().skip(prefix) {
        let is_prompt_flag = arg == "-p" || arg == "--print";
        let is_subcommand = arg == "exec";
        if !is_prompt_flag && !is_subcommand {
            continue;
        }
        let Some(next) = command.get(index + 1) else {
            return Some((index, None));
        };
        if Some(next.as_str()) == known {
            return Some((index, Some(next.clone())));
        }
        if next.starts_with('-') {
            return Some((index, None));
        }
        return Some((index, Some(next.clone())));
    }
    None
}

/// Finds the prompt in a headless agent command. Returns `None` rather than
/// guessing: a restart with the wrong prompt is worse than no restart.
pub fn extract_prompt(command: &[String]) -> Option<String> {
    locate_prompt(command, 1, None).and_then(|(_, prompt)| prompt)
}

/// M8: the user's own flags from the original `--` command, with only what
/// zirv itself re-supplies on every restart removed. Everything else the
/// operator passed -- `--model`, `--allowedTools`, anything at all -- must
/// reach a restarted child exactly as it reached the first one; silently
/// dropping it here was the asymmetry M8 fixed (zirv's own added flags, e.g.
/// the system prompt, always survived a restart; the operator's own did not).
///
/// Three kinds of token are dropped. The prompt, because every relaunch
/// regenerates it to carry a handoff. Anything pinning the launch to an
/// existing conversation, because the relaunch is escaping one -- only for
/// `adapter_name` that actually recognises those flags at all (issue #143,
/// `adapter_has_resume_flags`); every other adapter's own flags pass through
/// untouched, including one that happens to share a bare `-c` spelling for a
/// completely different, value-carrying purpose (codex's `-c, --config
/// <key>=<value>`). And the leading tokens of the program invocation itself
/// -- `prefix` of them from the adapter, plus any further positional before
/// the first flag, which is how `npx claude ...` and a positional prompt both
/// look -- because `headless_cmd` rebuilds the invocation and re-appending
/// them would leave a stray argument the agent reads as a second prompt.
pub fn extra_launch_flags(
    command: &[String],
    prefix: usize,
    known_prompt: Option<&str>,
    adapter_name: &str,
) -> Vec<String> {
    let recognizes_resume_flags = adapter_has_resume_flags(adapter_name);
    let located = locate_prompt(command, prefix, known_prompt);
    let prompt_at = located.as_ref().map(|(index, _)| *index);
    let prompt_takes_value = located.is_some_and(|(_, value)| value.is_some());

    let mut out = Vec::with_capacity(command.len());
    let mut skip_next = false;
    let mut in_prefix = true;
    for (index, arg) in command.iter().enumerate().skip(prefix) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if Some(index) == prompt_at {
            skip_next = prompt_takes_value;
            in_prefix = false;
            continue;
        }
        if in_prefix && !arg.starts_with('-') {
            continue;
        }
        in_prefix = false;

        if recognizes_resume_flags {
            if is_joined_form(arg, &RESUME_FLAGS_WITH_VALUE)
                || is_joined_form(arg, &RESUME_FLAGS_BARE)
            {
                continue;
            }
            if RESUME_FLAGS_WITH_VALUE.contains(&arg.as_str()) {
                // A bare `--resume` with a flag after it takes no value, so
                // the next token belongs to the operator and has to survive.
                skip_next = command
                    .get(index + 1)
                    .is_some_and(|next| !next.starts_with('-'));
                continue;
            }
            if RESUME_FLAGS_BARE.contains(&arg.as_str()) {
                continue;
            }
        }
        out.push(arg.clone());
    }
    out
}

/// C3: the consecutive-nudge budget after one supervised run.
///
/// `[supervise] max_nudges` has always been documented as bounding
/// *consecutive* nudge restarts -- "a session cannot be interrupted
/// indefinitely" -- but the counter was only ever incremented, so it actually
/// bounded nudges for the whole lifetime of the run. A session nudged three
/// times across an hour, doing real work between each, could never be nudged
/// again. Progress (a turn boundary reported by the session itself) ends the
/// consecutive run and restores the budget.
pub fn nudges_after(used: u32, progressed: bool) -> u32 {
    if progressed { 0 } else { used }
}

/// Compaction of a headless run is pointless (there is no TUI to type into), so
/// only a restart verdict acts, and only for the session this supervisor owns:
/// the socket is named after eight hex characters of a session id, so a stale
/// hook can reach it and must not be able to kill a healthy child.
pub fn should_stop_for_signal(signal: &TurnSignal, session: &str) -> bool {
    signal.session_id == session && signal.verdict == Verdict::Restart
}

fn build_command(command: &[String], repo: &Path) -> CtxResult<Command> {
    let (program, rest) = command
        .split_first()
        .ok_or("no command to supervise; pass it after --")?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(repo);
    Ok(cmd)
}

/// Whether this run's own headless launch reparses its downstream argv on a
/// Windows launcher -- `cmd.exe /c <shim>` (an npm-installed `.cmd`) or
/// `powershell -NoProfile -File <script>` (a `.ps1`) -- so the prompt has to
/// go on stdin instead of argv (FIX B). `adapter.launches_through_cmd_shim()`
/// only recognises the `cmd.exe` form; probing the real launcher prefix this
/// run's headless spawn will use (`headless_cmd("", ...)`, no prompt token
/// yet) and asking `adapters::launch_reparses_through_shim` covers both,
/// matching the M1 fix `dash/mod.rs`'s `task_prompt_fallback_is_safe` made
/// for the pty path. Split out for the same reason that one was: testable
/// without spawning anything.
fn prompt_delivery_via_stdin(adapter: &dyn adapters::AgentAdapter, session: &SessionId) -> bool {
    let probe = adapters::flatten_command(adapter.headless_cmd("", session, &[]));
    adapters::launch_reparses_through_shim(&probe)
}

/// Issue #220: whether `build_headless` should route THIS launch's prompt to
/// stdin rather than argv. `shim` is [`prompt_delivery_via_stdin`]'s own
/// answer -- a Windows `cmd.exe`/`powershell -File` reparse, which forces
/// stdin regardless of size, exactly as before this issue. `argv_total_len`
/// is the second, independent reason: `adapter.headless_cmd` puts the
/// prompt on argv verbatim on every platform, and the WHOLE resulting
/// command line -- program, prompt, and every other argument riding beside
/// it, not the prompt token in isolation -- overflows `CreateProcessW`'s
/// ~32KB command-line limit outright (`os error 206`) on a perfectly
/// ordinary, non-shim launch once it exceeds
/// [`super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES`] (issue #213's own
/// Windows-safe figure, reused rather than duplicated). Checked on every
/// platform, not only Windows, so the same launch always takes the same
/// delivery path regardless of where zirv runs.
///
/// Correctness follow-up (post-merge review): measuring the prompt alone
/// missed a real overflow -- the #213 system-prompt layer (folded into
/// `extra` as `--append-system-prompt <text>`/`-c developer_instructions=
/// <json>`) rides on this SAME command line and can itself occupy close to
/// the whole budget, so a prompt safely under budget by itself could still
/// leave the total argv over it. The caller ([`headless_argv_len`]) now
/// measures the fully assembled command `adapter.headless_cmd` would
/// actually emit, so this function's own logic did not need to change --
/// only what its second argument measures.
fn headless_prompt_via_stdin(shim: bool, argv_total_len: usize) -> bool {
    shim || argv_total_len > super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES
}

/// The total bytes `command`'s argv would actually put on the OS command
/// line: the program plus every argument, with one separator byte counted
/// between each token, and each argument's own length inflated for the
/// worst case of Windows' `CreateProcessW` quoting -- an under-count here
/// would let a launch through that still overflows.
/// [`headless_prompt_via_stdin`]'s own doc comment explains why this has to
/// be the WHOLE command, not just the prompt argument.
///
/// Review follow-up (post-merge): the raw byte length alone is not a safe
/// proxy for what actually lands on the command line. `std::process::
/// Command` on Windows builds a UTF-16 command line using the same quoting
/// `CommandLineToArgvW` expects: every `"` is escaped to `\"`, a run of
/// backslashes immediately before a quote doubles, and any argument
/// containing whitespace (a composed prompt almost always does) is wrapped
/// in a surrounding pair of quotes. A quote-and-backslash-heavy prompt can
/// therefore measure safely under `INLINE_ARGV_PROMPT_BUDGET_BYTES` in raw
/// bytes and still expand past `CreateProcessW`'s 32,767-char limit,
/// reproducing os error 206 despite this function's own budget check.
/// Per argument this counts `len + count('"') + count('\\') + 2` --
/// `len` for the literal bytes, one extra byte per `"`/`\\` for the
/// worst case where every one of them needs escaping, and `+ 2` for a
/// surrounding pair of quotes -- which over-counts an argument with no
/// quotes/backslashes/whitespace at all (no quoting needed) but never
/// under-counts one that does, which is the direction that matters: this
/// is deliberately a conservative, platform-independent estimate rather
/// than a byte-exact reproduction of `CreateProcessW`'s own algorithm.
fn headless_argv_len(command: &Command) -> usize {
    let mut total = command.get_program().to_string_lossy().len();
    for arg in command.get_args() {
        let arg = arg.to_string_lossy();
        let quotes = arg.matches('"').count();
        let backslashes = arg.matches('\\').count();
        total += 1;
        total += arg.len() + quotes + backslashes + 2;
    }
    total
}

/// T11: real-clock wrapper. `run_with_clock` (below) does the actual work;
/// this hands it the two real-world functions -- `state::now_secs` and a
/// genuine `std::thread::sleep` -- so every caller outside this module
/// (`script_runner::AgentCommand`, `agent.rs`, `dash`'s headless spawn
/// fallback) keeps calling `run_with` exactly as before, with no signature
/// change to ripple through.
pub fn run_with<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    run_with_clock(
        args,
        w,
        repo,
        env,
        &super::state::now_secs,
        &|d: Duration| std::thread::sleep(d),
    )
}

/// Same supervised execution as [run_with], plus the per-harness accounting
/// segments that make a cross-harness continuation observable to its caller.
pub fn run_with_report<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<(i32, ExecutionReport)> {
    let mut report = ExecutionReport::default();
    let code = run_with_clock_inner(
        args,
        w,
        repo,
        env,
        &super::state::now_secs,
        &|d: Duration| std::thread::sleep(d),
        None,
        &mut report,
    )?;
    Ok((code, report))
}

/// T11 (sleep injection): identical to the former run_with in every way
/// except now_fn/sleep_fn are now parameters instead of a real-clock
/// closure hardcoded two calls deep in the body.
pub(crate) fn run_with_clock<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
) -> CtxResult<i32> {
    let mut report = ExecutionReport::default();
    run_with_clock_inner(args, w, repo, env, now_fn, sleep_fn, None, &mut report)
}

#[allow(clippy::too_many_arguments)]
fn run_with_clock_inner<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
    stable_short: Option<&str>,
    report: &mut ExecutionReport,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Gated only by `cfg.chrome.events` (which already folds in `--quiet` on
    // `zirv ctx agent`, `ZIRV_CTX_QUIET` and `[chrome] events`), independent
    // of whatever terminal (if any) is attached: a headless supervised run
    // still wants these lines on its stderr.
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    let adapter = adapters::select(agent_name, &args.command, &cfg)?;
    let execution_started = Instant::now();
    let execution_model = adapters::last_model_flag(&args.command).map(str::to_string);
    // Issue #155 review finding C2: refused here, before anything is
    // spawned, rather than left to silently never fire -- see
    // `AgentAdapter::counts_tool_calls`'s own doc comment for why this
    // adapter cannot enforce the flag at all.
    if args.max_tool_calls.is_some() && !adapter.counts_tool_calls() {
        return Err(format!(
            "--max-tool-calls is not supported with the '{}' adapter: it has no verified way \
             to count tool calls in its transcript, so the ceiling would never be enforced",
            adapter.name()
        )
        .into());
    }
    // Resolved once, since `adapter` never changes across a nudge/rot/park
    // restart within one `run_with` call: the operator's own choice
    // (`handoff.model`) if set, else the resolved adapter's own default
    // (claude: "haiku"; codex: none, which `CodexAdapter::distiller_cmd`
    // reads as "omit --model").
    let distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    let state = StateDir::resolve(env)?;
    // Still needed standalone: the mail layer below needs a slug, and issue
    // #44's `compile::compile` (which now owns reading the memory bank; see
    // its own call below) computes this same slug internally but callers
    // still need their own copy for mail listing.
    let mail_slug = super::state::repo_slug(repo);

    // A wrapped command that matches no adapter (no explicit `--agent`,
    // detection came up empty) is not actually the agent whose flags we would
    // be injecting; see the matching gate in wrap.rs.
    let skip_injection = args.simple
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    // Issue #44: gathers memory, the canonical `.zirv/context/` layer, and
    // attaches the policy report -- see `compile::compile`'s own doc
    // comment. A Worker session never hears about the derived harness
    // roster either way; see `prompt::PromptSource::Harnesses`.
    let composed = super::compile::compile(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        skip_injection,
        &cfg,
        adapter.as_ref(),
        super::prompt::PromptRole::Worker,
        &state,
        now_secs(),
        super::adapters::LaunchMode::Headless,
        true,
    )
    .composed;
    // Known before argv is touched, because it decides how argv is read: the
    // token holding this exact text is the prompt, whatever it looks like.
    let prompt = args
        .prompt
        .clone()
        .or_else(|| extract_prompt(&args.command));
    // An argv that names no program -- empty, or starting with a flag -- is
    // not a command to pass through: the adapter builds the launch and these
    // are extra flags for it. That is how an agent step arrives, holding its
    // prompt as data with no argv to encode it into.
    let adapter_builds_launch = args
        .command
        .first()
        .is_none_or(|first| first.starts_with('-'));
    let prefix = if adapter_builds_launch {
        0
    } else {
        adapter.launch_prefix_len()
    };
    // The prompt is data, not argv to be interpreted. Protecting its index
    // keeps a prompt that happens to read like the adapter's own
    // system-prompt flag from being stripped out of the launch and promoted
    // into the composed prompt as an operator instruction.
    let prompt_value_at = locate_prompt(&args.command, prefix, prompt.as_deref())
        .and_then(|(index, value)| value.map(|_| index + 1));

    // Determined before mail is listed (N3: delivery is scoped to this
    // session's own short id, so the id has to exist first) and before
    // `prompt_args` (M7 needs a session id to name the private prompt file
    // after) rather than after, as this used to be.
    let session_raw = args
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);

    // Mail is delivered once, here, at the first launch: every restart below
    // reuses this same `composed` value (see the module doc), so a message
    // that arrives mid-run is not retroactively injected into an
    // already-running session. `run_loop`, by contrast, starts a fresh
    // session every cycle and re-lists mail on each one.
    //
    // `mut`: drained by the loop below, once, right after the first
    // successful spawn -- not here. Consuming this early (Item 3's fix) used
    // to mark the mail read before any child had actually started: a launch
    // that fails to spawn at all, or a long pacing park ahead of it, moved
    // it to `read/` with no session ever having seen it.
    //
    // N3: scoped to this run's own short id, so a message addressed to a
    // different session (`send --to-session`) never leaks into this launch's
    // prompt just because the two share a repo and an agent name.
    //
    // C7: this is the *registry* short -- the address `SessionGuard` files
    // this run under below and keeps stable for its whole lifetime (see
    // `SessionGuard::refresh_session`). Every later listing in this function
    // reuses this exact value rather than recomputing `short_id(session)`,
    // which rotates on every restart and stranded any mail addressed to the
    // session a sender had actually resolved.
    let registry_short = stable_short
        .map(str::to_string)
        .unwrap_or_else(|| super::sessions::short_id(session.as_str()));
    // An adapter with no system-prompt injection mechanism never reaches
    // `injection_args_for_session`'s output at all -- folding mail into
    // `composed` for one only would silently destroy it, so for such an
    // adapter it is instead appended straight onto the task prompt text
    // below (`task_prompt_with_mail_fallback`), the one channel such an
    // adapter does have. A capable adapter (claude) is unaffected either
    // way: this still folds mail into `composed` exactly as before.
    let system_prompt_supported = adapter.system_prompt_supported(&args.command);
    // But the task-prompt fallback only exists when zirv itself builds the
    // launch (`adapter_builds_launch`): when the caller passed an explicit
    // command (`-- codex exec "task" ...`), that argv is fixed by the caller
    // and zirv has no task-prompt text of its own to append a fallback to.
    // Rather than list mail this *initial launch* can never actually deliver
    // -- and then either destroy it by consuming an undelivered batch, or
    // strand it marked-unread-forever after a later restart silently did
    // deliver it -- it is left untouched in the mailbox entirely: still
    // visible to `zirv ctx inbox`, and to any other session (or this same
    // run's own later restart) that can actually deliver it.
    //
    // Final wave item 2: `mail_deliverable` restricts *only* this initial
    // launch's own listing (below), not any later restart. Every relaunch
    // arm -- nudge, limit-park, rot/timeout -- rebuilds through `build_
    // headless`, which is unconditionally zirv's own launch regardless of
    // what the original invocation's argv looked like, so the task-prompt-
    // text channel exists on every one of them even when it did not exist
    // at launch. The nudge arm accordingly lists mail fresh without this
    // flag; the park and rot-restart arms don't re-list at all, but reuse
    // whatever `mail_messages` currently holds -- the launch-time listing,
    // or a nudge's own fresher one if this run was nudged first (Medium 4
    // keeps `mail_messages` in lockstep with `mail_entries` wherever either
    // is reassigned).
    let mail_deliverable = adapter_builds_launch || system_prompt_supported;
    // Item 14: `composed.is_some()` only gates listing for an adapter whose
    // *only* delivery channel is `composed` (claude): under `--simple`
    // (`skip_injection`, so `composed` is always `None` regardless of
    // adapter), that used to also withhold mail from an injection-less
    // adapter (codex) whose real channel -- the task-prompt text,
    // `task_prompt_with_mail_fallback` further down -- exists entirely
    // independently of `composed` and does not care whether it is `--simple`
    // or not. `!system_prompt_supported` is the other way in.
    let mut mail_entries: Vec<(PathBuf, super::mail::Message)> =
        if cfg.mail.enabled && mail_deliverable && (composed.is_some() || adapter_builds_launch) {
            super::mail::list(
                &state,
                &mail_slug,
                Some(adapter.name()),
                super::sessions::delivery_filter(None, &registry_short),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
    // Low 8: `mail_deliverable == false` means this launch never lists mail
    // above at all (there is nowhere for it to go), so an operator watching
    // the `zirv ▸` channel saw nothing and had no way to tell "no mail was
    // pending" from "mail was pending but silently withheld" -- exactly the
    // visibility `dash/mod.rs`'s own worker-pane spawn already gives via
    // `push_error` for its narrower shim-unsafe case. A read-only listing,
    // never consumed here (this launch cannot deliver it, so it must stay
    // unread), just to say whether there is anything to announce.
    if cfg.mail.enabled && !mail_deliverable {
        let withheld = super::mail::list(
            &state,
            &mail_slug,
            Some(adapter.name()),
            super::sessions::delivery_filter(None, &registry_short),
        )
        .unwrap_or_default();
        if !withheld.is_empty() {
            announcer.emit(&super::announce::Event::MailWithheld {
                count: withheld.len(),
            });
        }
    }
    // Medium 4: `mut` -- kept in lockstep with `mail_entries` wherever that
    // is reassigned (the nudge arm, below), so a later park/rot-restart's
    // own `task_prompt_with_mail_fallback` call (which intentionally reuses
    // whatever this holds rather than re-listing) sees the most recent
    // listing, not permanently the launch-time one.
    let mut mail_messages: Vec<super::mail::Message> = mail_entries
        .iter()
        .map(|(path, msg)| super::mail::message_with_delivery_envelope(&state, path, msg))
        .collect();
    if !mail_messages.is_empty() {
        announcer.emit(&super::announce::Event::MailDelivered {
            count: mail_messages.len(),
        });
    }
    let composed = if system_prompt_supported {
        super::prompt::with_mail_layer(composed, &mail_messages, cfg.mail.max_delivered_bytes)
    } else {
        composed
    };

    // The first spawn's own argv may already carry the adapter's system-prompt
    // flag (e.g. `-- claude --append-system-prompt "..."`); merge it in rather
    // than letting `prompt_args` silently override it below.
    // `mut`: a nudge relaunch (N4) recomposes fresh, mail included, and
    // replaces this binding so any restart or park after it keeps using the
    // nudge-enriched prompt rather than silently reverting to the launch-time
    // one.
    // PLAUSIBLE-1 (confirmed): captured here, at launch, because this is the
    // only point the operator's own prompt flag is still present in argv.
    // `merge_command_line_prompt` strips it, and every relaunch below holds
    // the *cleaned* argv -- so re-running the merge on a relaunch found
    // nothing and silently dropped the operator's instruction from the
    // recomposed prompt. `relayer_recomposed` re-applies this instead.
    let operator_prompt_text: Option<String> = if composed.is_some() {
        super::prompt::extract_user_prompt_flag(adapter.as_ref(), &args.command, prompt_value_at)
            .ok()
            .and_then(|(_, text)| text)
    } else {
        None
    };
    let (launch_command, mut composed) = super::prompt::merge_command_line_prompt(
        adapter.as_ref(),
        &args.command,
        composed,
        prompt_value_at,
        super::prompt::PromptRole::Worker,
        &cfg.prompt,
    );

    // The user's own flags from the original `--` command (anything beyond
    // the prompt and the session-pinning flags, all of which every restart
    // regenerates fresh): see `extra_launch_flags`. M8: a restart used to
    // rebuild the command from scratch with only zirv's own added flags,
    // silently dropping these.
    let user_extra = extra_launch_flags(&launch_command, prefix, prompt.as_deref(), adapter.name());
    // Bug B (harness/model parity, 2026-08-22): the shipped-default
    // "sandboxed, no prompts" posture (`SandboxConfig`) plus any explicit
    // `[policy]` restriction, from the same seam every other real launch now
    // calls (`adapters::policy_launch_args`). Computed once here, ahead of
    // `user_extra` at every one of this function's four launch-building
    // sites (initial launch, nudge/park/rot-timeout relaunches), the same
    // discipline `user_extra` itself already follows -- see that binding's
    // own comment. `flags_pin_policy` (inside `policy_launch_args`) reads
    // `user_extra`, not the raw wrapped command, so an operator's own
    // `--sandbox`/`--ask-for-approval`/`--permission-mode`/
    // `--disallowedTools` anywhere in their own trailing flags still wins.
    //
    // Deliberately **not** gated on `skip_injection` (which also folds in
    // `args.simple`): `--simple` promises no *injected instruction text*,
    // and the sandbox posture is a safety flag layer, not instruction text
    // (mirrors `wrap.rs`'s identical `policy_skip` reasoning, and `chat.rs`'s
    // own `--simple` test). It is still gated on the one reason `skip_
    // injection` exists that *does* apply here: a wrapped command that does
    // not actually match this adapter must never receive this adapter's
    // flags -- the same leakage risk `skip_injection` exists to prevent for
    // `prompt_args`. `adapter_builds_launch` is exempt from that check
    // entirely: when zirv builds the launch itself (from `--prompt`, no
    // explicit `-- <command>`), there is no wrapped command to mismatch --
    // it is unconditionally this adapter's own launch.
    let policy_skip = !adapter_builds_launch
        && !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let policy_extra = if policy_skip {
        Vec::new()
    } else {
        adapters::policy_launch_args(
            &cfg,
            adapter.as_ref(),
            &user_extra,
            adapters::LaunchMode::Headless,
        )
    };
    // Visible, not silent: the shipped-default posture (or the operator's
    // own opt-out/override) is announced once, here, at session start --
    // not re-announced on a nudge/rot/park relaunch, since `policy_extra`
    // itself is computed once above and simply reused by every relaunch arm.
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if policy_extra.is_empty() {
            "not applied (operator flags, --simple/command mismatch is irrelevant here, or \
             [sandbox] enabled = false)"
                .to_string()
        } else {
            policy_extra.join(" ")
        },
    });

    // The probe has to hit the binary that will actually be spawned. When the
    // argv names no program the adapter builds the launch, so there is nothing
    // in `launch_command` to probe -- it is flags, and `--model --help` is not
    // a capability check.
    let probe_target: &[String] = if adapter_builds_launch {
        &[]
    } else {
        &launch_command
    };
    // `mut`: recomputed by a nudge relaunch alongside `composed` above.
    let mut prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        probe_target,
        composed.as_ref(),
        &state,
        session.as_str(),
    )?;
    super::prompt::log_injection(
        &state,
        "exec",
        session.as_str(),
        composed.as_ref(),
        system_prompt_supported,
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        system_prompt_supported,
    ));

    let derive_transcript = |session: &SessionId| {
        adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    };

    // `--transcript` describes the caller's own first child only. Every restart
    // is a new session launched by the adapter, so its transcript path has to be
    // derived again or the watcher would keep polling the dead child's file.
    let mut transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| derive_transcript(&session));

    // Surfaced once, upfront, rather than only when a restart is already
    // needed: an operator who never rots would otherwise never learn that
    // rotting is a dead end for this invocation until it actually happens.
    if prompt.is_none() {
        writeln!(
            w,
            "zirv ctx exec: no prompt could be found in the command; restarts and usage-limit \
             parking will be unavailable for this run. Pass --prompt to enable them."
        )?;
    }
    let max_restarts = args.max_restarts.unwrap_or(cfg.supervise.max_restarts);
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);
    // Issue #155, Phase 5(d): the CEILING is fixed for the whole run, same as
    // `max_restarts`/`timeout` above. Issue #169.2: SPEND is now accumulated
    // across every restart this run mints, not just measured against
    // whichever child happens to be running -- see `prior_usage`/`prior_
    // tool_calls` below, harvested from each outgoing transcript at every
    // restart/nudge/park site before a fresh one is minted. Before this fix
    // a rot/timeout/nudge restart or a usage-limit park -- all of which mint
    // a fresh transcript -- silently reset the meter, so N restarts allowed
    // N times the configured ceiling.
    let worker_budget = agent::WorkerBudget {
        tokens: args.budget_tokens,
        tool_calls: args.max_tool_calls,
    };
    // Issue #169.2: the running total of every PRIOR child's own spend this
    // invocation has already superseded (a rot/timeout/nudge restart, or a
    // usage-limit park). Folded into every budget check alongside the
    // current child's own transcript (`evaluate_worker_budget`), so the
    // budget bounds the whole supervised run, not just its latest incarnation.
    let mut prior_usage = TranscriptUsage::default();
    let mut prior_tool_calls: u32 = 0;

    let socket_path = state.socket_for(session.as_str());
    let server = match signal::SignalServer::bind(&socket_path) {
        Ok(server) => Some(server),
        Err(e) => {
            // Turn signals only accelerate detection; polling is the floor.
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "no-socket",
                    detail: &e.to_string(),
                },
            );
            None
        }
    };
    // Rebuilt for every session, because the hook inside a child reports the
    // session id this exports. Pinning the first one makes every restart's
    // signals look like they belong to a session that is already dead.
    //
    // `AGENT_ENV` is exported unconditionally, unlike the turn-signal env
    // above (which needs a bound socket): it names the same fact
    // `ctx.toml`'s own `agent` config key would, so a nested `zirv ctx ...`
    // call inside this session's own children defaults to this session's own
    // harness rather than re-resolving from scratch, whether or not turn
    // signals are available.
    let turn_env_for = |session: &SessionId| {
        let mut turn_env: Vec<(String, String)> = server
            .as_ref()
            .map(|server| {
                adapter
                    .register_turn_signal(
                        &SessionRef {
                            id: session.clone(),
                            cwd: repo.to_path_buf(),
                        },
                        server.path(),
                    )
                    .env
            })
            .unwrap_or_default();
        turn_env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
        // Security review round 2 (Finding 3): the work-group binding travels
        // by lineage. `dash::fulfill_spawn_request` already pushed this exact
        // pair into a pane's own `turn_env`; the headless launch pushed
        // nothing, so a headless sub-orchestrator's children resolved
        // `group = None` (`agent::resolve_group_binding`'s env fallback found
        // nothing) -- no `admit_child`, no `child_limit`, no token ceiling,
        // and every such delegation rendered "ungrouped" in `zirv ctx
        // status`'s group tree. Read from this run's own env lookup, which
        // `agent::run_with` folds its resolved `--group` into (`group_env`,
        // the same shape `chat::quiet_env` established), so an inherited
        // binding and a freshly resolved one reach the child identically.
        if let Some(group) = env(super::agent::WORK_GROUP_ENV).filter(|id| !id.is_empty()) {
            turn_env.push((super::agent::WORK_GROUP_ENV.to_string(), group));
        }
        turn_env
    };

    // F3: the one place a launch's session identity is applied, so the scrub
    // can never be forgotten on one of the four relaunch paths below. The
    // scrub is unconditional and comes first: `turn_env_for` yields nothing
    // when the socket bind failed, and without this the child inherited the
    // *outer* session's `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET` from this
    // process's own environment and reported its turns into somebody else's
    // supervisor. A worker legitimately runs inside a session (that is what
    // `zirv ctx agent` is), but it must still speak with its own identity or
    // none at all.
    let apply_session_env = |command: &mut Command, session: &SessionId| {
        super::sessions::scrub_supervision_env_cmd(command);
        for (key, value) in turn_env_for(session) {
            command.env(key, value);
        }
        // Issue #236: this module supervises only headless runs, so every
        // child it spawns gets this marker, read by `engine::refusal_for` to
        // refuse the interactive `brainstorm` skill. Scrubbed by
        // `scrub_supervision_env_cmd` above first, so a nested headless
        // launch never inherits a stale copy before this sets its own.
        command.env(adapters::HEADLESS_ENV, "1");
    };

    // FIX B: on a Windows npm `.cmd` shim launch, `cmd.exe /c <shim>` reparses
    // the whole downstream argv, so a headless prompt on argv -- operator task
    // text, plus any mail folded into a nudge/restart relaunch below -- would
    // be reinterpreted by cmd.exe. Deliver it on the child's stdin instead (the
    // same mechanism `handoff::run_model`'s distiller uses), and only on that
    // launch shape: off Windows, and for a directly executable `.exe`, the
    // prompt stays on argv exactly as before, so every `sh`-based fake-agent
    // test is byte-identical. Returns the built command and the stdin payload
    // (`Some` only when the prompt was kept off argv).
    //
    // Final wave item 1: `adapter.launches_through_cmd_shim()` only
    // recognises the `cmd.exe /c <shim>` form -- a `.ps1`-resolved
    // `agent_bin` would report "safe" here while `headless_cmd`'s own argv
    // (built below, on the `false` branch) still reached a `powershell
    // -File` launch with the prompt on the reparsed argv, the same M1 gap
    // dash/mod.rs's `task_prompt_fallback_is_safe` closed for the pty path.
    // The probe below builds exactly the launcher prefix this run's real
    // headless spawn will use (`headless_cmd("", ...)` -- no prompt token
    // yet, since deciding whether one is safe to put there is the point)
    // and asks `launch_reparses_through_shim`, which covers both forms.
    //
    // Final wave item 2: no longer ANDed with `adapter_builds_launch`.
    // `prompt_via_stdin` is consulted only inside `build_headless` below,
    // and `build_headless` is what *every* relaunch (nudge, park, rot/
    // timeout) uses regardless of what the *initial* launch looked like --
    // wave 5's item 2 made that explicit for mail deliverability, and the
    // same fact applies here: an explicit `-- <command>` at the initial
    // launch (`adapter_builds_launch == false`) does not stop a later
    // relaunch from rebuilding through `build_headless` on a shim-resolved
    // agent. With the old conjunct, `prompt_via_stdin` was pinned `false`
    // for that whole run, so a relaunch's multi-line composed/mail prompt
    // text landed on argv instead of stdin and `guard_cmd_shim_reparse`
    // aborted the run outright the moment one arrived -- pre-existing (it
    // affects claude too, not just codex), just widened by wave 5's own fix
    // making relaunches reachable in more shapes than before.
    let prompt_via_stdin = prompt_delivery_via_stdin(adapter.as_ref(), &session);
    let relaunch_system_prompt_supported = adapter.system_prompt_supported(&[]);
    // Issue #220: `headless_prompt_via_stdin` also routes an oversized
    // launch to stdin regardless of `prompt_via_stdin` -- see its own doc
    // comment. Measured per call, not hoisted out here as a single flag,
    // because `build_headless` is the one chokepoint every relaunch --
    // nudge, park, rot restart -- reuses with its own, possibly
    // differently-sized, `prompt_text`/`extra` (see the call sites' own
    // comments). Correctness follow-up (post-merge review): the probe
    // measures the FULL `adapter.headless_cmd(prompt_text, session, extra)`
    // argv -- not just `prompt_text.len()` -- because the #213 system-prompt
    // layer folded into `extra` rides the same command line and can itself
    // occupy close to the whole budget, so a prompt safely under budget on
    // its own could still leave the total argv over it. Built once and
    // reused as the argv-delivery fallback below, rather than built twice.
    let build_headless =
        |prompt_text: &str, session: &SessionId, extra: &[String]| -> (Command, Option<String>) {
            let probe = adapter.headless_cmd(prompt_text, session, extra);
            let argv_total_len = headless_argv_len(&probe);
            if headless_prompt_via_stdin(prompt_via_stdin, argv_total_len)
                && let Some(command) = adapter.headless_cmd_stdin(session, extra)
            {
                return (command, Some(prompt_text.to_string()));
            }
            (probe, None)
        };

    // With no argv to pass through, the first launch is built exactly the way
    // every relaunch builds one. That symmetry is the point: a caller holding
    // the prompt as data never encodes it into argv for this function to
    // decode again, so it can never be misread as a flag.
    let (mut command, mut stdin_prompt) = if adapter_builds_launch {
        let prompt_text = prompt.as_deref().ok_or(
            "no command to supervise; pass the agent command after --, \
             or --prompt to have zirv build the launch itself",
        )?;
        let prompt_text = super::prompt::task_prompt_with_composed_fallback(
            prompt_text,
            system_prompt_supported,
            composed.as_ref(),
        );
        let mail_in_composed = composed
            .as_ref()
            .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
        let prompt_text = super::prompt::task_prompt_with_mail_fallback(
            &prompt_text,
            (system_prompt_supported && composed.is_some()) || mail_in_composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
        );
        let extra: Vec<String> = policy_extra
            .iter()
            .cloned()
            .chain(user_extra.iter().cloned())
            .chain(prompt_args.iter().cloned())
            .collect();
        let (mut command, stdin_prompt) = build_headless(&prompt_text, &session, &extra);
        command.current_dir(repo);
        (command, stdin_prompt)
    } else {
        // An explicit `-- <command>` is the operator's own fixed argv: there
        // is no `user_extra` slot to prepend `policy_extra` ahead of, so
        // both zirv-owned additions are appended the same way `prompt_args`
        // already was here, before this fix -- see `policy_extra`'s own
        // comment for why `flags_pin_policy` still consulted the launch's
        // own trailing flags (folded into `user_extra` above) rather than
        // this branch's fixed command.
        let mut command = build_command(&launch_command, repo)?;
        for arg in policy_extra.iter().chain(prompt_args.iter()) {
            command.arg(arg);
        }
        (command, None)
    };
    apply_session_env(&mut command, &session);
    let mut restarts = 0;
    // N4: consecutive `zirv ctx nudge`-driven restarts, capped by `cfg.
    // supervise.max_nudges` -- a separate budget from `restarts` above,
    // since a nudge is not rot and must never spend it. A relaunch (nudge or
    // otherwise) needs a known prompt to carry forward; without one a nudge
    // is claimed but ignored, the same as being over the cap.
    let mut nudge_restarts = 0u32;
    let can_restart = prompt.is_some();

    // Best-effort registration: covers a hand-typed `zirv ctx exec` as well
    // as `zirv ctx agent` and a script `agent:` step, both of which delegate
    // to this same function. Refreshed (not re-registered) whenever a
    // restart or a usage-limit park mints a fresh session id below, and
    // released explicitly in every arm that leaves this loop -- the same
    // explicit-arm discipline `RawGuard` follows, since this binary's
    // release profile is `panic = "abort"` and `Drop` is not guaranteed.
    // Issue #139: see `wrap.rs::run_with`'s identical comment -- pure and
    // deterministic from the same `cfg.safety` this launch's own settings
    // file was built from, so `status.rs` can later detect a widened policy
    // this session's own launch snapshot has not adopted yet.
    //
    // Issue #155, Phase 5(e): the former heavy-worker registration gate
    // (`sessions::count_heavy_workers`, refusing a launch outright at this
    // point) is gone -- a session registration is no longer a heavy event by
    // itself. The machine-wide budget now gates the actual heavy COMMAND, at
    // `script_runner::Command::invoke` (`permit::acquire`), so an idle
    // supervised session here holds nothing.
    let safety_policy_sha256 = super::safety::policy_fingerprint(&cfg.safety).ok();
    let mut session_guard = super::sessions::SessionGuard::register(
        &state,
        super::sessions::Record::new(
            session.as_str(),
            adapter.name(),
            repo,
            super::sessions::Verb::Exec,
        )
        .with_stable_short(&registry_short)
        .with_safety_policy_sha256(safety_policy_sha256)
        // Issue #169: `exec::run_with` has no `PromptRole` parameter to get
        // wrong (see `agent.rs`'s own module doc comment: a delegated run is
        // always a worker session), so this is always `Worker` -- an
        // accurate reflection of what every headless delegation actually
        // runs as today.
        .with_role(super::prompt::PromptRole::Worker.label()),
    );

    // Item 10: owned across every cycle of the loop below (the pre-flight
    // check and, on a usage-limit park, the second call further down), so
    // the no-usage-source blind-delay line and `PacingBlind` announce once
    // for the whole run rather than once per restart.
    let mut pace_flags = pace::PaceGateFlags::default();
    let http_poller = super::poll::HttpPoller::new(cfg.chrome.events);

    loop {
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "exec",
            session.as_str(),
            now_fn,
            sleep_fn,
            Some(&announcer),
            adapter.provider(),
            pace::PaceGate {
                use_credits: cfg.pace.use_credits.for_provider(adapter.provider()),
                poller: cfg
                    .pace
                    .poll_enabled
                    .then_some(&http_poller as &dyn super::poll::UsagePoller),
            },
            &mut pace_flags,
        );

        // P2/P3: `_child_guard` holds this cycle's child in the console-close
        // pid registry and in a kill-on-close job for as long as it is in
        // scope -- which is this loop iteration, i.e. exactly the child's own
        // life. Dropped (and so released) at the end of the iteration, after
        // the child has been reaped, and again by every arm that returns.
        let (mut child, tap, _child_guard) =
            supervise::spawn_tapped(command, stdin_prompt.clone())?;
        // Item 3: the messages folded into the launch prompt are consumed
        // here, right after the spawn that actually carried them has
        // genuinely started -- not before pacing or the spawn itself, where
        // a park or a failed launch would have moved them to `read/` with no
        // session ever having seen them. Drains to empty on the first
        // successful spawn, so a later restart's own iteration through this
        // same loop finds nothing left to consume and is a no-op. A failed
        // consume must not fail the launch itself -- best effort, like the
        // rest of state-dir housekeeping -- since the mail has already
        // reached the prompt either way.
        for (path, _) in mail_entries.drain(..) {
            let _ = super::mail::consume_and_log(
                &state,
                &mail_slug,
                &path,
                &registry_short,
                "exec",
                "exec:launch-prompt",
            );
        }
        // Fresh scorer per iteration, over the current session's transcript.
        let mut scorer = score::IncrementalScorer::new(transcript.clone());
        let mut rotted = false;
        let mut limit_hit = false;
        let mut nudged_by: Option<String> = None;
        // Issue #155, Phase 5(d): fresh per iteration too -- the child a
        // restart mints is a fresh transcript, so its own soft-warn latch and
        // exhaustion flag start over along with it (see `worker_budget`'s own
        // doc comment for the scope this implies).
        let mut budget_soft_warned = false;
        let mut budget_exhausted = false;

        // C3: reset below whenever this run reported a turn of its own.
        let mut progressed = false;
        let outcome = supervise_run(
            &mut child,
            Instant::now() + timeout,
            poll,
            &mut scorer,
            adapter.as_ref(),
            &cfg.score,
            &state,
            server.as_ref(),
            session.as_str(),
            &registry_short,
            &mut rotted,
            &mut progressed,
            &tap,
            &mut limit_hit,
            &mut nudged_by,
            nudge_restarts,
            cfg.supervise.max_nudges,
            can_restart,
            &transcript,
            worker_budget,
            &prior_usage,
            prior_tool_calls,
            &mut budget_soft_warned,
            &mut budget_exhausted,
        )?;

        if budget_exhausted {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "budget",
                    score: 0,
                    action: "kill",
                    detail: &transcript.display().to_string(),
                },
            );
            writeln!(
                w,
                "zirv ctx exec: token/tool-call budget exhausted, stopping (exit \
                 {EXIT_BUDGET_EXHAUSTED})"
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(EXIT_BUDGET_EXHAUSTED);
        }

        // C3: the budget is *consecutive* nudge restarts, which is what
        // `[supervise] max_nudges` has always been documented as. It was
        // implemented cumulatively -- never reset -- so a long-lived session
        // that was nudged three times over an hour, doing useful work in
        // between each, permanently lost the ability to be nudged again.
        // A turn boundary reported by this session is the evidence that it
        // got somewhere, so the run of consecutive nudges is over.
        nudge_restarts = nudges_after(nudge_restarts, progressed);

        // `supervise_child` checks the child's exit status before calling the
        // tick, so a fast limit-hit exit (print the notice, exit immediately,
        // exactly what a real exhausted-window run looks like) can race past
        // the last tick that would have caught it. A final drain here closes
        // that race without touching supervise_child's general contract --
        // `drain_to_eof`, not `try_lines`, because `try_lines` alone is just
        // as non-blocking as every tick's own call and can still lose the
        // race it looks like it closes (root-caused via a deterministic
        // repro in `supervise.rs`'s own test module, not by inspection alone).
        // Issue #227: a provider capacity/overload error and an account/
        // billing exhaustion are both text-tail conditions, exactly like a
        // vendor usage-limit message, so they are read off the same final
        // drain rather than a second read of the tap (which would lose
        // lines: `drain_to_eof` is destructive). Checked only when `limit_
        // hit` is still false: a vendor-confirmed usage-limit message always
        // wins the classification on the rare tail that somehow carries more
        // than one of these phrasings. `account_pattern` wins over `capacity_
        // pattern` for the same reason -- burning the restart budget on a
        // capacity retry when the account itself is empty would just fail
        // again immediately.
        let mut capacity_pattern: Option<&'static str> = None;
        let mut account_pattern: Option<&'static str> = None;
        if !limit_hit {
            let final_lines = tap.drain_to_eof(supervise::FINAL_DRAIN_BUDGET);
            limit_hit = pace::scan_for_limit(
                &final_lines,
                &state,
                session.as_str(),
                "exec",
                &mut std::io::stderr(),
            );
            if !limit_hit {
                account_pattern = pace::scan_for_account_exhausted(&final_lines);
                if account_pattern.is_none() {
                    capacity_pattern = pace::scan_for_capacity_error(&final_lines);
                }
            }
        }

        // Issue #227: an account/billing exhaustion is a hard, non-retryable
        // condition -- unlike a usage window (which resets on its own) or a
        // capacity error (which is worth retrying), restarting cannot fix an
        // empty account, so this gives up immediately without spending any
        // of the restart budget. Gated on a genuinely non-zero exit: a clean
        // exit with incidental matching text (vanishingly unlikely given how
        // specific these phrases are) must still read as success.
        if let Some(label) = account_pattern
            && matches!(outcome, Outcome::Exited(code) if code != 0)
        {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "account",
                    score: 0,
                    action: "account-exhausted",
                    detail: label,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: {} account exhausted ({label}); this is not retryable -- \
                 check billing before restarting (exit {EXIT_ACCOUNT_EXHAUSTED})",
                adapter.name()
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(EXIT_ACCOUNT_EXHAUSTED);
        }

        match outcome {
            Outcome::Exited(code) if !(limit_hit || capacity_pattern.is_some() && code != 0) => {
                // Issue #37: a clean session end -- no rot, no timeout, no
                // restart -- previously never harvested at all. Gated on
                // `cfg.memory.harvest` here too, before the transcript is
                // even read, so an operator who left harvesting off never
                // pays for the read or the distiller call this seam can
                // make. Best-effort, discarded via `let _ =`: a harvest
                // failure must never turn a successful exit into a failed
                // one.
                if cfg.memory.enabled && cfg.memory.harvest {
                    let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
                    let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
                    let _ = super::memory::harvest_at_session_end(
                        adapter.as_ref(),
                        &distiller_model,
                        &ctx,
                        Duration::from_secs(cfg.handoff.timeout_secs),
                        repo,
                        &state,
                        &mail_slug,
                        &cfg,
                    );
                }
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );
                session_guard.release();
                return Ok(code);
            }
            Outcome::Exited(_) | Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        // N4: a nudge relaunch is neither a limit park nor a rot restart --
        // `supervise_run`'s own tick only ever sets `nudged` when a relaunch
        // is actually possible (a known prompt) and under the consecutive
        // cap, so this arm always follows through rather than needing its
        // own "no prompt"/"over budget" fallbacks the way rot's restart does.
        if let Some(nudged_from) = nudged_by.take() {
            let prompt_text = prompt
                .clone()
                .expect("supervise_run only sets `nudged` when a prompt is known");

            let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
            let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
            let (note, source) = handoff::distill_or_structural(
                adapter.as_ref(),
                &distiller_model,
                &ctx,
                Duration::from_secs(cfg.handoff.timeout_secs),
                cfg.chrome.events,
            );
            let stored = handoff::store(&state, repo, session.as_str(), &note)?;

            // Harvest every outgoing child even for an unbounded run: the same
            // accumulator now feeds both budget enforcement and delegation
            // accounting, so skipping it would hide pre-handoff/restart spend.
            harvest_spend(
                adapter.as_ref(),
                &transcript,
                &mut prior_usage,
                &mut prior_tool_calls,
            );
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);

            // Recompose fresh -- unlike an ordinary restart, which reuses
            // the launch-time `composed`/`prompt_args` untouched (see this
            // module's own doc comment), a nudge relaunch is explicitly the
            // chance to pick up what prompted it: the nudge's own payload
            // arrived as ordinary session-addressed mail (`sessions::run_
            // nudge_with` stores it before writing the wake-up marker), so
            // re-listing mail for the session that was just nudged and
            // folding it in through `with_mail_layer` delivers it with zero
            // new injection machinery.
            //
            // Issue #44: goes through `compile::compile` a second time here,
            // same as the launch-time call above. One small, deliberate
            // behavior refinement over the pre-compiler code this replaces:
            // `compile` re-reads the memory bank fresh (it is a pure function
            // of `state` at call time) rather than reusing the launch-time
            // `memory_entries` snapshot the old duplicated call passed in
            // verbatim. The bank is repo-wide and does not go stale *within*
            // one `run_with` call either way (see the removed comment this
            // replaced), so this is not a correctness change -- a nudge that
            // lands after something new was remembered now picks it up
            // instead of seeing the launch-time snapshot.
            let mut fresh = super::compile::compile(
                crate::utils::home_dir().ok().as_deref(),
                repo,
                skip_injection,
                &cfg,
                adapter.as_ref(),
                super::prompt::PromptRole::Worker,
                &state,
                now_secs(),
                super::adapters::LaunchMode::Headless,
                true,
            )
            .composed;
            // C7: `registry_short`, not `short_id(session)` -- `session`
            // has just been rotated above, and the nudge's own payload was
            // addressed to the stable registry address the sender resolved.
            //
            // N5: gated on `fresh.is_some()` as well as `cfg.mail.enabled`,
            // exactly like the launch path's `composed.is_some()` gate.
            // Under `--simple` there is no composed prompt for `with_mail_
            // layer` to fold mail into either, so listing it here only led
            // to it being consumed (moved to `read/`) by the post-spawn
            // drain below -- silently marking a message read that no
            // session ever saw.
            //
            // Medium 3: `|| !system_prompt_supported` is the same escape the
            // launch path's own gate (~401) has -- without it, `--simple`
            // makes `fresh` always `None` regardless of adapter, so a codex
            // run under `--simple` dropped the nudge's own guidance
            // silently while still spending a `max_nudges` slot on the
            // restart it triggered. Codex's real channel here is the task
            // prompt text (`task_prompt_with_mail_fallback` below), which
            // does not depend on `fresh`/`composed` existing at all.
            //
            // Final wave item 2: deliberately NOT also gated on the launch-
            // time `mail_deliverable` (`adapter_builds_launch ||
            // system_prompt_supported`) the way it used to be. That flag
            // answers "can *this launch's own argv shape* carry a fallback"
            // -- true only for a zirv-built launch, since an explicit `--
            // command` is the caller's fixed argv with nothing of zirv's own
            // to append to. A nudge restart is not that launch: every
            // relaunch arm (nudge, park, rot/timeout) rebuilds through
            // `build_headless`, which is *always* the adapter's own launch,
            // regardless of what the original invocation looked like. So by
            // the time this code runs, the task-prompt-text channel exists
            // unconditionally -- reusing the original launch's `mail_
            // deliverable` here understated what a relaunch can actually
            // deliver.
            let nudge_mail: Vec<(PathBuf, super::mail::Message)> = if cfg.mail.enabled {
                // Read back off the guard, which is the one thing that
                // demonstrably did not rotate when `refresh_session` ran
                // a few lines above.
                super::mail::list(
                    &state,
                    &mail_slug,
                    Some(adapter.name()),
                    super::sessions::delivery_filter(Some(session_guard.short()), &registry_short),
                )
                .unwrap_or_default()
            } else {
                Vec::new()
            };
            let nudge_mail_msgs: Vec<super::mail::Message> =
                nudge_mail.iter().map(|(_, msg)| msg.clone()).collect();
            if !nudge_mail_msgs.is_empty() {
                announcer.emit(&super::announce::Event::MailDelivered {
                    count: nudge_mail_msgs.len(),
                });
            }
            // Folded into `composed` only for an adapter with a real
            // injection mechanism: `injection_args_for_session` always
            // turns `composed` into an empty argv for one without, so
            // folding mail in here only would tag it `PromptSource::Mail`
            // on a prompt nobody ever receives -- the fallback below
            // (`task_prompt_with_mail_fallback`, which already gates on
            // this same flag internally) is that adapter's one real
            // channel.
            fresh = if relaunch_system_prompt_supported {
                super::prompt::with_mail_layer(
                    fresh,
                    &nudge_mail_msgs,
                    cfg.mail.max_delivered_bytes,
                )
            } else {
                fresh
            };
            // PLAUSIBLE-1: re-apply the adapter layer and the operator's own
            // command-line instruction from the text captured at launch.
            // `launch_command` is the cleaned argv, so merging against it
            // again would find no flag and drop the instruction entirely.
            let fresh = super::prompt::relayer_recomposed(
                adapter.as_ref(),
                fresh,
                operator_prompt_text.as_deref(),
                super::prompt::PromptRole::Worker,
                &cfg.prompt,
            );
            composed = fresh;
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            )?;
            // Folded into the prompt above, but only actually marked read
            // once the relaunch that carries it genuinely spawns -- the same
            // Item 3 discipline every other delivery seam in this function
            // follows.
            mail_entries = nudge_mail;
            // Medium 4: kept in lockstep with `mail_entries` just above --
            // a later park or rot-restart's own `task_prompt_with_mail_
            // fallback` call reuses `mail_messages` verbatim rather than
            // re-listing (see those arms' own comments), so leaving this
            // holding the stale launch-time list would have re-appended
            // already-consumed mail on that later restart while silently
            // dropping the nudge's own guidance from it entirely.
            mail_messages = nudge_mail_msgs.clone();

            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                relaunch_system_prompt_supported,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                relaunch_system_prompt_supported,
            ));
            announcer.emit(&super::announce::Event::Nudge {
                from: nudged_from,
                disposition: super::announce::NudgeDisposition::Relaunching,
            });

            nudge_restarts += 1;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-restart",
                    detail: &format!("{source} handoff at {}", stored.display()),
                },
            );
            writeln!(
                w,
                "zirv ctx exec: nudged ({nudge_restarts}/{}), restarting with a {source} handoff",
                cfg.supervise.max_nudges
            )?;

            let combined = format!("{prompt_text}\n\n{}", note.to_markdown());
            let combined = super::prompt::task_prompt_with_composed_fallback(
                &combined,
                relaunch_system_prompt_supported,
                composed.as_ref(),
            );
            let mail_in_composed = composed
                .as_ref()
                .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
            // A nudge relaunch re-lists mail fresh (`nudge_mail_msgs` above),
            // so the fallback for an uninjectable adapter has to use that
            // same fresh listing, not the launch-time `mail_messages`.
            let combined = super::prompt::task_prompt_with_mail_fallback(
                &combined,
                (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
                &nudge_mail_msgs,
                cfg.mail.max_delivered_bytes,
            );
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            let (mut rebuilt, sp) = build_headless(&combined, &session, &extra);
            rebuilt.current_dir(repo);
            apply_session_env(&mut rebuilt, &session);
            command = rebuilt;
            stdin_prompt = sp;
            continue;
        }

        if limit_hit {
            // Issue #186: a vendor-confirmed block is the only point where a
            // running session may move harnesses. The child is already stopped,
            // so this never interrupts an in-flight response. Only launches
            // zirv itself built from prompt data are portable across vendors;
            // an operator-owned explicit command keeps today's park behavior.
            let visited: Vec<String> = env(super::fallback::VISITED_ENV)
                .map(|raw| super::config::split_csv_list(&raw))
                .unwrap_or_default();
            let source_model = adapters::last_model_flag(&args.command);
            let route_request = super::fallback::RouteRequest {
                requested: adapter.name(),
                source_model,
                source_model_explicit: source_model.is_some(),
                bounds: super::fallback::TaskBounds {
                    tokens: worker_budget.tokens,
                    tool_calls: worker_budget.tool_calls,
                },
                now: now_fn(),
            };
            let route = (adapter_builds_launch && prompt.is_some())
                .then(|| {
                    super::fallback::route_blocked_session(&state, &cfg, route_request, &visited)
                })
                .flatten();
            let deferred_reset = (route.is_none() && adapter_builds_launch && prompt.is_some())
                .then(|| {
                    super::fallback::earliest_reset_choice(&state, &cfg, route_request, &visited)
                })
                .flatten();
            let alternate = route
                .as_ref()
                .map(|route| (route.selected.clone(), route.model.clone(), route.detail()))
                .or_else(|| {
                    deferred_reset.as_ref().and_then(|choice| {
                        if !choice.is_cross_harness() {
                            return None;
                        }
                        Some((
                            choice.selected.clone(),
                            choice.model.clone()?,
                            choice.detail(),
                        ))
                    })
                });

            if let Some((selected_agent, selected_model, selection_detail)) = alternate {
                let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
                let (note, source) = handoff::distill_or_structural(
                    adapter.as_ref(),
                    &distiller_model,
                    &ctx,
                    Duration::from_secs(cfg.handoff.timeout_secs),
                    cfg.chrome.events,
                );
                let stored = handoff::store(&state, repo, session.as_str(), &note)?;

                // The source-harness portion is complete at this boundary.
                // Record it before harvesting the current transcript into the
                // budget accumulator, otherwise the helper would count it twice.
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );

                // Preserve both accounting and any configured delegation budget
                // across the vendor boundary. This includes the just-stopped
                // child plus every prior restart already accumulated here.
                harvest_spend(
                    adapter.as_ref(),
                    &transcript,
                    &mut prior_usage,
                    &mut prior_tool_calls,
                );
                let spent_tokens = prior_usage
                    .context_total()
                    .saturating_add(prior_usage.output_tokens);
                let remaining_tokens = worker_budget
                    .tokens
                    .map(|limit| limit.saturating_sub(spent_tokens));
                let remaining_tool_calls = worker_budget
                    .tool_calls
                    .map(|limit| limit.saturating_sub(prior_tool_calls));
                if worker_budget
                    .tokens
                    .is_some_and(|_| remaining_tokens == Some(0))
                    || worker_budget
                        .tool_calls
                        .is_some_and(|_| remaining_tool_calls == Some(0))
                {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "budget",
                            score: 0,
                            action: "fallback-budget-exhausted",
                            detail: "usage limit coincided with the delegation budget ceiling",
                        },
                    );
                    session_guard.release();
                    return Ok(EXIT_BUDGET_EXHAUSTED);
                }

                let detail = format!(
                    "{}; {} handoff at {}",
                    selection_detail,
                    source,
                    stored.display()
                );
                let _ = log::append(
                    &state,
                    &log::Decision {
                        ts: now_secs(),
                        session: session.as_str(),
                        verb: "exec",
                        verdict: "limit",
                        score: 100,
                        action: "harness-handover",
                        detail: &detail,
                    },
                );
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit; continuing on another harness ({detail})"
                )?;

                let prompt_text = prompt.clone().expect("route requires a known prompt");
                let continuation = format!(
                    "{prompt_text}\n\nThe previous harness exhausted its usage window. Continue from this handoff without redoing completed work:\n\n{}",
                    note.to_markdown()
                );
                let target = adapters::select(Some(&selected_agent), &[], &cfg)?;
                let nested_args = ExecArgs {
                    agent: Some(selected_agent.clone()),
                    session_id: None,
                    transcript: None,
                    prompt: Some(continuation),
                    max_restarts: args.max_restarts,
                    timeout_secs: args.timeout_secs,
                    budget_tokens: remaining_tokens,
                    max_tool_calls: remaining_tool_calls,
                    command: target.model_args(&selected_model),
                    simple: args.simple,
                };

                let mut next_visited = visited;
                if !next_visited.iter().any(|name| name == adapter.name()) {
                    next_visited.push(adapter.name().to_string());
                }
                let visited_csv = next_visited.join(",");
                let nested_env = |key: &str| {
                    if key == super::fallback::VISITED_ENV {
                        Some(visited_csv.clone())
                    } else {
                        env(key)
                    }
                };

                // The old session has ended and its handoff is durable before
                // the continuation is registered. Releasing first prevents two
                // live registry entries from claiming one logical worker.
                session_guard.release();
                return run_with_clock_inner(
                    &nested_args,
                    w,
                    repo,
                    &nested_env,
                    now_fn,
                    sleep_fn,
                    Some(&registry_short),
                    report,
                );
            }

            let wait_detail = deferred_reset
                .as_ref()
                .map(super::fallback::ResetChoice::detail)
                .unwrap_or_else(|| {
                    "agent reported a usage limit; parking until the current window resets"
                        .to_string()
                });
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "limit",
                    score: 100,
                    action: "limit-park",
                    detail: &wait_detail,
                },
            );
            writeln!(w, "zirv ctx exec: {wait_detail}")?;

            pace::wait_for_window(
                w,
                &state,
                &cfg.pace,
                "exec",
                session.as_str(),
                now_fn,
                sleep_fn,
                Some(&announcer),
                adapter.provider(),
                pace::PaceGate {
                    // A vendor-reported limit hit parks even with use_credits
                    // enabled: the vendor limiting us means credits are
                    // exhausted or not actually enabled plan-side, and an
                    // immediate relaunch would just re-hit it.
                    use_credits: false,
                    poller: cfg
                        .pace
                        .poll_enabled
                        .then_some(&http_poller as &dyn super::poll::UsagePoller),
                },
                &mut pace_flags,
            );

            let Some(prompt_text) = prompt.clone() else {
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit and the original prompt is unknown, so it cannot relaunch. Pass --prompt to enable parking."
                )?;
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );
                session_guard.release();
                return Ok(EXIT_ROT_EXHAUSTED);
            };

            // A park mints a fresh transcript exactly like a restart, so the
            // outgoing child's spend must be harvested for both whole-run
            // accounting and any configured token/tool-call ceiling.
            harvest_spend(
                adapter.as_ref(),
                &transcript,
                &mut prior_usage,
                &mut prior_tool_calls,
            );
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            )?;
            // M2: a park mints a new session id, just like a restart, so the
            // injection attribution is re-logged under it rather than only
            // ever naming the first session this run started with.
            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                relaunch_system_prompt_supported,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                relaunch_system_prompt_supported,
            ));
            // M8: the user's own extra flags survive the relaunch too, not
            // just zirv's own (the system prompt args, and now the
            // sandbox/policy prepend).
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            let prompt_text = super::prompt::task_prompt_with_composed_fallback(
                &prompt_text,
                relaunch_system_prompt_supported,
                composed.as_ref(),
            );
            let mail_in_composed = composed
                .as_ref()
                .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
            // A park does not itself re-list mail (matching every other
            // value it reuses here), so the fallback for an uninjectable
            // adapter reuses whatever `mail_messages` currently holds --
            // the launch-time listing, or a nudge's own fresher one if this
            // run was nudged before it parked (Medium 4: `mail_messages` is
            // kept in lockstep with `mail_entries` at the one place that
            // reassigns it).
            let prompt_text = super::prompt::task_prompt_with_mail_fallback(
                &prompt_text,
                (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
                &mail_messages,
                cfg.mail.max_delivered_bytes,
            );
            let (mut rebuilt, sp) = build_headless(&prompt_text, &session, &extra);
            rebuilt.current_dir(repo);
            apply_session_env(&mut rebuilt, &session);
            command = rebuilt;
            stdin_prompt = sp;
            continue;
        }

        // Issue #227: only a genuinely non-zero `Outcome::Exited` counts --
        // the match arm above already excludes a capacity-flagged clean exit
        // from reaching here at all, and `TimedOut`/`StoppedByTick` are the
        // supervisor's own kill, never the child's own capacity-triggered
        // exit.
        let capacity_exit =
            capacity_pattern.is_some() && matches!(outcome, Outcome::Exited(code) if code != 0);

        let reason = if capacity_exit {
            "capacity"
        } else if rotted {
            "rot"
        } else {
            "timeout"
        };
        let exhausted_code = if capacity_exit {
            EXIT_CAPACITY_EXHAUSTED
        } else if rotted {
            EXIT_ROT_EXHAUSTED
        } else {
            EXIT_TIMEOUT
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "kill",
                detail: &transcript.display().to_string(),
            },
        );

        let Some(prompt_text) = prompt.clone() else {
            writeln!(
                w,
                "zirv ctx exec: {reason} detected but the original prompt is unknown, so it cannot restart. Pass --prompt to enable restarts."
            )?;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "stand-down",
                    detail: "no prompt available for restart",
                },
            );
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        };

        if restarts >= max_restarts {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "give-up",
                    detail: "restart budget exhausted",
                },
            );
            if capacity_exit {
                let label = capacity_pattern.unwrap_or("provider capacity error");
                writeln!(
                    w,
                    "zirv ctx exec: {} finished: provider capacity limit ({label}) after \
                     {restarts} restarts; workspace changes are uncommitted (exit \
                     {exhausted_code})",
                    adapter.name()
                )?;
            } else {
                writeln!(
                    w,
                    "zirv ctx exec: {reason} after {restarts} restarts, giving up with exit {exhausted_code}"
                )?;
            }
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        }

        let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
        let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
        let (note, source) = handoff::distill_or_structural(
            adapter.as_ref(),
            &distiller_model,
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
            cfg.chrome.events,
        );
        let stored = handoff::store(&state, repo, session.as_str(), &note)?;
        // N6: opt-in (`cfg.memory.harvest`, default off) and only from a
        // genuinely distilled handoff -- never the mechanical structural
        // fallback, which has nothing durable to offer. Best-effort: a
        // harvest failure must never turn a successful restart into a
        // failed one.
        if source == "distilled" {
            let _ = super::memory::harvest_durable(
                adapter.as_ref(),
                &distiller_model,
                &note,
                repo,
                &state,
                &mail_slug,
                &cfg,
            );
        }
        announcer.emit(&super::announce::Event::Restart {
            style: source.to_string(),
            stored: stored.display().to_string(),
        });

        restarts += 1;
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "restart",
                detail: &format!("{source} handoff at {}", stored.display()),
            },
        );
        writeln!(
            w,
            "zirv ctx exec: {reason} detected, restarting ({restarts}/{max_restarts}) with a {source} handoff"
        )?;

        // Issue #227: a short backoff before actually relaunching, only for
        // a capacity retry -- a rot/timeout restart is unaffected (the
        // session itself needed a fresh start, not a delay). Reuses the
        // injected `sleep_fn` (the same seam `pace::wait_for_window` already
        // relies on for tests), so no wall-clock time is spent under a fake
        // clock.
        if capacity_exit {
            let backoff = capacity_backoff_secs(restarts);
            if backoff > 0 {
                writeln!(w, "zirv ctx exec: backing off {backoff}s before retrying")?;
                sleep_fn(Duration::from_secs(backoff));
            }
        }

        // Harvest before superseding the transcript so both accounting and
        // any configured whole-run budget include this rot/timeout child.
        harvest_spend(
            adapter.as_ref(),
            &transcript,
            &mut prior_usage,
            &mut prior_tool_calls,
        );
        session = SessionId::new_v4();
        session_guard.refresh_session(session.as_str());
        // The new session writes somewhere new, so the next iteration's watcher
        // must follow it rather than the file the killed child left behind.
        transcript = derive_transcript(&session);
        prompt_args = super::prompt::injection_args_for_session(
            adapter.as_ref(),
            &[],
            composed.as_ref(),
            &state,
            session.as_str(),
        )?;
        // M2: README promises injection attribution "at every session
        // start"; a restart mints a new session id, so it needs its own
        // log entry rather than leaving attribution pinned to the first one.
        super::prompt::log_injection(
            &state,
            "exec",
            session.as_str(),
            composed.as_ref(),
            relaunch_system_prompt_supported,
        );
        announcer.emit(&super::prompt::injection_event(
            composed.as_ref(),
            relaunch_system_prompt_supported,
        ));
        let combined = format!("{prompt_text}\n\n{}", note.to_markdown());
        let combined = super::prompt::task_prompt_with_composed_fallback(
            &combined,
            relaunch_system_prompt_supported,
            composed.as_ref(),
        );
        let mail_in_composed = composed
            .as_ref()
            .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
        // A rot/timeout restart, like a park, does not itself re-list mail,
        // so the fallback for an uninjectable adapter reuses whatever
        // `mail_messages` currently holds -- the launch-time listing, or a
        // nudge's own fresher one if this run was nudged first (Medium 4).
        let combined = super::prompt::task_prompt_with_mail_fallback(
            &combined,
            (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
        );
        // M8: the user's own extra flags survive the restart too, not just
        // zirv's own (the system prompt args, and now the sandbox/policy
        // prepend) -- this used to be asymmetric.
        let extra: Vec<String> = policy_extra
            .iter()
            .cloned()
            .chain(user_extra.iter().cloned())
            .chain(prompt_args.iter().cloned())
            .collect();
        let (mut rebuilt, sp) = build_headless(&combined, &session, &extra);
        rebuilt.current_dir(repo);
        apply_session_env(&mut rebuilt, &session);
        command = rebuilt;
        stdin_prompt = sp;
    }
}

/// Reads `transcript` fresh and returns its own usage and tool-call count
/// (via `adapter.parse_events`), or `None` if it cannot be read yet -- a read
/// failure here must never be fatal, since it can just mean the child has not
/// flushed its first line. Shared by [`evaluate_worker_budget`] and
/// [`harvest_spend`] (issue #169.2), so the two can never drift on how one
/// transcript's own spend is computed.
fn record_execution_segment(
    report: &mut ExecutionReport,
    adapter: &dyn adapters::AgentAdapter,
    session: &SessionId,
    transcript: &Path,
    prior_usage: &TranscriptUsage,
    model: Option<&str>,
    started: Instant,
) {
    let current = read_transcript_spend(adapter, transcript)
        .map(|(usage, _)| usage)
        .unwrap_or_default();
    report.segments.push(ExecutionSegment {
        session: session.as_str().to_string(),
        agent: adapter.name().to_string(),
        model: model.map(str::to_string),
        usage: add_usage(prior_usage, &current),
        wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
}

fn read_transcript_spend(
    adapter: &dyn adapters::AgentAdapter,
    transcript: &Path,
) -> Option<(TranscriptUsage, u32)> {
    let body = std::fs::read_to_string(transcript).ok()?;
    let usage = adapter.transcript_usage(&body).unwrap_or_default();
    let tool_calls = adapter
        .parse_events(&body)
        .iter()
        .filter(|event| matches!(event, NormalizedEvent::ToolCall { .. }))
        .count();
    Some((usage, u32::try_from(tool_calls).unwrap_or(u32::MAX)))
}

/// Field-wise saturating sum of two [`TranscriptUsage`]s -- how a restart's
/// outgoing child's spend is folded into the running total, and how that
/// total is folded into the current child's own reading before a budget
/// check.
fn add_usage(a: &TranscriptUsage, b: &TranscriptUsage) -> TranscriptUsage {
    TranscriptUsage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        cache_creation_input_tokens: a
            .cache_creation_input_tokens
            .saturating_add(b.cache_creation_input_tokens),
        cache_read_input_tokens: a
            .cache_read_input_tokens
            .saturating_add(b.cache_read_input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
    }
}

/// Issue #169.2: folds `transcript`'s own usage and tool-call count into the
/// running `prior_usage`/`prior_tool_calls` accumulators. Called once, on the
/// OUTGOING transcript, at every restart/nudge/park site in `run_with` --
/// before a fresh session (and therefore a fresh transcript) is minted for
/// the next child. A transcript that cannot be read yet contributes nothing
/// rather than failing the restart it is called from (best-effort, matching
/// `evaluate_worker_budget`'s own tolerance).
fn harvest_spend(
    adapter: &dyn adapters::AgentAdapter,
    transcript: &Path,
    prior_usage: &mut TranscriptUsage,
    prior_tool_calls: &mut u32,
) {
    if let Some((usage, tool_calls)) = read_transcript_spend(adapter, transcript) {
        *prior_usage = add_usage(prior_usage, &usage);
        *prior_tool_calls = prior_tool_calls.saturating_add(tool_calls);
    }
}

/// Reads `transcript` fresh and evaluates `budget` against it PLUS every
/// prior child's own already-harvested spend (`prior_usage`/`prior_tool_
/// calls`, issue #169.2) -- so the ceiling bounds the whole supervised run
/// across every restart, not just whichever child happens to be running
/// right now. `None` when neither ceiling is configured (the common case,
/// and every delegation before 2.35.0) or the CURRENT transcript cannot be
/// read yet -- a read failure here must never be fatal, since it can just
/// mean the child has not flushed its first line; the next tick that can
/// read it still sees the full cumulative total, prior spend included.
///
/// Shared by `supervise_run`'s own tick (checked on every poll while the
/// child is alive) and its post-exit check just below (issue #155 review
/// finding C1): factored out so the two call sites can never drift on how
/// "spent" is computed.
fn evaluate_worker_budget(
    adapter: &dyn adapters::AgentAdapter,
    budget: agent::WorkerBudget,
    transcript: &Path,
    prior_usage: &TranscriptUsage,
    prior_tool_calls: u32,
) -> Option<agent::BudgetState> {
    if budget.tokens.is_none() && budget.tool_calls.is_none() {
        return None;
    }
    let (usage, tool_calls) = read_transcript_spend(adapter, transcript)?;
    let combined_usage = add_usage(prior_usage, &usage);
    let combined_tool_calls = prior_tool_calls.saturating_add(tool_calls);
    Some(agent::budget_state(
        &budget,
        &combined_usage,
        combined_tool_calls,
    ))
}

#[allow(clippy::too_many_arguments)]
fn supervise_run(
    child: &mut std::process::Child,
    deadline: Instant,
    poll: Duration,
    scorer: &mut score::IncrementalScorer,
    adapter: &dyn adapters::AgentAdapter,
    score_cfg: &super::config::ScoreConfig,
    state: &StateDir,
    server: Option<&signal::SignalServer>,
    session: &str,
    // C7: this run's stable registry short id, not `short_id(session)`.
    // `zirv ctx nudge` writes its wake-up marker under the address it
    // resolved from the registry, and that address does not rotate when a
    // restart mints a fresh session -- deriving it from `session` here meant
    // a nudge sent after the first restart was never claimed.
    registry_short: &str,
    rotted: &mut bool,
    // C3: set when this session reported a turn boundary of its own.
    progressed: &mut bool,
    tap: &supervise::OutputTap,
    limit_hit: &mut bool,
    nudged_by: &mut Option<String>,
    nudges_used: u32,
    max_nudges: u32,
    can_restart: bool,
    // Issue #155, Phase 5(d): a budget checkpoint, independent of rot/nudge/
    // limit above. `transcript` is read directly here rather than folded
    // into `scorer`'s own bounded fold, because a budget needs this child's
    // whole cumulative spend, which `RotState`'s windowed segments do not
    // retain once the window has moved past them.
    transcript: &Path,
    budget: agent::WorkerBudget,
    // Issue #169.2: every prior child's own already-harvested spend this
    // invocation has superseded, folded into every check below alongside
    // `transcript`'s own current reading (`evaluate_worker_budget`).
    prior_usage: &TranscriptUsage,
    prior_tool_calls: u32,
    soft_warned: &mut bool,
    budget_exhausted: &mut bool,
) -> CtxResult<Outcome> {
    // Issue #203: `evaluate_worker_budget` reads the transcript fresh on
    // every tick, so it can see a `HardStop` the instant the child's last
    // chunk lands on disk -- often milliseconds before the child itself
    // calls `exit()`. `supervise_child` checks `try_wait` *before* every
    // tick, including the very next one, but a `Tick::Stop` on this same
    // tick short-circuits straight to `terminate` and `Outcome::
    // StoppedByTick`, which carries no exit code -- so a child that was
    // already on its way out on its own has its real code discarded for
    // `EXIT_BUDGET_EXHAUSTED`. One tick of grace (`Tick::Continue` instead
    // of `Stop`, exactly once) gives that next `try_wait` a chance to
    // observe a natural exit first -- the same spirit as the `limit_hit`
    // path's own brief final drain/wait below, letting a child that is
    // already on its way out finish naturally instead of being overridden.
    // Only a child still alive on the SECOND consecutive `HardStop` tick is
    // actually killed for budget.
    let mut budget_grace_given = false;
    let mut tick = || {
        if pace::scan_for_limit(
            &tap.try_lines(),
            state,
            session,
            "exec",
            &mut std::io::stderr(),
        ) {
            *limit_hit = true;
            return Tick::Stop("limit");
        }
        if let Some(server) = server
            && let Some(received) = server.try_recv()
        {
            // C3: a turn boundary reported by *this* session is evidence it
            // got somewhere since the last nudge relaunch, which is what
            // makes the nudge budget consecutive rather than cumulative.
            // Recorded for any verdict, including the Restart one handled
            // just below: the session still did a turn's work.
            if received.session_id == session {
                *progressed = true;
            }
            if should_stop_for_signal(&received, session) {
                *rotted = true;
                return Tick::Stop("rot");
            }
        }
        // N4: claiming the marker is atomic (`remove_file`), so exactly one
        // observer ever sees `true` -- important even within one process,
        // since a stale marker from a previous cycle must never re-fire.
        // Gracefully stops the child (same `Tick::Stop` shape rot uses) only
        // when a relaunch is actually possible and the consecutive-nudge cap
        // has not been reached; otherwise the marker is still claimed (so it
        // never re-triggers) but the child runs on untouched and the mail
        // stays unread -- `nudge-ignored` in the decision log says why.
        if let Some(from) = super::sessions::claim_nudge_marker(state, registry_short) {
            if can_restart && nudges_used < max_nudges {
                // C4: the sender's own short id, read out of the marker, so
                // the announcement can name who actually nudged us.
                *nudged_by = Some(from);
                return Tick::Stop("nudge");
            }
            let _ = log::append(
                state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-ignored",
                    detail: if can_restart {
                        "consecutive nudge cap reached; message left unread"
                    } else {
                        "no prompt available for a nudge relaunch; message left unread"
                    },
                },
            );
            return Tick::Continue;
        }
        // Issue #155, Phase 5(d): `evaluate_worker_budget` itself skips the
        // transcript read entirely when no ceiling is configured (every
        // delegation before 2.35.0, and the common case even after), so a
        // run that never asked to be bounded pays nothing extra here.
        match evaluate_worker_budget(adapter, budget, transcript, prior_usage, prior_tool_calls) {
            Some(agent::BudgetState::HardStop { used, limit }) => {
                // Issue #203: give a child that is about to exit on its own
                // one poll's worth of room to do so, so `try_wait` -- not
                // this kill -- is what reports its real exit code.
                if !budget_grace_given {
                    budget_grace_given = true;
                    return Tick::Continue;
                }
                eprintln!(
                    "zirv ctx exec: token/tool-call budget exhausted ({used}/{limit}); \
                     stopping now -- this run will not restart"
                );
                *budget_exhausted = true;
                return Tick::Stop("budget");
            }
            Some(agent::BudgetState::SoftWarn { used, limit }) if !*soft_warned => {
                *soft_warned = true;
                eprintln!(
                    "zirv ctx exec: {used}/{limit} of the token/tool-call budget spent -- \
                     wrap up and checkpoint your result soon"
                );
            }
            Some(agent::BudgetState::SoftWarn { .. } | agent::BudgetState::Ok) | None => {}
        }
        // A scoring failure must never kill a healthy run.
        match scorer.poll(adapter, score_cfg) {
            Ok((Some(score), _)) if score.verdict == Verdict::Restart => {
                *rotted = true;
                Tick::Stop("rot")
            }
            _ => Tick::Continue,
        }
    };
    let outcome = supervise::supervise_child(child, deadline, poll, &mut tick)?;

    // C1 (issue #155 review finding): `supervise_child` checks `try_wait`
    // for a completed child *before* ever calling the tick above, so a
    // child that writes an over-budget final transcript and then exits
    // between two polls can race past the very last tick that would have
    // caught it -- and report its own clean exit code instead of the
    // budget stop its transcript actually earned. Caught here as a final
    // check on the transcript the child left behind, gated on
    // `Exited(0)` specifically: a child that exited with its own failure
    // code keeps that code untouched, since overriding it with a budget
    // verdict here would erase a real failure that may have nothing to do
    // with the budget at all.
    if !*budget_exhausted
        && matches!(outcome, Outcome::Exited(0))
        && let Some(agent::BudgetState::HardStop { used, limit }) =
            evaluate_worker_budget(adapter, budget, transcript, prior_usage, prior_tool_calls)
    {
        eprintln!(
            "zirv ctx exec: token/tool-call budget exhausted ({used}/{limit}) in the child's \
             final transcript; stopping now -- this run will not restart"
        );
        *budget_exhausted = true;
    }

    Ok(outcome)
}

pub fn run<W: Write>(args: &ExecArgs, w: &mut W) -> CtxResult<i32> {
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

    /// Runs the fake agent directly, so `exec` supervises a real child whose
    /// transcript path we control through `--transcript`.
    fn fake_agent_command(session: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-agent.sh").display().to_string(),
            "-p".to_string(),
            "do the work".to_string(),
            "--session-id".to_string(),
            session.to_string(),
        ]
    }

    fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                format!("sh {}", fixture("fake-agent.sh").display()),
            ),
            // T8: `run_with`'s `sleep_fn` is real `std::thread::sleep` (not
            // test-injectable -- see its own call site), and a fresh temp
            // state dir has no usage source by construction, so every test
            // built on this helper would otherwise pay the real, wall-clock
            // fail-safe delay (default 60s) on every call into `wait_for_
            // window`. Zeroed here, not by lowering the production default:
            // pace.rs's own unit tests already cover the delay's correctness
            // with a `FakeClock`, so exec.rs's tests (which are not testing
            // pacing) should not pay it in real time.
            (
                "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
                "0".to_string(),
            ),
        ]
        .into()
    }

    fn transcript_for(home: &std::path::Path, repo: &std::path::Path, session: &str) -> PathBuf {
        home.join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(repo))
            .join(format!("{session}.jsonl"))
    }

    /// Issue #220: a non-shim launch (`shim == false`) whose total argv is
    /// safely under the budget keeps the prompt on argv -- byte-for-byte the
    /// pre-#220 behavior, so an ordinary short task prompt never starts
    /// taking the stdin path it never needed.
    #[test]
    fn headless_prompt_via_stdin_stays_on_argv_when_short_and_no_shim() {
        assert!(!headless_prompt_via_stdin(false, 100));
        assert!(!headless_prompt_via_stdin(
            false,
            super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES
        ));
    }

    /// Issue #220's actual fix: a total argv over the budget routes to
    /// stdin even with no shim in play at all -- the class of overflow a
    /// `zirv workflow review run` package (full diff embedded) or a long
    /// `zirv agent codex "<...>"` prompt hits on a perfectly ordinary,
    /// direct `.exe` launch.
    #[test]
    fn headless_prompt_via_stdin_switches_to_stdin_once_the_total_argv_exceeds_the_budget() {
        assert!(headless_prompt_via_stdin(
            false,
            super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES + 1
        ));
    }

    /// The shim reason (issue #213/FIX B) must keep forcing stdin regardless
    /// of size, including for a total argv far under the budget -- this
    /// function must never regress that existing guarantee while adding the
    /// new one.
    #[test]
    fn headless_prompt_via_stdin_still_forces_stdin_for_a_shim_launch_regardless_of_size() {
        assert!(headless_prompt_via_stdin(true, 1));
    }

    /// Post-merge correctness follow-up: the #213 system-prompt layer
    /// (`--append-system-prompt <text>`/`-c developer_instructions=<json>`,
    /// folded into `extra`) rides the SAME command line as the task prompt.
    /// A prompt safely under the budget by itself must still route to
    /// stdin once that other argument pushes the WHOLE argv over budget --
    /// `headless_argv_len` is what has to catch this, not `prompt_text.
    /// len()` alone (the bug this follow-up fixes).
    #[test]
    fn headless_argv_len_counts_every_argument_not_just_the_prompt() {
        let prompt = "short task prompt";
        assert!(prompt.len() < super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES);

        let mut under_budget = Command::new("claude");
        under_budget
            .arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg("abc");
        assert!(!headless_prompt_via_stdin(
            false,
            headless_argv_len(&under_budget)
        ));

        // The system-prompt layer alone is large enough to push the total
        // over budget, even though the prompt itself stayed small.
        let large_system_prompt = "y".repeat(super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES);
        let mut over_budget = Command::new("claude");
        over_budget
            .arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg("abc")
            .arg("--append-system-prompt")
            .arg(&large_system_prompt);
        let total = headless_argv_len(&over_budget);
        assert!(
            total > super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES,
            "the extra system-prompt argument must be counted toward the total: {total}"
        );
        assert!(
            headless_prompt_via_stdin(false, total),
            "a prompt safely under budget on its own must still route to stdin once the other \
             arguments on the same command line push the WHOLE argv over budget"
        );
    }

    /// Review follow-up regression: raw byte length alone under-counts a
    /// quote-and-backslash-heavy prompt. Windows' `CreateProcessW`/
    /// `CommandLineToArgvW` quoting escapes every `"` and doubles a run of
    /// backslashes ahead of one, and wraps a whitespace-bearing argument in
    /// its own surrounding quotes, so a prompt built almost entirely of `"`
    /// and `\` characters can measure comfortably UNDER the raw-byte budget
    /// and still expand past the real 32,767-char Windows command-line
    /// limit -- reproducing os error 206 despite `headless_argv_len`'s own
    /// budget check. This prompt is constructed to sit just under the
    /// raw-byte budget by itself; the escaping-aware estimate must still
    /// route it to stdin.
    #[test]
    fn a_quote_and_backslash_heavy_prompt_under_the_raw_budget_still_routes_to_stdin() {
        let budget = super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES;
        let half = (budget - 200) / 2;
        let prompt = format!("{}{}", "\"".repeat(half), "\\".repeat(half));
        assert!(
            prompt.len() < budget,
            "the raw prompt must sit under the raw-byte budget by construction: {} vs {budget}",
            prompt.len()
        );

        let mut command = Command::new("claude");
        command
            .arg("-p")
            .arg(&prompt)
            .arg("--session-id")
            .arg("abc");
        let total = headless_argv_len(&command);
        assert!(
            headless_prompt_via_stdin(false, total),
            "a prompt under the raw-byte budget but heavy on quote/backslash characters must \
             still route to stdin once Windows' own command-line quoting is estimated: raw {} \
             vs budget {budget}, escaping-aware total {total}",
            prompt.len()
        );
    }

    /// Final wave item 1: `adapter.launches_through_cmd_shim()` only
    /// recognises the `cmd.exe /c <shim>` form -- a `.ps1`-resolved
    /// `agent_bin` used to report "safe" here (prompt stays on argv) while
    /// still actually launching through `powershell -File`, which reparses
    /// that argv exactly like a `.cmd` shim does. `prompt_delivery_via_
    /// stdin` must report `true` for it too, mirroring the `.cmd` case and
    /// the same fix dash/mod.rs already got for the pty path.
    #[cfg(windows)]
    #[test]
    fn prompt_delivery_via_stdin_recognises_a_powershell_shim_not_just_a_cmd_one() {
        let dir = tempfile::tempdir().expect("tempdir");

        let cmd_shim = dir.path().join("codex.cmd");
        std::fs::write(&cmd_shim, "@echo off\r\n").expect("write cmd shim");
        let cmd_adapter = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            &cmd_shim.display().to_string(),
        ));
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        assert!(
            prompt_delivery_via_stdin(&cmd_adapter, &session),
            "the .cmd shim shape must still be recognised"
        );

        let ps_shim = dir.path().join("codex.ps1");
        std::fs::write(&ps_shim, "exit 0\r\n").expect("write ps1 shim");
        let ps_adapter = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            &ps_shim.display().to_string(),
        ));
        assert!(
            prompt_delivery_via_stdin(&ps_adapter, &session),
            "the .ps1 shim shape must also route the prompt to stdin"
        );

        let direct = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            "/tmp/fake-codex-not-a-real-path",
        ));
        assert!(
            !prompt_delivery_via_stdin(&direct, &session),
            "a non-shim program must keep the prompt on argv"
        );
    }

    /// The compiler seam used by `run_with` must carry the bounded memory core.
    #[test]
    fn compose_worker_launch_prompt_carries_the_memory_layer_under_its_configured_cap() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(repo.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.core_max_bytes = 40;
        // Issue #155: the merged memory layer is capped by the SUM of the two
        // budgets now, not `core_max_bytes` alone -- zero the retrieval half
        // out so this test's tiny budget still actually bounds what gets
        // delivered.
        cfg.memory.retrieval_max_bytes = 0;
        let slug = crate::commands::ctx::state::repo_slug(repo.path());

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

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let composed = crate::commands::ctx::compile::compile(
            Some(&home),
            repo.path(),
            false,
            &cfg,
            &adapter,
            super::super::prompt::PromptRole::Worker,
            &state,
            1,
            crate::commands::ctx::adapters::LaunchMode::Headless,
            false,
        )
        .composed
        .expect("a worker launch still composes a prompt");

        assert!(
            composed.text.contains("seam-fact"),
            "the memory core layer must reach the composed prompt: {}",
            composed.text
        );
        assert!(
            !composed.text.contains("TAIL_MARKER_NOT_TRUNCATED"),
            "a tiny core_max_bytes must actually bound the delivered memory layer: {}",
            composed.text
        );
        assert!(
            composed.text.contains("[memory truncated:"),
            "the truncation must be visible, not silent: {}",
            composed.text
        );
    }

    /// Guards `.config/nextest.toml`'s `exec-nudge-restart` group against
    /// silent membership rot: its `filter = 'test(a) or test(b) or ...'`
    /// enumerates 8 test names verbatim, and nextest silently matches
    /// nothing for a clause naming a test that does not exist rather than
    /// erroring -- so a rename here would silently drop a test out of the
    /// serialized group with no signal anywhere. This extracts every
    /// `test(NAME)` clause from that override's filter and asserts each
    /// NAME still resolves to a real `fn` in this file. `include_str!` on
    /// this very file is deliberate, not an accident -- the whole
    /// exec-nudge-restart family lives here -- and the check is on the
    /// exact `fn NAME(` byte pattern (not a loose substring match) so it
    /// cannot be fooled by a name that only ever appears as this test's own
    /// dynamically-parsed data, never as a real function definition. The
    /// reverse direction (every such-shaped `fn` also present in the
    /// filter) is not checked: there is no reliable lexical marker that
    /// distinguishes a member of this family from any other test.
    #[test]
    fn the_nextest_exec_nudge_restart_group_names_still_resolve() {
        const NEXTEST_TOML: &str =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/.config/nextest.toml"));
        const THIS_FILE: &str = include_str!("exec.rs");

        let block = NEXTEST_TOML
            .split("[[profile.default.overrides]]")
            .find(|block| block.contains("test-group = 'exec-nudge-restart'"))
            .expect("nextest.toml must still have an override naming the exec-nudge-restart group");
        let filter_line = block
            .lines()
            .find(|line| line.trim_start().starts_with("filter = "))
            .expect("the exec-nudge-restart override must still have a filter line");

        let mut names: Vec<&str> = Vec::new();
        let mut rest = filter_line;
        while let Some(start) = rest.find("test(") {
            let after = &rest[start + "test(".len()..];
            let end = after
                .find(')')
                .expect("every test( clause in the filter must close with a )");
            names.push(&after[..end]);
            rest = &after[end + 1..];
        }

        assert!(
            names.len() >= 8,
            "expected at least the 8 known exec-nudge-restart tests, found {}: {:?}",
            names.len(),
            names
        );

        for name in names {
            let needle = format!("fn {name}(");
            assert!(
                THIS_FILE.contains(&needle),
                "nextest.toml's exec-nudge-restart filter names `{name}`, which no longer \
                 resolves to `fn {name}(` in exec.rs -- nextest silently drops a clause like \
                 this rather than erroring, so the test just as silently fell out of the \
                 serialized group"
            );
        }
    }

    #[test]
    fn prompt_extraction_finds_the_dash_p_argument() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert_eq!(extract_prompt(&cmd), Some("fix the bug".to_string()));
    }

    #[test]
    fn prompt_extraction_handles_print_and_positional_forms() {
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--print".to_string(),
                "go".to_string()
            ]),
            Some("go".to_string())
        );
        assert_eq!(
            extract_prompt(&["codex".to_string(), "exec".to_string(), "go".to_string()]),
            Some("go".to_string())
        );
    }

    #[test]
    fn prompt_extraction_gives_up_rather_than_guessing() {
        assert_eq!(
            extract_prompt(&["claude".to_string(), "-p".to_string()]),
            None
        );
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--resume".to_string(),
                "abc".to_string()
            ]),
            None
        );
        assert_eq!(extract_prompt(&[]), None);
    }

    /// M8: only the prompt and `--session-id` (both regenerated fresh on
    /// every restart) are stripped; everything else the operator passed
    /// survives.
    #[test]
    fn extra_launch_flags_strips_only_the_prompt_and_session_id() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn extra_launch_flags_is_empty_when_the_command_is_only_prompt_and_session_id() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert!(extra_launch_flags(&cmd, 1, None, "claude").is_empty());
    }

    /// A markdown bullet list is an ordinary prompt. Reading it as a flag left
    /// the `-p` pair in the operator's flags, so every restart passed the
    /// prompt twice: once with the handoff, once without, and the second one
    /// won.
    #[test]
    fn a_prompt_that_starts_with_a_dash_is_still_stripped_from_the_restart_flags() {
        let prompt = "- fix the failing tests\n- then run cargo fmt";
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, Some(prompt), "claude"),
            vec!["--model".to_string(), "opus".to_string()],
            "the prompt zirv already holds is recognised by value, not by shape"
        );
    }

    /// Without the prompt to compare against, a value shaped like a flag still
    /// reads as one -- but only the flag is dropped, never the token after it,
    /// which belongs to the operator.
    #[test]
    fn a_bare_prompt_flag_drops_itself_and_keeps_what_follows() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "--verbose".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "claude"),
            vec!["--verbose".to_string()]
        );
    }

    /// `headless_cmd` rebuilds the program invocation on every relaunch, so a
    /// launcher in front of the agent (or a positional prompt) must not come
    /// back as a stray argument the agent reads as a second prompt.
    #[test]
    fn the_program_invocation_is_never_carried_into_the_restart_flags() {
        let via_npx = vec![
            "npx".to_string(),
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
        ];
        assert!(
            extra_launch_flags(&via_npx, 1, Some("task"), "claude").is_empty(),
            "the launcher's own argument is part of the invocation, not a flag"
        );

        // `agent_bin = "/usr/bin/env claude"`: the adapter reports a prefix of
        // two, because that is how many tokens it spends before the flags.
        let via_env = vec![
            "/usr/bin/env".to_string(),
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
        ];
        assert!(extra_launch_flags(&via_env, 2, Some("task"), "claude").is_empty());

        let positional = vec!["claude".to_string(), "task".to_string()];
        assert!(extra_launch_flags(&positional, 1, Some("task"), "claude").is_empty());
    }

    /// A restart exists to escape the conversation that rotted. Every spelling
    /// that would pin it back to that conversation has to go.
    #[test]
    fn nothing_that_pins_the_launch_to_the_dead_session_survives_a_restart() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
            "--session-id=OLD".to_string(),
            "--continue".to_string(),
            "--resume".to_string(),
            "abc".to_string(),
            "--fork-session".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, Some("task"), "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// D3: the shared predicate `chat::dash_orchestrator_pane` asks before
    /// appending a session pin of its own. Both spellings of every
    /// value-carrying flag, and every bare one.
    #[test]
    fn pins_an_existing_conversation_recognises_every_resume_spelling() {
        let yes = [
            vec!["claude", "--resume", "abc"],
            vec!["claude", "--resume=abc"],
            vec!["claude", "--session-id", "abc"],
            vec!["claude", "--session-id=abc"],
            vec!["claude", "-c"],
            vec!["claude", "--continue"],
            vec!["claude", "--fork-session"],
        ];
        for argv in yes {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(
                pins_an_existing_conversation(&owned, "claude"),
                "must be recognised as a pin: {argv:?}"
            );
        }

        let no = [
            vec!["claude", "--model", "opus"],
            vec!["claude", "-p", "resume the migration"],
            vec!["claude"],
        ];
        for argv in no {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(
                !pins_an_existing_conversation(&owned, "claude"),
                "must not be mistaken for a pin: {argv:?}"
            );
        }
    }

    /// Issue #143: codex's own `-c, --config <key>=<value>` must never be
    /// mistaken for claude's bare resume shorthand -- codex has no verified
    /// pin flag at all (it always mints its own session id), so nothing in
    /// its own argv can mean "this pins an existing conversation".
    #[test]
    fn pins_an_existing_conversation_is_always_false_for_an_adapter_with_no_resume_flags() {
        let argv = vec![
            "codex".to_string(),
            "-c".to_string(),
            "approval_policy=never".to_string(),
        ];
        assert!(!pins_an_existing_conversation(&argv, "codex"));
    }

    /// `--resume` with a flag after it took no value, so swallowing the next
    /// token would eat one of the operator's own flags.
    #[test]
    fn a_valueless_resume_does_not_swallow_the_next_flag() {
        let cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn extra_launch_flags_keeps_everything_when_there_is_no_prompt_or_session_id() {
        let cmd = vec![
            "codex".to_string(),
            "--model".to_string(),
            "gpt".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "codex"),
            vec!["--model".to_string(), "gpt".to_string()]
        );
    }

    /// Issue #143: codex's own `-c` (`-c, --config <key>=<value>`) collided
    /// with claude's bare `-c`/`--continue` resume shorthand. `RESUME_FLAGS_
    /// BARE` used to be matched regardless of adapter, so a bare `-c` was
    /// dropped as claude's resume flag -- which takes no value -- while the
    /// very next token (codex's own config VALUE, e.g.
    /// `approval_policy=never`) survived untouched, landing on argv detached
    /// from its own flag. Real codex-cli then rejects it outright: `error:
    /// unexpected argument 'approval_policy=never' found`.
    #[test]
    fn extra_launch_flags_keeps_codexs_own_c_flag_paired_with_its_value() {
        let cmd = vec![
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "-c".to_string(),
            "approval_policy=never".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 0, None, "codex"),
            cmd,
            "codex's own -c/--config flag must never be mistaken for claude's bare resume \
             shorthand, which does not exist on this adapter at all"
        );
    }

    /// The same collision, the other direction: claude's own bare `-c` must
    /// still be recognised and stripped exactly as before -- this fix must
    /// not weaken the resume-flag guard for the adapter it actually protects.
    #[test]
    fn extra_launch_flags_still_strips_claudes_own_bare_c_flag() {
        let cmd = vec!["-c".to_string(), "--model".to_string(), "opus".to_string()];
        assert_eq!(
            extra_launch_flags(&cmd, 0, None, "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// F2: the nesting guard gates the *interactive* verbs only. Delegating
    /// to a headless worker from inside a session is the entire point of
    /// `zirv ctx agent`, and a worker never takes the shared console over,
    /// so `exec` must run normally with every piece of evidence the guard
    /// keys on present at once.
    ///
    /// A trivial shell command rather than the fake agent: this test is about
    /// what `run_with` refuses, not about supervision, and it should stay
    /// runnable on both platforms.
    /// C3: the cap counts *consecutive* nudge restarts, which is what
    /// `[supervise] max_nudges` has always claimed to bound. It was
    /// implemented cumulatively, so a long-lived session that did real work
    /// between nudges permanently exhausted its budget anyway.
    #[test]
    fn the_nudge_cap_resets_once_the_session_makes_progress() {
        // Without progress the budget is spent and stays spent.
        assert_eq!(nudges_after(0, false), 0);
        assert_eq!(nudges_after(2, false), 2);
        assert_eq!(nudges_after(3, false), 3);

        // A turn boundary from the session ends the consecutive run.
        assert_eq!(
            nudges_after(3, true),
            0,
            "a session that got somewhere may be nudged again"
        );
        assert_eq!(nudges_after(1, true), 0);
    }

    #[test]
    fn a_headless_exec_is_not_subject_to_the_nesting_guard() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let mut env = base_env(&tmp.path().join("state"));
        for (key, value) in [
            (
                adapters::SESSION_ENV,
                "abcdef12-3456-4789-8abc-def012345678",
            ),
            (adapters::SOCKET_ENV, "/tmp/outer.sock"),
            ("CLAUDE_PID", "4242"),
            ("CLAUDECODE", "1"),
        ] {
            env.insert(key.to_string(), value.to_string());
        }

        let command: Vec<String> = if cfg!(windows) {
            ["cmd", "/c", "exit", "0"]
        } else {
            ["sh", "-c", "exit 0", "--"]
        }
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: true,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a headless worker legitimately runs inside a session");
        assert_eq!(
            code,
            0,
            "exec ran the child rather than refusing: {}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn a_healthy_run_exits_with_the_childs_own_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    /// T11: the fail-safe blind delay (T8) actually reaches the injected
    /// `sleep_fn` with the right duration -- proof this path is real, not
    /// just claimed by `pace.rs`'s own unit tests, which never touch this
    /// integration seam at all. `base_env` zeros `ZIRV_CTX_PACE_BLIND_DELAY_
    /// SECS` for every other test in this file (see its own doc comment);
    /// this test overrides it back to a small nonzero value specifically so
    /// there is something real to observe, then verifies the observation
    /// through a recording `sleep_fn` rather than actually blocking --
    /// exactly the seam `pace.rs`'s own `FakeClock` tests already use, now
    /// available one layer up.
    #[test]
    fn the_blind_delay_reaches_the_injected_sleep_fn_with_the_right_duration() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "33333333-2222-4333-8444-555555555555";
        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
            "2".to_string(),
        );

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
        assert_eq!(
            slept.borrow().first().copied(),
            Some(2),
            "the blind-mode delay must actually be slept via the injected sleep_fn, got {:?}",
            slept.borrow()
        );
    }

    #[test]
    fn a_failing_child_propagates_its_exit_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3);
    }

    /// `FAKE_AGENT_MODE` applies to every invocation, so both the original child
    /// and the restarted one rot and the budget runs out.
    #[test]
    fn a_rotted_run_is_killed_restarted_and_capped() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "the caller applies its own policy after the budget is spent"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(
            log.contains("\"action\":\"restart\""),
            "a restart was attempted: {log}"
        );
        assert!(
            log.contains("\"action\":\"give-up\""),
            "and then it stopped: {log}"
        );

        let handoffs = state.join("handoffs");
        let stored: Vec<_> = walk_md(&handoffs);
        assert!(
            !stored.is_empty(),
            "a handoff is written before each restart"
        );
    }

    /// The "restarts >= max_restarts" give-up exit used to return without
    /// ever calling `record_execution_segment`, silently dropping the
    /// harvested spend for the child that just rotted from `ExecutionReport`.
    /// `max_restarts: 0` means give-up fires on the very first rot, so
    /// exactly one child ran and exactly one segment must be recorded for it.
    #[test]
    fn an_exhausted_restart_budget_still_records_its_final_segment() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "33333333-3333-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let result = run_with_report(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        let (code, report) = result.expect("runs");
        assert_eq!(code, EXIT_ROT_EXHAUSTED);
        assert_eq!(
            report.segments.len(),
            1,
            "the rotted child's spend must still be recorded before giving up: {:?}",
            report.segments
        );
    }

    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }

    fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(home.join(".claude/projects")) else {
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

    /// The restarted child is a new session writing to a new transcript, so
    /// supervision must follow it there. If the watcher kept polling the killed
    /// child's rotted file, this healthy second child would be killed too and
    /// the run would exit 75 instead of 0.
    #[test]
    fn a_restart_supervises_the_new_sessions_transcript() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "88888888-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // First child rots, second is healthy.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the healthy restarted child must be allowed to finish"
        );

        let found = transcripts_in(&home);
        assert_eq!(found.len(), 2, "one transcript per session: {found:?}");
        let first = transcript_for(&home, tmp.path(), session);
        assert!(
            found.contains(&first),
            "the original session's transcript: {found:?}"
        );
        assert!(
            found.iter().any(|p| *p != first),
            "the restarted session wrote its own transcript: {found:?}"
        );
    }

    #[test]
    fn a_run_with_no_discoverable_prompt_refuses_to_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "44444444-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "rot was detected but no restart was possible"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("cannot restart"),
            "say why supervision stood down: {text}"
        );
    }

    /// Same "no prompt to restart with" exit as
    /// `a_run_with_no_discoverable_prompt_refuses_to_restart`, but through
    /// `run_with_report`: exactly one child ever ran, so the accounting
    /// caller (`agent.rs`'s `append_execution_segments`) must see exactly one
    /// segment. Two identical `record_execution_segment` calls on this exit
    /// path used to double it, double-summing cost and writing the
    /// delegation log entry twice.
    #[test]
    fn the_no_prompt_exit_records_exactly_one_execution_segment() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "44444444-3333-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let result = run_with_report(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        let (code, report) = result.expect("runs");
        assert_eq!(code, EXIT_ROT_EXHAUSTED);
        assert_eq!(
            report.segments.len(),
            1,
            "exactly one child ran, so exactly one segment must be recorded: {:?}",
            report.segments
        );
    }

    /// The old warning about a missing prompt only ever surfaced once a
    /// restart was already needed (see `a_run_with_no_discoverable_prompt_
    /// refuses_to_restart` above), so a healthy run that never rots gave the
    /// operator no signal at all that restarts were a dead end for this
    /// invocation. It must appear upfront, regardless of whether the run
    /// ever actually needs to restart.
    #[test]
    fn an_upfront_warning_appears_even_when_the_run_never_needs_to_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "eeeeeeee-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "a healthy run that never rots must still succeed"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("--prompt") && text.to_lowercase().contains("restart"),
            "an upfront warning must appear even though this run never needed to restart: {text}"
        );
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let tmp = crate::commands::ctx::testenv::repo();
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: None,
            simple: false,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to supervise");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    /// Finding #1: `exec` launches/supervises a harness, so a syntax error in
    /// the operator's own HOME `ctx.toml` must refuse outright rather than
    /// silently falling back to permissive pacing/policy/sandbox defaults --
    /// unlike a repo-layer parse failure (still skipped, see `CtxConfig::
    /// load_for_launch`'s own doc comment) and unlike `status`, which stays
    /// on plain `load` and keeps reporting instead of refusing.
    #[test]
    fn a_home_layer_syntax_error_refuses_to_launch_naming_the_file() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(home.join(".zirv/ctx.toml"), "[score\n").expect("write broken home layer");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: None,
            simple: false,
            command: vec!["true".to_string()],
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("a broken home layer must refuse to launch");
        let msg = err.to_string();
        assert!(
            msg.contains(&home.join(".zirv").join("ctx.toml").display().to_string()),
            "names the broken file: {msg}"
        );
    }

    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    fn signal_with(verdict: Verdict, score: u32) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn: 4,
            score,
            verdict,
            transcript_path: None,
        }
    }

    #[test]
    fn only_a_restart_signal_stops_the_run() {
        assert!(should_stop_for_signal(
            &signal_with(Verdict::Restart, 95),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Compact, 65),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Advise, 45),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Healthy, 0),
            "s"
        ));
    }

    /// The socket path is derived from the first eight hex characters of a
    /// session id, so a stale hook or a neighbouring run can reach it. Killing
    /// a healthy child on someone else's verdict is the failure to avoid.
    #[test]
    fn a_verdict_about_another_session_is_ignored() {
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Restart, 95),
            "a-different-session"
        ));
    }

    /// Every restart is a new session, and the hook inside it reports whatever
    /// `ZIRV_CTX_SESSION` says. Leave that pinned to the dead session's id and
    /// the session check above rejects every signal the restart produces.
    #[test]
    fn a_restarted_child_is_told_its_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));
        let seen = tmp.path().join("sessions.txt");

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &seen);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged: Vec<String> = std::fs::read_to_string(&seen)
            .expect("the children recorded their session env")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(logged.len(), 2, "one line per child: {logged:?}");
        assert_eq!(logged[0], session, "the first child owns the given id");

        let first = transcript_for(&home, tmp.path(), session);
        let restarted = transcripts_in(&home)
            .into_iter()
            .find(|path| *path != first)
            .expect("the restarted child wrote its own transcript");
        let restarted_session = restarted
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("session id from the transcript name");
        assert_eq!(
            logged[1], restarted_session,
            "the restart must export the new session id, not the dead one's"
        );
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "55555555-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(1),
            simple: false,
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_TIMEOUT);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline must not wait for the child"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"timeout\""), "got {log}");
    }

    /// Issue #155, Phase 5(d), end to end: `hang` mode writes its whole
    /// transcript (12 turns, well over the tiny budget below) then never
    /// exits, so the very first budget check after spawn -- not the
    /// deadline, set generously long here -- is what actually stops it.
    /// Proves `EXIT_BUDGET_EXHAUSTED` is wired all the way from `ExecArgs`
    /// through `supervise_run`'s tick to the exit code `run_with` returns,
    /// and that it terminates outright rather than restarting.
    #[test]
    fn a_token_budget_stops_a_hanging_child_before_its_wall_clock_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "77777777-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // Fast polling, so the first budget check lands well inside the test
        // timeout below rather than waiting out the 2s production default.
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            // Far below what `hang` mode's own fixed 12-turn transcript
            // totals (24 assistant events x 20_000 cache-read tokens each).
            budget_tokens: Some(10_000),
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_BUDGET_EXHAUSTED);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the budget check must fire long before the 30s deadline"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"budget\""), "got {log}");
    }

    /// C1 (issue #155 review finding): `supervise_child` checks `try_wait`
    /// for a completed child *before* ever running the tick that evaluates
    /// the budget, so a child that writes its whole transcript and exits
    /// promptly -- exactly what `healthy` mode does -- can race past every
    /// tick that would have caught it and report its own clean `0` instead
    /// of the budget stop its transcript actually earned. The `hang`-mode
    /// budget test above cannot exercise this path at all, since a hanging
    /// child never exits on its own; this one proves the post-exit check
    /// added to `supervise_run` (not the tick) is what catches it.
    #[test]
    fn a_clean_exit_with_an_over_budget_final_transcript_reports_budget_exhausted() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "88888888-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            // Far below `healthy` mode's fixed 12-turn transcript total (24
            // assistant events x 20_000 cache-read tokens each). The child
            // exits on its own well before `max_restarts` above could ever
            // matter -- this is the "clean, over-budget exit" case, not a
            // restart.
            budget_tokens: Some(10_000),
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_BUDGET_EXHAUSTED,
            "a clean exit must not hide an over-budget final transcript"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"budget\""), "got {log}");
    }

    /// The other half of C1: a clean exit whose final transcript is
    /// comfortably under budget must be returned untouched, not overridden
    /// just because a budget was configured at all.
    #[test]
    fn a_clean_under_budget_exit_keeps_its_own_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            // Comfortably above `healthy` mode's fixed 480_000-token total.
            budget_tokens: Some(1_000_000),
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    /// C1's asymmetry: a child that exited with its OWN failure code keeps
    /// that code even when its final transcript is over budget -- only a
    /// clean (`0`) exit is eligible to be overridden with
    /// `EXIT_BUDGET_EXHAUSTED`, so a budget verdict never erases a real
    /// failure it may have nothing to do with.
    #[test]
    fn a_failed_exit_with_an_over_budget_transcript_keeps_its_failure_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "10101010-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            // `fail` mode writes the same over-budget transcript `healthy`
            // does, then exits 3.
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: Some(10_000),
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            3,
            "a real failure code must survive an over-budget final transcript"
        );
    }

    /// C2 (issue #155 review finding): the codex adapter's own
    /// `parse_events` never emits `NormalizedEvent::ToolCall` (see its own
    /// doc comment), so `--max-tool-calls` would otherwise be accepted for
    /// a codex worker and then silently never fire. Refused up front,
    /// before anything is spawned -- no fake agent, no transcript, nothing
    /// to poll -- the same shape
    /// `the_delegation_verb_refuses_an_agent_the_settings_file_disabled`
    /// uses in `agent.rs` for a different early refusal.
    #[test]
    fn max_tool_calls_is_refused_up_front_for_the_codex_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: Some("20202020-2222-4333-8444-555555555555".to_string()),
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: Some(5),
            timeout_secs: Some(30),
            simple: false,
            command: vec!["codex".to_string(), "exec".to_string()],
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex cannot count tool calls");
        let msg = err.to_string();
        assert!(msg.contains("--max-tool-calls"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
    }

    /// The other half of C2: an adapter that CAN count tool calls (claude,
    /// the default) must not be caught by the same refusal just because a
    /// budget was configured at all.
    #[test]
    fn max_tool_calls_is_accepted_for_the_claude_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "21212121-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            // Comfortably above what `healthy` mode's fixed 12-turn
            // transcript could ever produce (it has no tool calls to count
            // either, but the point here is that the flag is accepted at
            // all, not rejected before the child ever runs).
            max_tool_calls: Some(1_000),
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_child_is_told_where_the_socket_is() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "66666666-2222-4333-8444-555555555555";
        let env = base_env(&state);
        let marker = tmp.path().join("socket-env.txt");

        // A child that records the socket env it inherited, then exits.
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$ZIRV_CTX_SOCKET\" > {}; exit 0",
                marker.display()
            ),
        ];

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let seen = std::fs::read_to_string(&marker).expect("marker written");
        assert!(seen.ends_with(".sock"), "socket path exported: {seen}");
        assert!(seen.contains("66666666"), "per-session socket: {seen}");
    }

    #[test]
    fn an_unbindable_socket_does_not_stop_the_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "77777777-2222-4333-8444-555555555555";
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("x".repeat(120));
        let mut env = base_env(&long_state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0, "polling still supervises the run");
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
    fn a_limit_hit_hands_over_to_an_enabled_alternate_before_parking() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        // A confirmed vendor limit is stronger evidence than proactive pacing,
        // so fallback remains useful even when the operator disabled the pace
        // gate itself.
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        // The first (codex) child reports a hard limit. The same fixture then
        // stands in for the selected claude continuation and exits cleanly.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("finish the requested work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"harness-handover\""),
            "the confirmed limit should cross harnesses: {log}"
        );
        assert!(
            log.contains("codex -> claude"),
            "the selected route should be transparent: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "an admissible alternate should be used before the legacy park path: {log}"
        );
        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            argv.contains("finish the requested work"),
            "the logical task must survive the handoff: {argv}"
        );
    }

    #[test]
    fn a_limit_hit_parks_and_relaunches_without_spending_the_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // A reset one second out plus no jitter keeps the park short; the point
        // is that it parks and relaunches, not how long it waits.
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());

        // First child hits the limit, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero budget: a limit hit must park even with no restarts allowed,
            // because a park is not a restart.
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the relaunched child finished cleanly, so exec exits with its code"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "a park must not consume the restart budget: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the relaunch is a new session with its own transcript"
        );
    }

    /// Wording that only loosely resembles a usage-limit notice leaves a
    /// breadcrumb in the decision log and changes nothing else: the run is not
    /// parked, and its exit code is still the child's own.
    #[test]
    fn a_loose_limit_wording_is_noted_without_parking_the_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "77777777-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "drift");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0, "a breadcrumb is not a park");
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"limit-wording-drift\""),
            "the drift must be recorded: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "and it must never park a healthy run: {log}"
        );
    }

    // -- Issue #227: provider capacity errors -------------------------------

    /// A worker that hits a transient provider capacity error (codex's
    /// `Selected model is at capacity`) is restarted within the existing
    /// restart budget, with a short backoff between attempts -- not parked
    /// (there is no usage-window reset to wait for) and not silently
    /// returned as a bare `exit 1`.
    #[test]
    fn a_capacity_error_restarts_within_budget_with_a_backoff() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "11111111-c000-4333-8444-555555555555";
        let env = base_env(&state);

        // First child hits a capacity error, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "capacity\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the restarted child finished cleanly, so exec exits with its code"
        );
        // T8's blind-mode pacing delay also calls `sleep_fn` (with `0`, since
        // `base_env` zeros `ZIRV_CTX_PACE_BLIND_DELAY_SECS`) ahead of every
        // launch, so the capacity backoff is not necessarily the only entry
        // -- just the one call for its own real, nonzero duration.
        assert_eq!(
            slept.borrow().iter().filter(|&&secs| secs == 15).count(),
            1,
            "the first capacity retry backs off 15s exactly once via the injected sleep_fn, got {:?}",
            slept.borrow()
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"verdict\":\"capacity\"") && log.contains("\"action\":\"restart\""),
            "a capacity retry is a restart, not a park or a bare failure: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "a capacity error has no usage window to park against: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the retry is a new session with its own transcript"
        );
    }

    /// Once the restart budget is spent, a capacity error gives up with a
    /// dedicated exit code and a stderr line naming the pattern and attempt
    /// count -- never a bare `exit 1`.
    #[test]
    fn a_capacity_error_exhausts_the_restart_budget_with_a_structured_reason() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "22222222-c000-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "capacity");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_CAPACITY_EXHAUSTED);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("Selected model is at capacity"),
            "names the matched pattern: {printed}"
        );
        assert!(
            printed.contains("0 restarts"),
            "names the attempt count: {printed}"
        );
        assert!(
            printed.contains("uncommitted"),
            "warns that workspace changes are uncommitted: {printed}"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"verdict\":\"capacity\"") && log.contains("\"action\":\"give-up\""),
            "got {log}"
        );
    }

    // -- Issue #227 (operator follow-up): account/billing exhaustion --------

    /// An account/billing exhaustion (e.g. `insufficient_quota`) is a hard,
    /// non-retryable condition: the worker gives up immediately, spending
    /// none of the restart budget, even though it is configured.
    #[test]
    fn an_account_exhaustion_gives_up_immediately_without_spending_the_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-c000-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "account");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // A generous budget: an account exhaustion must never touch it.
            max_restarts: Some(5),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_ACCOUNT_EXHAUSTED);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("not retryable"),
            "must say this cannot be fixed by restarting: {printed}"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"account-exhausted\""),
            "got {log}"
        );
        assert!(
            !log.contains("\"action\":\"restart\""),
            "must never restart on an account exhaustion: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            1,
            "no retry was attempted -- exactly the one child that failed"
        );
    }

    #[test]
    fn an_exhausted_window_delays_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "aaaaaaaa-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0, "a pause is never an exit");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(1),
            "it should have waited before spawning"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    /// Bug B seam coverage (2026-08-22, fix round 3): `exec.rs` is one of
    /// the three seams that had only full-suite-green plus log inspection
    /// backing its own `policy_extra` wiring, not a dedicated exact-argv
    /// test -- exactly the shape of regression that would not fail any
    /// existing test if this seam silently lost its policy prefix. Asserts
    /// the real argv the launched child receives (`FAKE_AGENT_ARGV_LOG`),
    /// not merely that the run succeeded.
    #[test]
    fn the_initial_launch_carries_the_shipped_sandbox_posture() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "cccccccc-3333-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("--permission-mode") && argv.contains("dontAsk"),
            "the shipped-default posture must reach the real launched argv: {argv}"
        );
        assert!(
            argv.contains("--allowedTools=") && argv.contains("Edit(./**)"),
            "the generated permission set must reach it too: {argv}"
        );
    }

    #[test]
    fn a_restart_relaunches_with_the_system_prompt_too() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("--append-system-prompt"),
            "the restarted child must carry the prompt too: {argv}"
        );
    }

    /// M8: a restart used to rebuild the headless command from scratch with
    /// only zirv's own added flags (the system prompt), silently dropping any
    /// extra flag the operator themselves had passed after `--`. Only lines
    /// carrying `--session-id` are real agent invocations (a `--help` probe,
    /// if any ran, never gets one), so filtering on it keeps this assertion
    /// meaningful regardless of what else shares the log.
    #[test]
    fn a_restart_preserves_the_users_own_extra_flags_not_just_zirvs() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "12121212-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--zzz-custom-flag".to_string());
        command.push("custom-value".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        let invocations: Vec<&str> = argv
            .lines()
            .filter(|line| line.contains("--session-id"))
            .collect();
        assert_eq!(
            invocations.len(),
            2,
            "one real invocation per child: {argv:?}"
        );
        for line in &invocations {
            assert!(
                line.contains("--zzz-custom-flag") && line.contains("custom-value"),
                "the user's own extra flag must survive every restart, not just the first spawn: {argv}"
            );
        }
    }

    /// M2: README promises that "whether a prompt was injected, and from
    /// which layers, is recorded in the decision log at every session
    /// start". A restart mints a new session id, so its own attribution
    /// entry must be logged under that id too, not only the first session's.
    #[test]
    fn injection_is_logged_again_for_each_restarts_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "ffffffff-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        let injected_sessions: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("\"action\":\"prompt-injected\""))
            .filter_map(|l| {
                let key = "\"session\":\"";
                let start = l.find(key)? + key.len();
                let end = l[start..].find('"')? + start;
                Some(&l[start..end])
            })
            .collect();
        assert_eq!(
            injected_sessions.len(),
            2,
            "one attribution entry per actual session id, including the restart: {log}"
        );
        assert_ne!(
            injected_sessions[0], injected_sessions[1],
            "the restart mints a new session id and must be logged under it: {log}"
        );
    }

    /// T7: unread mail addressed to this session's agent is folded into the
    /// composed system prompt at launch, the same way the repo layer is.
    #[test]
    fn unread_mail_is_delivered_into_the_launch_system_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "abababab-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail must reach the launch's composed prompt: {argv}"
        );
        assert!(
            argv.contains("another agent session"),
            "labeled as mail, not as an operator instruction: {argv}"
        );
    }

    /// codex has no system-prompt injection mechanism at all
    /// (`capabilities().system_prompt == false`), so `injection_args_for_
    /// session` always returns an empty argv for it -- folding mail into
    /// `composed` the way `unread_mail_is_delivered_into_the_launch_system_
    /// prompt` proves for claude would silently destroy the message for
    /// codex. `task_prompt_with_mail_fallback` is what rescues it: the mail
    /// block lands on the task prompt text itself instead, which is always
    /// delivered (here, as the `exec` positional argv token; on a Windows
    /// shim launch it would be stdin instead, same mechanism). Mail is
    /// consumed only because it was genuinely delivered this way -- the same
    /// Item 3 discipline the claude path already follows.
    #[test]
    fn a_codex_worker_receives_mail_in_its_task_prompt_since_it_cannot_be_injected() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            // No trailing command: zirv builds the launch itself
            // (`adapter_builds_launch`), which is the shape both
            // `zirv ctx agent codex <prompt>` and a bare `zirv ctx exec
            // --agent codex --prompt <text>` produce, and the one shape
            // `task_prompt_with_mail_fallback` can actually append to. See
            // `explicit_command_mail_is_left_untouched_for_an_uninjectable_
            // adapter` below for the other shape (an explicit `-- <command>`),
            // where there is no such text to append to at all.
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail must reach codex's task prompt text: {argv}"
        );
        assert!(
            argv.contains("another agent session"),
            "still labeled as mail, not as an operator instruction: {argv}"
        );
        let task_at = argv.find("do the work").expect("the task prompt itself");
        let mail_at = argv
            .find("heads up: the webhook route moved")
            .expect("checked above");
        assert!(
            task_at < mail_at,
            "the mail must be appended after the operator's own task prompt, not before it: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail actually delivered into the task prompt must be consumed: {unread:?}"
        );
    }

    /// Item 14: `--simple` (`skip_injection`) makes `composed` always `None`,
    /// for either adapter -- but codex's real mail channel, the task-prompt
    /// text `task_prompt_with_mail_fallback` appends to, has nothing to do
    /// with `composed` at all. Before this fix, gating mail listing on
    /// `composed.is_some()` withheld mail from codex under `--simple` for a
    /// reason that only ever applied to claude.
    #[test]
    fn simple_mode_does_not_withhold_mail_from_codex() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: true,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "--simple must not withhold mail from an adapter whose channel does not need \
             composed: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail actually delivered into the task prompt must be consumed: {unread:?}"
        );
    }

    /// Direct codex launches support `developer_instructions`, including the
    /// explicit `-- <command>` shape. Zirv therefore delivers and consumes
    /// mail through the same configuration override without rewriting the
    /// caller's task prompt.
    #[test]
    fn explicit_command_mail_uses_codex_developer_instructions() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            // Extracted from the explicit command below (`locate_prompt`
            // recognises codex's own `exec <prompt>` shape), the same way a
            // hand-typed `zirv ctx exec --agent codex -- codex exec "..."`
            // would resolve it.
            prompt: None,
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: vec![
                "sh".to_string(),
                fixture("fake-codex-agent.sh").display().to_string(),
                "exec".to_string(),
                "do the work".to_string(),
            ],
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("-c developer_instructions=")
                && argv.contains("heads up: the webhook route moved"),
            "direct codex must receive mail through developer instructions: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail delivered through developer instructions must be consumed: {unread:?}"
        );
    }

    /// Final wave item 2: a nudge restart of an explicit-command codex run
    /// delivers the
    /// nudge's own guidance -- stored as ordinary session-addressed mail by
    /// `sessions::run_nudge_with` -- because the relaunch it triggers always
    /// rebuilds through `build_headless`, unconditionally zirv's own launch
    /// regardless of what the original `-- <command>` argv looked like. The
    /// task-prompt-text channel `task_prompt_with_mail_fallback` uses exists
    /// on that relaunch even though it never existed at the initial launch.
    #[test]
    fn a_nudge_on_an_explicit_command_codex_run_delivers_the_nudge_mail_on_the_relaunch() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        // The nudge restart rebuilds its launch through the adapter's own
        // `headless_cmd` (`build_headless`), not by re-running the original
        // explicit `-- sh <fixture> ...` argv verbatim -- so the fixture has
        // to also be reachable as this adapter's *configured* binary, the
        // same way `a_codex_worker_receives_mail_in_its_task_prompt_since_
        // it_cannot_be_injected` wires it, or the relaunch resolves to
        // whatever `codex` happens to mean on the machine running this test.
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            // No session-env log for codex (it never receives `--session-id`
            // at all -- see the fixture's own doc comment), so liveness is
            // polled straight off the real session registry instead of a
            // log-line count, mirroring `nudge_live_session`'s own read.
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: vec![
                "sh".to_string(),
                fixture("fake-codex-agent.sh").display().to_string(),
                "exec".to_string(),
                "do the work".to_string(),
            ],
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"nudge-restart\""),
            "the nudge still restarts the process: {log}"
        );

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            argv.contains("heads up: switch focus"),
            "the relaunch rebuilds through build_headless -- zirv's own launch -- so the \
             nudge's guidance must reach its argv even though the original command was \
             explicit: {argv}"
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail actually delivered into the relaunch's task prompt must be consumed: \
             {unread:?}"
        );
    }

    /// Final wave item 2: an explicit `-- <command>` initial launch
    /// (`adapter_builds_launch == false`) never itself goes through
    /// `build_headless` -- but a relaunch (nudge, park, rot/timeout) always
    /// does, on a Windows npm `.cmd`/`.ps1`-resolved `agent_bin` regardless
    /// of what the original invocation's argv looked like. Before this fix,
    /// `prompt_via_stdin` was ANDed with `adapter_builds_launch` and
    /// therefore pinned `false` for this whole run, so the nudge relaunch --
    /// built through `build_headless`, carrying the nudge's own multi-line
    /// mail block (`\n\n---\n\n...`) -- put that composed task prompt text
    /// on the reparsed `cmd.exe /c <shim>` argv instead of stdin, and
    /// `guard_cmd_shim_reparse` aborted the entire run the moment that
    /// relaunch tried to spawn. A trivial "do the work" prompt with no mail
    /// pending would not reproduce this (no metacharacters to trip the
    /// guard on), which is why the nudge's own guidance -- always multi-line
    /// -- is what this test carries.
    #[cfg(windows)]
    #[test]
    fn a_nudge_relaunch_of_an_explicit_command_codex_run_survives_a_cmd_shim_agent_bin() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        // The nudge's own relaunch is built through `adapter.headless_cmd`
        // (`build_headless`), which resolves `agent_bin` -- a real `.cmd`
        // file on disk, so `resolve_program` genuinely routes it through
        // `cmd.exe /c` the way an npm install would. A bare in-memory path
        // is not enough to reproduce the shim shape. The initial explicit
        // command never touches `agent_bin` at all (`sh` invokes the
        // fixture directly), so only the relaunch exercises it.
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        env.insert("ZIRV_CTX_AGENT_BIN".to_string(), shim.display().to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\n").expect("write modes");
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: vec![
                "sh".to_string(),
                fixture("fake-codex-agent.sh").display().to_string(),
                "exec".to_string(),
                "do the work".to_string(),
            ],
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");

        assert_eq!(
            code.expect("the nudge relaunch must spawn, not be aborted by the argv guard"),
            0
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
    }

    /// Medium 3: the opposite shape from the explicit-command test above --
    /// zirv builds this launch itself (`command: Vec::new()`, so `adapter_
    /// builds_launch` and therefore `mail_deliverable` are both true
    /// regardless of `--simple`), so the nudge's own guidance must reach the
    /// relaunch's task-prompt text. Before this fix the nudge arm's mail
    /// gate lacked the launch path's `|| !system_prompt_supported` escape,
    /// so `--simple` (which always makes `fresh` `None`) silently dropped
    /// the guidance here too, even though codex's real channel -- the task
    /// prompt text -- never depended on `fresh`/`composed` in the first
    /// place.
    #[test]
    fn a_nudge_on_a_simple_codex_run_still_delivers_its_own_guidance() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: true,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"nudge-restart\""),
            "the nudge still restarts the process: {log}"
        );

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            argv.contains("heads up: switch focus"),
            "--simple must not drop the nudge's own guidance for an adapter whose channel does \
             not need composed: {argv}"
        );
    }

    /// Issue #220, end to end: `zirv workflow review run`'s compact review
    /// package embeds the FULL diff as the task prompt, and a plain `zirv
    /// agent codex "<...>"` can just be handed a long string -- either way,
    /// the old code always put that text on argv (`adapter.headless_cmd`),
    /// which overflows `CreateProcessW`'s ~32KB command-line limit on
    /// Windows (`os error 206`) even on a perfectly ordinary, non-shim
    /// launch (`sh <fixture>`, never `cmd.exe /c <shim>` -- so the ONLY
    /// reason this prompt can land on stdin here is the new size-based
    /// routing, not `prompt_delivery_via_stdin`'s pre-existing shim check).
    /// `fake-codex-agent.sh` drains and logs stdin exactly so a test like
    /// this one can tell the two delivery paths apart.
    #[test]
    fn an_oversized_prompt_is_delivered_on_stdin_instead_of_argv() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_ARGV_LOG",
            argv_log.to_str(),
        )]);

        let marker = "OVERSIZED_PROMPT_MARKER_9f3c1a";
        let oversized_prompt = format!(
            "{marker}{}",
            "x".repeat(super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES + 500)
        );

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some(oversized_prompt.clone()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: true,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(
            code.expect("an oversized prompt must not fail the launch"),
            0
        );

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        for line in argv.lines().filter(|line| !line.starts_with("stdin: ")) {
            assert!(
                !line.contains(marker),
                "an oversized prompt must never be encoded onto argv: {line}"
            );
        }
        assert!(
            argv.contains(&format!("stdin: {oversized_prompt}")),
            "an oversized prompt must instead reach the child on stdin: {argv}"
        );
    }

    /// Medium 4: `mail_entries` gets reassigned to the nudge's own fresh
    /// listing in the nudge arm, but `mail_messages` (the text-content list
    /// `task_prompt_with_mail_fallback` reuses verbatim in the park and
    /// rot-restart arms, since neither re-lists mail) used to stay pinned to
    /// whatever the *launch* computed. Sequence: launch mail is delivered
    /// and consumed normally on the first spawn; a nudge delivers a second,
    /// different message; the nudged relaunch then itself hits a usage
    /// limit and parks. Before this fix, the park's own relaunch re-
    /// appended the launch mail's text (already consumed, stale) instead of
    /// the nudge's; it must instead carry the nudge's guidance, and the
    /// launch mail's text must never reach argv a second time.
    ///
    /// KNOWN ISSUE (perf/test-suite-speed fix round 2, 2026-08-24): this
    /// test was originally suspected of a nextest-only failure; re-review
    /// corrected that -- the discriminator is process *warmth*, not the
    /// runner (a cold filtered serial `cargo test` run failed it ~60% of
    /// the time; nextest gives every test a cold process and failed it
    /// ~100%; a full serial run reaches it warm, after ~2300 others, and
    /// passed). One real mechanism behind that has been found and fixed:
    /// `OutputTap::try_lines` (`supervise.rs`) was a pure, instantaneous,
    /// non-blocking drain with no synchronization against `forward`'s
    /// reader threads reaching EOF, so a child that prints its limit line
    /// and exits immediately could still have that line in flight when
    /// `child.wait()` observed the exit -- defeating both the poll-loop
    /// `scan_for_limit` and the "final drain" right after `supervise_run`
    /// returns, whose own comment claimed (wrongly) to close this race.
    /// Fixed via `OutputTap::drain_to_eof`, a bounded blocking drain that
    /// waits only as long as it takes for the reader threads to disconnect;
    /// verified independently via a real `spawn_tapped` child reproducing
    /// exactly this shape (`drain_to_eof_catches_a_real_childs_last_line_
    /// even_though_it_already_exited`, `supervise.rs`), 20/20 passes
    /// including under genuine heavy host contention.
    ///
    /// This test also failed intermittently (~30% of filtered, cold,
    /// single-run attempts observed on the dev machine, 100% on CI run
    /// 32723969751) with a DIFFERENT signature than the race above, and the
    /// tap-vs-exit fix did not explain or claim to fix it. Root cause: the
    /// adapter probes capability support by spawning `exec --help`
    /// (`detect_ignore_flags`, `adapters/codex.rs`) before composing the
    /// nudge restart's distiller call. This test's `ZIRV_CTX_AGENT_BIN`
    /// override redirects that probe to `fake-codex-agent.sh` too, and the
    /// probe's argv has no `--sandbox read-only` pair, so the fixture's
    /// `is_distiller` check did not exempt it -- it popped a real line off
    /// `FAKE_AGENT_MODE_FILE`, shifting hang/limit/healthy by one and making
    /// the "limit" stage silently run as "healthy" instead (no
    /// prompt-injection log entry, no limit-park, launch mail re-appended).
    /// The initial suspicion that this was machine-specific (a CI runner
    /// with no `codex` on PATH would never trigger the probe) was wrong:
    /// the probe targets the `ZIRV_CTX_AGENT_BIN` override directly, not a
    /// PATH-resolved `codex`, so it fires on CI just as reliably -- which is
    /// what CI run 32723969751's deterministic failure confirmed. Fixed in
    /// `fake-codex-agent.sh`: a bare `--help` probe is now recognized the
    /// same way `is_distiller` special-cases `--sandbox read-only`, logged
    /// but never popping a mode.
    #[test]
    fn a_post_nudge_park_carries_the_nudges_own_mail_not_the_stale_launch_mail() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        // This regression specifically isolates the same-harness park/relaunch
        // mail path. #186 enables fallback by default, which would correctly
        // continue the already-stopped codex child on claude instead.
        env.insert("ZIRV_CTX_FALLBACK".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store launch mail");

        // hang (nudge target) -> limit (the nudged relaunch parks) -> healthy
        // (the park's own relaunch, the one under test).
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nlimit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let modes_for_writer = modes.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            // Registration precedes the fixture consuming its mode. Wait for
            // that observable transition so the nudge cannot steal `hang`.
            wait_for_first_line_or_panic(&modes_for_writer, "limit", Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert_eq!(
            argv.matches("heads up: the webhook route moved").count(),
            1,
            "the launch mail was delivered and consumed once, on the first spawn -- it must \
             never be re-appended on the post-nudge park's own relaunch: {argv}"
        );
        assert!(
            argv.matches("heads up: switch focus").count() >= 2,
            "the nudge's own guidance reaches both its own relaunch and the park that followed \
             it: {argv}"
        );
    }

    /// B3: `mail.enabled = false` must gate delivery at every seam that folds
    /// mail into a composed prompt, not just `send`/`inbox`.
    #[test]
    fn disabled_mail_is_not_delivered_into_a_headless_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "eeeeeeee-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_MAIL".to_string(), "false".to_string());

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            !argv.contains("heads up: the webhook route moved"),
            "mail.enabled = false must gate delivery, not just send/inbox: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a delivery that never happened must not consume the message either"
        );
    }

    /// S3: mail delivered into a launch prompt is consumed right after, so a
    /// later launch does not redeliver it.
    #[test]
    fn delivered_mail_is_not_delivered_a_second_time() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let session1 = "abababab-2222-4333-8444-555555555555";
        let argv_log1 = tmp.path().join("argv1.log");
        // NEW-1: a guard. Three panicking statements sit between the old
        // set and its restore, so any of them leaked `FAKE_AGENT_*` into
        // every later test in this process.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv_log1.to_str()),
        ]);
        let args1 = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session1.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session1)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session1),
        };
        let mut out1 = Vec::new();
        let code1 = run_with(&args1, &mut out1, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code1.expect("first launch runs"), 0);
        let argv1 = std::fs::read_to_string(&argv_log1).expect("argv recorded");
        assert!(
            argv1.contains("heads up: the webhook route moved"),
            "the first launch must see the mail: {argv1}"
        );

        let session2 = "cdcdcdcd-2222-4333-8444-555555555555";
        let argv_log2 = tmp.path().join("argv2.log");
        // Nested guard: restores to `argv_log1` on drop, and the outer guard
        // then restores whatever the process had before the test.
        let _second_argv_log = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_ARGV_LOG",
            argv_log2.to_str(),
        )]);
        let args2 = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session2.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session2)),
            prompt: Some("do more work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session2),
        };
        let mut out2 = Vec::new();
        let code2 = run_with(&args2, &mut out2, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code2.expect("second launch runs"), 0);
        let argv2 = std::fs::read_to_string(&argv_log2).expect("argv recorded");
        assert!(
            !argv2.contains("heads up: the webhook route moved"),
            "the mail was already delivered once and must not be redelivered: {argv2}"
        );
    }

    /// Issue #30, item 3: mail consumed on a session's behalf -- here, an
    /// exec cycle folding it into its own launch prompt, never in answer to
    /// that session's own explicit `zirv ctx inbox` -- must leave a
    /// decision-log trail naming the mail file and who claimed it.
    #[test]
    fn consuming_mail_into_the_launch_prompt_is_logged() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let path = crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");
        let file_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf8")
            .to_string();

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let session = "10111011-2222-4333-8444-555555555555";
        let argv_log = tmp.path().join("argv.log");
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"mail-consumed\""), "got {log}");
        assert!(
            log.contains(&file_id),
            "the entry names the mail file: {log}"
        );
    }

    /// Item 3 (regression): a launch that never actually spawns must not
    /// consume the mail it would have delivered -- no session ever saw it,
    /// so it must stay unread for whichever later invocation actually gets
    /// one running. The old ordering consumed mail immediately after
    /// composing the prompt, well before `spawn_tapped` (and the pacing
    /// gate ahead of it) ever ran.
    #[test]
    fn mail_is_not_consumed_when_the_launch_fails_before_spawning() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

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
                body: "must stay unread".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let session = "12312312-2222-4333-8444-555555555555";
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            // `adapters::select` still resolves and readies "claude" (via
            // `ZIRV_CTX_AGENT_BIN` in `base_env`, unaffected by this); only
            // the actual spawn of *this* program has to fail, deterministically
            // and without depending on any real binary's own behavior.
            command: vec![
                "zirv-test-binary-that-does-not-exist-anywhere".to_string(),
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                session.to_string(),
            ],
        };
        let mut out = Vec::new();
        let result = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert!(result.is_err(), "the launch must fail to spawn: {result:?}");

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a launch that never spawned must not have consumed the mail"
        );
    }

    /// S3: a consume failure (e.g. `read/` cannot be created) must not sink
    /// the launch -- the mail already reached the prompt either way.
    #[test]
    fn a_failed_consume_does_not_stop_the_launch() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "ffffffff-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

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
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        // Block `mail::consume`'s own `read/` directory creation by putting
        // an ordinary file where it needs a directory: a deterministic way
        // to force the consume step to fail without racing a real
        // filesystem deletion mid-flight.
        std::fs::write(state.mail().join(&slug).join("read"), b"not a directory")
            .expect("write blocker");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("a failed consume must not fail the launch"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail still had to reach the prompt even though consuming it afterward failed: {argv}"
        );
    }

    /// I2: a user's own --append-system-prompt inside the `--` command must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[test]
    fn a_users_own_append_system_prompt_is_merged_into_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "dddddddd-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--append-system-prompt".to_string());
        command.push("always answer in Danish".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
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

    /// Shared with `zirv ctx agent` (agent.rs) and script `agent:` steps
    /// (agent_command.rs), which both delegate to this supervisor and want
    /// the same wording for the same two outcomes: the supervisor's own exit
    /// codes read as outcomes, not agent failures.
    #[test]
    fn describe_exit_names_the_supervisors_own_outcomes() {
        assert!(describe_exit(EXIT_ROT_EXHAUSTED).contains("restart budget"));
        assert!(describe_exit(EXIT_TIMEOUT).contains("wall-clock timeout"));
        assert_eq!(describe_exit(1), "exited with code 1");
    }

    #[test]
    fn a_healthy_window_adds_no_delay() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "bbbbbbbb-2222-4333-8444-555555555555";
        let env = base_env(&state);
        store_collector(&state, 5.0, 3600);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(!log.contains("pace-wait"), "nothing to wait for: {log}");
    }

    // N4: `zirv ctx nudge` restarting a headless worker.
    //
    // Every test here drives a real `run_with` call whose first agent
    // invocation hangs (`FAKE_AGENT_MODE_FILE` starting with "hang") and is
    // nudged from a background thread once its transcript is up. Like every
    // other test in this module that spawns `sh`/`fake-agent.sh`, these are
    // blocked on Windows by the pre-existing os-193 spawn issue (see this
    // module's other `sh`-spawning tests); written to the same standard the
    // rest of this suite holds regardless.

    /// Polls `path` until it has at least `n` lines or `timeout` elapses,
    /// returning whatever was there either way -- the same "best effort,
    /// bounded wait" shape `run_loop.rs`'s own synchronized tests use via a
    /// marker file, adapted here to a growing log instead of a touch-once
    /// marker since more than one invocation is expected.
    fn wait_for_lines(path: &std::path::Path, n: usize, timeout: Duration) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                if lines.len() >= n {
                    return lines;
                }
            }
            if Instant::now() >= deadline {
                return std::fs::read_to_string(path)
                    .map(|t| t.lines().map(|l| l.to_string()).collect())
                    .unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Nudges whichever session is currently live. `exec` (like `loop`)
    /// keeps exactly one registry record at a time, refreshed on every
    /// restart or park, so resolving the registry is how this test finds the
    /// run it is driving without knowing that session's id up front.
    ///
    /// This used to pass an empty prefix and lean on `starts_with("")`
    /// matching everything. F6 made `zirv ctx nudge` refuse any prefix
    /// shorter than four characters -- a unique-but-mistyped prefix could
    /// otherwise wake, and in `exec`'s case restart, a session the operator
    /// never named -- and an empty prefix is the extreme case of exactly
    /// that. The helper now resolves the live short id and passes it whole,
    /// which is what an operator reading `zirv ctx status` would type.
    fn nudge_live_session(state_dir: &std::path::Path, repo: &std::path::Path, message: &str) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let prefix = crate::commands::ctx::sessions::list(&state)
            .into_iter()
            .find(|(_, liveness)| *liveness == crate::commands::ctx::sessions::Liveness::Live)
            .map(|(record, _)| record.short)
            .expect("exactly one live session to nudge");
        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]
        .into();
        let args = crate::commands::ctx::sessions::NudgeArgs {
            prefix,
            message: Some(message.to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::sessions::run_nudge_with(
            &args,
            &mut out,
            repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge the live session");
    }

    /// `wait_for_lines`, but a give-up (never reaching `n` lines within
    /// `budget`) panics with a clear message instead of silently returning
    /// whatever partial result it has. Every writer-thread test in this
    /// nudge family used to swallow that case (`if lines.is_empty() {
    /// return; }`) and just never nudge -- the only visible symptom, tens of
    /// seconds later, was the launch's own exec timeout (a bare exit 76),
    /// indistinguishable from a real regression. `budget` is sized honestly
    /// against each test's own exec timeout (not the old, uniformly tight
    /// 5s this whole family shared regardless of how much headroom its own
    /// launch actually had), leaving real margin for the rest of the test's
    /// work after the wait. A panic here is caught by the caller's own
    /// `writer.join().expect(...)`, which is what actually fails the test --
    /// this only makes the *reason* legible.
    fn wait_for_lines_or_panic(path: &std::path::Path, n: usize, budget: Duration) -> Vec<String> {
        let lines = wait_for_lines(path, n, budget);
        assert!(
            lines.len() >= n,
            "never saw {n} line(s) in {} within {budget:?} -- the hang-mode agent likely never \
             started, or this machine is starved badly enough that it could not be observed in \
             time (check for CPU contention before assuming a real regression): got {lines:?}",
            path.display()
        );
        lines
    }

    /// The registry-poll sibling of `wait_for_lines_or_panic`, for the
    /// codex-shaped tests that have no session-env log to poll and instead
    /// watch the session registry directly (see the identical comment on
    /// each of their own writer threads before this helper existed).
    fn wait_for_live_session_or_panic(state_dir: &std::path::Path, budget: Duration) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if crate::commands::ctx::sessions::list(&state)
                .iter()
                .any(|(_, liveness)| *liveness == crate::commands::ctx::sessions::Liveness::Live)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "no live session appeared within {budget:?} -- the hang-mode agent likely never \
             started, or this machine is starved badly enough that it could not be observed in \
             time (check for CPU contention before assuming a real regression)"
        );
    }

    fn wait_for_first_line_or_panic(path: &std::path::Path, expected: &str, budget: Duration) {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if std::fs::read_to_string(path)
                .ok()
                .is_some_and(|text| text.lines().next() == Some(expected))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "{} never advanced to mode {expected:?} within {budget:?}",
            path.display()
        );
    }

    #[test]
    fn a_headless_worker_stops_at_the_next_poll_and_relaunches_with_the_guidance() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session_log = tmp.path().join("session.log");
        let session = "10101010-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _agent =
            crate::commands::ctx::testenv::VarGuard::set(&[("ZIRV_CTX_AGENT", Some("claude"))]);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_lines_or_panic`'s own doc comment.
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "switch to the new failing test",
            );
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(
            code.expect("the second (healthy) launch finishes the run"),
            0
        );

        let sessions = wait_for_lines(&session_log, 2, Duration::from_millis(1));
        assert_eq!(
            sessions.len(),
            2,
            "exactly one relaunch: the nudge, then a clean exit"
        );
        assert_ne!(
            sessions[0], sessions[1],
            "the relaunch mints a fresh session id"
        );

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("switch to the new failing test"),
            "the nudge's guidance must reach the relaunch's composed prompt: {argv}"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
    }

    /// N4: a nudge-driven restart must never touch the rot restart budget --
    /// with `max_restarts: 0`, an ordinary rot or timeout restart would
    /// immediately "give up"; a nudge restart must succeed anyway, and the
    /// normal rot-restart machinery (`"action":"restart"`, a `"rot"` or
    /// `"timeout"` verdict) must never fire at all.
    #[test]
    fn a_nudge_restart_does_not_spend_the_rot_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "20202020-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_lines_or_panic`'s own doc comment.
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero rot-restart budget: proves the nudge restart below is not
            // drawing from it.
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(
            code.expect("a nudge restart with zero rot budget must still succeed"),
            0
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"restart\""),
            "the ordinary rot-restart action must never fire: {log}"
        );
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "zero budget only matters to rot/timeout, which never triggered: {log}"
        );
        assert!(
            !log.contains("\"verdict\":\"rot\"") && !log.contains("\"verdict\":\"timeout\""),
            "nothing here rotted or timed out: {log}"
        );
    }

    /// N4: a nudge restart carries a handoff forward exactly like a rot or
    /// timeout restart does -- distilled or structural, stored under the old
    /// session, and named in the decision log detail the same way
    /// `"{source} handoff at {path}"` already reads for the ordinary path.
    #[test]
    fn a_nudge_restart_carries_a_handoff_forward_like_every_other_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "30303030-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_lines_or_panic`'s own doc comment.
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        assert!(
            crate::commands::ctx::handoff::latest_for_repo(&state, tmp.path())
                .expect("handoff lookup")
                .is_some(),
            "a nudge restart must distill and store a handoff, like every other restart"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        let nudge_restart_line = log
            .lines()
            .find(|l| l.contains("\"action\":\"nudge-restart\""))
            .unwrap_or_else(|| panic!("no nudge-restart entry: {log}"));
        assert!(
            nudge_restart_line.contains("handoff at"),
            "names the handoff the same way an ordinary restart does: {nudge_restart_line}"
        );
    }

    /// Issue #169.2: a restart must never reset the token budget meter. Two
    /// children, each well under `--budget-tokens` on its own transcript
    /// alone, whose COMBINED spend exceeds it -- before this fix, a fresh
    /// transcript per restart meant the second child's own (still-under-
    /// budget) reading was all `evaluate_worker_budget` ever saw, so the run
    /// finished with exit `0` instead of `EXIT_BUDGET_EXHAUSTED`.
    ///
    /// `FAKE_AGENT_TURNS=1` shrinks one child's own transcript to a known,
    /// small total (2 assistant events x 20_000 cache-read tokens = 40_004
    /// with the fixture's own `input_tokens: 2` per event) so both runs can
    /// sit comfortably under a budget individually while landing over it
    /// together. The restart itself is a real nudge (`nudge_live_session`),
    /// the same deterministic trigger `a_nudge_restart_does_not_spend_the_
    /// rot_restart_budget` already uses -- not rot or a timeout -- because a
    /// nudge relaunch mints a fresh transcript exactly the same way, and
    /// does not depend on the rot scorer's own heuristics to fire on cue.
    #[test]
    fn a_restart_accumulates_spend_instead_of_resetting_the_budget_meter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "40404040-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair -- see the identical
        // comment on the nudge tests above this one.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
            ("FAKE_AGENT_TURNS", Some("1")),
        ]);

        let first_transcript = transcript_for(&home, tmp.path(), session);
        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let transcript_for_writer = first_transcript.clone();
        let writer = std::thread::spawn(move || {
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            // The session-env line only says the child STARTED: the fixture
            // appends it before it has even created its transcript, let alone
            // written a turn into it. Nudging on that line alone raced the
            // restart ahead of the outgoing child's own spend, so
            // `harvest_spend` folded in a transcript that did not exist yet
            // (0 tokens) and the incoming child's own under-budget reading was
            // all the meter ever saw -- exit `0` instead of the accumulated
            // `EXIT_BUDGET_EXHAUSTED` this test is about. Wait for the whole
            // `FAKE_AGENT_TURNS=1` turn (4 lines) that harvest has to see.
            wait_for_lines_or_panic(&transcript_for_writer, 4, Duration::from_secs(20));
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(first_transcript),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            // Above one child's own ~40_004-token transcript, below two
            // combined (~80_008): neither child trips the budget on its own
            // reading, only the accumulated total does.
            budget_tokens: Some(60_000),
            max_tool_calls: None,
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");

        assert_eq!(
            code.expect("runs"),
            EXIT_BUDGET_EXHAUSTED,
            "the second child's own transcript alone is under budget -- only the accumulated \
             total exceeds it"
        );
        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"budget\""), "got {log}");
        assert!(
            log.contains("\"action\":\"nudge-restart\""),
            "the restart that must not reset the meter actually happened: {log}"
        );
    }

    /// N4: `cfg.supervise.max_nudges` caps consecutive nudge restarts. Past
    /// the cap the marker is still claimed (so it does not keep re-firing)
    /// but nothing is stopped or relaunched, and the nudge's own mail stays
    /// unread -- still visible via `zirv ctx inbox` -- rather than being
    /// silently dropped.
    #[test]
    fn consecutive_nudge_restarts_are_capped_and_the_message_is_left_unread() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "40404040-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert("ZIRV_CTX_MAX_NUDGES".to_string(), "1".to_string());

        // Three potential runs scripted; only two are ever expected to
        // start (the first nudge restarts once, the second is ignored, and
        // the second run's own hang has to end some other way -- the
        // `timeout_secs` below, with a zero rot budget, is what ends it).
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || -> Vec<String> {
            // 2s: the first hang-mode agent only has to spawn and write its
            // one line, no termination involved, so this is comfortably
            // honest against the time actually available. A give-up here
            // panics instead of silently skipping its own nudge -- see
            // `wait_for_lines_or_panic`'s own doc comment.
            let first = wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(2));
            debug_assert!(!first.is_empty());
            nudge_live_session(&state_for_writer, &repo_for_writer, "first nudge, honored");

            // 10s, not 2s: unlike the first wait, this one sits behind a
            // real `terminate()` of the first hang-mode child -- on Windows
            // that is a synchronous `taskkill /T /F` spawn, verified (via
            // temporary timing instrumentation, since removed) to cost
            // 1.4-1.9s on an ordinarily loaded dev machine even outside this
            // test, well before the handoff/compile/relaunch work that
            // follows it. A 2s budget here was never honest against that
            // cost, and failed deterministically under real host
            // contention (confirmed independent of any code change: the
            // same failure reproduces at a commit two revisions before this
            // one) -- 10s leaves the same order-of-magnitude margin the
            // sibling `a_nudge_restart_carries_a_handoff_forward_like_
            // every_other_restart` test already gives its own 20s wait
            // against the same taskkill cost.
            let second =
                wait_for_lines_or_panic(&session_log_for_writer, 2, Duration::from_secs(10));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "second nudge, should be ignored",
            );
            second
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            // Short enough that the second (ignored-nudge) hang ends the
            // run on its own once the cap has been proven, rather than
            // hanging the test forever.
            timeout_secs: Some(3),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        let sessions = writer.join().expect("writer thread");
        assert_eq!(
            code.expect("runs"),
            EXIT_TIMEOUT,
            "the second hang is never nudged into relaunching again, so it eventually times out \
             with no rot budget left to restart on"
        );

        let all_sessions = wait_for_lines(&session_log, 2, Duration::from_millis(1));
        assert_eq!(
            all_sessions.len(),
            2,
            "exactly one relaunch (the first nudge); the second was ignored: {all_sessions:?}"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"action\":\"nudge-restart\""))
                .count(),
            1,
            "only the first nudge restarts: {log}"
        );
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"action\":\"nudge-ignored\""))
                .count(),
            1,
            "the second is claimed but ignored, not silently dropped: {log}"
        );

        // The second nudge's own mail must still be sitting there, unread.
        // It is addressed to this run's *registry* short id -- the address
        // `SessionGuard::refresh_session` deliberately leaves untouched
        // across a restart (C7) and the one `nudge_live_session` itself
        // resolves and sends to -- not to `short_id` of the second
        // session's own rotated id, which is a different value entirely.
        assert!(
            sessions.len() >= 2,
            "the second session started: {sessions:?}"
        );
        let registry_short = crate::commands::ctx::sessions::short_id(session);
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let unread = crate::commands::ctx::mail::list(&state, &slug, None, Some(&registry_short))
            .expect("list");
        assert_eq!(
            unread.len(),
            1,
            "the ignored nudge's mail is left unread, still visible via `zirv ctx inbox`: {unread:?}"
        );
        assert_eq!(unread[0].1.body, "second nudge, should be ignored");
    }
}
