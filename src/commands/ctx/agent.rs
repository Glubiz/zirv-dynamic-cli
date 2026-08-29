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
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::CtxResult;
use super::adapters::{self, AgentAdapter};
use super::announce::{Announcer, Event};
use super::chat::quiet_env;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::dash::spawnreq;
use super::event::{SessionId, TranscriptUsage};
use super::exec::{self, ExecArgs};
use super::pace;

#[derive(Debug, Clone, clap::Args)]
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
    /// What this delegated worker should present as: `worker` (the default,
    /// unstated) or `sub-orchestrator` (issue #155, Phase 5). Travels on the
    /// `SpawnRequest` (`spawnreq::role_of`) when a dashboard pane fulfils
    /// this delegation, and is re-validated there against the depth cap
    /// (`dash::mod::depth_refusal`) -- this flag is a request, not a grant.
    #[arg(long)]
    pub role: Option<String>,
    /// The work group (`zirv ctx group create`) this delegation belongs to.
    /// Its own token budget, when set, is a ceiling `--budget-tokens` may
    /// only tighten, never raise (`resolve_budget_tokens`). Unstated falls
    /// back to `WORK_GROUP_ENV` (`resolve_group_binding`, issue #170) --
    /// the group a SubOrchestrator was itself launched under, so every
    /// worker it spawns lands in that group by lineage rather than by typing
    /// `--group` on every call.
    #[arg(long)]
    pub group: Option<String>,
    /// Issue #170: what this group of delegated work is for. Meaningful only
    /// alongside `--role sub-orchestrator` and with no `--group` already
    /// resolved (explicitly or via `WORK_GROUP_ENV`): mints a fresh work
    /// group scoped to this text (`group::create`'s own defaults for
    /// everything else, `--budget-tokens` as its token ceiling) and binds
    /// this delegation to it, rather than requiring the operator to run
    /// `zirv ctx group create` as a separate step first.
    #[arg(long)]
    pub scope: Option<String>,
    /// Token ceiling for this worker (issue #155, Phase 5(d)). Checkpoints
    /// at `BUDGET_SOFT_FRACTION` of the ceiling and stops at the ceiling
    /// itself -- never a signal to change models. `None` (the default) is
    /// unbounded, exactly today's behaviour.
    #[arg(long)]
    pub budget_tokens: Option<u64>,
    /// Tool-call ceiling for this worker, independent of `--budget-tokens`.
    #[arg(long)]
    pub max_tool_calls: Option<u32>,
    /// Issue #155, Phase 6(c): spend anyway at or above `pace.spawn_hard_pct`
    /// (`pace::SpawnGate::Refuse`). The operator's own informed call --
    /// never a signal that reaches rotation, which this gate never touches
    /// either way. Travels on the `SpawnRequest` (`SpawnRequest::force`) the
    /// same way `--role`/`--group` do, so a pane spawn fulfilled by a
    /// dashboard honours the same override this process already decided on.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

/// The soft threshold, as a fraction of a budget. At or above it the worker
/// is nudged to wrap up and checkpoint while it still has room to write a
/// usable result; at the budget itself it is stopped.
pub const BUDGET_SOFT_FRACTION: f64 = 0.8;

/// What a delegated worker is allowed to spend. `None` on a field means no
/// ceiling for it -- which is every delegation before 2.35.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkerBudget {
    pub tokens: Option<u64>,
    pub tool_calls: Option<u32>,
}

/// A budget's state against what a worker has spent so far. Never a
/// downgrade signal: see `budget_state`'s own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetState {
    Ok,
    SoftWarn { used: u64, limit: u64 },
    HardStop { used: u64, limit: u64 },
}

/// HardStop > SoftWarn > Ok, so the worst ceiling wins when both are set.
fn rank(state: BudgetState) -> u8 {
    match state {
        BudgetState::Ok => 0,
        BudgetState::SoftWarn { .. } => 1,
        BudgetState::HardStop { .. } => 2,
    }
}

/// Pure: no clock, no filesystem. The worst state across both ceilings wins
/// (HardStop > SoftWarn > Ok), the same "most restrictive answer" fold
/// `safety::evaluate_candidates` uses.
///
/// Spend is `usage.context_total() + usage.output_tokens`, not
/// `usage.input_tokens`: uncached input is near zero in a cached session, so
/// budgeting on it alone would mean the budget effectively never fires.
///
/// A budget CHECKPOINTS, it never downgrades the model: this function only
/// ever reports `Ok`/`SoftWarn`/`HardStop`, never anything that could be read
/// as "switch to a cheaper model" -- that path does not exist, on purpose
/// (issue #155's own architect ruling: a cheaper answer to the wrong
/// question is not a saving, and an automatic downshift is out of scope).
pub fn budget_state(
    budget: &WorkerBudget,
    usage: &TranscriptUsage,
    tool_calls: u32,
) -> BudgetState {
    let spent = usage.context_total().saturating_add(usage.output_tokens);
    let mut worst = BudgetState::Ok;
    let mut consider = |used: u64, limit: u64| {
        if limit == 0 {
            return;
        }
        let soft = (limit as f64 * BUDGET_SOFT_FRACTION) as u64;
        let state = if used >= limit {
            BudgetState::HardStop { used, limit }
        } else if used >= soft {
            BudgetState::SoftWarn { used, limit }
        } else {
            BudgetState::Ok
        };
        if rank(state) > rank(worst) {
            worst = state;
        }
    };
    if let Some(limit) = budget.tokens {
        consider(spent, limit);
    }
    if let Some(limit) = budget.tool_calls {
        consider(u64::from(tool_calls), u64::from(limit));
    }
    worst
}

/// A group's own token budget is a ceiling its children may only TIGHTEN. An
/// explicit `--budget-tokens` larger than the group's own is clamped, never
/// honoured: a child must not be able to raise the batch's own limit.
pub fn resolve_budget_tokens(group: Option<u64>, explicit: Option<u64>) -> Option<u64> {
    match (group, explicit) {
        (Some(group), Some(explicit)) => Some(group.min(explicit)),
        (Some(group), None) => Some(group),
        (None, explicit) => explicit,
    }
}

/// `--role` accepts exactly two spellings; anything else is refused before
/// this delegation ever launches, the same discipline `validate_flags`
/// applies to `flags`.
pub fn validate_role(role: &Option<String>) -> CtxResult<()> {
    match role.as_deref() {
        None | Some("worker") | Some("sub-orchestrator") => Ok(()),
        Some(other) => {
            Err(format!("--role must be 'worker' or 'sub-orchestrator'; got '{other}'").into())
        }
    }
}

/// Issue #170: exported into a SubOrchestrator delegation's own env
/// (`resolve_group_binding`'s caller, once the child actually launches) so
/// every `zirv agent` call ITS OWN harness makes -- with no `--group` of its
/// own -- lands in the same work group by lineage rather than by the
/// harness remembering to type `--group` every time.
pub const WORK_GROUP_ENV: &str = "ZIRV_CTX_WORK_GROUP";

/// Issue #170: resolves what `--group` this delegation actually binds to,
/// mutating `args.group` in place -- called once, at the very top of
/// [`run_with`], before the dashboard-join fork so BOTH forks of this
/// delegation (a live pane, or the headless fallback) see the same answer.
///
/// Three cases, in order:
/// 1. `args.group` already named -- unchanged. An operator's own explicit
///    choice always wins.
/// 2. Unstated, but [`WORK_GROUP_ENV`] is set -- inherited. This is the
///    "lineage rather than convention" binding itself: a SubOrchestrator's
///    own further `zirv agent` calls, run from inside its own harness with
///    no `--group` of their own, pick up the same group its OWN launch was
///    bound to.
/// 3. Unstated, no inherited env, but `args.scope` names one and `args.role`
///    is `sub-orchestrator` -- mints a fresh group scoped to it
///    (`group::create`, `--budget-tokens` as its ceiling) so a coordinator
///    can be launched in one command rather than requiring `zirv ctx group
///    create` as a separate step first.
///
/// Neither case touches an existing group's own terms: case 2 only ever
/// reads an id, and case 3 only ever creates a brand new one.
///
/// Returns the id it MINTED (case 3), if any -- `None` for an explicit or
/// inherited binding, which this delegation does not own and must never
/// unwind. Security review round 2 (Finding 4): the caller rolls a minted
/// group back (`group::discard_if_unused`) on every path that ends without
/// the delegation actually starting, and this call now happens only after
/// that caller's own spawn gate has already passed.
fn resolve_group_binding(
    args: &mut AgentArgs,
    state: &super::state::StateDir,
    env: EnvLookup<'_>,
) -> CtxResult<Option<String>> {
    if args.group.is_some() {
        return Ok(None);
    }
    if let Some(inherited) = env(WORK_GROUP_ENV).filter(|s| !s.is_empty()) {
        args.group = Some(inherited);
        return Ok(None);
    }
    if let Some(scope) = &args.scope
        && args.role.as_deref() == Some("sub-orchestrator")
    {
        let id = super::group::run_create(
            state,
            &mut std::io::sink(),
            &super::group::CreateArgs {
                scope: scope.clone(),
                child_limit: super::group::DEFAULT_CHILD_LIMIT,
                token_budget: args.budget_tokens,
                deadline_secs: None,
                completion_contract: super::group::DEFAULT_COMPLETION_CONTRACT.to_string(),
                parent_session: super::mail::session_identity(env),
            },
            super::state::now_secs(),
        )?;
        args.group = Some(id.clone());
        return Ok(Some(id));
    }
    Ok(None)
}

/// Security review round 2 (Finding 3): folds the group this delegation
/// actually resolved ([`resolve_group_binding`]) into the env lookup its
/// headless launch runs under, exactly as `chat::quiet_env` folds `--quiet`
/// -- `exec::run_with`'s own `turn_env_for`/`apply_session_env` exports
/// [`WORK_GROUP_ENV`] from it into the child, so a headless coordinator's
/// children inherit the binding by lineage the same way a dashboard-spawned
/// coordinator's already did (`dash::fulfill_spawn_request` pushes the same
/// pair into its pane's `turn_env`). Reusing the env lookup, rather than
/// adding a parallel parameter or an `ExecArgs` field, is the same trade-off
/// `quiet_env`'s own doc comment spells out: one lookup already threaded
/// through every downstream signature, carrying exactly the fact the child
/// needs to read back.
///
/// `None` leaves the lookup untouched, so an inherited `WORK_GROUP_ENV` (a
/// coordinator's own child calling `zirv agent` with no `--group`) still
/// reaches the child unchanged.
fn group_env<'a>(
    env: EnvLookup<'a>,
    group: Option<String>,
) -> impl Fn(&str) -> Option<String> + 'a {
    move |key: &str| match &group {
        Some(id) if key == WORK_GROUP_ENV => Some(id.clone()),
        _ => env(key),
    }
}

