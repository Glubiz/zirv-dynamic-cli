//! Composed, explainable per-session attention model (issue #349): the
//! single question "does an operator need to look at this session, and why"
//! resolved the same way regardless of which adapter, supervisor or hook
//! reported it. Pure: [`compose`]/[`project`]/[`mark_seen`] take a value and
//! return a value; every filesystem access lives in [`record`]/[`load`]/
//! [`mark_seen_io`] at the bottom of this file (the same "pure core, thin
//! I/O in the same file" shape `sessions.rs`/`hook.rs` already use -- only
//! `rot.rs` itself is held to a stricter, separate-file purity).
//!
//! # Model
//!
//! An [`Observation`] is one fact from one [`Authority`]: "as of now, this
//! session's lifecycle is X and/or it needs Y attention, because Z, with W
//! confidence." [`compose`] folds a batch of observations (in practice
//! almost always one, from whichever hook/supervisor/workflow choke point
//! just fired) onto the previous [`SessionStatus`] and returns the new one.
//! [`project`] turns a `SessionStatus` into the small, stable [`Projection`]
//! enum a UI renders.
//!
//! # Suppression is per-axis, not global
//!
//! Lifecycle and attention are resolved on two SEPARATE axes, each ordered by
//! [`AUTHORITY_ORDER`] (`AdapterHook > Supervisor > Workflow > Transcript >
//! ScreenManifest > QuietHeuristic`): among this tick's observations that
//! assert a lifecycle, the highest-ranked authority wins and every other
//! lifecycle-asserting observation is recorded in `skipped`; attention
//! resolves the same way, independently. A single "highest authority wins
//! everything" vote was considered and rejected: a `Supervisor` stall latch
//! and an `AdapterHook` permission prompt can both be live in the same tick
//! from different authorities, and neither axis's winner may silently erase
//! the other axis's real signal just because it out-ranks it on the OTHER
//! axis. `SessionStatus::authority`/`evidence`/`confidence` name the
//! LIFECYCLE winner specifically (mirroring `Observation`'s own field
//! names); when the attention winner is a different authority, its evidence
//! rides as a second clause in `evidence` so `explain-status` can still say
//! where it came from.
//!
//! # The unseen latch
//!
//! A `Working -> Settled` lifecycle transition sets `visibility` to
//! `Unseen` -- background completion that would otherwise read as "idle" the
//! next time anyone looks. Only [`mark_seen`] ever clears it; `compose` and
//! every read (`explain-status`, `status --json`, `wait`) leave it exactly
//! as they found it.

use std::io::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::config::EnvLookup;

// ---------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------

/// A session's coarse-grained phase, independent of which adapter reported
/// it. `Unknown` is the fallback both for "we have never observed this
/// session" and for a variant a future build added that this one has never
/// heard of -- the two are indistinguishable to a reader and both mean "no
/// opinion available", never a false `Working`/`Settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Starting,
    Working,
    Waiting,
    Settled,
    Exited,
    #[serde(other)]
    #[default]
    Unknown,
}

/// What, specifically, needs an operator's attention -- or nothing.
/// `Unknown` is the forward-compat fallback for a variant this build has
/// never heard of; deliberately distinct from `None` ("we positively know
/// nothing needs attention right now"), and treated as attention-worthy by
/// [`project`] -- over-flagging an unrecognized state is the safe direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    #[default]
    None,
    Approval,
    Question,
    Permission,
    Quota,
    WorkflowGate,
    WriterConflict,
    VerificationFailure,
    Stalled,
    #[serde(other)]
    Unknown,
}

/// Whether the operator has already seen the CURRENT projection. Only
/// [`mark_seen`] ever produces `Seen`; [`compose`] only ever produces
/// `Unseen` (on a `Working -> Settled` transition) and otherwise carries the
/// previous value forward untouched. An unrecognized on-disk value degrades
/// to `Unseen`, not `Seen` -- the safe direction for "did the operator
/// actually look at this" is to assume they did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Seen,
    #[serde(other)]
    Unseen,
}

/// Who reported an [`Observation`]. Order matters: see [`AUTHORITY_ORDER`].
/// An unrecognized on-disk value degrades to `QuietHeuristic`, the weakest
/// authority, so a future variant this build cannot parse can never
/// out-rank one it does understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// A native adapter lifecycle hook (Claude's Stop/Notify/SessionStart/
    /// PreToolUse/Permission hooks). The most direct, lowest-latency signal
    /// available, so it out-ranks everything else.
    AdapterHook,
    /// zirv's own supervisor loop (`exec.rs`/`wrap.rs`): stall latch,
    /// process exit, turn-signal socket.
    Supervisor,
    /// `workflow::engine`'s gate/approval/verification state.
    Workflow,
    /// A transcript re-read/re-score (the rot engine's own scoring pass),
    /// used when nothing more direct is available.
    Transcript,
    /// Stage 2 (issue #349, design point 5): a bottom-buffer pattern match
    /// against a screen manifest. Advisory only, hence the low rank.
    ScreenManifest,
    /// Time-since-last-output with no other signal at all (`dash::pane`'s
    /// `PaneState`). The weakest authority, and the forward-compat fallback
    /// for any authority value a future build introduces that this one
    /// cannot parse.
    #[serde(other)]
    #[default]
    QuietHeuristic,
}

