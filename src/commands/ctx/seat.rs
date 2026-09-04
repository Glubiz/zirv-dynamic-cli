//! Issue #358 (task 4): the orchestrator's own seat -- a persisted record of
//! WHICH agent/model/provider is currently sitting in an interactive
//! orchestrator session, distinct from `sessions::Record` (which only ever
//! names the *session*, not which harness generation currently answers to
//! it). A seat survives an automatic cross-harness rollover: `sessions::
//! SessionGuard::refresh_session` already establishes that a session's short
//! id is a stable *address* that outlives the session id rotating underneath
//! it (see that function's own "stranded mail" doc comment) -- a seat is the
//! same idea one level up, for the *agent* answering at that address.
//!
//! Persisted at `<state>/sessions/<short>.seat.json`, a sibling of the
//! session registry record rather than a field on it: `sessions::Record` is
//! rewritten on every ordinary supervision tick (`refresh_session`,
//! `adopt_child_pid`, `stamp_in_flight`/`clear_in_flight`), and folding seat
//! state into it would mean every one of those unrelated writes could race a
//! rollover transaction. A dedicated file with its own lock
//! (`<short>.seat.lock`, the same OS-advisory-lock idiom `group.rs::
//! GroupLock` already uses over `<id>.lock`) makes that impossible by
//! construction.
//!
//! This module only ever writes the seat record and decides, purely, when a
//! rollover should happen -- it never spawns a process, swaps an adapter, or
//! touches a pty. That belongs to task 5 (later), which calls this module's
//! transaction API from the two live-swap seams `handover.rs` already
//! established: `wrap::perform_handover_swap` and `dash::pane::Pane::
//! handover`. This task does not edit `wrap.rs` or `dash/pane.rs` at all --
//! see `register`'s own doc comment for exactly where task 5 must wire the
//! two calls this module cannot make for itself without touching those
//! files.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup};
use super::state::StateDir;

/// Env var a superseded session's own `ZIRV_CTX_SEAT_GENERATION` is compared
/// against -- see [`fence`].
pub const GENERATION_ENV: &str = "ZIRV_CTX_SEAT_GENERATION";

/// Env var an operator (or `zirv ctx chat --pin-harness`) sets to keep a seat
/// off the automatic rollover path entirely -- see [`pin_from_env`].
pub const PIN_ENV: &str = "ZIRV_CTX_SEAT_PIN";

/// One entry in [`Seat::visited`]: `agent` was tried (successfully or not)
/// against the evidence stamped at `epoch` (a `Cause`'s own `observed_at`),
/// recorded at wall-clock `at`. `decide`'s candidate filter excludes a
/// candidate already visited at the SAME epoch (the decision would be
/// re-litigating unchanged evidence) but allows one visited only at a
/// strictly OLDER epoch (fresh evidence deserves a fresh attempt) -- see
/// [`decide`]'s own doc comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Visit {
    pub agent: String,
    pub epoch: u64,
    pub at: u64,
}

/// Why a rollover (prepared, committed, or about to be attempted) happened.
/// `Proactive`/`Reactive` both carry `observed_at`: the epoch of the reading
/// that triggered this decision, and the same value [`Visit::epoch`] and
/// `decide`'s own candidate filter key off of. `Manual` (no fields) is for a
/// human-triggered `zirv ctx handover`-style seat change, which carries no
/// headroom evidence at all and therefore no epoch to record a visit against
/// -- see [`Cause::observed_at`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cause {
    Proactive { headroom_pct: f64, observed_at: u64 },
    Reactive { detail: String, observed_at: u64 },
    Manual,
}

impl Cause {
    /// The epoch this cause's own evidence was read at, or `None` for
    /// `Manual` (no evidence to date). Callers that need a `Visit::epoch`
    /// regardless (`commit`/`abort`) fall back to the current wall clock for
    /// `Manual`, since there is no reading to key the visit against.
    pub fn observed_at(&self) -> Option<u64> {
        match self {
            Cause::Proactive { observed_at, .. } | Cause::Reactive { observed_at, .. } => {
                Some(*observed_at)
            }
            Cause::Manual => None,
        }
    }
}

/// A seat's own state machine. `Idle` is the steady state a seat spends
/// almost all of its life in; `Prepared` is the narrow, transactional window
/// between deciding to roll over and either `commit`ting or `abort`ing that
/// decision (see [`prepare`]); `Parked` is an operator- or policy-driven
/// pause (a rate-limit window, a maintenance hold) that is not itself a
/// rollover at all.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Prepared {
        successor_agent: String,
        successor_model: Option<String>,
        generation: u64,
        since: u64,
        cause: Cause,
    },
    Parked {
        until: u64,
        window: String,
        reason: String,
        since: u64,
    },
}

/// A pending rollover cause not yet acted on -- bookkeeping distinct from
/// [`Phase::Prepared`] (which is a live transaction with a provisional
/// generation already reserved). Set by [`mark_pending`]/[`clear_pending`]
/// for a caller (task 5) that wants to remember "I saw a trigger" across a
/// poll boundary before it has actually decided to spend a generation on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub cause: Cause,
    pub since: u64,
}

/// The persisted seat record -- `<state>/sessions/<short>.seat.json`. Every
/// optional field is `#[serde(default)]` so a record from an earlier schema
/// (or a hand-edited one) still round-trips rather than failing to parse; the
/// registry's own tolerant-read convention (`sessions::load_record`, `group::
/// load`) applies here too -- a malformed or missing seat file is `None`,
/// never an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seat {
    pub short: String,
    pub session: String,
    pub generation: u64,
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    pub provider: String,
    pub role: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub visited: Vec<Visit>,
    #[serde(default)]
    pub last_rollover_at: Option<u64>,
    #[serde(default)]
    pub pending: Option<Pending>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn record_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.seat.json"))
}

fn lock_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.seat.lock"))
}