/// Issue #155, Phase 6(c): whether this spawn must not start. Only a
/// `Refuse` blocks, and only without `--force`: a `Warn` is information, not
/// a gate, and a `Proceed` has nothing to override. `pub(crate)`, not
/// private: `dash::mod::fulfill_spawn_request` reuses this exact rule
/// (`SpawnRequest::force` carrying forward whatever this process already
/// decided) so a pane spawn is held to the identical override, never a
/// second, independently-drifting copy of it. Deliberately NOT a rot
/// signal -- see `pace::spawn_gate`'s own doc comment for why quota pressure
/// must never reach rotation.
pub(crate) fn spawn_blocked(gate: &pace::SpawnGate, force: bool) -> bool {
    matches!(gate, pace::SpawnGate::Refuse { .. }) && !force
}

/// Resolves this delegation's [`WorkerBudget`] from `--budget-tokens`/
/// `--max-tool-calls` and, when `--group` names one, that group's own token
/// ceiling -- which `--budget-tokens` may only tighten (`resolve_budget_
/// tokens`). An unknown or closed group is a hard error: silently ignoring
/// it would let a mistyped `--group` run unbounded work a batch's own budget
/// was meant to cap. `--max-tool-calls` has no group-level counterpart
/// (`group::WorkGroup` carries no such field) and so is never clamped.
///
/// Issue #155 review finding D2: this is also the headless admission choke
/// point for `child_limit` -- `group::admit_child` is called here, once,
/// only on the path that actually runs the delegation headlessly. The
/// dashboard-pane fork of the same request (`agent::try_join_dashboard`,
/// tried BEFORE this function is ever reached -- see `run_with`) never
/// admits here; `dash::fulfill_spawn_request` admits on that side instead,
/// so a single `zirv ctx agent --group` invocation is counted exactly once
/// regardless of which fork actually spawns.
fn resolve_worker_budget(env: EnvLookup<'_>, args: &AgentArgs) -> CtxResult<WorkerBudget> {
    let group_token_budget = match &args.group {
        Some(id) => {
            let state = super::state::StateDir::resolve(env)?;
            let group = super::group::load(&state, id)?.ok_or_else(|| {
                format!("zirv ctx agent: no work group '{id}' -- create one with `zirv ctx group create`")
            })?;
            if group.closed_at.is_some() {
                return Err(format!("zirv ctx agent: work group '{id}' is closed").into());
            }
            super::group::admit_child(&state, id).map_err(|e| format!("zirv ctx agent: {e}"))?;
            group.token_budget
        }
        None => None,
    };
    Ok(WorkerBudget {
        tokens: resolve_budget_tokens(group_token_budget, args.budget_tokens),
        tool_calls: args.max_tool_calls,
    })
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

/// Cross-harness fallback cannot forward arbitrary vendor CLI flags: a claude
/// flag may mean something different (or be invalid) on codex and vice versa.
/// Empty passthrough is safe, and a model-only passthrough can be replaced by
/// the verified equivalent tier. Anything else declines automatic routing.
fn translated_route_flags(
    flags: &[String],
    target: &dyn AgentAdapter,
    target_model: &str,
) -> Option<Vec<String>> {
    let mut i = 0;
    while i < flags.len() {
        match adapters::classify_model_flag(&flags[i]) {
            Some(adapters::ModelFlagForm::Separated) => {
                if i + 1 >= flags.len() {
                    return None;
                }
                i += 2;
            }
            Some(adapters::ModelFlagForm::Joined(_)) => i += 1,
            None => return None,
        }
    }
    Some(target.model_args(target_model))
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
///
/// Bug B (harness/model parity): also prepends `adapters::policy_launch_
/// args`, the same argv one `cfg.policy`/`cfg.sandbox` produces on whichever
/// adapter this delegates to -- since 2026-08-22 that includes the shipped-
/// default "sandboxed, no prompts" posture (`AgentAdapter::default_sandbox_
/// args`), not only an explicit `[policy]` `Deny`. A delegated headless
/// worker has nobody present to answer an approval prompt, which is exactly
/// why this seam -- not only the interactive `wrap`/`chat` launches, which
/// still have an operator watching -- is where this is wired into a real
/// launch first; see `config.rs`'s `a_repo_cannot_widen_its_way_to_a_
/// permissive_launch_on_either_adapter` for the end-to-end repo-narrow-only
/// guarantee `cfg.policy` carries, and `SandboxConfig`'s own doc comment for
/// `cfg.sandbox`'s identical guarantee. `policy_launch_args` itself declines
/// to prepend anything when the operator's own `flags` already pin one of
/// the same CLI flags (`adapters::flags_pin_policy`), so an operator's own
/// explicit `--sandbox`/`--ask-for-approval`/`--permission-mode`/
/// `--disallowedTools` demonstrably wins rather than merely surviving
/// because a CLI takes the last occurrence of a repeated flag.
fn worker_launch_flags(
    cfg: &CtxConfig,
    name: &str,
    adapter: &dyn AgentAdapter,
    flags: &[String],
) -> Vec<String> {
    let policy_extra =
        adapters::policy_launch_args(cfg, adapter, flags, adapters::LaunchMode::Headless);
    if flags_pin_model(flags) {
        let mut out = policy_extra;
        out.extend_from_slice(flags);
        return out;
    }
    let mut out = adapters::worker_model_args(cfg, name, adapter);
    out.extend(policy_extra);
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
    matches!(
        code,
        exec::EXIT_ROT_EXHAUSTED | exec::EXIT_TIMEOUT | exec::EXIT_BUDGET_EXHAUSTED
    )
    .then(|| exec::describe_exit(code))
}

/// Which of the supervisor's outcomes this exit code represents. Mirrors
/// `exit_note`'s own special cases: those are zirv giving up (or, for
/// `EXIT_BUDGET_EXHAUSTED`, zirv stopping the run on purpose), not the
/// worker failing, and they cost very differently.
fn delegation_outcome(code: i32) -> &'static str {
    match code {
        0 => "ok",
        exec::EXIT_ROT_EXHAUSTED => "rot-exhausted",
        exec::EXIT_TIMEOUT => "timeout",
        exec::EXIT_BUDGET_EXHAUSTED => "budget-exhausted",
        _ => "failed",
    }
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

/// The stdout line a delegation prints when the dashboard took the request and
/// spawned a *pane* for it. Exit is 0, but nothing has run yet -- a caller that
/// records evidence from a delegated run (the workflow reviewer) has to be able
/// to tell this apart from a completed one, so the prefix is named rather than
/// spelled out at two call sites. The printed text is unchanged.
pub const DASH_SPAWN_ACK_PREFIX: &str = "spawned in dashboard as ";

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
            writeln!(w, "{DASH_SPAWN_ACK_PREFIX}{short}")
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
/// unchanged, which covers: no dashboard channel at all (`DASH_REQUESTS_ENV`
/// unset -- silent, byte-for-byte the pre-Task-11 behavior); the inherited
/// directory absent or its owner dead, AND issue #145's own fallback scan of
/// every other `<state>/dash/*` token directory (`live_join_target`) also
/// found nothing live (notice printed, naming every candidate considered);
/// options a pane cannot honour (`--max-restarts`/`--timeout-secs`/
/// `--budget-tokens`/`--max-tool-calls`/`--flags` other than a lone
/// `--model` pin, notice printed -- `--role`/`--group` are NOT in this list:
/// both travel on the `SpawnRequest` itself, see its own fields); a prompt
/// that would be misread as a flag (notice printed); a request that could
/// not even be written (notice printed); an unclaimed ack timeout (notice
/// printed, since that is a live channel that simply did not respond); and a
/// `retryable` refusal, where the dashboard has answered that it spawned
/// nothing for a reason that says nothing about whether the task may run
/// (O2).
fn try_join_dashboard<W: Write>(
    args: &AgentArgs,
    prompt: &str,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    ack_timeout: Duration,
    claim_extension: Duration,
) -> Option<CtxResult<i32>> {
    let inherited = env(spawnreq::DASH_REQUESTS_ENV).map(std::path::PathBuf::from)?;
    let dir = live_join_target(&inherited, env)?;
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
    // Issue #155, Phase 5(d): `--budget-tokens`/`--max-tool-calls` join the
    // same decline list as `--max-restarts`/`--timeout-secs`, for the same
    // reason -- a pane is not a supervised headless run and `SpawnRequest`
    // carries neither ceiling, so silently dropping one would be worse than
    // not using the dashboard at all. `--role`/`--group` deliberately do
    // NOT join this list: both ride in the request itself (`spawnreq::
    // SpawnRequest::role`/`work_group_id`, below) and the dashboard's own
    // `fulfill_spawn_request` re-validates both at the authority side, the
    // same as every other field a pane can honour.
    if args.max_restarts.is_some()
        || args.timeout_secs.is_some()
        || args.budget_tokens.is_some()
        || args.max_tool_calls.is_some()
        || (!args.flags.is_empty() && pinned_model.is_none())
    {
        eprintln!(
            "zirv ctx agent: dashboard panes don't support --max-restarts/--timeout-secs/\
             --budget-tokens/--max-tool-calls/-- flags other than a --model pin; running headless"
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
        // This is a scripted request-file hand-off to whatever dashboard
        // picks it up next -- this call site cannot vouch that a human is
        // watching that dashboard, so it does not claim `interactive`.
        interactive: false,
        role: args.role.clone(),
        // `session_identity`, not `requested_by` above: the two are
        // deliberately separate (see `SpawnRequest::parent_session`'s own
        // doc comment) even though this call site happens to derive both
        // from the same `ZIRV_CTX_SESSION` env var today.
        parent_session: super::mail::session_identity(env),
        work_group_id: args.group.clone(),
        // Issue #155, Phase 6(c): this process's own spawn gate (`run_with`,
        // above) already evaluated `--force` against the reading it had
        // before this request was ever written. Carrying it forward lets
        // `fulfill_spawn_request` honour the identical override rather than
        // re-litigating it blind and refusing a spawn the requester already
        // chose to force -- see `SpawnRequest::force`'s own doc comment.
        force: args.force,
    };
    let path = match spawnreq::write_request(&dir, &req) {
        Ok(path) => path,
        Err(e) => {
            eprintln!(
                "zirv ctx agent: could not write a spawn request into {}: {e}; running headless",
                dir.display()
            );
            return None;
        }
    };
    let Some(stem) = spawnreq::request_stem(&path) else {
        eprintln!(
            "zirv ctx agent: could not derive a request stem from {}; running headless",
            path.display()
        );
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
                eprintln!(
                    "zirv ctx agent: dashboard did not answer within {ack_timeout:?} (request \
                     was {}); running headless",
                    path.display()
                );
                return None;
            }
            wait_out_a_claimed_request(&dir, &stem, claim_extension, w)
        }
    }
}

/// The requests directory `try_join_dashboard` should actually offer the
/// spawn request to, given the one directory it inherited via
/// `DASH_REQUESTS_ENV` (`inherited`).
///
/// The inherited directory is tried first, exactly as before issue #145:
/// absent outright, or present with a dead/missing `owner.pid`, both refuse
/// immediately -- no ack wait is ever spent probing a directory that was
/// never going to answer (issue #144's own fix). Only past that point does
/// issue #145's fallback run: every OTHER `<state>/dash/*` token directory is
/// scanned (`dash::discover_live_dash_dirs`) for a live dashboard to offer
/// the request to instead. The case this recovers: a dashboard restarted (a
/// fresh token dir, a fresh `owner.pid`) while this process's own shell still
/// carries the old, now-dead `DASH_REQUESTS_ENV` value inherited from before
/// the restart -- without this fallback the worker went silently headless
/// even though a perfectly live dashboard was one directory away.
///
/// Selection rule (`dash::select_live_dash_dir`): the live candidate whose
/// `owner.pid` has the newest mtime wins (`owner.pid` is written exactly
/// once, at dashboard startup, so its mtime is that dashboard's own start
/// time -- i.e. the most recently started dashboard wins), tied broken by
/// the lexicographically greatest `<dash_short>-<token>` directory name for a
/// deterministic pick when two dashboards start within the filesystem's own
/// mtime resolution. Deliberately NOT filtered or weighted by the
/// requester's own repo: a candidate whose repo does not match this
/// request's `cwd` is not wasted effort to join -- `fulfill_spawn_request`'s
/// `accepted_spawn_cwd` gate refuses such a request outright with a
/// `retryable` ack, which `answer_for_ack` already reads as "fall back to
/// headless" rather than a hard failure, and any request that gate DOES
/// accept always spawns its pane at the request's own `cwd`, never the
/// dashboard's own -- so joining a dashboard hosting a different repo is
/// display-only (the pane simply appears in that dashboard's own sidebar)
/// and can never misroute the task's working directory. See `dash::
/// discover_live_dash_dirs`'s own doc comment for the full argument.
///
/// Every candidate considered (the inherited directory, and every fallback
/// one) is logged via `eprintln`, live or not and whichever way this
/// resolves -- issue #145's own acceptance criterion is that "my pane never
/// appeared" must be diagnosable from this process's own log alone, with no
/// need to go spelunking through dashboard-side state to find out why.
///
/// `None` only when nothing usable was found anywhere; the caller's existing
/// headless fallback then runs unchanged.
fn live_join_target(inherited: &Path, env: EnvLookup<'_>) -> Option<PathBuf> {
    if inherited.is_dir() {
        match super::sessions::dashboard_owner_liveness(inherited) {
            super::sessions::OwnerLiveness::Live => return Some(inherited.to_path_buf()),
            super::sessions::OwnerLiveness::Dead(pid) => {
                eprintln!(
                    "zirv ctx agent: {} names a dashboard that already quit (owner.pid names \
                     dead pid {pid}); looking for another live dashboard",
                    inherited.display()
                );
            }
            super::sessions::OwnerLiveness::Missing => {
                eprintln!(
                    "zirv ctx agent: {} has no readable owner.pid, so no dashboard can be \
                     confirmed live; looking for another live dashboard",
                    inherited.display()
                );
            }
        }
    } else {
        eprintln!(
            "zirv ctx agent: {} (inherited via {}) no longer exists; looking for another live \
             dashboard",
            inherited.display(),
            spawnreq::DASH_REQUESTS_ENV
        );
    }

    let state = match super::state::StateDir::resolve(env) {
        Ok(state) => state,
        Err(e) => {
            eprintln!(
                "zirv ctx agent: could not resolve the state dir to look for another live \
                 dashboard: {e}; running headless"
            );
            return None;
        }
    };
    // The inherited directory may itself live under `state.dash()` and would
    // otherwise show up a second time here, already explained above -- this
    // keeps the fallback log free of a duplicate line for the very directory
    // whose rejection reason was just printed.
    let others: Vec<super::dash::DashCandidate> = super::dash::discover_live_dash_dirs(&state)
        .into_iter()
        .filter(|c| c.requests_dir != inherited)
        .collect();
    // Selected before the log loop below, not after: whether a live candidate
    // is the winner or merely a live-but-passed-over sibling changes what
    // gets printed for it, and every candidate -- winner included -- must
    // appear exactly once. See this function's own doc comment: "every
    // candidate ... is logged, live or not", which a silent `Live => {}` arm
    // here used to violate for every live sibling that lost the selection.
    let winner = super::dash::select_live_dash_dir(&others);
    for candidate in &others {
        let is_winner = winner.is_some_and(|w| w.requests_dir == candidate.requests_dir);
        match candidate.status {
            super::dash::CandidateStatus::Live { pid, .. } if !is_winner => eprintln!(
                "zirv ctx agent: candidate {} is live (owner pid {pid}), not selected",
                candidate.requests_dir.display()
            ),
            super::dash::CandidateStatus::Live { .. } => {}
            super::dash::CandidateStatus::NoOwnerPid => eprintln!(
                "zirv ctx agent: candidate {} has no owner.pid; skipped",
                candidate.requests_dir.display()
            ),
            super::dash::CandidateStatus::DeadOwner(pid) => eprintln!(
                "zirv ctx agent: candidate {} names dead pid {pid}; skipped",
                candidate.requests_dir.display()
            ),
        }
    }
    match winner {
        Some(winner) => {
            eprintln!(
                "zirv ctx agent: joining {} instead",
                winner.requests_dir.display()
            );
            Some(winner.requests_dir.clone())
        }
        None => {
            let considered: Vec<String> = others
                .iter()
                .map(|c| c.requests_dir.display().to_string())
                .collect();
            eprintln!(
                "zirv ctx agent: no other live dashboard found under {} ({}); running headless",
                state.dash().display(),
                if considered.is_empty() {
                    "no other candidates".to_string()
                } else {
                    format!("candidates: {}", considered.join(", "))
                }
            );
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
    validate_role(&args.role)?;
    let prompt = resolve_prompt(&args.prompt, &mut std::io::stdin())?;

    // Loaded here rather than after the dashboard-join attempt below (its
    // former position): the spawn gate needs `cfg.pace` before either fork
    // of this delegation -- a pane spawn and a headless run -- is chosen,
    // and `try_join_dashboard` is the fork point between them. `exec::
    // run_with` still loads its own copy internally on the headless path
    // (the same pattern `chat.rs` already uses ahead of `wrap::run_with`),
    // so this remains one extra read of the same layered config rather than
    // a new code path.
    let cfg = CtxConfig::load_for_launch(repo, env)?;

    // Issue #186: resolve the requested worker model before the spawn gate so
    // an exhausted/low-headroom seat can be translated to an equivalent tier
    // on another enabled harness. This does not launch anything.
    let state = super::state::StateDir::resolve(env)?;
    let now = super::state::now_secs();
    let requested_adapter = adapters::select(Some(&args.name), &[], &cfg)?;
    let requested_command =
        worker_launch_flags(&cfg, &args.name, requested_adapter.as_ref(), &args.flags);
    let requested_model = adapters::last_model_flag(&requested_command);
    let source_model_explicit = flags_pin_model(&args.flags);
    let bounds = super::fallback::TaskBounds {
        tokens: args.budget_tokens,
        tool_calls: args.max_tool_calls,
    };

    let route = super::fallback::route_new_delegation(
        &state,
        &cfg,
        super::fallback::RouteRequest {
            requested: &args.name,
            source_model: requested_model,
            source_model_explicit,
            bounds,
            now,
        },
        args.force,
    );
    let mut routed_args = args.clone();
    let mut route_applied = None;
    if let Some(route) = route
        && let Ok(target_adapter) = adapters::select(Some(&route.selected), &[], &cfg)
        && let Some(flags) =
            translated_route_flags(&args.flags, target_adapter.as_ref(), &route.model)
    {
        routed_args.name = route.selected.clone();
        routed_args.flags = flags;
        let parent_session =
            super::mail::session_identity(env).unwrap_or_else(|| "delegation".to_string());
        let detail = route.detail();
        let _ = super::log::append(
            &state,
            &super::log::Decision {
                ts: now,
                session: &parent_session,
                verb: "agent",
                verdict: "reroute",
                score: 0,
                action: "harness-reroute",
                detail: &detail,
            },
        );
        eprintln!("zirv ctx agent: automatically routed {detail}");
        route_applied = Some(route);
    }

    // The ordinary spawn gate still owns the final decision for whichever
    // harness will actually launch. If cross-harness translation was unsafe
    // (for example vendor-specific passthrough flags), this is the requested
    // harness and today's refusal behavior remains intact.
    let provider = adapters::provider_for_agent_name(Some(&routed_args.name));
    let (collector, estimator) = pace::current_windows(&state, &cfg.pace, now, provider);
    let gate = pace::spawn_gate(&collector, estimator.as_ref(), now, &cfg.pace);
    if let Some(note) = pace::describe_spawn_gate(&gate) {
        eprintln!("zirv ctx agent: {note}");
    }
    if spawn_blocked(&gate, args.force) {
        let fallback_note = if route_applied.is_none() && cfg.fallback.enabled {
            " No admissible fallback harness had enough trusted/assumed headroom."
        } else {
            ""
        };
        return Err(format!(
            "refusing to start new delegated work at this usage level; wait for the window to \
             reset, or pass --force to spend anyway.{fallback_note}"
        )
        .into());
    }

    // Issue #170: resolved once, here, before the dashboard-join fork below,
    // so a pane spawn and the headless fallback both see the identical
    // answer -- an inherited `WORK_GROUP_ENV` binding, or a freshly minted
    // scope-bound group. Shadows the caller's own `&AgentArgs` with an owned
    // copy; every read of `args` below (including inside `try_join_
    // dashboard`) is unaffected by this rebinding.
    //
    // Security review round 2 (Finding 4): deliberately AFTER the spawn gate
    // above, which refuses without ever reaching a launch -- a `--scope`
    // group minted ahead of it was left open, unclaimed and childless on disk
    // for every such refusal. `minted_group` is what this invocation created
    // itself (never an inherited or explicitly named one), and every path
    // below that ends without the delegation starting unwinds it.
    let mut args = routed_args;
    let minted_group = resolve_group_binding(&mut args, &state, env)?;
    let args = &args;
    let discard_minted_group = || {
        if let Some(id) = &minted_group {
            super::group::discard_if_unused(&state, id);
        }
    };

    if let Some(result) = try_join_dashboard(
        args,
        &prompt,
        w,
        repo,
        env,
        DASH_ACK_TIMEOUT,
        DASH_CLAIM_EXTENSION,
    ) {
        // Finding 4: the dashboard answered definitively, and only `Ok(0)`
        // (`answer_for_ack`'s spawned-a-pane arm) means work actually
        // started. A refusal spawned nothing, so a group minted for it
        // moments ago holds nothing -- and `discard_if_unused` still checks
        // that for itself, so the genuinely ambiguous "claimed but never
        // confirmed" answer cannot delete a group a pane really did claim.
        //
        // Bounded race on `Ok(EXIT_DASH_UNCONFIRMED)`: the dashboard has
        // already taken the request (so it will not be retried) but a slow
        // dashboard may not yet have reached `admit_child` on this group when
        // the discard below runs. If it lands in that window the still-
        // pristine group is deleted out from under the in-flight admission,
        // which then finds no group and refuses ("no work group") instead of
        // spawning. Accepted: a clean refusal here is preferable to leaving
        // group cleanup dependent on winning a race with a dashboard that may
        // be arbitrarily slow or may never answer at all.
        if !matches!(result, Ok(0)) {
            discard_minted_group();
        }
        return result;
    }

    let announcer = Announcer::new(
        cfg.chrome.events && !args.quiet,
        console::colors_enabled_stderr(),
    );
    // Finding 3: the launch below runs under an env lookup that carries this
    // delegation's own group, so `exec::run_with` can export it to the child.
    let quieted = quiet_env(env, args.quiet);
    let env = group_env(&quieted, args.group.clone());

    // Resolved here, ahead of `exec::run_with`'s own (identical) selection
    // further down, purely to compute the default worker model this spawn
    // launches with -- see `worker_launch_flags`. `&[]` for the command: it
    // only matters to `select` when `name` is `None`, and this call always
    // passes the delegation target explicitly.
    let adapter = adapters::select(Some(&args.name), &[], &cfg)?;
    let command = worker_launch_flags(&cfg, &args.name, adapter.as_ref(), &args.flags);
    // Read back out of the effective argv rather than re-deriving it: this
    // is whichever of the operator's own `--model`/`-m` passthrough or the
    // configured/default worker-model prepend actually won.
    let model = adapters::last_model_flag(&command).map(str::to_string);
    let worker_session = SessionId::new_v4().to_string();
    // Issue #170: this delegation binds `args.group` (if any) to the child
    // about to run headlessly as its SubOrchestrator -- first-claim-wins, so
    // a group shared by an operator across several `--group` invocations is
    // only ever auto-closed by whichever one actually claimed it (below).
    // Best-effort: a claim failure (a group swept between `resolve_group_
    // binding` and here) must not fail an otherwise-runnable delegation --
    // `resolve_worker_budget`, right after this, is what actually enforces
    // that the named group still exists at all.
    if args.role.as_deref() == Some("sub-orchestrator")
        && let Some(id) = &args.group
    {
        let _ = super::group::claim_sub_orchestrator(
            &state,
            id,
            &super::sessions::short_id(&worker_session),
        );
    }
    // Issue #155, Phase 5(d): resolved before the launch, not inside
    // `exec::run_with` -- an unknown or closed `--group` must fail this
    // delegation outright rather than silently running it unbounded.
    let worker_budget = match resolve_worker_budget(&env, args) {
        Ok(budget) => budget,
        Err(e) => {
            // Finding 4: nothing ran, so a group minted moments ago for this
            // delegation must not outlive it.
            discard_minted_group();
            return Err(e);
        }
    };

    let exec_args = ExecArgs {
        agent: Some(args.name.clone()),
        session_id: Some(worker_session.clone()),
        transcript: None,
        // Data, never argv: `run_with` builds the launch from the adapter
        // itself when the trailing command carries no program name, exactly
        // as every restart already does. Encoding the prompt into `command`
        // here only to have `exec::run_with` parse it back out is what would
        // let a prompt shaped like a flag be misread as one.
        prompt: Some(prompt),
        max_restarts: args.max_restarts,
        timeout_secs: args.timeout_secs,
        budget_tokens: worker_budget.tokens,
        max_tool_calls: worker_budget.tool_calls,
        command,
        simple: false,
    };

    announcer.emit(&Event::DelegatedStart {
        agent: args.name.clone(),
    });
    let started = std::time::Instant::now();
    // Re-review (2026-08-27) finding 1: `resolve_worker_budget` above already
    // admitted this delegation into `--group` (if any) before this fallible
    // spawn is even attempted -- `exec::run_with` can still fail outright
    // (e.g. `--max-tool-calls` refused up front for an adapter that cannot
    // enforce it, issue #155 review finding C2), and a failure here must not
    // permanently burn the group's admission slot for a child that never
    // ran. `state` is the same handle resolved above for the spawn gate,
    // unused since; reused here rather than re-resolved.
    let code = match exec::run_with(&exec_args, w, repo, &env) {
        Ok(code) => code,
        Err(e) => {
            if let Some(id) = &args.group {
                super::group::rollback_admission(&state, id);
            }
            // Finding 4: with the admission rolled back the group is pristine
            // again, so a group this invocation minted for a launch that
            // never happened is removed rather than left open forever.
            discard_minted_group();
            return Err(e);
        }
    };
    // Issue #170: this delegation's scope is done -- successfully or not --
    // the moment its supervised run exits (completion contract and spend are
    // both left for a reviewer to check against `zirv ctx group status`,
    // never machine-verified -- see `WorkGroup::completion_contract`'s own
    // doc comment). Closes the group only when THIS session is the one that
    // actually claimed it above, never a group some other, concurrent
    // claimant owns -- an operator's own shared `--group` across several
    // unrelated invocations must not be closed out from under whichever of
    // them still has work left.
    if args.role.as_deref() == Some("sub-orchestrator")
        && let Some(id) = &args.group
        && let Ok(Some(group)) = super::group::load(&state, id)
        && group.sub_orchestrator_session.as_deref()
            == Some(super::sessions::short_id(&worker_session).as_str())
    {
        let _ = super::group::close(&state, id, super::state::now_secs());
    }
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    announcer.emit(&Event::DelegatedFinish {
        agent: args.name.clone(),
        meaning: exec::describe_exit(code),
    });
    if let Some(note) = exit_note(code) {
        eprintln!("zirv ctx agent: {note}");
    }

    // Best effort throughout: a delegation that ran must never fail because
    // its accounting could not be written (issue #155, Phase 2).
    if let Ok(state_dir) = super::state::StateDir::resolve(&env) {
        let usage = std::fs::read_to_string(adapter.transcript_path(&super::event::SessionRef {
            id: SessionId::parse(&worker_session),
            cwd: repo.to_path_buf(),
        }))
        .ok()
        .and_then(|body| adapter.transcript_usage(&body))
        .unwrap_or_default();
        let parent_session = super::mail::session_identity(&env).unwrap_or_default();
        let outcome = delegation_outcome(code);
        let detail = format!(
            "{} ({}): {} in / {} cache-creation / {} cache-read / {} out in {}ms -- {}",
            args.name,
            model.as_deref().unwrap_or("default worker model"),
            usage.input_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens,
            wall_ms,
            outcome,
        );
        let _ = super::log::append_delegation(
            &state_dir,
            &super::log::Delegation {
                ts: super::state::now_secs(),
                session: &worker_session,
                parent_session: &parent_session,
                // Issue #155 review finding D2: this used to be hardcoded
                // `None`, so a completed headless delegation never recorded
                // the group it actually ran under -- `zirv ctx status`'s
                // group tree (`status::group_tree_lines`) renders every such
                // delegation as ungrouped even though `--group` traveled all
                // the way through `resolve_worker_budget` above.
                work_group_id: args.group.as_deref(),
                agent: &args.name,
                model: model.as_deref(),
                input_tokens: usage.input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
                wall_ms,
                exit_code: code,
                outcome,
            },
        );
        let _ = super::log::append(
            &state_dir,
            &super::log::Decision {
                ts: super::state::now_secs(),
                session: &worker_session,
                verb: "agent",
                verdict: "n/a",
                score: 0,
                action: super::log::DELEGATION_ACTION,
                detail: &detail,
            },
        );
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

    /// The classifier half, testable without spawning anything: a completed
    /// delegation's outcome label must distinguish the supervisor's own two
    /// failure modes from an ordinary non-zero exit, because "the worker
    /// failed" and "zirv gave up on the worker" cost very different things.
    #[test]
    fn a_delegation_outcome_names_the_supervisors_own_failures() {
        assert_eq!(delegation_outcome(0), "ok");
        assert_eq!(
            delegation_outcome(exec::EXIT_ROT_EXHAUSTED),
            "rot-exhausted"
        );
        assert_eq!(delegation_outcome(exec::EXIT_TIMEOUT), "timeout");
        assert_eq!(
            delegation_outcome(exec::EXIT_BUDGET_EXHAUSTED),
            "budget-exhausted"
        );
        assert_eq!(delegation_outcome(1), "failed");
    }

    /// `--force` is the operator saying they accept the spend. Only a
    /// Refuse is overridable; a Warn was never blocking, and a Proceed has
    /// nothing to override.
    #[test]
    fn only_a_refusal_is_overridable_and_only_by_force() {
        let refuse = pace::SpawnGate::Refuse {
            window: "five_hour",
            percent: 97.0,
            source: pace::Source::Collector,
        };
        assert!(spawn_blocked(&refuse, false));
        assert!(!spawn_blocked(&refuse, true), "--force proceeds");

        let warn = pace::SpawnGate::Warn {
            window: "five_hour",
            percent: 85.0,
            source: pace::Source::Collector,
        };
        assert!(!spawn_blocked(&warn, false), "a warning never blocks");
        assert!(!spawn_blocked(&pace::SpawnGate::Proceed, false));
    }

    /// Issue #155, Phase 5(d): a budget bounds WORK. At 80% the worker is
    /// nudged to wrap up and checkpoint; at 100% it is checkpointed and
    /// stopped with a structured result demand. It is NEVER a signal to
    /// switch models -- a cheaper answer to the wrong question is not a
    /// saving, and automatic downshift is explicitly out of scope.
    #[test]
    fn a_token_budget_warns_at_eighty_percent_and_stops_at_the_limit() {
        let budget = WorkerBudget {
            tokens: Some(100_000),
            tool_calls: None,
        };
        let at = |context: u64| TranscriptUsage {
            input_tokens: context,
            ..Default::default()
        };

        assert_eq!(budget_state(&budget, &at(79_999), 0), BudgetState::Ok);
        assert!(matches!(
            budget_state(&budget, &at(80_000), 0),
            BudgetState::SoftWarn { limit: 100_000, .. }
        ));
        assert!(matches!(
            budget_state(&budget, &at(100_000), 0),
            BudgetState::HardStop { .. }
        ));
        assert!(matches!(
            budget_state(&budget, &at(1_000_000), 0),
            BudgetState::HardStop { .. }
        ));
    }

    /// The budget counts what the run actually spends -- every input class
    /// plus output -- not just uncached input, which is near zero in a cached
    /// session and would make the budget never fire.
    #[test]
    fn a_token_budget_counts_every_class_the_run_spends() {
        let budget = WorkerBudget {
            tokens: Some(100_000),
            tool_calls: None,
        };
        let cached = TranscriptUsage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 9_000,
            cache_read_input_tokens: 89_000,
            output_tokens: 1_000,
        };
        assert!(
            matches!(
                budget_state(&budget, &cached, 0),
                BudgetState::HardStop { .. }
            ),
            "100k spent across four classes is 100k spent"
        );
    }

    /// Tool calls are their own ceiling: a worker can burn a budget in cheap
    /// calls without moving the token count much, and a runaway loop is
    /// exactly what the rot engine's repetition signal already watches for.
    #[test]
    fn a_tool_call_ceiling_is_independent_of_the_token_ceiling() {
        let budget = WorkerBudget {
            tokens: None,
            tool_calls: Some(50),
        };
        let none = TranscriptUsage::default();
        assert_eq!(budget_state(&budget, &none, 39), BudgetState::Ok);
        assert!(matches!(
            budget_state(&budget, &none, 40),
            BudgetState::SoftWarn { .. }
        ));
        assert!(matches!(
            budget_state(&budget, &none, 50),
            BudgetState::HardStop { .. }
        ));
    }

    /// No budget is no change: every delegation before 2.35.0 ran unbounded
    /// and must continue to.
    #[test]
    fn no_budget_never_warns_and_never_stops() {
        let budget = WorkerBudget {
            tokens: None,
            tool_calls: None,
        };
        let huge = TranscriptUsage {
            input_tokens: u64::MAX,
            ..Default::default()
        };
        assert_eq!(budget_state(&budget, &huge, u32::MAX), BudgetState::Ok);
    }

    /// A `--group` supplies defaults the flags may only TIGHTEN. A worker
    /// must not be able to talk its way past the group's own ceiling by
    /// passing a larger `--budget-tokens`.
    #[test]
    fn an_explicit_budget_may_only_tighten_the_groups_own() {
        let group_budget = Some(100_000);
        assert_eq!(
            resolve_budget_tokens(group_budget, Some(50_000)),
            Some(50_000)
        );
        assert_eq!(
            resolve_budget_tokens(group_budget, Some(500_000)),
            Some(100_000)
        );
        assert_eq!(resolve_budget_tokens(group_budget, None), Some(100_000));
        assert_eq!(resolve_budget_tokens(None, Some(50_000)), Some(50_000));
        assert_eq!(resolve_budget_tokens(None, None), None);
    }

    fn sample_work_group(
        id: &str,
        child_limit: u32,
        admitted: u32,
    ) -> crate::commands::ctx::group::WorkGroup {
        crate::commands::ctx::group::WorkGroup {
            work_group_id: id.to_string(),
            parent_session_id: String::new(),
            scope: "test batch".to_string(),
            child_limit,
            token_budget: None,
            deadline_secs: None,
            completion_contract: String::new(),
            created_at: 0,
            closed_at: None,
            admitted_children: admitted,
            sub_orchestrator_session: None,
        }
    }

    /// Issue #155 review finding D2: `resolve_worker_budget` is the headless
    /// admission choke point for `child_limit` -- a `--group` naming a group
    /// already at its limit must be refused outright, the same "hard error,
    /// not silent ignore" contract this function's own doc comment already
    /// applies to an unknown or closed group.
    #[test]
    fn resolve_worker_budget_refuses_a_group_already_at_its_child_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 1, 1))
            .expect("create");
        let mut env = HashMap::new();
        env.insert(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().display().to_string(),
        );

        let mut args = args_for("claude", "go");
        args.group = Some("wg-1".to_string());
        let err =
            resolve_worker_budget(&|k| env.get(k).cloned(), &args).expect_err("group is full");
        assert!(err.to_string().contains("wg-1"), "got {err}");
    }

    /// The same choke point admits a delegation cleanly under the limit, and
    /// advances the group's own `admitted_children` count by exactly one.
    #[test]
    fn resolve_worker_budget_admits_a_delegation_under_the_child_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 3, 1))
            .expect("create");
        let mut env = HashMap::new();
        env.insert(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().display().to_string(),
        );

        let mut args = args_for("claude", "go");
        args.group = Some("wg-1".to_string());
        resolve_worker_budget(&|k| env.get(k).cloned(), &args).expect("admitted under the limit");

        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            2,
            "the admission must advance the group's own count"
        );
    }

    /// Re-review (2026-08-27) finding 1: a delegation that admits into a
    /// group and then fails before a child is genuinely launched must not
    /// permanently burn that admission slot. `--max-tool-calls` with the
    /// codex adapter is refused by `exec::run_with` itself (issue #155
    /// review finding C2) strictly AFTER `resolve_worker_budget` has already
    /// admitted the child into its group -- exactly this finding's failure
    /// window.
    #[test]
    fn a_failed_delegation_after_admission_rolls_back_the_group_slot() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 3, 0))
            .expect("create group");

        let env = base_env(&state_path);
        let mut args = args_for("codex", "go");
        args.group = Some("wg-1".to_string());
        args.max_tool_calls = Some(5);

        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex + --max-tool-calls is refused by exec::run_with");
        assert!(err.to_string().contains("--max-tool-calls"), "got {err}");

        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            0,
            "the failed delegation must not have permanently burned the group slot"
        );
    }

    /// A successful delegation still counts exactly once against its group --
    /// the rollback added for the failure path above must never also undo a
    /// genuine admission for a child that actually ran.
    #[test]
    fn a_successful_delegation_still_counts_exactly_one_admission() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 3, 0))
            .expect("create group");

        let env = base_env(&state_path);
        let mut args = args_for("claude", "go");
        args.group = Some("wg-1".to_string());

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a plain claude delegation with no --max-tool-calls runs cleanly");
        assert_eq!(code, 0);

        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            1,
            "a successful spawn must still count exactly one admission"
        );
    }

    /// `--role` accepts exactly the two spellings the depth cap and
    /// `spawnreq::role_of` understand; anything else must be refused before
    /// launch, not silently read as a Worker three call frames later.
    #[test]
    fn validate_role_accepts_only_the_two_known_spellings() {
        assert!(validate_role(&None).is_ok());
        assert!(validate_role(&Some("worker".to_string())).is_ok());
        assert!(validate_role(&Some("sub-orchestrator".to_string())).is_ok());
        let err = validate_role(&Some("orchestrator".to_string()))
            .expect_err("orchestrator is not a spawnable role");
        assert!(err.to_string().contains("--role"), "got {err}");
    }

    /// Issue #170: an operator's own explicit `--group` always wins, over
    /// both the inherited env binding and a `--scope` that would otherwise
    /// mint a fresh one.
    #[test]
    fn resolve_group_binding_prefers_an_explicit_group_over_everything_else() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = super::super::state::StateDir::from_root(tmp.path().to_path_buf());
        let env: HashMap<String, String> =
            [(WORK_GROUP_ENV.to_string(), "wg-inherited".to_string())].into();

        let mut args = args_for("claude", "do the work");
        args.group = Some("wg-explicit".to_string());
        args.scope = Some("some scope".to_string());
        args.role = Some("sub-orchestrator".to_string());
        resolve_group_binding(&mut args, &state, &|k| env.get(k).cloned()).expect("resolves");
        assert_eq!(args.group.as_deref(), Some("wg-explicit"));
    }

    /// The "lineage rather than convention" binding itself: no `--group` of
    /// its own, but `WORK_GROUP_ENV` was inherited (the same real process env
    /// a SubOrchestrator's own launch was seeded with) -- picked up with no
    /// operator action required.
    #[test]
    fn resolve_group_binding_falls_back_to_the_inherited_env_var() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = super::super::state::StateDir::from_root(tmp.path().to_path_buf());
        let env: HashMap<String, String> =
            [(WORK_GROUP_ENV.to_string(), "wg-inherited".to_string())].into();

        let mut args = args_for("claude", "do the work");
        resolve_group_binding(&mut args, &state, &|k| env.get(k).cloned()).expect("resolves");
        assert_eq!(args.group.as_deref(), Some("wg-inherited"));
    }

    /// Issue #170: `--scope` alongside `--role sub-orchestrator`, with no
    /// `--group` resolved any other way, mints a fresh group scoped to it --
    /// one command instead of a separate `zirv ctx group create` step first.
    #[test]
    fn resolve_group_binding_mints_a_scope_bound_group_for_a_sub_orchestrator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = super::super::state::StateDir::from_root(tmp.path().to_path_buf());
        let env: HashMap<String, String> = HashMap::new();

        let mut args = args_for("claude", "do the work");
        args.role = Some("sub-orchestrator".to_string());
        args.scope = Some("own the frontend rewrite".to_string());
        args.budget_tokens = Some(250_000);
        let minted =
            resolve_group_binding(&mut args, &state, &|k| env.get(k).cloned()).expect("resolves");

        let id = args.group.clone().expect("a group was minted");
        assert_eq!(
            minted.as_deref(),
            Some(id.as_str()),
            "a minted group is reported back, so its caller can unwind it (Finding 4)"
        );
        let group = super::super::group::load(&state, &id)
            .expect("load")
            .expect("present");
        assert_eq!(group.scope, "own the frontend rewrite");
        assert_eq!(group.token_budget, Some(250_000));
        assert_eq!(group.child_limit, super::super::group::DEFAULT_CHILD_LIMIT);
    }

    /// A plain worker request (no `--role sub-orchestrator`) must never mint
    /// a group just because `--scope` happened to be set -- only a
    /// coordinator owns a scope.
    #[test]
    fn resolve_group_binding_does_not_mint_for_a_plain_worker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = super::super::state::StateDir::from_root(tmp.path().to_path_buf());
        let env: HashMap<String, String> = HashMap::new();

        let mut args = args_for("claude", "do the work");
        args.scope = Some("own the frontend rewrite".to_string());
        resolve_group_binding(&mut args, &state, &|k| env.get(k).cloned()).expect("resolves");
        assert_eq!(args.group, None);
    }

    // `worker_launch_flags`/`flags_pin_model`: pure, so these are testable
    // against a plain adapter without spawning anything.

    /// The `--permission-mode`/`--sandbox`/`--ask-for-approval` prefix these
    /// assertions expect is the shipped-default "sandboxed, no prompts"
    /// posture (2026-08-22) -- see `SandboxConfig`'s own doc comment. It is
    /// independent of the model pin: an operator's own `--model` still wins
    /// over the *model* prepend, but the policy/sandbox prepend is a
    /// separate concern and still applies.
    #[test]
    fn flag_passthrough_wins_over_the_configured_or_default_worker_model() {
        let adapter = super::super::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let flags = vec!["--model".to_string(), "opus".to_string()];
        let mut expected = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::adapters::LaunchMode::Headless,
        );
        expected.extend(flags.iter().cloned());
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &adapter, &flags),
            expected,
            "the operator's own --model must reach argv unchanged (after the sandbox prefix)"
        );

        let joined = vec!["--model=opus".to_string()];
        let mut expected_joined = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::adapters::LaunchMode::Headless,
        );
        expected_joined.extend(joined.iter().cloned());
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &adapter, &joined),
            expected_joined,
            "the --model=value joined form must also be recognised as already pinned"
        );
    }

    /// FIX 2: codex's own `-m` short alias must pin exactly like `--model`,
    /// or a configured `worker.codex` gets a conflicting `--model` prepended
    /// ahead of an operator's own `-m <value>`.
    #[test]
    fn codexs_short_m_alias_also_pins_the_model_for_worker_launches() {
        let adapter = super::super::adapters::codex::CodexAdapter::new(None)
            .with_exec_ask_for_approval_forced(true);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: None,
                codex: Some("gpt-5.6-terra".to_string()),
            },
            ..CtxConfig::default()
        };
        let sandbox_prefix = || {
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        };

        let bare = vec!["-m".to_string(), "opus".to_string()];
        let mut expected = sandbox_prefix();
        expected.extend(bare.iter().cloned());
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &bare),
            expected,
            "the operator's own -m must reach argv unchanged, not gain a conflicting --model"
        );

        let joined = vec!["-m=opus".to_string()];
        let mut expected_joined = sandbox_prefix();
        expected_joined.extend(joined.iter().cloned());
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &joined),
            expected_joined,
            "the -m=value joined form must also be recognised as already pinned"
        );

        // FIX 2 (round 2): the attached short form, `-mopus` with no
        // separator at all, must be recognised too, or `zirv ctx agent
        // codex "p" -- -mopus` with `worker.codex` configured gets a
        // conflicting `--model` prepended ahead of the operator's own flag.
        let attached = vec!["-mopus".to_string()];
        let mut expected_attached = sandbox_prefix();
        expected_attached.extend(attached.iter().cloned());
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &adapter, &attached),
            expected_attached,
            "the attached -mvalue short form must also be recognised as already pinned"
        );
    }

    /// A long flag that merely starts with `-m` once its leading `-` is
    /// peeled (`--model-foo`) must not be misread as the attached short
    /// form: `worker.codex`'s configured model still gets prepended ahead
    /// of it, exactly as for any other unrelated flag.
    #[test]
    fn a_long_flag_starting_with_m_does_not_false_positive_as_pinning() {
        let adapter = super::super::adapters::codex::CodexAdapter::new(None)
            .with_exec_ask_for_approval_forced(true);
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
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
                "--model-foo".to_string(),
                "opus".to_string(),
            ],
            "an unrelated flag must not suppress the configured-model prepend, and the \
             shipped-default sandbox prefix still applies"
        );
    }

    /// `--allowedTools` is itself one of the flags `adapters::flags_pin_
    /// policy` recognises, so the shipped-default sandbox prefix is
    /// correctly withheld here -- the operator's own explicit tool-access
    /// flag already pins the same concern.
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
            ],
            "flags_pin_policy withholds the sandbox prefix: the operator's own \
             --allowedTools already pins the same concern"
        );
    }

    /// The shipped default (2026-08-22) is no longer empty: `cfg.sandbox.
    /// enabled` defaults `true`, so `default_sandbox_args` prepends the
    /// posture's own flags ahead of the model default.
    #[test]
    fn claude_gets_the_sonnet_default_and_the_shipped_sandbox_posture_when_nothing_is_configured_or_passed()
     {
        let adapter = super::super::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let mut expected = vec!["--model".to_string(), "sonnet".to_string()];
        expected.extend(adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::adapters::LaunchMode::Headless,
        ));
        assert_eq!(worker_launch_flags(&cfg, "claude", &adapter, &[]), expected);
    }

    /// No model flag (codex has no adapter-owned worker-model default), but
    /// the shipped-default sandbox posture still applies.
    #[test]
    fn codex_gets_no_model_flag_but_still_gets_the_shipped_sandbox_posture() {
        let adapter = super::super::adapters::codex::CodexAdapter::new(None)
            .with_exec_ask_for_approval_forced(true);
        let cfg = CtxConfig::default();
        let out = worker_launch_flags(&cfg, "codex", &adapter, &[]);
        assert!(
            !out.contains(&"--model".to_string()),
            "codex has no adapter-owned default, so its own config default applies untouched: {out:?}"
        );
        assert_eq!(
            out,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );
    }

    /// Bug B, 2026-08-22 revision: the shipped default is now the
    /// "sandboxed, no prompts" posture, not an empty argv -- this test used
    /// to assert byte-for-byte silence under the default; it is inverted
    /// (per instruction, not deleted) to assert the exact new-default argv
    /// per adapter, plus the explicit opt-out (`[sandbox] enabled = false`)
    /// restoring the old, empty-by-default behaviour.
    #[test]
    fn worker_launch_flags_emits_the_shipped_sandbox_posture_by_default_and_nothing_when_opted_out()
    {
        let cfg = CtxConfig::default();
        assert_eq!(
            cfg.policy,
            crate::commands::ctx::policy::EffectivePolicy::default(),
            "[policy] itself is untouched by this posture"
        );
        assert!(cfg.sandbox.enabled, "the shipped default is sandboxed");

        let claude = super::super::adapters::claude::ClaudeAdapter::new(None);
        let mut expected_claude = vec!["--model".to_string(), "sonnet".to_string()];
        expected_claude.extend(claude.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::adapters::LaunchMode::Headless,
        ));
        assert_eq!(
            worker_launch_flags(&cfg, "claude", &claude, &[]),
            expected_claude
        );
        let codex = super::super::adapters::codex::CodexAdapter::new(None)
            .with_exec_ask_for_approval_forced(true);
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &codex, &[]),
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );

        // The opt-out: an operator who explicitly disables the posture gets
        // the pre-2026-08-22 behaviour back -- an empty argv from this seam,
        // with no `[policy]` configured either.
        let opted_out = CtxConfig {
            sandbox: crate::commands::ctx::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..CtxConfig::default()
        };
        assert!(
            worker_launch_flags(&opted_out, "codex", &codex, &[]).is_empty(),
            "codex has no worker-model default either, so an opted-out launch is silent"
        );
        assert_eq!(
            worker_launch_flags(&opted_out, "claude", &claude, &[]),
            vec!["--model".to_string(), "sonnet".to_string()],
            "claude still gets its own worker-model default; only the sandbox prefix is gone"
        );
    }

    /// A `[policy] shell_exec = "deny"` must reach a delegated headless
    /// worker's real launch argv on both adapters *on top of* the shipped
    /// sandbox baseline, from the same `cfg.policy` -- claude's tool-deny
    /// pin, and codex's `policy_args` restating (more strictly) the same
    /// `--sandbox`/`--ask-for-approval` flags `default_sandbox_args` already
    /// emitted. The duplication is intentional and harmless: both CLIs take
    /// the last occurrence of a single-value flag, and the later, explicit
    /// `Deny`-driven values (`read-only`) are strictly stricter than the
    /// baseline (`workspace-write`), so they are the ones that end up
    /// governing the launch.
    #[test]
    fn worker_launch_flags_layers_an_explicit_policy_deny_on_top_of_the_sandbox_baseline() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let cfg = CtxConfig {
            policy: EffectivePolicy {
                shell_exec: Stance::Deny,
                ..EffectivePolicy::default()
            },
            ..CtxConfig::default()
        };

        let claude = super::super::adapters::claude::ClaudeAdapter::new(None);
        let claude_flags = worker_launch_flags(&cfg, "claude", &claude, &[]);
        let mut expected_claude = vec!["--model".to_string(), "sonnet".to_string()];
        expected_claude.extend(claude.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::adapters::LaunchMode::Headless,
        ));
        expected_claude.push("--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string());
        assert_eq!(claude_flags, expected_claude);

        let codex = super::super::adapters::codex::CodexAdapter::new(None)
            .with_ignore_flags_forced(false)
            .with_exec_ask_for_approval_forced(true);
        let codex_flags = worker_launch_flags(&cfg, "codex", &codex, &[]);
        assert_eq!(
            codex_flags,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ],
            "the later, stricter --sandbox read-only is the one that wins (last occurrence)"
        );
    }

    /// The operator's own trailing flags still reach argv unchanged (and
    /// still win, since both CLIs take the last occurrence of a single-value
    /// flag) even under the sandbox baseline plus a configured `[policy]`
    /// restriction.
    #[test]
    fn worker_launch_flags_keeps_the_operators_own_flags_after_a_configured_policy() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let cfg = CtxConfig {
            policy: EffectivePolicy {
                shell_exec: Stance::Deny,
                ..EffectivePolicy::default()
            },
            ..CtxConfig::default()
        };
        let codex = super::super::adapters::codex::CodexAdapter::new(None)
            .with_ignore_flags_forced(false)
            .with_exec_ask_for_approval_forced(true);
        let flags = vec!["--verbose".to_string()];
        let out = worker_launch_flags(&cfg, "codex", &codex, &flags);
        assert_eq!(
            out,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
                "--verbose".to_string(),
            ]
        );
    }

    /// An operator's own explicit `--sandbox`/`--ask-for-approval`/
    /// `--permission-mode`/`--disallowedTools` pin (any spelling `adapters::
    /// flags_pin_policy` recognises) suppresses the *entire* zirv-computed
    /// prefix -- baseline sandbox posture and any explicit `[policy]` Deny
    /// alike -- not merely the baseline half of it. The operator's own
    /// choice must demonstrably win, not merely happen to survive because a
    /// CLI takes the last occurrence.
    #[test]
    fn an_operators_own_sandbox_flag_suppresses_the_entire_zirv_computed_prefix() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let cfg = CtxConfig {
            policy: EffectivePolicy {
                shell_exec: Stance::Deny,
                ..EffectivePolicy::default()
            },
            ..CtxConfig::default()
        };
        let codex = super::super::adapters::codex::CodexAdapter::new(None);
        let flags = vec!["--sandbox".to_string(), "danger-full-access".to_string()];
        assert_eq!(
            worker_launch_flags(&cfg, "codex", &codex, &flags),
            flags,
            "the operator's own --sandbox pin must reach argv completely unaugmented"
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
                format!("sh {}", fixture("fake-agent.sh").display()),
            ),
            // T8: `run_with`'s `sleep_fn` is real `std::thread::sleep`, and a
            // fresh temp state dir has no usage source by construction --
            // see the identical comment on `exec.rs`'s and `run_loop.rs`'s
            // own `base_env`. Without this, any delegated run through this
            // module's `run_with` pays the real, wall-clock fail-safe delay
            // (default 60s) once per call.
            (
                "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
                "0".to_string(),
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
            role: None,
            group: None,
            scope: None,
            budget_tokens: None,
            max_tool_calls: None,
            force: false,
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

    /// Issue #155, Phase 2: the end-to-end write, against a real
    /// `AgentAdapter` (`ClaudeAdapter`) and a real fake-agent transcript --
    /// not just `log.rs`'s own isolated `append_delegation`/`tail_
    /// delegations` round trip. A completed delegation must leave exactly
    /// one `Delegation` record with a real (non-zero) cache-read count read
    /// back off the worker's own transcript, and exactly one
    /// `delegation-complete` line in the main decision log naming the same
    /// verb.
    #[test]
    fn a_completed_delegation_writes_a_checkpoint_record() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let state = crate::commands::ctx::state::StateDir::resolve(&|k| env.get(k).cloned())
            .expect("state resolves");
        let delegations = crate::commands::ctx::log::tail_delegations(&state, 10).expect("tail");
        assert_eq!(
            delegations.len(),
            1,
            "exactly one checkpoint record per delegation: {delegations:?}"
        );
        let record: serde_json::Value = serde_json::from_str(&delegations[0]).expect("json");
        assert_eq!(record["agent"], "claude");
        // Pins the argv -> model field wiring end-to-end: no `--model` was
        // passed, so `worker_launch_flags` prepends claude's own configured-
        // or-default worker model (`ClaudeAdapter::default_worker_model`,
        // "sonnet" with nothing configured), and `adapters::last_model_flag`
        // must read that exact value back out of the effective argv.
        assert_eq!(record["model"], "sonnet");
        assert_eq!(record["exit_code"], 0);
        assert_eq!(record["outcome"], "ok");
        assert!(
            record["cache_read_input_tokens"].as_u64().unwrap_or(0) > 0,
            "must read real usage back off the worker's own transcript: {record}"
        );
        assert!(
            !record["session"].as_str().unwrap_or("").is_empty(),
            "must carry the worker's own session id: {record}"
        );

        let decisions = crate::commands::ctx::log::tail(&state, 10).expect("tail");
        assert!(
            decisions
                .iter()
                .any(|line| line.contains(crate::commands::ctx::log::DELEGATION_ACTION)),
            "the main decision log must also get a one-line delegation-complete marker: \
             {decisions:?}"
        );
    }

    /// Issue #155 review finding D2: a completed headless delegation must
    /// record the group it actually ran under -- before this fix `agent.rs`
    /// hardcoded `work_group_id: None` on every completion log record, so
    /// `zirv ctx status`'s group tree rendered every delegation as
    /// ungrouped no matter what `--group` was passed.
    #[test]
    fn a_completed_delegation_records_its_own_work_group_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "test batch".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut args = args_for("claude", "do the work");
        args.group = Some(group_id.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let delegations = crate::commands::ctx::log::tail_delegations(&state, 10).expect("tail");
        assert_eq!(delegations.len(), 1);
        let record: serde_json::Value = serde_json::from_str(&delegations[0]).expect("json");
        assert_eq!(
            record["work_group_id"].as_str(),
            Some(group_id.as_str()),
            "the completion record must name the real group: {record}"
        );
    }

    /// Issue #170: a SubOrchestrator's group closes automatically once its
    /// own supervised run ends -- "when a sub-orchestrator finishes its
    /// scope, its group closes" -- with no separate `zirv ctx group close`
    /// step required.
    #[test]
    fn a_completed_sub_orchestrator_delegation_closes_its_own_claimed_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "own the frontend rewrite".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut args = args_for("claude", "do the work");
        args.role = Some("sub-orchestrator".to_string());
        args.group = Some(group_id.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let group = crate::commands::ctx::group::load(&state, &group_id)
            .expect("load")
            .expect("present");
        assert!(
            group.closed_at.is_some(),
            "the sub-orchestrator's own scope finished; the group must close itself"
        );
        assert!(
            group.sub_orchestrator_session.is_some(),
            "the group must name who claimed and closed it"
        );
    }

    /// Security review round 2 (Finding 3): the headless fork of a
    /// coordinator delegation resolved (and here mints) a group, but exported
    /// nothing -- only the dashboard fork pushed `WORK_GROUP_ENV` into its
    /// pane's environment. So a headless sub-orchestrator's own children
    /// resolved `group = None`: no admission, no child limit, no token
    /// ceiling, "ungrouped" in the status tree. The child's real environment
    /// is what proves the fix, read back through the fixture's own env log.
    #[test]
    fn a_headless_coordinators_child_inherits_the_group_it_was_bound_to() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_env_log = tmp.path().join("group-env.log");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_GROUP_ENV_LOG", &group_env_log);
        }
        let mut args = args_for("claude", "own this scope");
        args.role = Some("sub-orchestrator".to_string());
        args.scope = Some("the frontend rewrite".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_GROUP_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let groups = crate::commands::ctx::group::list(&state);
        assert_eq!(groups.len(), 1, "the scope minted exactly one group");
        let inherited = std::fs::read_to_string(&group_env_log).expect("the child logged its env");
        assert_eq!(
            inherited.trim(),
            groups[0].work_group_id,
            "the coordinator's own child must carry the group it was bound to"
        );
    }

    /// The other half of Finding 3, end to end: a child that inherits that
    /// exact environment -- a `zirv ctx agent` call with no `--group` of its
    /// own, run from inside a coordinator's harness -- resolves the SAME
    /// group and is admitted against its child limit, which is what the
    /// missing export cost.
    #[test]
    fn a_child_that_inherits_the_group_env_is_admitted_into_that_same_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "the frontend rewrite".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");
        // Exactly what the coordinator's child process inherits, per the test
        // above: the binding in the environment, and no `--group` typed.
        env.insert(WORK_GROUP_ENV.to_string(), group_id.clone());

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = args_for("claude", "do a slice of it");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let group = crate::commands::ctx::group::load(&state, &group_id)
            .expect("load")
            .expect("present");
        assert_eq!(
            group.admitted_children, 1,
            "the inherited binding is what makes `admit_child` fire at all"
        );
        assert!(
            group.closed_at.is_none(),
            "a plain worker child never closes its coordinator's group"
        );
        let delegations = crate::commands::ctx::log::tail_delegations(&state, 10).expect("tail");
        let record: serde_json::Value = serde_json::from_str(&delegations[0]).expect("json");
        assert_eq!(
            record["work_group_id"].as_str(),
            Some(group_id.as_str()),
            "and the delegation is recorded inside that group, not as ungrouped: {record}"
        );
    }

    /// The other half: a plain WORKER delegation into the same group must
    /// never close it -- only the coordinator that owns a group's scope may
    /// finish it. Otherwise the very first worker to complete would close a
    /// batch its siblings are still working through.
    #[test]
    fn a_completed_plain_worker_delegation_never_closes_its_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "own the frontend rewrite".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut args = args_for("claude", "do the work");
        args.group = Some(group_id.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let group = crate::commands::ctx::group::load(&state, &group_id)
            .expect("load")
            .expect("present");
        assert!(
            group.closed_at.is_none(),
            "a plain worker completing must never close the group it ran in"
        );
        assert!(group.sub_orchestrator_session.is_none());
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
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

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

    /// Issue #155, Phase 5(c): unlike `--budget-tokens`/`--max-tool-calls`
    /// (which decline the dashboard join, see `options_a_pane_cannot_honour_
    /// decline_the_dashboard_join`), `--role`/`--group` DO travel -- they
    /// ride on the `SpawnRequest` itself, for `fulfill_spawn_request`'s own
    /// depth cap and budget resolution to read on the fulfilment side.
    #[test]
    fn role_and_group_join_the_dashboard_and_travel_in_the_request() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, mut env) = live_dashboard_dir(tmp.path());
        // This request's own lineage: the calling process's own session id,
        // which `parent_session` is derived from (`mail::session_identity`).
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "eeee5555".to_string(),
        );

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "go");
        args.role = Some("sub-orchestrator".to_string());
        args.group = Some("wg-1".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0, "must NOT decline for --role/--group");
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(req.role.as_deref(), Some("sub-orchestrator"));
        assert_eq!(req.work_group_id.as_deref(), Some("wg-1"));
        assert!(
            req.parent_session.is_some(),
            "this process's own session id must be the lineage link: {request_body}"
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
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

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

    /// Security review round 2 (Finding 4): `--scope` mints a group, and a
    /// dashboard that then refuses the spawn means the delegation never
    /// starts -- so the group must not be left open, unclaimed and childless
    /// on disk. (The refusal here is the same non-retryable shape the
    /// dashboard's own lineage and depth gates produce.)
    #[test]
    fn a_refused_scope_spawn_leaves_no_work_group_behind() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"a worker may not delegate onward"}"#,
                )
            }
        });

        let mut args = joinable_args("claude", "own this scope");
        args.role = Some("sub-orchestrator".to_string());
        args.scope = Some("the frontend rewrite".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 1, "a policy refusal ends the delegation");
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert!(
            req.work_group_id.is_some(),
            "the minted group still travels in the request: {request_body}"
        );
        assert!(
            crate::commands::ctx::group::list(&state).is_empty(),
            "a refused spawn must leave no group behind: {:?}",
            crate::commands::ctx::group::list(&state)
        );
    }

    /// A live requests directory, and the `AgentArgs`/env pair that reaches
    /// it: the shape every `try_join_dashboard`-level test below shares.
    ///
    /// Issue #144: also writes `owner.pid` naming this test process itself
    /// (guaranteed live for as long as the test runs), matching the real
    /// dashboard's own startup sequence -- `try_join_dashboard` now checks
    /// `sessions::dashboard_owner_is_live` before using this channel at all,
    /// so a "live" fixture with no pidfile would be refused before ever
    /// reaching the responder every test below sets up.
    fn live_dashboard_dir(root: &Path) -> (PathBuf, HashMap<String, String>) {
        let requests_dir = root.join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");
        std::fs::write(
            requests_dir.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
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
            // Issue #155, Phase 5(d): a pane cannot honour either budget
            // ceiling, same reasoning as `max_restarts`/`timeout_secs` above.
            |a: &mut AgentArgs| a.budget_tokens = Some(100_000),
            |a: &mut AgentArgs| a.max_tool_calls = Some(50),
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

    /// Issue #144: `sessions::nested_session_evidence` was hardened to treat
    /// a `DASH_REQUESTS_ENV` directory whose `owner.pid` names a dead process
    /// as no evidence of a live dashboard (see that module's own
    /// `only_a_live_dashboard_owner_pidfile_counts_as_a_dashboard_owner`),
    /// but `try_join_dashboard` was never updated to match: it only ever
    /// checked `dir.is_dir()`. A directory a crashed or force-quit dashboard
    /// left behind (a clean quit removes it; an abnormal exit does not) is
    /// therefore wrongly treated as a live channel by this side of the
    /// rendezvous -- a request is written into it, nobody is listening, and
    /// the caller burns the *entire* ack timeout finding that out. That is
    /// exactly the "dashboard did not answer" symptom the issue reports, and
    /// it fires even while some other, unrelated dashboard is genuinely
    /// running elsewhere.
    #[test]
    fn a_dead_dashboards_leftover_directory_is_refused_immediately() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());
        std::fs::write(
            requests_dir.parent().expect("parent").join("owner.pid"),
            crate::commands::ctx::testenv::dead_pid().to_string(),
        )
        .expect("write owner.pid");

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let started = std::time::Instant::now();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(
            joined.is_none(),
            "no live dashboard owns this directory; falls back to headless"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dead dashboard's leftover directory must be refused immediately, never waited \
             out against the full ack timeout"
        );
        assert!(
            std::fs::read_dir(&requests_dir)
                .expect("read requests dir")
                .flatten()
                .next()
                .is_none(),
            "no request may be written into a dead dashboard's directory at all"
        );
    }

    /// The other half of `dashboard_owner_liveness`'s three-way split: a
    /// requests directory with no `owner.pid` at all (rather than one naming
    /// a dead pid) must be refused exactly the same way -- immediately, with
    /// no request ever written. Distinct code path from the dead-pid test
    /// above (`OwnerLiveness::Missing` vs `OwnerLiveness::Dead`), so both are
    /// covered rather than assuming one implies the other.
    #[test]
    fn a_dashboard_directory_with_no_owner_pid_is_refused_immediately() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        // Deliberately not `live_dashboard_dir`: this is exactly the one
        // thing it always writes that this test needs absent.
        let requests_dir = tmp.path().join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");
        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            requests_dir.display().to_string(),
        );

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let started = std::time::Instant::now();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(
            joined.is_none(),
            "no owner.pid means no dashboard can be confirmed live; falls back to headless"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a missing owner.pid must be refused immediately, never waited out against the \
             full ack timeout"
        );
        assert!(
            std::fs::read_dir(&requests_dir)
                .expect("read requests dir")
                .flatten()
                .next()
                .is_none(),
            "no request may be written when no dashboard owner can be confirmed"
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

    // Issue #145: dashboard discovery fallback. `try_join_dashboard` used to
    // give up the moment its own inherited `DASH_REQUESTS_ENV` directory was
    // absent or its owner dead, even when a perfectly live dashboard was
    // sitting right next to it under `<state>/dash/*` -- the shape left
    // behind by a dashboard restart, where a pane's own child shell still
    // carries the old, now-stale env value. These tests build that layout
    // directly under `<state>/dash/`, matching `StateDir::dash()`'s own
    // production form, rather than `live_dashboard_dir`'s arbitrary single
    // directory (which only ever stood in for the one inherited channel).

    #[test]
    fn a_dead_inherited_dashboard_falls_back_to_a_live_sibling() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");

        let old_requests = state_dir
            .join("dash")
            .join("aaaa1111-oldtoken")
            .join("requests");
        std::fs::create_dir_all(&old_requests).expect("mkdir old requests");
        std::fs::write(
            old_requests.parent().expect("parent").join("owner.pid"),
            crate::commands::ctx::testenv::dead_pid().to_string(),
        )
        .expect("write dead owner.pid");

        let new_requests = state_dir
            .join("dash")
            .join("bbbb2222-newtoken")
            .join("requests");
        std::fs::create_dir_all(&new_requests).expect("mkdir new requests");
        std::fs::write(
            new_requests.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write live owner.pid");

        let mut env = base_env(&state_dir);
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            old_requests.display().to_string(),
        );

        let responder = std::thread::spawn({
            let dir = new_requests.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"cafe1234","reason":null}"#)
        });

        let args = joinable_args("claude", "delegated after a dashboard restart");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("falls back to the live sibling dashboard");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as cafe1234"),
            "got {output}"
        );
        assert!(
            request_body.contains("delegated after a dashboard restart"),
            "the request must actually land in the live sibling's own requests dir: \
             {request_body}"
        );
        assert!(
            std::fs::read_dir(&old_requests)
                .expect("read old requests dir")
                .flatten()
                .next()
                .is_none(),
            "the dead dashboard's own directory must stay untouched"
        );
    }

    #[test]
    fn two_live_siblings_pick_the_most_recently_started_dashboard() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");

        // The inherited directory is simply gone (e.g. the operator quit that
        // dashboard outright) -- both candidates below are genuine fallback
        // siblings, neither privileged by being "the inherited one".
        let inherited = state_dir
            .join("dash")
            .join("aaaa1111-goneinherited")
            .join("requests");

        let older = state_dir
            .join("dash")
            .join("bbbb2222-oldertoken")
            .join("requests");
        let newer = state_dir
            .join("dash")
            .join("cccc3333-newertoken")
            .join("requests");
        std::fs::create_dir_all(&older).expect("mkdir older");
        std::fs::create_dir_all(&newer).expect("mkdir newer");
        let older_owner = older.parent().expect("parent").join("owner.pid");
        let newer_owner = newer.parent().expect("parent").join("owner.pid");
        std::fs::write(&older_owner, std::process::id().to_string()).expect("write older owner");
        std::fs::write(&newer_owner, std::process::id().to_string()).expect("write newer owner");

        let base = std::time::SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&older_owner)
            .expect("open older owner.pid")
            .set_modified(base)
            .expect("set_modified older");
        std::fs::File::options()
            .write(true)
            .open(&newer_owner)
            .expect("open newer owner.pid")
            .set_modified(base + std::time::Duration::from_secs(5))
            .expect("set_modified newer");

        let mut env = base_env(&state_dir);
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            inherited.display().to_string(),
        );

        let responder = std::thread::spawn({
            let dir = newer.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"feed5678","reason":null}"#)
        });

        let args = joinable_args("claude", "go to the newest dashboard");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("falls back to the most recently started sibling");
        responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as feed5678"),
            "the newer-mtime sibling must be the one selected: {output}"
        );
        assert!(
            std::fs::read_dir(&older)
                .expect("read older requests dir")
                .flatten()
                .next()
                .is_none(),
            "the older sibling must never receive a request when a newer one exists"
        );
    }

    /// The other half of issue #145: when nothing under `<state>/dash/*` is
    /// live either, the fallback gives up (`None`) exactly like before this
    /// issue's fix -- and the reasons behind that are structured data
    /// (`dash::DashCandidate`/`CandidateStatus`), not only an `eprintln` a
    /// test has no seam to observe.
    #[test]
    fn no_live_dashboard_anywhere_falls_back_to_headless_with_reasons_available() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state_dir = tmp.path().join("state");

        let dead = state_dir
            .join("dash")
            .join("aaaa1111-deadtoken")
            .join("requests");
        let ownerless = state_dir
            .join("dash")
            .join("bbbb2222-noownertoken")
            .join("requests");
        std::fs::create_dir_all(&dead).expect("mkdir dead");
        std::fs::create_dir_all(&ownerless).expect("mkdir ownerless");
        let dead_pid_value = crate::commands::ctx::testenv::dead_pid();
        std::fs::write(
            dead.parent().expect("parent").join("owner.pid"),
            dead_pid_value.to_string(),
        )
        .expect("write dead owner.pid");
        // `ownerless` deliberately gets no `owner.pid` at all.

        let inherited = state_dir
            .join("dash")
            .join("cccc3333-goneinherited")
            .join("requests");
        let env = base_env(&state_dir);

        let target = live_join_target(&inherited, &|k| env.get(k).cloned());
        assert!(
            target.is_none(),
            "no live dashboard exists anywhere, so the fallback must give up"
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir);
        let candidates = crate::commands::ctx::dash::discover_live_dash_dirs(&state);
        assert_eq!(
            candidates.len(),
            2,
            "both candidates are still reported, not silently dropped: {candidates:?}"
        );
        let dead_status = candidates
            .iter()
            .find(|c| c.requests_dir == dead)
            .expect("dead candidate present")
            .status;
        assert_eq!(
            dead_status,
            crate::commands::ctx::dash::CandidateStatus::DeadOwner(dead_pid_value)
        );
        let ownerless_status = candidates
            .iter()
            .find(|c| c.requests_dir == ownerless)
            .expect("ownerless candidate present")
            .status;
        assert_eq!(
            ownerless_status,
            crate::commands::ctx::dash::CandidateStatus::NoOwnerPid
        );
    }
}