/// Highest-ranked first. [`authority_rank`] is the only thing that reads
/// this order; every suppression decision in [`compose`] goes through it, so
/// the order is declared exactly once.
const AUTHORITY_ORDER: [Authority; 6] = [
    Authority::AdapterHook,
    Authority::Supervisor,
    Authority::Workflow,
    Authority::Transcript,
    Authority::ScreenManifest,
    Authority::QuietHeuristic,
];

fn authority_rank(authority: Authority) -> usize {
    AUTHORITY_ORDER
        .iter()
        .position(|candidate| *candidate == authority)
        .unwrap_or(AUTHORITY_ORDER.len())
}

// ---------------------------------------------------------------------
// Observation / SessionStatus
// ---------------------------------------------------------------------

/// One fact reported by one authority. `lifecycle`/`attention` are
/// independently optional: an observation may speak to one axis, the other,
/// both, or (a still-valid no-op) neither. `evidence`/`confidence` describe
/// whichever axis (or axes) it does speak to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub authority: Authority,
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    #[serde(default)]
    pub attention: Option<Attention>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub confidence: u8,
    #[serde(default)]
    pub observed_at: u64,
}

impl Observation {
    /// Convenience constructor for the common case: one axis touched, the
    /// other left `None`. Callers that need both set `lifecycle`/`attention`
    /// directly on the returned value.
    pub fn new(
        authority: Authority,
        evidence: impl Into<String>,
        confidence: u8,
        now: u64,
    ) -> Self {
        Observation {
            authority,
            lifecycle: None,
            attention: None,
            evidence: evidence.into(),
            confidence,
            observed_at: now,
        }
    }

    pub fn with_lifecycle(mut self, lifecycle: Lifecycle) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }

    pub fn with_attention(mut self, attention: Attention) -> Self {
        self.attention = Some(attention);
        self
    }
}

/// One lifecycle- or attention-asserting observation that lost this tick's
/// vote on its axis, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skipped {
    pub authority: Authority,
    pub reason: String,
}

/// The composed, persisted view of one session's attention state. One JSON
/// file per session (`state.rs`'s `StateDir::attention()`); every field is
/// `#[serde(default)]` so a row written by an older build, or one missing a
/// field entirely, still parses -- degrading to `Lifecycle::Unknown`/
/// `Attention::None`/`Visibility::Seen`, never a parse failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionStatus {
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub attention: Attention,
    #[serde(default)]
    pub visibility: Visibility,
    /// The authority that currently owns `lifecycle` (not necessarily the
    /// authority behind `attention` -- see this module's own doc comment on
    /// per-axis suppression).
    #[serde(default)]
    pub authority: Authority,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub confidence: u8,
    #[serde(default)]
    pub last_transition: u64,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

/// The stable, small enum a UI or `--json` consumer actually renders.
/// `Blocked` carries the specific [`Attention`] so a caller never has to
/// re-derive "why" from `SessionStatus` separately -- [`reason`] does that
/// once, here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    Working,
    Blocked(Attention),
    DoneUnread,
    IdleSeen,
    Failed,
    Unknown,
}

impl Projection {
    /// A stable, short machine-parseable label -- what `status --json` and
    /// the dashboard sidebar badge key off, never `{self:?}` (which would
    /// change shape the moment `Blocked` gained a payload).
    pub fn label(&self) -> &'static str {
        match self {
            Projection::Working => "working",
            Projection::Blocked(_) => "blocked",
            Projection::DoneUnread => "done-unread",
            Projection::IdleSeen => "idle",
            Projection::Failed => "failed",
            Projection::Unknown => "unknown",
        }
    }
}

// ---------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------

/// Splits `observations` into the highest-ranked (by [`authority_rank`]) one
/// for which `has_field` holds, and every other matching one -- one axis
/// (lifecycle or attention) at a time. Ties (same authority) break on
/// `observed_at` (latest wins), then on position in `observations` (earliest
/// wins), so the result never depends on the caller's own ordering.
fn pick_winner(
    observations: &[Observation],
    has_field: impl Fn(&Observation) -> bool,
) -> Option<(&Observation, Vec<&Observation>)> {
    let mut candidates: Vec<&Observation> = observations.iter().filter(|o| has_field(o)).collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| {
        authority_rank(a.authority)
            .cmp(&authority_rank(b.authority))
            .then_with(|| b.observed_at.cmp(&a.observed_at))
    });
    let winner = candidates.remove(0);
    Some((winner, candidates))
}