/// One advisory OS lock per seat record, mirroring `group.rs::GroupLock`
/// exactly (including that reuse: `super::group::open_lock_file` is
/// `pub(crate)` precisely so a second lock-file idiom in this crate does not
/// have to re-derive the unix-mode-0600-vs-portable `OpenOptions` split).
struct SeatLock(std::fs::File);

impl Drop for SeatLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock_seat(state: &StateDir, short: &str) -> CtxResult<SeatLock> {
    super::state::create_private_dir_all(&state.sessions())?;
    let file = super::group::open_lock_file(&lock_path(state, short))?;
    file.lock()?;
    Ok(SeatLock(file))
}

/// One seat record read straight off disk, tolerant like every other
/// registry read in this codebase: a missing or malformed file is `None`,
/// never an error.
pub fn load(state: &StateDir, short: &str) -> Option<Seat> {
    let contents = std::fs::read_to_string(record_path(state, short)).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Writes `seat`'s record, best-effort directory creation matching every
/// other piece of state-dir housekeeping in this codebase.
pub fn store(state: &StateDir, seat: &Seat) -> CtxResult<()> {
    super::state::create_private_dir_all(&state.sessions())?;
    let json = serde_json::to_string_pretty(seat)?;
    super::state::write_private(&record_path(state, &seat.short), &json)?;
    Ok(())
}

/// Removes `short`'s own seat record, if any. Best-effort: a seat file that
/// outlives its session is inert (nothing reads a seat without a live
/// session's own env naming it), so a failed removal costs disk, never
/// correctness.
pub fn remove(state: &StateDir, short: &str) {
    let _ = std::fs::remove_file(record_path(state, short));
}

/// Registers (or re-registers) `short`'s own seat.
///
/// Creates generation 1 when no seat record exists yet. When one already
/// does, this is idempotent identity bookkeeping only: `session`, `agent`,
/// `model` and `provider` are refreshed to whatever this registration names
/// (a session id rotated under the same short address, or the same session
/// re-registering), but `generation`, `visited`, `phase`, `pinned` and `role`
/// are left exactly as they were -- a seat's rollover history and its pin
/// are decided once, at genuine creation, never silently reset by a later
/// re-registration of the same address.
///
/// **Wiring note for task 5**: the two places an *orchestrator* session is
/// actually registered (`sessions::SessionGuard::register`) are `wrap.rs`'s
/// own `run_with` (around its own `let mut session_guard = ...` call) and
/// `dash/pane.rs`'s own `Pane::spawn` (around its own `let guard =
/// SessionGuard::register(state, record);` call, gated on `role ==
/// PromptRole::Orchestrator`) -- both files this task is deliberately not
/// touching (see this module's own doc comment). Task 5 must call `seat::
/// register` right after each of those two `SessionGuard::register` calls,
/// with `pinned` read via [`pin_from_env`] against whichever `EnvLookup` is
/// already in scope there. See [`generation_env`] for the matching
/// child-env-export wiring note.
#[allow(clippy::too_many_arguments)]
pub fn register(
    state: &StateDir,
    short: &str,
    session: &str,
    agent: &str,
    model: Option<&str>,
    provider: &str,
    role: &str,
    pinned: bool,
    now: u64,
) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let seat = match load(state, short) {
        Some(mut existing) => {
            existing.session = session.to_string();
            existing.agent = agent.to_string();
            existing.model = model.map(str::to_string);
            existing.provider = provider.to_string();
            existing.updated_at = now;
            existing
        }
        None => Seat {
            short: short.to_string(),
            session: session.to_string(),
            generation: 1,
            agent: agent.to_string(),
            model: model.map(str::to_string),
            provider: provider.to_string(),
            role: role.to_string(),
            pinned,
            phase: Phase::Idle,
            visited: Vec::new(),
            last_rollover_at: None,
            pending: None,
            created_at: now,
            updated_at: now,
        },
    };
    store(state, &seat)?;
    Ok(seat)
}

/// The `(key, value)` pair a seat's own generation must ride into every
/// child it spawns, alongside `SEAT_ROLE_ENV` -- see [`GENERATION_ENV`] and
/// [`fence`].
///
/// **Wiring note for task 5**: the orchestrator's own turn-signal env is
/// built at `wrap.rs`'s own `turn_env.extend(adapters::seat_role_env(role));`
/// call (inside `run_with`) and, for a dashboard pane, at `dash/mod.rs`'s own
/// `turn_env.extend(super::adapters::seat_role_env(first.role));` call --
/// note the dashboard's env-building lives in `dash/mod.rs`, not the
/// forbidden `dash/pane.rs`, though the seat this env describes is not
/// registered until `Pane::spawn` runs afterward, so task 5 will likely need
/// to thread the freshly registered `Seat` (or just its `generation`)
/// forward from wherever it ends up calling `register`. Both sites should
/// `turn_env.push(seat::generation_env(&seat))` right after their own
/// `seat_role_env` call.
pub fn generation_env(seat: &Seat) -> (String, String) {
    (GENERATION_ENV.to_string(), seat.generation.to_string())
}