/// Folds `observations` (usually one) onto `prev` and returns the new
/// [`SessionStatus`]. See this module's own doc comment for the per-axis
/// suppression rule and the unseen latch. An empty `observations` slice is a
/// pure no-op: it returns `prev` (or a fresh default) completely unchanged,
/// including `revision`/`last_transition` -- "nothing happened this tick" is
/// not itself a transition.
pub fn compose(
    prev: Option<&SessionStatus>,
    observations: &[Observation],
    now: u64,
) -> SessionStatus {
    let base = prev.cloned().unwrap_or_default();
    if observations.is_empty() {
        return base;
    }

    let mut skipped = Vec::new();

    // -- lifecycle axis ----------------------------------------------------
    let (lifecycle, authority, mut evidence, confidence) =
        match pick_winner(observations, |o| o.lifecycle.is_some()) {
            Some((winner, losers)) => {
                for loser in losers {
                    skipped.push(Skipped {
                        authority: loser.authority,
                        reason: format!(
                            "lifecycle: outranked by {:?} ({})",
                            winner.authority, winner.evidence
                        ),
                    });
                }
                (
                    winner.lifecycle.expect("has_field guaranteed Some"),
                    winner.authority,
                    winner.evidence.clone(),
                    winner.confidence,
                )
            }
            None => (
                base.lifecycle,
                base.authority,
                base.evidence.clone(),
                base.confidence,
            ),
        };

    // -- attention axis, resolved independently -----------------------------
    let attention = match pick_winner(observations, |o| o.attention.is_some()) {
        Some((winner, losers)) => {
            for loser in losers {
                skipped.push(Skipped {
                    authority: loser.authority,
                    reason: format!(
                        "attention: outranked by {:?} ({})",
                        winner.authority, winner.evidence
                    ),
                });
            }
            // The attention winner's own evidence rides as a second clause
            // only when it came from a DIFFERENT authority than the
            // lifecycle winner above -- otherwise it is the same fact and
            // `evidence` already carries it.
            if winner.authority != authority {
                if evidence.is_empty() {
                    evidence = format!("{:?}: {}", winner.authority, winner.evidence);
                } else {
                    evidence = format!("{evidence}; {:?}: {}", winner.authority, winner.evidence);
                }
            }
            winner.attention.expect("has_field guaranteed Some")
        }
        None => base.attention,
    };

    // -- unseen latch: only a genuine Working -> Settled transition -------
    let visibility = if base.lifecycle == Lifecycle::Working && lifecycle == Lifecycle::Settled {
        Visibility::Unseen
    } else {
        base.visibility
    };

    let mut candidate = SessionStatus {
        lifecycle,
        attention,
        visibility,
        authority,
        evidence,
        confidence,
        last_transition: base.last_transition,
        revision: base.revision,
        skipped,
    };

    if project(&candidate) != project(&base) {
        candidate.revision = base.revision + 1;
        candidate.last_transition = now;
    }
    candidate
}

/// A `SessionStatus` into the small enum a UI actually renders. Attention
/// always wins over lifecycle -- a session that is technically `Working` but
/// has a pending permission prompt is blocked, not busy.
pub fn project(status: &SessionStatus) -> Projection {
    if status.attention != Attention::None {
        return Projection::Blocked(status.attention);
    }
    match status.lifecycle {
        Lifecycle::Exited => Projection::Failed,
        Lifecycle::Settled => {
            if status.visibility == Visibility::Unseen {
                Projection::DoneUnread
            } else {
                Projection::IdleSeen
            }
        }
        Lifecycle::Starting | Lifecycle::Working => Projection::Working,
        // A `Waiting` lifecycle with no attention observation to say WHAT it
        // is waiting on (the ordinary case pairs `Waiting` with a concrete
        // `Attention::Question` on the same observation, caught by the
        // branch above) still reads as blocked-on-something-unnamed rather
        // than a misleading `Working`.
        Lifecycle::Waiting => Projection::Blocked(Attention::None),
        Lifecycle::Unknown => Projection::Unknown,
    }
}

/// The reason text `explain-status`/`status --json`/the dashboard badge show
/// alongside [`Projection::label`]. Prefers `status.evidence` (the actual
/// recorded "why"); falls back to a generic sentence per projection when no
/// evidence was ever recorded (e.g. a fresh, never-observed session).
pub fn reason(status: &SessionStatus) -> String {
    let projection = project(status);
    if !status.evidence.is_empty() {
        return match projection {
            Projection::Blocked(attention) if !matches!(attention, Attention::None) => {
                format!("{attention:?}: {}", status.evidence)
            }
            _ => status.evidence.clone(),
        };
    }
    match projection {
        Projection::Working => "working".to_string(),
        Projection::Blocked(Attention::None) => "waiting, reason unknown".to_string(),
        Projection::Blocked(attention) => format!("{attention:?}"),
        Projection::DoneUnread => "finished, not yet acknowledged".to_string(),
        Projection::IdleSeen => "idle".to_string(),
        Projection::Failed => "exited".to_string(),
        Projection::Unknown => "no attention data recorded yet".to_string(),
    }
}

/// The only thing that ever clears [`Visibility::Unseen`]. Never called by
/// `compose`, `explain-status`, `status --json` or `wait` -- only by an
/// operator action that actually looked (a dashboard pane gaining focus, a
/// future `explain-status --ack`). `last_transition` is deliberately left
/// untouched: acknowledging a projection is not a new fact about the
/// session, and a read-shaped action must never look like a write to the
/// clock `wait` and chain-recording reason from.
pub fn mark_seen(status: SessionStatus) -> SessionStatus {
    if status.visibility == Visibility::Seen {
        return status;
    }
    let before = project(&status);
    let mut next = status;
    next.visibility = Visibility::Seen;
    if project(&next) != before {
        next.revision += 1;
    }
    next
}

// ---------------------------------------------------------------------
// Verbs: `zirv ctx explain-status` / `zirv ctx wait` (design point 4)
// ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct ExplainStatusArgs {
    /// Short id (or a unique prefix of one) of the session to explain.
    pub session: String,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run_explain_status_with<W: Write>(
    args: &ExplainStatusArgs,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let state = super::state::StateDir::resolve(env)?;
    let record = super::sessions::resolve_prefix(&state, &args.session).map_err(|e| {
        format!(
            "zirv ctx explain-status: {}",
            super::sessions::resolve_error_with_diagnostics(&e, &state, env)
        )
    })?;
    let status = load(&state, &record.short);
    let projection = project(&status);

    if args.json {
        let skipped: Vec<_> = status
            .skipped
            .iter()
            .map(|s| serde_json::json!({"authority": s.authority, "reason": s.reason}))
            .collect();
        let value = serde_json::json!({
            "session": record.short,
            "projection": projection.label(),
            "reason": reason(&status),
            "lifecycle": status.lifecycle,
            "attention": status.attention,
            "visibility": status.visibility,
            "authority": status.authority,
            "confidence": status.confidence,
            "last_transition": status.last_transition,
            "revision": status.revision,
            "skipped": skipped,
        });
        writeln!(w, "{}", serde_json::to_string_pretty(&value)?)?;
        return Ok(0);
    }

    writeln!(
        w,
        "zirv ctx explain-status: {} -- {}",
        record.short,
        projection.label()
    )?;
    writeln!(w, "  reason:          {}", reason(&status))?;
    writeln!(w, "  authority:       {:?}", status.authority)?;
    writeln!(w, "  confidence:      {}", status.confidence)?;
    writeln!(
        w,
        "  last transition: {}",
        if status.last_transition == 0 {
            "--".to_string()
        } else {
            status.last_transition.to_string()
        }
    )?;
    writeln!(w, "  revision:        {}", status.revision)?;
    if status.skipped.is_empty() {
        writeln!(w, "  skipped:         --")?;
    } else {
        writeln!(w, "  skipped:")?;
        for s in &status.skipped {
            writeln!(w, "    - {:?}: {}", s.authority, s.reason)?;
        }
    }
    Ok(0)
}

pub fn run_explain_status<W: Write>(args: &ExplainStatusArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    run_explain_status_with(args, w, &env)
}

/// The four projection buckets `wait` can be asked to block on. `Done`
/// deliberately matches EITHER `DoneUnread` or `IdleSeen` -- both are
/// `Lifecycle::Settled`, and a caller that only cares whether the session
/// has finished (not whether an operator has acknowledged it yet) should
/// not have to know about the unseen latch at all; `Idle` is the narrower
/// "finished AND already acknowledged" case for a caller that does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WaitTarget {
    Blocked,
    Done,
    Idle,
    Failed,
}