/// Whether `ZIRV_CTX_SEAT_PIN` (or, for `zirv ctx chat`, `--pin-harness`
/// folded into the same `EnvLookup` the way `chat::quiet_env` already folds
/// `--quiet` into `ZIRV_CTX_QUIET`) asks this seat to be pinned at
/// registration -- any of `1`/`true`/`yes`, case-insensitively, matching
/// every other boolean this codebase reads out of the environment.
pub fn pin_from_env(env: EnvLookup<'_>) -> bool {
    env(PIN_ENV)
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

fn no_seat(short: &str) -> Box<dyn std::error::Error> {
    format!("zirv ctx seat: no seat record for {short}").into()
}

/// Begins a rollover transaction: reserves the provisional next generation
/// and moves the seat into [`Phase::Prepared`], without touching `agent`/
/// `model`/`provider`/`session` yet -- those only change on [`commit`].
/// Refuses a seat that is pinned, or one that already has a rollover
/// prepared (one in flight at a time; the caller must `commit` or `abort`
/// the existing one first).
pub fn prepare(
    state: &StateDir,
    short: &str,
    successor_agent: &str,
    successor_model: Option<&str>,
    cause: Cause,
    now: u64,
) -> CtxResult<u64> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    if seat.pinned {
        return Err(
            format!("zirv ctx seat: {short} is pinned; refusing to prepare a rollover").into(),
        );
    }
    if matches!(seat.phase, Phase::Prepared { .. }) {
        return Err(format!("zirv ctx seat: {short} already has a rollover prepared").into());
    }
    let generation = seat.generation + 1;
    seat.phase = Phase::Prepared {
        successor_agent: successor_agent.to_string(),
        successor_model: successor_model.map(str::to_string),
        generation,
        since: now,
        cause,
    };
    seat.updated_at = now;
    store(state, &seat)?;
    Ok(generation)
}

/// Commits a prepared rollover: `generation` must match the one
/// [`prepare`] reserved. Records a [`Visit`] for the OLD agent (the one
/// being rolled away from) at the prepared cause's own epoch, then adopts the
/// successor's `agent`/`model`/`provider`/`session`, stamps
/// `last_rollover_at`, and returns the seat to [`Phase::Idle`] with `pending`
/// cleared.
///
/// `provider` is re-derived from `successor_agent` via
/// `state::provider_slug` rather than carried on `Phase::Prepared` itself:
/// the prepared phase only ever names the successor by adapter/model (the
/// two things a rollover decision actually chooses between), and this
/// module's own `provider` field follows the same "provider tracks agent
/// name" convention `sessions::Record` and `CandidateHeadroom` both already
/// use.
pub fn commit(
    state: &StateDir,
    short: &str,
    generation: u64,
    session_of_successor: &str,
    now: u64,
) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    let Phase::Prepared {
        successor_agent,
        successor_model,
        generation: prepared_generation,
        cause,
        ..
    } = seat.phase.clone()
    else {
        return Err(format!("zirv ctx seat: {short} has no prepared rollover to commit").into());
    };
    if prepared_generation != generation {
        return Err(format!(
            "zirv ctx seat: {short} prepared generation {prepared_generation} does not match \
             commit request {generation}"
        )
        .into());
    }
    let old_agent = seat.agent.clone();
    seat.visited.push(Visit {
        agent: old_agent,
        epoch: cause.observed_at().unwrap_or(now),
        at: now,
    });
    seat.generation = generation;
    seat.provider = super::state::provider_slug(&successor_agent);
    seat.agent = successor_agent;
    seat.model = successor_model;
    seat.session = session_of_successor.to_string();
    seat.last_rollover_at = Some(now);
    seat.phase = Phase::Idle;
    seat.pending = None;
    seat.updated_at = now;
    store(state, &seat)?;
    Ok(seat)
}

/// Aborts a prepared rollover: `generation` must match the one [`prepare`]
/// reserved. The provisional generation is released (`seat.generation` is
/// left unchanged -- the successor never actually took the seat), and a
/// [`Visit`] is recorded for the FAILED successor at the prepared cause's own
/// epoch, so [`decide`]'s candidate filter will not retry that same agent
/// against the same, unchanged evidence.
pub fn abort(state: &StateDir, short: &str, generation: u64, now: u64) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    let Phase::Prepared {
        successor_agent,
        generation: prepared_generation,
        cause,
        ..
    } = seat.phase.clone()
    else {
        return Err(format!("zirv ctx seat: {short} has no prepared rollover to abort").into());
    };
    if prepared_generation != generation {
        return Err(format!(
            "zirv ctx seat: {short} prepared generation {prepared_generation} does not match \
             abort request {generation}"
        )
        .into());
    }
    seat.visited.push(Visit {
        agent: successor_agent,
        epoch: cause.observed_at().unwrap_or(now),
        at: now,
    });
    seat.phase = Phase::Idle;
    seat.pending = None;
    seat.updated_at = now;
    store(state, &seat)?;
    Ok(seat)
}

/// Parks a seat: moves it to [`Phase::Parked`] until `until`, for whatever
/// `window`/`reason` the caller names (a rate-limit window, an operator
/// maintenance hold). Unconditional -- unlike `prepare`, parking is not
/// itself a rollover and is not refused for a pinned seat: pinning only
/// opts a seat out of *automatic rollover*, not out of every state change.
pub fn park(
    state: &StateDir,
    short: &str,
    until: u64,
    window: &str,
    reason: &str,
    now: u64,
) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    seat.phase = Phase::Parked {
        until,
        window: window.to_string(),
        reason: reason.to_string(),
        since: now,
    };
    seat.updated_at = now;
    store(state, &seat)?;
    Ok(seat)
}

/// Returns a parked seat to [`Phase::Idle`]. A no-op success (not an error)
/// when the seat is not currently parked, matching this module's general
/// "a caller re-asserting an already-true state is not a failure" shape.
pub fn resume(state: &StateDir, short: &str, now: u64) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    if matches!(seat.phase, Phase::Parked { .. }) {
        seat.phase = Phase::Idle;
        seat.updated_at = now;
        store(state, &seat)?;
    }
    Ok(seat)
}

/// Records a rollover cause as seen but not yet acted on -- see [`Pending`].
pub fn mark_pending(state: &StateDir, short: &str, cause: Cause, now: u64) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    seat.pending = Some(Pending { cause, since: now });
    seat.updated_at = now;
    store(state, &seat)?;
    Ok(seat)
}

/// Clears whatever [`mark_pending`] last recorded.
pub fn clear_pending(state: &StateDir, short: &str, now: u64) -> CtxResult<Seat> {
    let _lock = lock_seat(state, short)?;
    let mut seat = load(state, short).ok_or_else(|| no_seat(short))?;
    seat.pending = None;
    seat.updated_at = now;
    store(state, &seat)?;
    Ok(seat)
}