impl WaitTarget {
    fn matches(self, projection: Projection) -> bool {
        match self {
            WaitTarget::Blocked => matches!(projection, Projection::Blocked(_)),
            WaitTarget::Done => {
                matches!(projection, Projection::DoneUnread | Projection::IdleSeen)
            }
            WaitTarget::Idle => matches!(projection, Projection::IdleSeen),
            WaitTarget::Failed => matches!(projection, Projection::Failed),
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct WaitArgs {
    /// Short id (or a unique prefix of one) of the session to wait on.
    pub session: String,
    #[arg(long, value_enum)]
    pub until: WaitTarget,
    /// Give up and exit 2 after this many seconds.
    #[arg(long, default_value_t = 600)]
    pub timeout_secs: u64,
}

/// The starting poll interval, doubling ([`next_poll_interval`]) up to a
/// [`WAIT_POLL_MAX`] cap -- cheap enough to feel responsive on a session that
/// resolves in the first second or two, bounded enough not to hammer the
/// state dir on one that runs for the whole timeout.
const WAIT_POLL_START: Duration = Duration::from_millis(500);
const WAIT_POLL_MAX: Duration = Duration::from_secs(5);

/// Pure: the next poll interval after `current`.
fn next_poll_interval(current: Duration) -> Duration {
    (current * 2).min(WAIT_POLL_MAX)
}

/// Pure: whether `current` is still the SAME process `wait` originally
/// pinned -- both the pid and the process's own start time (when known) must
/// match, so an OS pid reuse can never read as "the same session". Mirrors
/// `sessions::record_is_alive`'s own pid+start_time disambiguation.
fn same_generation(
    pinned_pid: u32,
    pinned_started_at: Option<u64>,
    current: &super::sessions::Record,
) -> bool {
    current.pid == pinned_pid && current.start_time == pinned_started_at
}

/// Resolves `args.session` once, pins `(pid, started_at)`, then polls the
/// persisted attention file at a bounded, doubling interval until the
/// projection matches `args.until`. Exit 0 on match, 2 on timeout, 3 if the
/// pinned process is replaced (by a new one reusing its identity) or its
/// registry entry disappears entirely -- never satisfied by a replacement.
pub fn run_wait_with<W: Write>(
    args: &WaitArgs,
    w: &mut W,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
) -> CtxResult<i32> {
    let state = super::state::StateDir::resolve(env)?;
    let resolved = super::sessions::resolve_prefix(&state, &args.session).map_err(|e| {
        format!(
            "zirv ctx wait: {}",
            super::sessions::resolve_error_with_diagnostics(&e, &state, env)
        )
    })?;
    let pinned_pid = resolved.pid;
    let pinned_started_at = resolved.start_time;
    let short = resolved.short.clone();

    let start = now_fn();
    let mut interval = WAIT_POLL_START;
    loop {
        let Some(current) = super::sessions::load_record(&state, &short) else {
            writeln!(w, "zirv ctx wait: {short}: registry entry disappeared")?;
            return Ok(3);
        };
        if !same_generation(pinned_pid, pinned_started_at, &current) {
            writeln!(
                w,
                "zirv ctx wait: {short}: the pinned process was replaced by a new one reusing \
                 its identity; not waiting on it"
            )?;
            return Ok(3);
        }
        let status = load(&state, &short);
        let projection = project(&status);
        if args.until.matches(projection) {
            writeln!(
                w,
                "zirv ctx wait: {short}: reached {} ({})",
                projection.label(),
                reason(&status)
            )?;
            return Ok(0);
        }
        if now_fn().saturating_sub(start) >= args.timeout_secs {
            writeln!(
                w,
                "zirv ctx wait: {short}: timed out after {}s waiting for --until {:?} \
                 (currently {})",
                args.timeout_secs,
                args.until,
                projection.label()
            )?;
            return Ok(2);
        }
        sleep_fn(interval);
        interval = next_poll_interval(interval);
    }
}

pub fn run_wait<W: Write>(args: &WaitArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    run_wait_with(args, w, &env, &super::state::now_secs, &std::thread::sleep)
}

// ---------------------------------------------------------------------
// I/O: one JSON file per session under `StateDir::attention()`, keyed by the
// same stable short id the session registry (`sessions::Record::short`)
// uses -- so `zirv ctx explain-status <session>`/`wait` can resolve a
// session the same way every other `ctx` verb already does
// (`sessions::resolve_prefix`). Tolerant of a missing or corrupt file in
// every direction: a session this build has never recorded anything about
// reads back as `SessionStatus::default()` (`Lifecycle::Unknown`,
// `Attention::None`), never an error.
// ---------------------------------------------------------------------

fn status_path(state: &super::state::StateDir, short: &str) -> std::path::PathBuf {
    state.attention().join(format!("{short}.json"))
}

/// Reads the persisted status for `short`, or a fresh default when the file
/// is missing or fails to parse.
pub fn load(state: &super::state::StateDir, short: &str) -> SessionStatus {
    std::fs::read_to_string(status_path(state, short))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Folds one new `observation` onto `short`'s persisted status and writes
/// the result back, returning it. Best-effort on the write, matching every
/// other piece of state-dir housekeeping in this codebase: a write failure
/// is silent (every hook call site here must exit 0 regardless) and the
/// composed value is still returned so the caller can act on it in-process
/// even if persistence failed.
pub fn record(
    state: &super::state::StateDir,
    short: &str,
    observation: Observation,
    now: u64,
) -> SessionStatus {
    let prev = load(state, short);
    let next = compose(Some(&prev), std::slice::from_ref(&observation), now);
    persist(state, short, &next);
    next
}

/// The `mark_seen` counterpart to [`record`]: loads, clears `Unseen`, writes
/// back. See [`mark_seen`]'s own doc comment for who may call this.
pub fn mark_seen_io(state: &super::state::StateDir, short: &str) -> SessionStatus {
    let prev = load(state, short);
    let before = prev.clone();
    let next = mark_seen(prev);
    // Skip the write entirely when nothing changed -- a caller wired to a
    // frequent, low-cost UI event (a dashboard's own focus-change navigation,
    // issue #349) must not turn every arrow keypress into a state-dir write
    // just because the pane it landed on was already `Seen`.
    if next != before {
        persist(state, short, &next);
    }
    next
}

fn persist(state: &super::state::StateDir, short: &str, status: &SessionStatus) {
    if let Ok(json) = serde_json::to_string_pretty(status) {
        let _ = super::state::create_private_dir_all(&state.attention());
        let _ = super::state::write_private(&status_path(state, short), &json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(authority: Authority, now: u64) -> Observation {
        Observation::new(authority, format!("{authority:?}"), 80, now)
    }

    // -- per-axis suppression: every authority pair, both slice orders -----

    #[test]
    fn lifecycle_axis_suppression_holds_for_every_authority_pair_in_both_orders() {
        for (i, &high) in AUTHORITY_ORDER.iter().enumerate() {
            for &low in AUTHORITY_ORDER.iter().skip(i + 1) {
                for reversed in [false, true] {
                    let high_obs = obs(high, 10).with_lifecycle(Lifecycle::Working);
                    let low_obs = obs(low, 10).with_lifecycle(Lifecycle::Settled);
                    let observations = if reversed {
                        vec![low_obs, high_obs]
                    } else {
                        vec![high_obs, low_obs]
                    };
                    let status = compose(None, &observations, 100);
                    assert_eq!(
                        status.lifecycle,
                        Lifecycle::Working,
                        "{high:?} must outrank {low:?} regardless of slice order (reversed={reversed})"
                    );
                    assert_eq!(status.authority, high);
                    assert!(
                        status
                            .skipped
                            .iter()
                            .any(|s| s.authority == low && s.reason.contains("lifecycle")),
                        "{low:?} must be recorded as a skipped lifecycle fallback"
                    );
                }
            }
        }
    }

    #[test]
    fn attention_axis_suppression_holds_for_every_authority_pair() {
        for (i, &high) in AUTHORITY_ORDER.iter().enumerate() {
            for &low in AUTHORITY_ORDER.iter().skip(i + 1) {
                let high_obs = obs(high, 10).with_attention(Attention::Approval);
                let low_obs = obs(low, 10).with_attention(Attention::Stalled);
                let status = compose(None, &[low_obs, high_obs], 100);
                assert_eq!(
                    status.attention,
                    Attention::Approval,
                    "{high:?} must outrank {low:?} on the attention axis"
                );
                assert!(
                    status
                        .skipped
                        .iter()
                        .any(|s| s.authority == low && s.reason.contains("attention")),
                );
            }
        }
    }

    #[test]
    fn lifecycle_and_attention_axes_are_independent() {
        // Supervisor's Stalled attention must survive even though an
        // AdapterHook observation with NO attention opinion outranks it on
        // the lifecycle axis -- a single "one authority wins everything"
        // vote would have erased it.
        let adapter_hook = obs(Authority::AdapterHook, 10).with_lifecycle(Lifecycle::Working);
        let supervisor_stall = obs(Authority::Supervisor, 10).with_attention(Attention::Stalled);
        let status = compose(None, &[adapter_hook, supervisor_stall], 100);
        assert_eq!(status.lifecycle, Lifecycle::Working);
        assert_eq!(status.attention, Attention::Stalled);
        assert_eq!(project(&status), Projection::Blocked(Attention::Stalled));
    }

    #[test]
    fn working_to_settled_transition_latches_unseen() {
        let prev = SessionStatus {
            lifecycle: Lifecycle::Working,
            visibility: Visibility::Seen,
            ..Default::default()
        };
        let settle = obs(Authority::AdapterHook, 10)
            .with_lifecycle(Lifecycle::Settled)
            .with_attention(Attention::None);
        let status = compose(Some(&prev), &[settle], 100);
        assert_eq!(status.visibility, Visibility::Unseen);
        assert_eq!(project(&status), Projection::DoneUnread);
    }

    #[test]
    fn a_non_working_to_settled_transition_does_not_latch_unseen() {
        // Already Settled -> Settled again must not re-latch.
        let prev = SessionStatus {
            lifecycle: Lifecycle::Settled,
            visibility: Visibility::Seen,
            ..Default::default()
        };
        let settle_again = obs(Authority::AdapterHook, 10).with_lifecycle(Lifecycle::Settled);
        let status = compose(Some(&prev), &[settle_again], 100);
        assert_eq!(status.visibility, Visibility::Seen);
        assert_eq!(project(&status), Projection::IdleSeen);
    }

    #[test]
    fn mark_seen_clears_unseen_and_bumps_revision_reads_never_do() {
        let prev = SessionStatus {
            lifecycle: Lifecycle::Working,
            visibility: Visibility::Seen,
            ..Default::default()
        };
        let settle = obs(Authority::AdapterHook, 10).with_lifecycle(Lifecycle::Settled);
        let unread = compose(Some(&prev), &[settle], 100);
        assert_eq!(project(&unread), Projection::DoneUnread);
        let revision_after_settle = unread.revision;

        // A read-shaped operation (compose with nothing new to say) must
        // never clear it.
        let still_unread = compose(Some(&unread), &[], 200);
        assert_eq!(still_unread.visibility, Visibility::Unseen);
        assert_eq!(still_unread.revision, revision_after_settle);
        assert_eq!(still_unread.last_transition, unread.last_transition);

        let acked = mark_seen(still_unread);
        assert_eq!(acked.visibility, Visibility::Seen);
        assert_eq!(project(&acked), Projection::IdleSeen);
        assert_eq!(acked.revision, revision_after_settle + 1);

        // Idempotent: marking an already-seen status seen again changes
        // nothing, including revision.
        let acked_again = mark_seen(acked.clone());
        assert_eq!(acked_again, acked);
    }

    #[test]
    fn revision_is_stable_across_empty_or_no_op_composes() {
        let obs1 = obs(Authority::Supervisor, 10).with_attention(Attention::Stalled);
        let stalled = compose(None, &[obs1], 100);
        assert_eq!(stalled.revision, 1);

        // Re-asserting the exact same fact from the same authority: the
        // projection does not change, so revision must not move either.
        let obs2 = obs(Authority::Supervisor, 20).with_attention(Attention::Stalled);
        let still_stalled = compose(Some(&stalled), &[obs2], 200);
        assert_eq!(still_stalled.revision, 1);

        // An empty observation batch is a pure no-op.
        let untouched = compose(Some(&still_stalled), &[], 300);
        assert_eq!(untouched, still_stalled);
    }

    #[test]
    fn empty_observations_return_prev_unchanged_including_a_missing_prev() {
        let fresh = compose(None, &[], 100);
        assert_eq!(fresh, SessionStatus::default());
        assert_eq!(project(&fresh), Projection::Unknown);
    }

    #[test]
    fn project_prioritises_attention_over_a_terminal_lifecycle() {
        let status = SessionStatus {
            lifecycle: Lifecycle::Exited,
            attention: Attention::VerificationFailure,
            ..Default::default()
        };
        assert_eq!(
            project(&status),
            Projection::Blocked(Attention::VerificationFailure)
        );
    }

    #[test]
    fn exited_with_no_attention_projects_failed() {
        let status = SessionStatus {
            lifecycle: Lifecycle::Exited,
            ..Default::default()
        };
        assert_eq!(project(&status), Projection::Failed);
    }

    // -- serde backward/forward compatibility -------------------------------

    #[test]
    fn unrecognized_lifecycle_degrades_to_unknown_not_a_parse_error() {
        let status: SessionStatus =
            serde_json::from_str(r#"{"lifecycle":"some_future_phase"}"#).unwrap();
        assert_eq!(status.lifecycle, Lifecycle::Unknown);
    }

    #[test]
    fn unrecognized_attention_degrades_to_unknown_and_still_blocks() {
        let status: SessionStatus =
            serde_json::from_str(r#"{"attention":"some_future_reason"}"#).unwrap();
        assert_eq!(status.attention, Attention::Unknown);
        assert_eq!(project(&status), Projection::Blocked(Attention::Unknown));
    }

    #[test]
    fn unrecognized_authority_degrades_to_the_weakest_one() {
        let obs: Observation =
            serde_json::from_str(r#"{"authority":"some_future_authority"}"#).unwrap();
        assert_eq!(obs.authority, Authority::QuietHeuristic);
    }

    #[test]
    fn a_row_with_no_fields_at_all_still_parses() {
        let status: SessionStatus = serde_json::from_str("{}").unwrap();
        assert_eq!(status, SessionStatus::default());
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let status = SessionStatus {
            lifecycle: Lifecycle::Waiting,
            attention: Attention::Question,
            visibility: Visibility::Unseen,
            authority: Authority::AdapterHook,
            evidence: "waiting for input".to_string(),
            confidence: 90,
            last_transition: 42,
            revision: 3,
            skipped: vec![Skipped {
                authority: Authority::Supervisor,
                reason: "lifecycle: outranked by AdapterHook".to_string(),
            }],
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: SessionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn explain_status_reason_names_the_attention_when_present() {
        let status = SessionStatus {
            attention: Attention::Approval,
            evidence: "permission requested for Bash".to_string(),
            ..Default::default()
        };
        assert_eq!(reason(&status), "Approval: permission requested for Bash");
    }

    // -- I/O round trip ------------------------------------------------------

    #[test]
    fn record_persists_and_load_reads_back_the_same_status() {
        let dir = std::env::temp_dir().join(format!(
            "zirv-attention-test-{}-{}",
            std::process::id(),
            now_for_test()
        ));
        let state = super::super::state::StateDir::from_root(dir.clone());
        let short = "abcd1234";

        let missing = load(&state, short);
        assert_eq!(missing, SessionStatus::default());

        let observation = Observation::new(Authority::AdapterHook, "session started", 100, 1)
            .with_lifecycle(Lifecycle::Working);
        let recorded = record(&state, short, observation, 1);
        assert_eq!(recorded.lifecycle, Lifecycle::Working);

        let reloaded = load(&state, short);
        assert_eq!(reloaded, recorded);

        let acked = mark_seen_io(&state, short);
        assert_eq!(acked.visibility, Visibility::Seen);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_tolerates_a_corrupt_file() {
        let dir = std::env::temp_dir().join(format!(
            "zirv-attention-test-corrupt-{}-{}",
            std::process::id(),
            now_for_test()
        ));
        let state = super::super::state::StateDir::from_root(dir.clone());
        let short = "deadbeef";
        let _ = super::super::state::create_private_dir_all(&state.attention());
        let _ = std::fs::write(status_path(&state, short), "not json");

        let status = load(&state, short);
        assert_eq!(status, SessionStatus::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn now_for_test() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    // -- verbs ---------------------------------------------------------------

    fn test_state() -> (std::path::PathBuf, super::super::state::StateDir) {
        let dir = std::env::temp_dir().join(format!(
            "zirv-attention-verb-test-{}-{}",
            std::process::id(),
            now_for_test()
        ));
        let state = super::super::state::StateDir::from_root(dir.clone());
        (dir, state)
    }

    /// Writes a registry record directly (bypassing `SessionGuard`, which
    /// would delete it on drop) so a test can control `pid`/`start_time`
    /// precisely and have the file outlive the call that wrote it.
    fn write_test_record(
        state: &super::super::state::StateDir,
        short: &str,
        pid: u32,
        start_time: Option<u64>,
    ) {
        let mut record = super::super::sessions::Record::new(
            &format!("{short}-session"),
            "claude",
            std::path::Path::new("/repo"),
            super::super::sessions::Verb::Exec,
        )
        .with_stable_short(short);
        record.pid = pid;
        record.start_time = start_time;
        let _ = super::super::state::create_private_dir_all(&state.sessions());
        let json = serde_json::to_string_pretty(&record).unwrap();
        std::fs::write(state.sessions().join(format!("{short}.json")), json).unwrap();
    }

    fn env_with_state(dir: &std::path::Path) -> impl Fn(&str) -> Option<String> + '_ {
        move |key: &str| {
            if key == super::super::state::STATE_ENV {
                Some(dir.display().to_string())
            } else {
                None
            }
        }
    }

    #[test]
    fn explain_status_reports_projection_reason_and_skipped_fallbacks() {
        let (dir, state) = test_state();
        let short = "aaaa1111";
        // A live pid: this test process's own, so `resolve_prefix` treats it
        // as live regardless of platform.
        write_test_record(&state, short, std::process::id(), None);
        // Both observations in the same tick, via the real I/O path, so the
        // suppressed one lands in the persisted `skipped` list.
        let prev = load(&state, short);
        let combined = compose(
            Some(&prev),
            &[
                Observation::new(Authority::AdapterHook, "permission requested", 90, 1)
                    .with_attention(Attention::Approval),
                Observation::new(Authority::Supervisor, "no progress", 60, 1)
                    .with_attention(Attention::Stalled),
            ],
            1,
        );
        persist(&state, short, &combined);

        let args = ExplainStatusArgs {
            session: short.to_string(),
            json: false,
        };
        let env = env_with_state(&dir);
        let mut out = Vec::new();
        let code = run_explain_status_with(&args, &mut out, &env).unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("blocked"), "{text}");
        assert!(text.contains("Approval"), "{text}");
        assert!(
            text.contains("Supervisor"),
            "{text}: must name the skipped fallback"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_status_json_round_trips_the_projection() {
        let (dir, state) = test_state();
        let short = "bbbb2222";
        write_test_record(&state, short, std::process::id(), None);
        // The unseen latch only fires on a genuine `Working -> Settled`
        // transition, so this has to actually pass through `Working` first.
        record(
            &state,
            short,
            Observation::new(Authority::AdapterHook, "turn started", 100, 1)
                .with_lifecycle(Lifecycle::Working),
            1,
        );
        record(
            &state,
            short,
            Observation::new(Authority::AdapterHook, "settling", 100, 2)
                .with_lifecycle(Lifecycle::Settled),
            2,
        );

        let args = ExplainStatusArgs {
            session: short.to_string(),
            json: true,
        };
        let env = env_with_state(&dir);
        let mut out = Vec::new();
        let code = run_explain_status_with(&args, &mut out, &env).unwrap();
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["projection"], "done-unread");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_status_refuses_an_unknown_session() {
        let (dir, _state) = test_state();
        let args = ExplainStatusArgs {
            session: "zzzzzzzz".to_string(),
            json: false,
        };
        let env = env_with_state(&dir);
        let mut out = Vec::new();
        assert!(run_explain_status_with(&args, &mut out, &env).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_resolves_immediately_when_already_matching() {
        let (dir, state) = test_state();
        let short = "cccc3333";
        write_test_record(&state, short, std::process::id(), None);
        record(
            &state,
            short,
            Observation::new(Authority::AdapterHook, "approval needed", 90, 1)
                .with_attention(Attention::Approval),
            1,
        );

        let args = WaitArgs {
            session: short.to_string(),
            until: WaitTarget::Blocked,
            timeout_secs: 5,
        };
        let env = env_with_state(&dir);
        let mut out = Vec::new();
        let slept = std::cell::Cell::new(false);
        let sleep_fn = |_: Duration| slept.set(true);
        let code = run_wait_with(&args, &mut out, &env, &|| 1000, &sleep_fn).unwrap();
        assert_eq!(code, 0);
        assert!(
            !slept.get(),
            "already matching must resolve without ever polling again"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_times_out_when_the_projection_never_matches() {
        let (dir, state) = test_state();
        let short = "dddd4444";
        write_test_record(&state, short, std::process::id(), None);
        // Working, never blocked/done/idle/failed.
        record(
            &state,
            short,
            Observation::new(Authority::AdapterHook, "turn started", 90, 1)
                .with_lifecycle(Lifecycle::Working),
            1,
        );

        let args = WaitArgs {
            session: short.to_string(),
            until: WaitTarget::Failed,
            timeout_secs: 1,
        };
        let env = env_with_state(&dir);
        let mut out = Vec::new();
        // A fake clock that jumps straight past the 1s timeout on its second
        // read, and a no-op sleep so the test never actually blocks.
        let clock = std::cell::Cell::new(0u64);
        let now_fn = || {
            let v = clock.get();
            clock.set(v + 2);
            v
        };
        let code = run_wait_with(&args, &mut out, &env, &now_fn, &|_| {}).unwrap();
        assert_eq!(code, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_exits_3_when_the_pinned_process_is_replaced() {
        let (dir, state) = test_state();
        let short = "eeee5555";
        write_test_record(&state, short, std::process::id(), Some(111));
        record(
            &state,
            short,
            Observation::new(Authority::AdapterHook, "turn started", 90, 1)
                .with_lifecycle(Lifecycle::Working),
            1,
        );

        let args = WaitArgs {
            session: short.to_string(),
            until: WaitTarget::Done,
            timeout_secs: 5,
        };
        let env = env_with_state(&dir);
        // A poll callback that swaps the registry record for a "replacement"
        // (same short, different start_time) the moment `wait` sleeps once --
        // simulating a restart under the same short id.
        let swapped = std::cell::Cell::new(false);
        let sleep_fn = |_: Duration| {
            if !swapped.get() {
                write_test_record(&state, short, std::process::id(), Some(222));
                swapped.set(true);
            }
        };
        let mut out = Vec::new();
        let code = run_wait_with(
            &args,
            &mut out,
            &env,
            &super::super::state::now_secs,
            &sleep_fn,
        )
        .unwrap();
        assert_eq!(code, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_exits_3_when_the_registry_entry_disappears() {
        let (dir, state) = test_state();
        let short = "ffff6666";
        write_test_record(&state, short, std::process::id(), None);
        record(
            &state,
            short,
            Observation::new(Authority::AdapterHook, "turn started", 90, 1)
                .with_lifecycle(Lifecycle::Working),
            1,
        );

        let args = WaitArgs {
            session: short.to_string(),
            until: WaitTarget::Done,
            timeout_secs: 5,
        };
        let env = env_with_state(&dir);
        let removed = std::cell::Cell::new(false);
        let sleep_fn = |_: Duration| {
            if !removed.get() {
                let _ = std::fs::remove_file(state.sessions().join(format!("{short}.json")));
                removed.set(true);
            }
        };
        let mut out = Vec::new();
        let code = run_wait_with(
            &args,
            &mut out,
            &env,
            &super::super::state::now_secs,
            &sleep_fn,
        )
        .unwrap();
        assert_eq!(code, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_poll_interval_doubles_and_caps() {
        let mut interval = WAIT_POLL_START;
        assert_eq!(interval, Duration::from_millis(500));
        interval = next_poll_interval(interval);
        assert_eq!(interval, Duration::from_secs(1));
        interval = next_poll_interval(interval);
        assert_eq!(interval, Duration::from_secs(2));
        interval = next_poll_interval(interval);
        assert_eq!(interval, Duration::from_secs(4));
        interval = next_poll_interval(interval);
        assert_eq!(
            interval, WAIT_POLL_MAX,
            "must cap rather than keep doubling"
        );
    }
}