/// Crash recovery for a seat left mid-transaction: a supervisor that
/// `prepare`d a rollover and then died (or was killed) before it could
/// `commit`/`abort` leaves the seat stuck in `Phase::Prepared` forever,
/// refusing every future `prepare` (see that function's own "already has a
/// rollover prepared" guard) even though nothing is actually in flight any
/// more.
///
/// `successor_alive` is the caller's own liveness question -- "is there a
/// live session for the successor this prepared generation named?" -- kept
/// as a callback rather than this module reaching into `sessions::list`
/// itself, so this stays deterministic and clock-free beyond the `now` it is
/// handed: identical `(seat, successor_alive answer, now)` always yields the
/// identical verdict. `Some(session)` commits the seat onto that successor's
/// session id (the successor genuinely started and is running); `None`
/// aborts it (the successor never came up, or died before ever registering).
/// A seat that is not `Prepared` at all needs no recovery and yields
/// `Ok(None)`.
pub fn recover(
    state: &StateDir,
    short: &str,
    successor_alive: &dyn Fn(u64) -> Option<String>,
    now: u64,
) -> CtxResult<Option<Seat>> {
    let Some(seat) = load(state, short) else {
        return Ok(None);
    };
    let Phase::Prepared { generation, .. } = seat.phase else {
        return Ok(None);
    };
    match successor_alive(generation) {
        Some(session) => Ok(Some(commit(state, short, generation, &session, now)?)),
        None => Ok(Some(abort(state, short, generation, now)?)),
    }
}

// -- Fencing --------------------------------------------------------------

/// Refuses to let a superseded session keep coordinating after an automatic
/// rollover replaced it. Reads `ZIRV_CTX_SESSION`/`ZIRV_CTX_SEAT_GENERATION`
/// straight from the process environment (unlike every other verb in this
/// module, this fence is called from the TOP of a mutating verb before any
/// `EnvLookup` closure is necessarily in scope, and it only ever needs to
/// answer a yes/no about THIS process's own real environment, never a
/// caller-substituted one). Missing env, or no seat record for the named
/// short id, both mean "nothing to fence against" -- `Ok(())`, never a
/// refusal: a plain headless verb run outside any seat (a bare terminal, a
/// CI job) must be unaffected.
pub fn fence(state: &StateDir) -> CtxResult<()> {
    let session = std::env::var(super::adapters::SESSION_ENV).ok();
    let generation = std::env::var(GENERATION_ENV).ok();
    fence_with(
        session
            .as_deref()
            .and_then(|s| load_short(state, s))
            .as_ref(),
        generation.as_deref(),
    )
}

fn load_short(state: &StateDir, session: &str) -> Option<Seat> {
    load(state, &super::sessions::short_id(session))
}

/// The pure half of [`fence`]: given the seat record (if any) for the
/// session named in the environment and the raw `ZIRV_CTX_SEAT_GENERATION`
/// string (if any), decides whether to refuse. Split out so this module's
/// own tests never have to mutate real process environment variables (a
/// documented hazard under a threaded, non-nextest `cargo test` run -- see
/// this repo's own working instructions on why nextest is preferred).
fn fence_with(seat: Option<&Seat>, env_generation: Option<&str>) -> CtxResult<()> {
    let (Some(seat), Some(raw)) = (seat, env_generation) else {
        return Ok(());
    };
    let Ok(env_generation) = raw.parse::<u64>() else {
        return Ok(());
    };
    if seat.generation > env_generation {
        return Err(format!(
            "stale seat generation {env_generation} (current {}): this session was superseded by \
             an automatic rollover; stop coordinating",
            seat.generation
        )
        .into());
    }
    Ok(())
}

// -- Pure rollover decision -------------------------------------------------

/// One fallback candidate's projected headroom, as measured or estimated by
/// the caller (task 5's own capacity snapshot -- this module takes plain
/// numbers rather than depending on that task's `allocator::CapacitySnapshot`
/// types, per this task's own scope note). `assumed` marks a synthetic
/// reading (no real usage signal yet, akin to `fallback.unknown_headroom_pct`
/// elsewhere in this codebase) rather than one backed by an actual poll.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateHeadroom {
    pub agent: String,
    pub model: Option<String>,
    pub projected_headroom_pct: f64,
    pub assumed: bool,
    pub observed_at: u64,
}

/// Everything [`decide`] needs to reach a verdict, gathered once by the
/// caller so the decision itself stays a pure function of its inputs.
pub struct RolloverInputs<'a> {
    pub seat: &'a Seat,
    pub now: u64,
    /// The orchestrator's own harness's current headroom reading, or `None`
    /// when it is unknown/stale -- an unknown reading never triggers a
    /// proactive rollover (see [`decide`]'s own doc comment).
    pub source_headroom_pct: Option<f64>,
    /// The epoch `source_headroom_pct` (or the hard-block signal) was
    /// observed at -- this is the epoch a resulting [`Cause`] carries, and
    /// therefore the epoch [`Visit`]s and the candidate filter key off of.
    pub source_observed_at: u64,
    /// Whether the source session is actively, definitely blocked right now
    /// (a hard rate-limit/quota refusal), as opposed to merely low on
    /// projected headroom. Takes the reactive path, which ignores the idle
    /// boundary and the cooldown.
    pub source_hard_blocked: bool,
    pub auto_enabled: bool,
    /// Whether the orchestrator session is at a clean turn boundary right
    /// now. The proactive path refuses to move a seat mid-turn; the reactive
    /// path does not wait for one (a hard block does not care what the
    /// session was doing when it happened).
    pub idle: bool,
    pub candidates: &'a [CandidateHeadroom],
}

/// [`decide`]'s verdict. `Proceed` names the chosen successor and the cause
/// to hand to [`prepare`]; `Wait` means try again later for a reason that may
/// resolve on its own (an idle boundary, a cooldown, no trigger yet);
/// `Refuse` means this call will not proceed for a reason that will not
/// resolve just by calling again with the same inputs (disabled, pinned, not
/// idle-phased, or no candidate cleared the bar).
#[derive(Debug, Clone, PartialEq)]
pub enum RolloverDecision {
    Proceed {
        agent: String,
        model: Option<String>,
        cause: Cause,
    },
    Wait(String),
    Refuse(String),
}

/// Pure decision: whether, and onto what, `inputs.seat` should automatically
/// roll over right now.
///
/// Order of checks (each short-circuits the rest):
/// 1. `auto_enabled`/`pinned`/phase-not-`Idle` all refuse outright -- moving a
///    seat that is disabled, pinned, or already mid-transaction/parked is
///    never correct regardless of headroom.
/// 2. The reactive path (`source_hard_blocked`) is taken over the proactive
///    one whenever both could apply; it ignores the idle boundary and the
///    cooldown (a hard block already happened -- waiting for a clean
///    boundary that may never come, or a cooldown meant to prevent thrashing
///    near a soft threshold, both miss the point).
/// 3. Otherwise, the proactive path triggers only from a KNOWN reading at or
///    under `cfg.fallback.rollover_headroom_pct()` -- `None` (unknown/stale)
///    never triggers it, matching this codebase's existing "never migrate on
///    missing data" convention (`fallback.unknown_headroom_pct` is the
///    opposite, deliberately conservative, choice for background delegation,
///    not this seat). Proactive additionally requires `inputs.idle` (else
///    `Wait("idle boundary")`) and respects `rollover_cooldown_secs` against
///    `seat.last_rollover_at` (else `Wait("cooldown")`).
/// 4. Candidates are filtered: the current agent is never its own successor;
///    one already [`Visit`]ed at the SAME epoch this decision's cause would
///    carry is excluded (unchanged evidence, already tried), but a visit at
///    a strictly OLDER epoch does not exclude (fresh evidence deserves a
///    fresh attempt) -- this is what keeps a bounded number of attempts per
///    epoch (at most `candidates.len()`) and what makes an A -> B -> A flap
///    on UNCHANGED evidence impossible: once B is current and a caller
///    re-decides with B as the source using the SAME observed epoch, A is
///    excluded by that same visited-at-this-epoch rule. An `assumed`
///    candidate is only ever accepted on the reactive path -- never migrate
///    a seat onto a pure estimate while there is still time to wait for a
///    real reading.
/// 5. Surviving candidates need hysteresis clearance on BOTH the rollover
///    threshold and the (known) source reading: `projected >=
///    rollover_headroom_pct + min_candidate_headroom_pct` AND `projected >=
///    source_headroom.unwrap_or(0.0) + min_candidate_headroom_pct` -- a
///    candidate only marginally ahead of the seat it would replace is
///    refused, so headroom noise near the threshold cannot bounce the seat
///    back and forth.
/// 6. The candidate with the greatest projected headroom wins; ties break by
///    position in `cfg.fallback.order`. If nothing survives, `Refuse` joins
///    every excluded candidate's own reason.
pub fn decide(inputs: &RolloverInputs<'_>, cfg: &CtxConfig) -> RolloverDecision {
    if !inputs.auto_enabled {
        return RolloverDecision::Refuse("automatic orchestrator rollover is disabled".to_string());
    }
    if inputs.seat.pinned {
        return RolloverDecision::Refuse(
            "seat is pinned; automatic rollover is refused".to_string(),
        );
    }
    if !matches!(inputs.seat.phase, Phase::Idle) {
        return RolloverDecision::Refuse(format!(
            "seat is not idle ({:?}); automatic rollover is refused",
            inputs.seat.phase
        ));
    }

    let threshold = cfg.fallback.rollover_headroom_pct();
    let reactive = inputs.source_hard_blocked;
    let proactive_triggered = inputs.source_headroom_pct.is_some_and(|h| h <= threshold);

    if !reactive && !proactive_triggered {
        return RolloverDecision::Wait(
            "no rollover trigger: source is neither hard-blocked nor below headroom threshold"
                .to_string(),
        );
    }

    if !reactive {
        if !inputs.idle {
            return RolloverDecision::Wait("idle boundary".to_string());
        }
        if let Some(last) = inputs.seat.last_rollover_at
            && inputs.now.saturating_sub(last) < cfg.fallback.rollover_cooldown_secs
        {
            return RolloverDecision::Wait("cooldown".to_string());
        }
    }

    let epoch = inputs.source_observed_at;
    let min_headroom = cfg.fallback.min_candidate_headroom_pct;
    let source_floor = inputs.source_headroom_pct.unwrap_or(0.0);

    let mut excluded: Vec<String> = Vec::new();
    let mut eligible: Vec<&CandidateHeadroom> = Vec::new();

    for c in inputs.candidates {
        if c.agent == inputs.seat.agent {
            excluded.push(format!("{}: is the current agent", c.agent));
            continue;
        }
        if inputs
            .seat
            .visited
            .iter()
            .any(|v| v.agent == c.agent && v.epoch == epoch)
        {
            excluded.push(format!(
                "{}: already visited against this evidence (epoch {epoch})",
                c.agent
            ));
            continue;
        }
        if c.assumed && !reactive {
            excluded.push(format!(
                "{}: an assumed headroom reading is only trusted reactively",
                c.agent
            ));
            continue;
        }
        if !(c.projected_headroom_pct >= threshold + min_headroom
            && c.projected_headroom_pct >= source_floor + min_headroom)
        {
            excluded.push(format!(
                "{}: projected headroom {:.1}% does not clear the hysteresis floor",
                c.agent, c.projected_headroom_pct
            ));
            continue;
        }
        eligible.push(c);
    }

    let order = &cfg.fallback.order;
    eligible.sort_by(|a, b| {
        b.projected_headroom_pct
            .total_cmp(&a.projected_headroom_pct)
            .then_with(|| {
                let ai = order
                    .iter()
                    .position(|o| o == &a.agent)
                    .unwrap_or(usize::MAX);
                let bi = order
                    .iter()
                    .position(|o| o == &b.agent)
                    .unwrap_or(usize::MAX);
                ai.cmp(&bi)
            })
    });

    match eligible.first() {
        Some(chosen) => {
            let cause = if reactive {
                Cause::Reactive {
                    detail: "source session is hard-blocked".to_string(),
                    observed_at: epoch,
                }
            } else {
                Cause::Proactive {
                    headroom_pct: inputs.source_headroom_pct.unwrap_or(0.0),
                    observed_at: epoch,
                }
            };
            RolloverDecision::Proceed {
                agent: chosen.agent.clone(),
                model: chosen.model.clone(),
                cause,
            }
        }
        None => RolloverDecision::Refuse(if excluded.is_empty() {
            "no fallback candidates available".to_string()
        } else {
            excluded.join("; ")
        }),
    }
}

/// Test/assertion helper: whether `seat.visited` never records the same
/// `(agent, epoch)` pair twice -- the invariant [`decide`]'s visited-filter
/// is supposed to guarantee no matter how many prepare/commit/abort cycles a
/// seat goes through. `#[cfg(test)]` because it asserts a property rather
/// than deciding anything: no production path branches on it.
#[cfg(test)]
pub fn no_flap_invariant(seat: &Seat) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    seat.visited
        .iter()
        .all(|v| seen.insert((v.agent.clone(), v.epoch)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::CtxConfig;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        (dir, state)
    }

    fn cfg() -> CtxConfig {
        CtxConfig::default()
    }

    fn base_seat() -> Seat {
        Seat {
            short: "abcd1234".to_string(),
            session: "session-a".to_string(),
            generation: 1,
            agent: "claude".to_string(),
            model: None,
            provider: "claude".to_string(),
            role: "orchestrator".to_string(),
            pinned: false,
            phase: Phase::Idle,
            visited: Vec::new(),
            last_rollover_at: None,
            pending: None,
            created_at: 1_000,
            updated_at: 1_000,
        }
    }

    #[test]
    fn register_creates_generation_one_and_is_idempotent() {
        let (_dir, state) = state();
        let first = register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            Some("opus"),
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        assert_eq!(first.generation, 1);
        assert_eq!(first.agent, "claude");

        let second = register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            Some("opus"),
            "claude",
            "orchestrator",
            false,
            1_050,
        )
        .expect("re-register");
        assert_eq!(
            second.generation, 1,
            "re-registration must not bump generation"
        );
        assert!(second.visited.is_empty());
    }

    #[test]
    fn register_keeps_generation_and_visited_but_updates_identity_fields() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let prepared =
            prepare(&state, "abcd1234", "codex", None, Cause::Manual, 1_100).expect("prepare");
        commit(&state, "abcd1234", prepared, "session-b", 1_200).expect("commit");

        let reregistered = register(
            &state,
            "abcd1234",
            "session-c",
            "gemini",
            Some("pro"),
            "gemini",
            "orchestrator",
            false,
            1_300,
        )
        .expect("re-register after rollover");
        assert_eq!(
            reregistered.generation, 2,
            "generation survives re-registration"
        );
        assert_eq!(
            reregistered.visited.len(),
            1,
            "visited history survives re-registration"
        );
        assert_eq!(reregistered.agent, "gemini", "identity fields do refresh");
        assert_eq!(reregistered.session, "session-c");
    }

    #[test]
    fn prepare_commit_bumps_generation_and_records_the_visit() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let generation = prepare(
            &state,
            "abcd1234",
            "codex",
            Some("gpt5"),
            Cause::Reactive {
                detail: "hard blocked".to_string(),
                observed_at: 1_050,
            },
            1_100,
        )
        .expect("prepare");
        assert_eq!(generation, 2);

        let committed = commit(&state, "abcd1234", generation, "session-b", 1_200).expect("commit");
        assert_eq!(committed.generation, 2);
        assert_eq!(committed.agent, "codex");
        assert_eq!(committed.model.as_deref(), Some("gpt5"));
        assert_eq!(committed.session, "session-b");
        assert_eq!(committed.phase, Phase::Idle);
        assert_eq!(committed.last_rollover_at, Some(1_200));
        assert_eq!(
            committed.visited,
            vec![Visit {
                agent: "claude".to_string(),
                epoch: 1_050,
                at: 1_200,
            }]
        );
    }

    #[test]
    fn prepare_abort_releases_the_generation_and_records_the_failed_successor() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let generation = prepare(
            &state,
            "abcd1234",
            "codex",
            None,
            Cause::Proactive {
                headroom_pct: 5.0,
                observed_at: 1_050,
            },
            1_100,
        )
        .expect("prepare");

        let aborted = abort(&state, "abcd1234", generation, 1_150).expect("abort");
        assert_eq!(aborted.generation, 1, "provisional generation is released");
        assert_eq!(
            aborted.agent, "claude",
            "successor never actually took the seat"
        );
        assert_eq!(aborted.phase, Phase::Idle);
        assert_eq!(
            aborted.visited,
            vec![Visit {
                agent: "codex".to_string(),
                epoch: 1_050,
                at: 1_150,
            }]
        );
    }

    #[test]
    fn recover_commits_when_the_successor_is_alive() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let generation =
            prepare(&state, "abcd1234", "codex", None, Cause::Manual, 1_100).expect("prepare");

        let alive = move |g: u64| (g == generation).then(|| "session-b".to_string());
        let recovered = recover(&state, "abcd1234", &alive, 1_200)
            .expect("recover io")
            .expect("recovery decided something");
        assert_eq!(recovered.phase, Phase::Idle);
        assert_eq!(recovered.agent, "codex");
        assert_eq!(recovered.session, "session-b");
    }

    #[test]
    fn recover_aborts_when_the_successor_never_came_up() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        prepare(&state, "abcd1234", "codex", None, Cause::Manual, 1_100).expect("prepare");

        let dead = |_gen: u64| None;
        let recovered = recover(&state, "abcd1234", &dead, 1_200)
            .expect("recover io")
            .expect("recovery decided something");
        assert_eq!(recovered.phase, Phase::Idle);
        assert_eq!(
            recovered.agent, "claude",
            "aborted back to the original agent"
        );
    }

    #[test]
    fn recover_is_a_no_op_when_nothing_is_prepared() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let dead = |_gen: u64| None;
        let recovered = recover(&state, "abcd1234", &dead, 1_200).expect("recover io");
        assert_eq!(recovered, None);
    }

    #[test]
    fn fence_errors_on_a_stale_generation() {
        let mut seat = base_seat();
        seat.generation = 3;
        let err = fence_with(Some(&seat), Some("1")).expect_err("stale generation must refuse");
        let message = err.to_string();
        assert!(message.contains("stale seat generation 1"), "{message}");
        assert!(message.contains("current 3"), "{message}");
    }

    #[test]
    fn fence_passes_on_the_current_generation() {
        let mut seat = base_seat();
        seat.generation = 3;
        fence_with(Some(&seat), Some("3")).expect("current generation must pass");
        fence_with(Some(&seat), Some("4")).expect("a generation ahead of the record must pass");
    }

    #[test]
    fn fence_passes_with_no_env_or_no_seat() {
        let seat = base_seat();
        fence_with(None, Some("1")).expect("no seat record passes");
        fence_with(Some(&seat), None).expect("no env passes");
        fence_with(None, None).expect("neither present passes");
    }

    fn candidate(agent: &str, projected: f64) -> CandidateHeadroom {
        CandidateHeadroom {
            agent: agent.to_string(),
            model: None,
            projected_headroom_pct: projected,
            assumed: false,
            observed_at: 500,
        }
    }

    #[test]
    fn decide_refuses_when_disabled_or_pinned() {
        let cfg = cfg();
        let seat = base_seat();
        let candidates = vec![candidate("codex", 90.0)];
        let disabled_inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: false,
            idle: true,
            candidates: &candidates,
        };
        assert!(matches!(
            decide(&disabled_inputs, &cfg),
            RolloverDecision::Refuse(_)
        ));

        let mut pinned_seat = base_seat();
        pinned_seat.pinned = true;
        let pinned_inputs = RolloverInputs {
            seat: &pinned_seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: true,
            candidates: &candidates,
        };
        assert!(matches!(
            decide(&pinned_inputs, &cfg),
            RolloverDecision::Refuse(_)
        ));
    }

    #[test]
    fn decide_never_proceeds_on_an_unknown_source_reading() {
        let cfg = cfg();
        let seat = base_seat();
        let candidates = vec![candidate("codex", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: None,
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: true,
            candidates: &candidates,
        };
        assert!(matches!(decide(&inputs, &cfg), RolloverDecision::Wait(_)));
    }

    #[test]
    fn decide_proactive_waits_for_an_idle_boundary() {
        let cfg = cfg();
        let seat = base_seat();
        let candidates = vec![candidate("codex", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: false,
            candidates: &candidates,
        };
        assert_eq!(
            decide(&inputs, &cfg),
            RolloverDecision::Wait("idle boundary".to_string())
        );
    }

    #[test]
    fn decide_proactive_waits_out_the_cooldown() {
        let cfg = cfg();
        let mut seat = base_seat();
        seat.last_rollover_at = Some(1_900);
        let candidates = vec![candidate("codex", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: true,
            candidates: &candidates,
        };
        assert_eq!(
            decide(&inputs, &cfg),
            RolloverDecision::Wait("cooldown".to_string())
        );
    }

    #[test]
    fn decide_reactive_ignores_the_cooldown() {
        let cfg = cfg();
        let mut seat = base_seat();
        seat.last_rollover_at = Some(1_900);
        let candidates = vec![candidate("codex", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: true,
            auto_enabled: true,
            idle: false,
            candidates: &candidates,
        };
        assert!(matches!(
            decide(&inputs, &cfg),
            RolloverDecision::Proceed { .. }
        ));
    }

    #[test]
    fn decide_excludes_a_candidate_visited_at_the_same_epoch_but_allows_an_older_one() {
        let cfg = cfg();
        let mut seat = base_seat();
        seat.visited.push(Visit {
            agent: "codex".to_string(),
            epoch: 500,
            at: 400,
        });
        let candidates = vec![candidate("codex", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: true,
            auto_enabled: true,
            idle: false,
            candidates: &candidates,
        };
        assert!(matches!(decide(&inputs, &cfg), RolloverDecision::Refuse(_)));

        let inputs = RolloverInputs {
            source_observed_at: 900,
            ..inputs
        };
        assert!(matches!(
            decide(&inputs, &cfg),
            RolloverDecision::Proceed { .. }
        ));
    }

    #[test]
    fn decide_hysteresis_rejects_a_marginal_candidate() {
        let mut cfg = cfg();
        cfg.fallback.orchestrator_rollover_headroom_pct = Some(20.0);
        cfg.fallback.min_candidate_headroom_pct = 10.0;
        let seat = base_seat();
        let candidates = vec![candidate("codex", 25.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(15.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: true,
            candidates: &candidates,
        };
        assert!(matches!(decide(&inputs, &cfg), RolloverDecision::Refuse(_)));
    }

    #[test]
    fn decide_assumed_candidate_refused_proactively_but_accepted_reactively() {
        let cfg = cfg();
        let seat = base_seat();
        let mut assumed = candidate("codex", 90.0);
        assumed.assumed = true;
        let candidates = vec![assumed];

        let proactive = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: true,
            candidates: &candidates,
        };
        assert!(matches!(
            decide(&proactive, &cfg),
            RolloverDecision::Refuse(_)
        ));

        let reactive = RolloverInputs {
            source_hard_blocked: true,
            ..proactive
        };
        assert!(matches!(
            decide(&reactive, &cfg),
            RolloverDecision::Proceed { .. }
        ));
    }

    #[test]
    fn decide_ties_break_by_configured_order() {
        let mut cfg = cfg();
        cfg.fallback.order = vec!["gemini".to_string(), "codex".to_string()];
        let seat = base_seat();
        let candidates = vec![candidate("codex", 90.0), candidate("gemini", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: false,
            auto_enabled: true,
            idle: true,
            candidates: &candidates,
        };
        match decide(&inputs, &cfg) {
            RolloverDecision::Proceed { agent, .. } => assert_eq!(agent, "gemini"),
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[test]
    fn a_to_b_to_a_on_unchanged_evidence_is_impossible() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let cfg = cfg();

        let seat_a = load(&state, "abcd1234").expect("seat exists");
        let candidates = vec![candidate("codex", 90.0)];
        let inputs = RolloverInputs {
            seat: &seat_a,
            now: 2_000,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: true,
            auto_enabled: true,
            idle: false,
            candidates: &candidates,
        };
        let RolloverDecision::Proceed { agent, cause, .. } = decide(&inputs, &cfg) else {
            panic!("expected the first decision to proceed onto codex");
        };
        assert_eq!(agent, "codex");
        let generation = prepare(&state, "abcd1234", &agent, None, cause, 2_000).expect("prepare");
        commit(&state, "abcd1234", generation, "session-b", 2_100).expect("commit");

        // Now decide again with B (codex) as the current seat, against the
        // SAME unchanged evidence (epoch 500): A (claude) must be excluded.
        let seat_b = load(&state, "abcd1234").expect("seat exists");
        let candidates_back = vec![candidate("claude", 90.0)];
        let inputs_back = RolloverInputs {
            seat: &seat_b,
            now: 2_200,
            source_headroom_pct: Some(1.0),
            source_observed_at: 500,
            source_hard_blocked: true,
            auto_enabled: true,
            idle: false,
            candidates: &candidates_back,
        };
        assert!(matches!(
            decide(&inputs_back, &cfg),
            RolloverDecision::Refuse(_)
        ));
    }

    #[test]
    fn no_flap_invariant_holds_through_a_scripted_sequence() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");

        let mut epoch = 100u64;
        let mut t = 1_000u64;
        for successor in ["codex", "claude", "codex", "claude", "codex", "claude"] {
            epoch += 100;
            t += 100;
            let generation = prepare(
                &state,
                "abcd1234",
                successor,
                None,
                Cause::Reactive {
                    detail: "test".to_string(),
                    observed_at: epoch,
                },
                t,
            )
            .expect("prepare");
            t += 10;
            commit(&state, "abcd1234", generation, "session-x", t).expect("commit");
            let seat = load(&state, "abcd1234").expect("seat exists");
            assert!(
                no_flap_invariant(&seat),
                "no_flap_invariant must hold after every transition"
            );
        }
    }

    #[test]
    fn park_and_resume_round_trip() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let parked = park(
            &state,
            "abcd1234",
            2_000,
            "rate-limit",
            "quota exhausted",
            1_100,
        )
        .expect("park");
        assert!(matches!(parked.phase, Phase::Parked { .. }));

        let resumed = resume(&state, "abcd1234", 1_200).expect("resume");
        assert_eq!(resumed.phase, Phase::Idle);

        // Idempotent: resuming an already-idle seat is not an error.
        let resumed_again = resume(&state, "abcd1234", 1_300).expect("resume again");
        assert_eq!(resumed_again.phase, Phase::Idle);
    }

    #[test]
    fn pending_mark_and_clear_round_trip() {
        let (_dir, state) = state();
        register(
            &state,
            "abcd1234",
            "session-a",
            "claude",
            None,
            "claude",
            "orchestrator",
            false,
            1_000,
        )
        .expect("register");
        let marked = mark_pending(&state, "abcd1234", Cause::Manual, 1_100).expect("mark pending");
        assert!(marked.pending.is_some());
        let cleared = clear_pending(&state, "abcd1234", 1_200).expect("clear pending");
        assert!(cleared.pending.is_none());
    }

    #[test]
    fn pin_from_env_recognizes_common_truthy_spellings() {
        for value in ["1", "true", "TRUE", "yes", "Yes"] {
            let map = std::collections::HashMap::from([(PIN_ENV.to_string(), value.to_string())]);
            let lookup = |key: &str| map.get(key).cloned();
            assert!(pin_from_env(&lookup), "{value} should read as pinned");
        }
        let empty = std::collections::HashMap::<String, String>::new();
        let lookup = |key: &str| empty.get(key).cloned();
        assert!(!pin_from_env(&lookup));
    }

    #[test]
    fn seat_and_phase_and_cause_round_trip_every_variant() {
        let phases = vec![
            Phase::Idle,
            Phase::Prepared {
                successor_agent: "codex".to_string(),
                successor_model: Some("gpt5".to_string()),
                generation: 2,
                since: 10,
                cause: Cause::Proactive {
                    headroom_pct: 5.0,
                    observed_at: 9,
                },
            },
            Phase::Prepared {
                successor_agent: "codex".to_string(),
                successor_model: None,
                generation: 2,
                since: 10,
                cause: Cause::Reactive {
                    detail: "blocked".to_string(),
                    observed_at: 9,
                },
            },
            Phase::Prepared {
                successor_agent: "codex".to_string(),
                successor_model: None,
                generation: 2,
                since: 10,
                cause: Cause::Manual,
            },
            Phase::Parked {
                until: 100,
                window: "rate-limit".to_string(),
                reason: "quota".to_string(),
                since: 10,
            },
        ];
        for phase in phases {
            let mut seat = base_seat();
            seat.phase = phase.clone();
            let json = serde_json::to_string(&seat).expect("serialize");
            let round_tripped: Seat = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped.phase, phase);
        }
    }
}
