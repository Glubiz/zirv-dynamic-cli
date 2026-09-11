//! Issue #455: pure per-harness route health, the transport/server half of
//! routing.
//!
//! Usage headroom answers "may this account spend more". It says nothing
//! about whether the endpoint can be reached at all: a session that dies on
//! `API Error: Connection refused` has full headroom and zero capacity. This
//! module is the state machine that turns observed [`ProviderErrorClass`]
//! values into an admission verdict for one route.
//!
//! A route is a HARNESS, not a harness/model pair (review round 1, finding
//! 1). The failures that trip this breaker are transport and server
//! failures, whose failing hop is the connection or the endpoint, not the
//! model -- so routing always judged them per harness anyway. Keying the
//! record per model additionally made a terminal `Unavailable` unhealable:
//! an auth failure observed before any model line was parsed landed on the
//! bare harness key, while every later success landed on `harness/<model>`,
//! so the deny never cleared. The last model seen is still recorded, as
//! information for `zirv ctx status` only.
//!
//! Pure in exactly the sense `rot.rs` documents for itself -- no fs, clock,
//! env or net. Every function takes the current record and an explicit
//! `now`, so identical inputs always give the identical verdict and a
//! transition can be replayed or diffed. All I/O lives in
//! [`super::health_store`].

use serde::{Deserialize, Serialize};

use super::event::ProviderErrorClass;

/// How many observations one route's record keeps. A circuit breaker only
/// ever reasons about the current window, so the file is a bounded ring, not
/// a history: an endpoint flapping for a week must not grow this record.
pub const MAX_OBSERVATIONS: usize = 20;

/// How many dated turn outcomes one route keeps for the rolling error rate
/// the `Degraded` phase is judged on. Larger than [`MAX_OBSERVATIONS`]
/// because this ring is a DENOMINATOR: a route answering normally produces
/// far more successes than failures, and a denominator capped at the failure
/// ring's size would read every busy window as a 50% error rate.
pub const MAX_SAMPLES: usize = 40;

/// One turn's time-to-first-text. `at` is the transcript row's own time in
/// epoch SECONDS -- the same unit [`Observation::at`] uses, so one window
/// rule prunes every ring -- while `ttft_ms` is the measured latency in
/// milliseconds, which is the unit the policy knob is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySample {
    pub at: u64,
    pub ttft_ms: u64,
}

/// The single recovery attempt a half-open breaker has handed out: who
/// claimed it and when. It expires on its own after `cooldown_secs`, so a
/// claimant that crashes before reporting anything back cannot hold the
/// route shut -- the next caller simply claims the expired slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trial {
    pub claim: String,
    pub at: u64,
}

/// Which route an observation belongs to: one harness, lower-cased. See this
/// module's own header for why the model is deliberately NOT part of the
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RouteKey {
    pub harness: String,
}

impl RouteKey {
    pub fn new(harness: &str) -> Self {
        Self {
            harness: harness.to_lowercase(),
        }
    }

    /// The human form -- the harness name.
    pub fn label(&self) -> String {
        self.harness.clone()
    }

    /// The file-safe form used as a record's basename: everything outside
    /// `[a-z0-9._-]` folds to `-`. The record itself carries the key
    /// verbatim, so this never has to be parsed back.
    pub fn file_stem(&self) -> String {
        sanitize(&self.harness)
    }
}

fn sanitize(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// One provider error as an adapter parsed it, before any clock is
/// consulted. `at` is the transcript row's OWN timestamp and `id` its own
/// row identity -- both `None` for a transcript shape that states neither.
///
/// Review round 1, finding 2: stamping an observation with wall-clock `now`
/// meant a poll that re-read a transcript from offset 0 (a missing or
/// version-bumped checkpoint) folded months-old error rows into the current
/// window and opened the breaker on a perfectly healthy harness. The row's
/// own time is what `prune` must judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub class: ProviderErrorClass,
    pub at: Option<u64>,
    pub id: Option<String>,
}

impl Observed {
    pub fn new(class: ProviderErrorClass, at: Option<u64>, id: Option<String>) -> Self {
        Self { class, at, id }
    }
}

/// One observed provider error, kept only while it is inside the policy
/// window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub class: ProviderErrorClass,
    pub at: u64,
}

/// The route's current phase.
///
/// `Open` is the tripped breaker: nothing is routed here until `until`. Past
/// `until` the breaker is half-open -- one trial is admitted, and its
/// outcome either recovers the route or re-opens it for another cooldown.
/// `Unavailable` is what an authentication or configuration error produces:
/// no cooldown can FIX a wrong API key, so the route is denied immediately
/// rather than after `open_after_failures`. It is not terminal, though
/// (review round 1, finding 1): after `cooldown_secs` it re-probes exactly
/// like `Open` does, and a success heals it. A breaker that could only ever
/// be cleared by a success it also refused to admit was a permanent deny.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Phase {
    #[default]
    Healthy,
    Suspect {
        failures: u32,
        first_at: u64,
    },
    /// Reachable, but measurably worse than it should be: a rolling
    /// transport/server error rate at or above `degrade_error_rate_pct`, or
    /// (only when the operator opted in) a first-token latency at or above
    /// `degrade_ttft_ms`. Deliberately NOT a block -- work still routes
    /// here, it is simply ranked behind every healthy alternative, and a
    /// degraded route that is the only candidate still wins.
    Degraded {
        since: u64,
        reason: String,
    },
    Open {
        opened_at: u64,
        until: u64,
        reason: String,
        /// Which class tripped this breaker. Only `Transport`/`Server`
        /// describe a failing HOP rather than a failing account, so only
        /// those propagate to another harness sharing the same configured
        /// endpoint host (`health_store::admissions`). `#[serde(default)]`
        /// leaves a record written before this field existed reading
        /// `Other`, which propagates nothing.
        #[serde(default)]
        class: ProviderErrorClass,
    },
    HalfOpen {
        since: u64,
        /// The one in-flight recovery attempt, when some caller has claimed
        /// it. `#[serde(default)]` for the same legacy-file reason as
        /// `Open::class` above.
        #[serde(default)]
        trial: Option<Trial>,
    },
    Unavailable {
        since: u64,
        reason: String,
    },
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Suspect { .. } => "suspect",
            Self::Degraded { .. } => "degraded",
            Self::Open { .. } => "open",
            Self::HalfOpen { .. } => "half-open",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// One route's whole health record: its phase, the bounded observation ring
/// the phase was derived from, and the bounded ring of row ids already
/// folded in.
///
/// `seen_ids` is what makes [`observe`] idempotent (review round 1, finding
/// 4). Two supervisors can legitimately read the same transcript rows -- a
/// headless `exec` scorer and the Stop hook's checkpointed one, for
/// instance -- and without this the same three retries would count as six,
/// opening the breaker after two real failures instead of the configured
/// three. Same `MAX_OBSERVATIONS` bound, for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteHealth {
    pub phase: Phase,
    pub observations: Vec<Observation>,
    pub seen_ids: Vec<String>,
    /// The rolling rate's DENOMINATOR: the row time of every completed,
    /// error-free assistant turn seen for this route, bounded by
    /// [`MAX_SAMPLES`] and the policy window.
    pub successes: Vec<u64>,
    /// The rolling rate's NUMERATOR: the row time of every Transport/Server
    /// observation folded in.
    ///
    /// Deliberately separate from `observations`, which [`record_success`]
    /// CLEARS whenever a turn completes on a merely-suspect route. That is
    /// right for the breaker (its question is "is this route broken right
    /// now") and fatal for a rate: a flaky-but-working route completes a
    /// turn after nearly every failure, so a numerator read off
    /// `observations` would reset on every poll and no rate could ever
    /// accumulate. This ring ages out of the window, it is never cleared.
    pub failures: Vec<u64>,
    /// Time-to-first-text samples, only ever read when the operator set
    /// `degrade_ttft_ms`.
    pub latency: Vec<LatencySample>,
    /// R2: row ids for classes that can never open the circuit
    /// (rate limits, overflows, `Other`), kept in their own small ring.
    ///
    /// They used to share `seen_ids` with the counting classes, where a
    /// burst of rate limits -- the single most common thing a busy account
    /// produces -- evicted the ids of Transport/Server failures the
    /// `failures` ring was still holding. A replay from offset 0 then no
    /// longer recognised those rows, counted them a second time, and
    /// reopened a route on history it had already healed from.
    pub seen_other_ids: Vec<String>,
    /// R3: the `at` of the newest success this record has EVICTED by cap,
    /// `None` while the ring has never overflowed.
    ///
    /// It is the boundary below which the success ring no longer describes
    /// anything: failures older than it have no surviving successes to be
    /// weighed against, so counting them reported a rate the window never
    /// ran. A length test cannot stand in for this -- an exactly-full ring
    /// that has evicted nothing still covers its whole span, and treating it
    /// as overflowed discarded real failures and hid a real degradation.
    pub success_floor: Option<u64>,
}

/// Operator policy for the breaker (`[fallback.health]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthPolicy {
    /// Master switch. `false` makes [`admission`] unconditionally `Allow`
    /// and leaves every phase untouched.
    pub enabled: bool,
    /// How many Transport/Server errors inside `window_secs` open the
    /// breaker.
    pub open_after_failures: u32,
    /// The sliding window failures are counted over.
    pub window_secs: u64,
    /// How long an open breaker stays open before one trial is admitted.
    pub cooldown_secs: u64,
    /// The rolling transport/server error rate, in percent, at which a
    /// reachable route is marked `Degraded` and ranked behind its healthy
    /// alternatives.
    pub degrade_error_rate_pct: u8,
    /// How many dated turn outcomes (failures plus successes, or latency
    /// samples) the window must hold before either degrade signal is
    /// allowed to fire. Guards against one bad turn out of two reading as a
    /// 50% error rate.
    pub degrade_min_samples: u32,
    /// Opt-in only: the first-token latency, in milliseconds, whose median
    /// marks a route `Degraded`. `None` (the default) switches the latency
    /// signal off entirely -- see the README on why this is not on by
    /// default. Latency NEVER opens a breaker and never marks a route
    /// unavailable.
    pub degrade_ttft_ms: Option<u64>,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            open_after_failures: 3,
            window_secs: 600,
            cooldown_secs: 300,
            degrade_error_rate_pct: 25,
            degrade_min_samples: 8,
            degrade_ttft_ms: None,
        }
    }
}

/// A phase change worth one line in the decision log. Never emitted per
/// observation: only the edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    Opened(String),
    HalfOpened(String),
    Recovered(String),
    MarkedUnavailable(String),
    Degraded(String),
    Restored(String),
    TrialClaimed(String),
}

impl Transition {
    /// The `log::Decision::verdict` this transition is recorded under.
    pub fn verdict(&self) -> &'static str {
        match self {
            Self::Opened(_) => "health-open",
            Self::HalfOpened(_) => "health-half-open",
            Self::Recovered(_) => "health-recovered",
            Self::MarkedUnavailable(_) => "health-unavailable",
            Self::Degraded(_) => "health-degraded",
            Self::Restored(_) => "health-restored",
            Self::TrialClaimed(_) => "health-trial",
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Opened(reason)
            | Self::HalfOpened(reason)
            | Self::Recovered(reason)
            | Self::MarkedUnavailable(reason)
            | Self::Degraded(reason)
            | Self::Restored(reason)
            | Self::TrialClaimed(reason) => reason,
        }
    }
}

/// Whether a route may be given work.
///
/// `Trial` is a deliberate middle: the breaker's cooldown has elapsed, so
/// exactly one attempt is admitted to find out whether the endpoint is back.
/// A caller that cannot express "try cautiously" should treat it as `Allow`
/// and say so in its human line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "admission", rename_all = "kebab-case")]
pub enum Admission {
    #[default]
    Allow,
    Trial,
    /// Admissible, but ranked behind every healthy candidate. `denied()` is
    /// `None` here on purpose: a degraded route reduces, it never excludes.
    Degraded {
        reason: String,
    },
    Deny {
        reason: String,
    },
}

impl Admission {
    pub fn denied(&self) -> Option<&str> {
        match self {
            Self::Deny { reason } => Some(reason),
            _ => None,
        }
    }

    /// The degradation reason, when this route is reachable but should lose
    /// every tie to a healthy one.
    pub fn degraded(&self) -> Option<&str> {
        match self {
            Self::Degraded { reason } => Some(reason),
            _ => None,
        }
    }

    pub fn is_trial(&self) -> bool {
        matches!(self, Self::Trial)
    }

    fn deny(reason: String) -> Self {
        Self::Deny { reason }
    }
}

/// Only a transport or server failure is evidence against the route itself.
/// A rate limit is capacity (`pace`/`fallback` already own it), an overflow
/// is the session's own context (rot owns it), and `Other` is by definition
/// unattributed -- opening a breaker on any of them would route work away
/// from a perfectly reachable endpoint.
fn counts_toward_opening(class: ProviderErrorClass) -> bool {
    matches!(
        class,
        ProviderErrorClass::Transport | ProviderErrorClass::Server
    )
}

fn class_label(class: ProviderErrorClass) -> &'static str {
    match class {
        ProviderErrorClass::Transport => "transport",
        ProviderErrorClass::Server => "server",
        ProviderErrorClass::Auth => "auth",
        ProviderErrorClass::RateLimit => "rate-limit",
        ProviderErrorClass::Overflow => "overflow",
        ProviderErrorClass::Other => "other",
    }
}

/// Drops observations that have aged out of `window_secs`, then caps the
/// ring at [`MAX_OBSERVATIONS`] newest.
fn prune(mut observations: Vec<Observation>, now: u64, policy: &HealthPolicy) -> Vec<Observation> {
    let floor = now.saturating_sub(policy.window_secs);
    observations.retain(|obs| obs.at >= floor);
    if observations.len() > MAX_OBSERVATIONS {
        let drop = observations.len() - MAX_OBSERVATIONS;
        observations.drain(..drop);
    }
    observations
}

/// The same window rule for the bare row-time rings (`successes`,
/// `failures`), capped at [`MAX_SAMPLES`] newest.
fn prune_times(mut times: Vec<u64>, now: u64, policy: &HealthPolicy) -> Vec<u64> {
    let floor = now.saturating_sub(policy.window_secs);
    times.retain(|at| *at >= floor);
    if times.len() > MAX_SAMPLES {
        let drop = times.len() - MAX_SAMPLES;
        times.drain(..drop);
    }
    times
}

/// The same window rule again for the latency ring, capped at
/// [`MAX_OBSERVATIONS`]: a median needs far fewer samples than a rate does.
fn prune_latency(
    mut samples: Vec<LatencySample>,
    now: u64,
    policy: &HealthPolicy,
) -> Vec<LatencySample> {
    let floor = now.saturating_sub(policy.window_secs);
    samples.retain(|sample| sample.at >= floor);
    if samples.len() > MAX_OBSERVATIONS {
        let drop = samples.len() - MAX_OBSERVATIONS;
        samples.drain(..drop);
    }
    samples
}

/// Every ring pruned to the current window at once, so no caller can fold
/// evidence into one ring and judge it against another's stale contents.
fn pruned(health: &RouteHealth, now: u64, policy: &HealthPolicy) -> RouteHealth {
    let (kept_successes, success_floor) =
        prune_successes(health.successes.clone(), health.success_floor, now, policy);
    RouteHealth {
        phase: health.phase.clone(),
        observations: prune(health.observations.clone(), now, policy),
        seen_ids: health.seen_ids.clone(),
        successes: kept_successes,
        failures: prune_times(health.failures.clone(), now, policy),
        latency: prune_latency(health.latency.clone(), now, policy),
        seen_other_ids: health.seen_other_ids.clone(),
        success_floor,
    }
}

fn median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

/// The rolling rate's numerator and denominator, over the span BOTH rings
/// still cover.
///
/// Finding 10: the rings are capped independently, so a busy window can
/// retain 20 failures while the success ring has already dropped everything
/// older than its newest [`MAX_SAMPLES`]. Counting those failures against
/// that truncated denominator reported 20/60 for a window that really ran
/// 20/120 -- a degradation invented by the cap rather than by the route.
/// Once the success ring has overflowed, its oldest retained entry is the
/// floor for both sides.
fn rate_samples(health: &RouteHealth) -> (u64, u64) {
    let floor = health.success_floor.unwrap_or(0);
    let failures = health.failures.iter().filter(|at| **at > floor).count() as u64;
    (failures, failures + health.successes.len() as u64)
}

/// Window-prunes and caps the success ring, advancing `floor` to the `at` of
/// the NEWEST entry a CAP eviction removed (R3).
///
/// Only a cap eviction moves the floor. Ageing out of the window does not:
/// the failure ring ages out under the identical rule, so both sides lose
/// the same span and the rate stays honest without a floor at all.
///
/// The newest evicted rather than the oldest: everything between the two is
/// exactly the region whose successes are now gone, and keeping the failures
/// that fall in it is the inflated numerator finding 10 set out to remove.
fn prune_successes(
    successes: Vec<u64>,
    floor: Option<u64>,
    now: u64,
    policy: &HealthPolicy,
) -> (Vec<u64>, Option<u64>) {
    let mut kept = successes;
    let window_floor = now.saturating_sub(policy.window_secs);
    kept.retain(|at| *at >= window_floor);
    if kept.len() <= MAX_SAMPLES {
        return (kept, floor);
    }
    let drop = kept.len() - MAX_SAMPLES;
    let evicted = kept[..drop].iter().copied().max();
    kept.drain(..drop);
    let advanced = match (floor, evicted) {
        (Some(previous), Some(evicted)) => Some(previous.max(evicted)),
        (previous, evicted) => previous.or(evicted),
    };
    (kept, advanced)
}

/// The rolling error-rate verdict over the already-pruned rings: `Some` with
/// the human reason when the rate is at or above the policy threshold.
fn error_rate_reason(health: &RouteHealth, policy: &HealthPolicy) -> Option<String> {
    let (failures, samples) = rate_samples(health);
    if samples < u64::from(policy.degrade_min_samples.max(2)) {
        return None;
    }
    if failures * 100 < samples * u64::from(policy.degrade_error_rate_pct) {
        return None;
    }
    Some(format!(
        "error rate {}% over {samples} turns",
        failures * 100 / samples
    ))
}

/// The latency verdict, `None` whenever the operator left `degrade_ttft_ms`
/// unset -- which is the default.
fn latency_reason(health: &RouteHealth, policy: &HealthPolicy) -> Option<String> {
    let threshold = policy.degrade_ttft_ms?;
    if health.latency.len() < policy.degrade_min_samples.max(2) as usize {
        return None;
    }
    let samples: Vec<u64> = health.latency.iter().map(|s| s.ttft_ms).collect();
    let p50 = median(&samples)?;
    if p50 < threshold {
        return None;
    }
    Some(format!(
        "first-token p50 {:.1}s over {} turns",
        p50 as f64 / 1000.0,
        samples.len()
    ))
}

/// Whether both degrade signals have cleared their HYSTERESIS bands -- half
/// the error-rate threshold, three quarters of the latency one -- or have no
/// evidence left in the window at all. Deliberately stricter than "the
/// degrade condition no longer holds": a route sitting exactly at the
/// threshold would otherwise flap in and out of `Degraded` on every poll.
fn restored(health: &RouteHealth, policy: &HealthPolicy) -> bool {
    let (failures, samples) = rate_samples(health);
    let min_samples = u64::from(policy.degrade_min_samples.max(2));
    let error_clear = failures == 0
        || (samples >= min_samples
            && failures * 200 < samples * u64::from(policy.degrade_error_rate_pct));
    let latency_clear = match policy.degrade_ttft_ms {
        None => true,
        Some(threshold) => {
            if health.latency.is_empty() {
                true
            } else if health.latency.len() < min_samples as usize {
                false
            } else {
                median(&health.latency.iter().map(|s| s.ttft_ms).collect::<Vec<_>>())
                    .is_some_and(|p50| p50 * 4 < threshold * 3)
            }
        }
    };
    error_clear && latency_clear
}

/// Lays the `Degraded` overlay over a freshly recomputed `baseline` phase
/// (always `Healthy` or `Suspect`), given what the phase was BEFORE this
/// fold. Never touches `Open`/`HalfOpen`/`Unavailable`: a breaker that is
/// already shut has nothing to say about a rate.
fn settle_degradation(
    prior: &Phase,
    baseline: Phase,
    health: &RouteHealth,
    now: u64,
    policy: &HealthPolicy,
) -> (Phase, Option<Transition>) {
    if !policy.enabled {
        return (baseline, None);
    }
    if let Phase::Degraded { reason, .. } = prior {
        if restored(health, policy) {
            let reason = format!("{reason} cleared; route restored");
            return (baseline, Some(Transition::Restored(reason)));
        }
        return (prior.clone(), None);
    }
    match error_rate_reason(health, policy).or_else(|| latency_reason(health, policy)) {
        Some(reason) => (
            Phase::Degraded {
                since: now,
                reason: reason.clone(),
            },
            Some(Transition::Degraded(reason)),
        ),
        None => (baseline, None),
    }
}

/// The `host[:port]` a base URL points at, lower-cased, with scheme, any
/// userinfo, path, query and fragment stripped -- the identity two harnesses
/// SHARE when an operator retargets both at one gateway. `None` for a URL
/// with no host at all, which is what a malformed value degrades to: an
/// unparseable endpoint must never make two routes look related.
///
/// Never carries a credential: userinfo is dropped before the host is read,
/// so this string is safe to put in a log line or a status row.
pub fn dependency_from_base_url(raw: &str) -> Option<String> {
    let after_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

/// When an `Open` or `Unavailable` breaker starts admitting its next trial:
/// `cooldown_secs` after it tripped. `None` for every other phase.
pub fn trial_at(phase: &Phase, policy: &HealthPolicy) -> Option<u64> {
    match phase {
        Phase::Open { until, .. } => Some(*until),
        // Finding 1: an `Unavailable` route re-probes on the same cooldown
        // an `Open` one does, measured from when it was marked.
        Phase::Unavailable { since, .. } => Some(since.saturating_add(policy.cooldown_secs)),
        _ => None,
    }
}

/// Normalizes a breaker whose cooldown has elapsed into `HalfOpen`, so both
/// [`observe`] and [`record_success`] reason about one shape. Every other
/// phase passes through untouched.
///
/// `pub` (review round 1, finding 6) because this is the ONLY producer of
/// `Transition::HalfOpened`, and both internal callers go on to supersede it
/// with an `Opened` or a `Recovered`. `health_store::observe_poll` therefore
/// applies it first, on its own, so the half-open edge is actually persisted
/// and logged instead of being a phase no `zirv ctx status` ever showed and
/// no `health-half-open` line ever recorded.
pub fn promote(
    health: &RouteHealth,
    now: u64,
    policy: &HealthPolicy,
) -> (RouteHealth, Option<Transition>) {
    let Some(trial_at) = trial_at(&health.phase, policy) else {
        return (health.clone(), None);
    };
    if now < trial_at {
        return (health.clone(), None);
    }
    let reason = "cooldown elapsed; admitting one trial".to_string();
    (
        RouteHealth {
            phase: Phase::HalfOpen {
                since: now,
                trial: None,
            },
            ..health.clone()
        },
        Some(Transition::HalfOpened(reason)),
    )
}

fn open_reason(failures: u32, class: ProviderErrorClass, policy: &HealthPolicy) -> String {
    format!(
        "{failures} {} error(s) in {}; next health check in ~{} (estimate)",
        class_label(class),
        crate::style::format_age(policy.window_secs),
        crate::style::format_age(policy.cooldown_secs),
    )
}

/// Folds one observed provider error into `health`.
///
/// Transport/Server accumulate towards `open_after_failures`; Auth denies
/// the route immediately (no number of retries fixes a rejected
/// credential); RateLimit/Overflow/Other are recorded as observations but
/// never change the phase.
///
/// `observed.at` is the transcript row's own time, which is what the window
/// is judged against; a row with no time of its own falls back to `now`, and
/// it is the CALLER's job (see `health_store::observe_poll`) not to hand
/// such a row over unless the poll that read it was genuinely incremental.
/// An `observed.id` already in `seen_ids` is a no-op: the same row reaching
/// two supervisors must count once.
pub fn observe(
    health: &RouteHealth,
    observed: &Observed,
    now: u64,
    policy: &HealthPolicy,
) -> (RouteHealth, Option<Transition>) {
    if let Some(id) = &observed.id
        && (health.seen_ids.iter().any(|seen| seen == id)
            || health.seen_other_ids.iter().any(|seen| seen == id))
    {
        return (health.clone(), None);
    }
    let class = observed.class;
    let at = observed.at.unwrap_or(now);
    let (promoted, _) = promote(health, now, policy);
    let mut next = pruned(&promoted, now, policy);
    next.observations.push(Observation { class, at });
    next.observations = prune(next.observations, now, policy);
    if counts_toward_opening(class) {
        next.failures.push(at);
        next.failures = prune_times(next.failures, now, policy);
    }
    if let Some(id) = &observed.id {
        // R2: a counting class's id shares the `failures` ring's own cap and
        // its own FIFO order, so an id is only ever evicted alongside (or
        // after) the failure it belongs to. Everything else goes in its own
        // ring, where no burst of rate limits can push a still-retained
        // failure's id out -- finding 8's single shared cap fixed the size
        // but left the eviction competition in place.
        let (ring, cap) = if counts_toward_opening(class) {
            (&mut next.seen_ids, MAX_SAMPLES)
        } else {
            (&mut next.seen_other_ids, MAX_OBSERVATIONS)
        };
        ring.push(id.clone());
        if ring.len() > cap {
            let drop = ring.len() - cap;
            ring.drain(..drop);
        }
    }

    if !policy.enabled {
        return (next, None);
    }
    // The phase this fold started from, kept because the `Healthy`/`Suspect`
    // recomputation below overwrites it -- `settle_degradation` needs to know
    // whether the route was ALREADY degraded, or a route sitting at the
    // threshold would re-enter `Degraded` (and log a fresh transition) on
    // every single observation.
    let prior_phase = next.phase.clone();

    if class == ProviderErrorClass::Auth {
        // Review round 2, finding 1: Auth is the one class that does NOT go
        // through `prune`'s window filter on its way to a phase change, so
        // it needed the same guard explicitly. Without it a dated week-old
        // `authentication_error` row -- exactly what a fresh scorer
        // re-parsing a transcript from offset 0 hands over -- denied the
        // harness at the CURRENT clock, healed after the cooldown, and was
        // marked again on the next iteration: a route flapping every cycle
        // on one long-fixed credential error. An undated row relies on the
        // caller's incremental vetting (`health_store::observe_poll`),
        // exactly as Transport/Server do.
        let inside_window = observed
            .at
            .is_none_or(|at| at >= now.saturating_sub(policy.window_secs));
        if !inside_window {
            return (next, None);
        }
        let already = matches!(next.phase, Phase::Unavailable { .. });
        let reason = format!(
            "provider rejected authentication or configuration; next health check in ~{} \
             (estimate)",
            crate::style::format_age(policy.cooldown_secs)
        );
        if !already {
            next.phase = Phase::Unavailable {
                since: now,
                reason: reason.clone(),
            };
        }
        let transition = (!already).then_some(Transition::MarkedUnavailable(reason));
        return (next, transition);
    }

    if !counts_toward_opening(class) {
        return (next, None);
    }

    // An already-open breaker (still inside its cooldown) just accumulates:
    // re-opening it on every retry would push `until` out indefinitely.
    if matches!(next.phase, Phase::Open { .. } | Phase::Unavailable { .. }) {
        return (next, None);
    }

    let failures = next
        .observations
        .iter()
        .filter(|obs| counts_toward_opening(obs.class))
        .count() as u32;
    if failures < policy.open_after_failures.max(1) {
        // `failures == 0` means every counting observation aged out of the
        // window -- including the one just folded in, which is what a full
        // re-parse of an old transcript produces. That is HEALTHY, not
        // `Suspect { failures: 0 }`: a phase that is not healthy shows up in
        // `zirv ctx status` and keeps the record alive past its expiry.
        let baseline = if failures == 0 {
            Phase::Healthy
        } else {
            Phase::Suspect {
                failures,
                first_at: next
                    .observations
                    .iter()
                    .filter(|obs| counts_toward_opening(obs.class))
                    .map(|obs| obs.at)
                    .min()
                    .unwrap_or(at),
            }
        };
        // Finding 9: the degrade rule is evaluated ONCE per poll, by
        // [`record_samples`], after every piece of that poll's evidence is
        // folded. Deriving it here too meant a poll carrying one failure and
        // one success evaluated the failure against a denominator the
        // success had not yet reached (2/8 rather than the poll's true 2/9),
        // degraded on it, and then hysteresis held the route there. So this
        // only ever CARRIES an existing degradation forward -- it never
        // derives a new one.
        next.phase = if matches!(prior_phase, Phase::Degraded { .. }) {
            prior_phase
        } else {
            baseline
        };
        return (next, None);
    }

    let reason = open_reason(failures, class, policy);
    next.phase = Phase::Open {
        opened_at: now,
        until: now.saturating_add(policy.cooldown_secs),
        reason: reason.clone(),
        class,
    };
    (next, Some(Transition::Opened(reason)))
}

/// Folds one poll's dated turn OUTCOMES into `health` and then evaluates the
/// degrade/restore rule -- the ONE place that rule is applied, and the last
/// step of every poll (finding 9), so it always judges a complete poll's
/// evidence rather than a half-folded prefix of it.
///
/// Called even for a poll with no samples at all: a poll carrying only
/// failures still has to reach a verdict, and its numerator is exactly the
/// one [`observe`] just folded.
///
/// Separate from [`observe`] because these samples change only the rolling
/// rate, never the breaker: no number of slow or successful turns can open a
/// circuit, and a latency sample can do nothing at all unless the operator
/// set `degrade_ttft_ms`.
///
/// Both rings are de-duplicated by the sample's own time rather than by a row
/// id: a turn's closing row carries a millisecond timestamp, so two distinct
/// turns landing on the same epoch SECOND is the only collision possible, and
/// dropping one sample of a pair that close together costs a rate nothing.
/// The id ring is reserved for `observations`, whose double-count would move
/// a phase.
pub fn record_samples(
    health: &RouteHealth,
    successes: &[u64],
    latencies: &[LatencySample],
    now: u64,
    policy: &HealthPolicy,
) -> (RouteHealth, Option<Transition>) {
    let (promoted, _) = promote(health, now, policy);
    let prior_phase = promoted.phase.clone();
    let mut next = pruned(&promoted, now, policy);
    let floor = now.saturating_sub(policy.window_secs);
    for at in successes {
        if *at >= floor && !next.successes.contains(at) {
            next.successes.push(*at);
        }
    }
    for sample in latencies {
        if sample.at >= floor && !next.latency.iter().any(|kept| kept.at == sample.at) {
            next.latency.push(*sample);
        }
    }
    let (kept, floor) = prune_successes(
        std::mem::take(&mut next.successes),
        next.success_floor,
        now,
        policy,
    );
    next.successes = kept;
    next.success_floor = floor;
    next.latency = prune_latency(next.latency, now, policy);

    // Only a route the breaker is not already reasoning about has a rate
    // worth reading: `Open`/`HalfOpen`/`Unavailable` are shut or probing,
    // and a rate cannot say anything more about them.
    if !matches!(
        prior_phase,
        Phase::Healthy | Phase::Suspect { .. } | Phase::Degraded { .. }
    ) {
        return (next, None);
    }
    let baseline = match &prior_phase {
        Phase::Degraded { .. } => {
            let failures = next
                .observations
                .iter()
                .filter(|obs| counts_toward_opening(obs.class))
                .count() as u32;
            if failures == 0 {
                Phase::Healthy
            } else {
                Phase::Suspect {
                    failures,
                    first_at: next
                        .observations
                        .iter()
                        .filter(|obs| counts_toward_opening(obs.class))
                        .map(|obs| obs.at)
                        .min()
                        .unwrap_or(now),
                }
            }
        }
        other => other.clone(),
    };
    let (phase, transition) = settle_degradation(&prior_phase, baseline, &next, now, policy);
    next.phase = phase;
    (next, transition)
}

/// Records one caller's claim on a half-open route's single recovery trial.
///
/// Pure, like everything else here: the returned record is what the store
/// must persist, and the returned [`Admission`] is what the CALLER may do --
/// `Trial` when it won the claim, a `Deny` naming the current holder when it
/// lost, and whatever [`admission`] says for any other phase.
pub fn claim(
    health: &RouteHealth,
    claimant: &str,
    now: u64,
    policy: &HealthPolicy,
) -> (RouteHealth, Admission, Option<Transition>) {
    let (promoted, _) = promote(health, now, policy);
    if !policy.enabled {
        return (promoted, Admission::Allow, None);
    }
    let Phase::HalfOpen { since, trial } = &promoted.phase else {
        let verdict = admission(&promoted, now, policy);
        return (promoted, verdict, None);
    };
    if let Some(held) = trial
        && now < held.at.saturating_add(policy.cooldown_secs)
    {
        let verdict = trial_in_flight_denial(&held.claim, held.at, now, policy);
        return (promoted, verdict, None);
    }
    let reason = format!("recovery trial claimed by {claimant}");
    let claimed = RouteHealth {
        phase: Phase::HalfOpen {
            since: *since,
            trial: Some(Trial {
                claim: claimant.to_string(),
                at: now,
            }),
        },
        ..promoted
    };
    (
        claimed,
        Admission::Trial,
        Some(Transition::TrialClaimed(reason)),
    )
}

fn trial_in_flight_denial(
    claim: &str,
    claimed_at: u64,
    now: u64,
    policy: &HealthPolicy,
) -> Admission {
    let expiry = claimed_at.saturating_add(policy.cooldown_secs);
    Admission::deny(format!(
        "half-open: recovery trial already in flight ({claim}); next attempt in ~{} (estimate)",
        crate::style::format_age(expiry.saturating_sub(now))
    ))
}

/// Folds one successfully completed turn into `health`. A trial that
/// succeeds recovers the route and clears its observation ring; an open
/// breaker still inside its cooldown is left alone (nothing was routed
/// there, so nothing was proven).
pub fn record_success(
    health: &RouteHealth,
    now: u64,
    policy: &HealthPolicy,
) -> (RouteHealth, Option<Transition>) {
    let (promoted, _) = promote(health, now, policy);
    // Finding 1(b): a heal clears the PHASE and the observation ring, never
    // the seen-id ring. Returning `RouteHealth::default()` wiped the ids, so
    // the very rows that had just been folded in became eligible again and
    // the next poll re-observed them -- the other half of the flap.
    let healed = |reason: String| {
        (
            RouteHealth {
                phase: Phase::Healthy,
                observations: Vec::new(),
                seen_ids: promoted.seen_ids.clone(),
                // The rolling-rate rings age out of the window, they are
                // never wiped: see `RouteHealth::failures`' own comment on
                // why a numerator cleared by every completed turn can never
                // accumulate a rate.
                successes: promoted.successes.clone(),
                failures: promoted.failures.clone(),
                latency: promoted.latency.clone(),
                seen_other_ids: promoted.seen_other_ids.clone(),
                success_floor: promoted.success_floor,
            },
            Some(Transition::Recovered(reason)),
        )
    };
    match &promoted.phase {
        Phase::Healthy => (promoted, None),
        Phase::Open { .. } => (promoted, None),
        // One completed turn says nothing about a RATE, so it never lifts a
        // degradation on its own -- `record_samples` restores the route once
        // the window's own evidence clears the hysteresis band.
        Phase::Degraded { .. } => (promoted, None),
        Phase::HalfOpen { .. } => healed("a trial turn completed; route reopened".to_string()),
        Phase::Suspect { failures, .. } => healed(format!(
            "a turn completed after {failures} failure(s); route is healthy again"
        )),
        // Only reachable inside the cooldown (past it, `promote` has
        // already turned this into `HalfOpen`). A success there still
        // proves the route works, so it heals.
        Phase::Unavailable { .. } => {
            healed("a turn completed; the route is reachable again".to_string())
        }
    }
}

/// Whether this route may be given work right now.
pub fn admission(health: &RouteHealth, now: u64, policy: &HealthPolicy) -> Admission {
    if !policy.enabled {
        return Admission::Allow;
    }
    match &health.phase {
        Phase::Healthy | Phase::Suspect { .. } => Admission::Allow,
        // The EFFECTIVE verdict, not the stored one -- the same rule
        // `Open` follows past its cooldown. A degraded route's evidence
        // ages out of the window whether or not anything is still polling
        // it, and a record left behind by a finished session must not rank
        // a route last forever. The next poll persists the restoration; until
        // one happens, this is what is actually true.
        Phase::Degraded { reason, .. } => {
            let floor = now.saturating_sub(policy.window_secs);
            let fresh = health.failures.iter().any(|at| *at >= floor)
                || health.latency.iter().any(|sample| sample.at >= floor);
            if !fresh {
                return Admission::Allow;
            }
            Admission::Degraded {
                reason: reason.clone(),
            }
        }
        // A trial that is still inside its own cooldown belongs to whoever
        // claimed it: admitting a second one defeats the whole point of a
        // half-open probe. An EXPIRED trial is simply ignored, so a claimant
        // that never came back cannot hold the route shut.
        Phase::HalfOpen { trial, .. } => match trial {
            Some(held) if now < held.at.saturating_add(policy.cooldown_secs) => {
                trial_in_flight_denial(&held.claim, held.at, now, policy)
            }
            _ => Admission::Trial,
        },
        Phase::Open { until, reason, .. } => {
            if now >= *until {
                return Admission::Trial;
            }
            Admission::deny(format!("route health open: {reason}"))
        }
        // Finding 1: re-probes on the same cooldown, so a credential the
        // operator has since fixed heals on its own.
        Phase::Unavailable { reason, .. } => {
            if trial_at(&health.phase, policy).is_some_and(|at| now >= at) {
                return Admission::Trial;
            }
            Admission::deny(format!("route health unavailable: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HealthPolicy {
        HealthPolicy::default()
    }

    /// Every observation gets a distinct id, so nothing in these tests is
    /// silently de-duplicated -- the idempotency behaviour has its own test.
    fn err(class: ProviderErrorClass, at: u64) -> Observed {
        Observed::new(class, Some(at), Some(format!("{class:?}-{at}")))
    }

    fn observe_many(
        events: &[Observed],
        now: u64,
        policy: &HealthPolicy,
    ) -> (RouteHealth, Option<Transition>) {
        let mut health = RouteHealth::default();
        let mut last = None;
        for observed in events {
            let (next, transition) = observe(&health, observed, observed.at.unwrap_or(now), policy);
            health = next;
            last = transition;
        }
        (health, last)
    }

    /// Slice A: the rolling rate degrades a route that is still reachable.
    /// Two failures and six successes is 25% over eight turns -- exactly the
    /// default threshold, and the default minimum sample count.
    fn degraded_route(policy: &HealthPolicy) -> RouteHealth {
        let mut health = RouteHealth::default();
        for at in [1_000, 1_010] {
            let (next, _) = observe(&health, &err(ProviderErrorClass::Transport, at), at, policy);
            health = next;
        }
        let successes: Vec<u64> = (0..6).map(|i| 1_020 + i * 10).collect();
        let (next, transition) = record_samples(&health, &successes, &[], 1_100, policy);
        assert!(
            matches!(transition, Some(Transition::Degraded(_))),
            "{transition:?}"
        );
        next
    }

    #[test]
    fn a_rolling_error_rate_degrades_a_reachable_route() {
        let policy = policy();
        let health = degraded_route(&policy);
        assert!(matches!(health.phase, Phase::Degraded { .. }), "{health:?}");
        let verdict = admission(&health, 1_100, &policy);
        assert_eq!(verdict.degraded(), Some("error rate 25% over 8 turns"));
        assert_eq!(
            verdict.denied(),
            None,
            "a degraded route is reduced, never excluded"
        );
    }

    #[test]
    fn a_degraded_record_whose_evidence_aged_out_admits_normally_again() {
        let policy = policy();
        let health = degraded_route(&policy);
        assert!(admission(&health, 1_100, &policy).degraded().is_some());
        assert_eq!(
            admission(&health, 1_100 + policy.window_secs, &policy),
            Admission::Allow,
            "a record left behind by a finished session must not rank a route \
             last forever"
        );
    }

    #[test]
    fn a_rate_below_the_threshold_leaves_the_route_healthy() {
        let policy = policy();
        let mut health = RouteHealth::default();
        let (next, _) = observe(
            &health,
            &err(ProviderErrorClass::Transport, 1_000),
            1_000,
            &policy,
        );
        health = next;
        let successes: Vec<u64> = (0..9).map(|i| 1_010 + i * 10).collect();
        let (next, transition) = record_samples(&health, &successes, &[], 1_100, &policy);
        assert_eq!(transition, None, "10% over ten turns is not degradation");
        assert!(matches!(next.phase, Phase::Suspect { .. }), "{next:?}");
    }

    #[test]
    fn too_few_samples_never_degrade_a_route() {
        let policy = policy();
        let mut health = RouteHealth::default();
        for at in [1_000, 1_010] {
            let (next, _) = observe(
                &health,
                &err(ProviderErrorClass::Transport, at),
                at,
                &policy,
            );
            health = next;
        }
        // 2 failures, 2 successes: a 50% rate, but only four samples.
        let (next, transition) = record_samples(&health, &[1_020, 1_030], &[], 1_100, &policy);
        assert_eq!(transition, None);
        assert!(!matches!(next.phase, Phase::Degraded { .. }), "{next:?}");
    }

    /// Hysteresis: dropping just under the threshold is not enough, half of
    /// it is -- otherwise a route sitting at 25% flaps on every poll.
    /// Finding 10: the two rings are capped independently, so a long busy
    /// window retains every failure while the success ring has already
    /// dropped the oldest successes. Counting those failures against the
    /// truncated denominator invented a degradation the route never had.
    #[test]
    fn failures_older_than_the_retained_successes_do_not_inflate_the_rate() {
        let policy = policy();
        let mut health = RouteHealth::default();
        // 20 failures first, then 100 clean turns -- a 17% window, well
        // under the 25% threshold, but only the newest 40 successes survive
        // the ring.
        for i in 0..20u64 {
            let at = 10_000 + i;
            let (next, _) = observe(
                &health,
                &err(ProviderErrorClass::Transport, at),
                at,
                &policy,
            );
            health = next;
        }
        let successes: Vec<u64> = (0..100).map(|i| 10_100 + i).collect();
        let (next, transition) = record_samples(&health, &successes, &[], 10_200, &policy);

        assert_eq!(next.successes.len(), MAX_SAMPLES, "the ring overflowed");
        assert_eq!(
            transition, None,
            "every retained failure predates the oldest retained success, so the covered \
             span carries no failures at all: {next:?}"
        );
        assert!(!matches!(next.phase, Phase::Degraded { .. }), "{next:?}");
    }

    /// Finding 9: one degrade evaluation per poll, after ALL of that poll's
    /// evidence is folded. Evaluating the failure first saw 2/8 (degraded)
    /// where the poll's own true rate was 2/9 (healthy), and hysteresis then
    /// held the route there.
    #[test]
    fn a_polls_failure_is_judged_against_that_same_polls_successes() {
        let policy = policy();
        // Six clean turns and one failure already in the window: 1/7.
        let mut health = RouteHealth::default();
        let (next, _) = observe(
            &health,
            &err(ProviderErrorClass::Transport, 1_000),
            1_000,
            &policy,
        );
        health = next;
        let (next, _) = record_samples(
            &health,
            &(0..6).map(|i| 1_010 + i * 10).collect::<Vec<_>>(),
            &[],
            1_100,
            &policy,
        );
        health = next;
        assert!(
            !matches!(health.phase, Phase::Degraded { .. }),
            "{health:?}"
        );

        // One more poll carrying BOTH a failure and a success: 2/9, still
        // healthy. `observe` alone must reach no verdict.
        let (after_error, transition) = observe(
            &health,
            &err(ProviderErrorClass::Transport, 1_110),
            1_110,
            &policy,
        );
        assert_eq!(transition, None, "observe never degrades on its own");
        let (settled, transition) = record_samples(&after_error, &[1_120], &[], 1_120, &policy);
        assert_eq!(
            transition, None,
            "2 failures over 9 turns is 22%, under the 25% threshold: {settled:?}"
        );
        assert!(
            !matches!(settled.phase, Phase::Degraded { .. }),
            "{settled:?}"
        );
    }

    /// R2: a burst of rate limits must not evict the row ids of failures the
    /// numerator is still holding. Three healed failures followed by 41 rate
    /// limits used to push all three ids out of the shared ring, so a replay
    /// from offset 0 counted them again and reopened a route that had
    /// already healed.
    #[test]
    fn a_burst_of_rate_limits_cannot_make_a_replay_reopen_a_healed_route() {
        let policy = policy();
        let failures: Vec<Observed> = (0..3)
            .map(|i| err(ProviderErrorClass::Transport, 1_000 + i))
            .collect();
        // The state a healed route is left in: the phase and the observation
        // ring are cleared, the failure times and their row ids are not.
        let mut health = RouteHealth {
            phase: Phase::Healthy,
            failures: failures.iter().filter_map(|f| f.at).collect(),
            seen_ids: failures
                .iter()
                .filter_map(|f| f.id.clone())
                .collect::<Vec<_>>(),
            ..RouteHealth::default()
        };

        for i in 0..41u64 {
            let (next, _) = observe(
                &health,
                &err(ProviderErrorClass::RateLimit, 1_200 + i),
                1_300,
                &policy,
            );
            health = next;
        }

        // The replay a fresh scorer performs from offset 0.
        for observed in &failures {
            let (next, transition) = observe(&health, observed, 1_400, &policy);
            assert_eq!(transition, None, "an already-counted row must be a no-op");
            health = next;
        }
        assert!(
            health.phase.is_healthy(),
            "a healed route must not reopen on rows it already folded: {health:?}"
        );
        assert_eq!(health.failures.len(), 3, "and none was counted twice");
    }

    /// R3: an exactly-full success ring has evicted nothing, so it still
    /// covers its whole span and every failure in it counts. A length test
    /// could not tell that apart from an overflowed ring, and discarded both
    /// failures below -- hiding a real degradation.
    #[test]
    fn an_exactly_full_success_ring_still_counts_its_failures() {
        let policy = HealthPolicy {
            degrade_error_rate_pct: 4,
            ..policy()
        };
        let mut health = RouteHealth::default();
        for i in 0..2u64 {
            let at = 1_000 + i;
            let (next, _) = observe(
                &health,
                &err(ProviderErrorClass::Transport, at),
                at,
                &policy,
            );
            health = next;
        }
        let successes: Vec<u64> = (0..MAX_SAMPLES as u64).map(|i| 1_010 + i).collect();
        let (settled, transition) = record_samples(&health, &successes, &[], 1_100, &policy);

        assert_eq!(settled.successes.len(), MAX_SAMPLES, "full, not overflowed");
        assert_eq!(
            settled.success_floor, None,
            "nothing was evicted, so there is no uncovered span"
        );
        assert!(
            matches!(transition, Some(Transition::Degraded(_))),
            "2 failures over 42 turns is 4%, at the threshold: {settled:?}"
        );
    }

    #[test]
    fn an_overflowed_success_ring_records_the_span_it_no_longer_covers() {
        let policy = policy();
        let mut health = RouteHealth::default();
        for i in 0..20u64 {
            let at = 10_000 + i;
            let (next, _) = observe(
                &health,
                &err(ProviderErrorClass::Transport, at),
                at,
                &policy,
            );
            health = next;
        }
        let successes: Vec<u64> = (0..100).map(|i| 10_100 + i).collect();
        let (settled, transition) = record_samples(&health, &successes, &[], 10_200, &policy);

        assert_eq!(settled.successes.len(), MAX_SAMPLES);
        assert!(
            settled.success_floor.is_some_and(|at| at > 10_019),
            "the floor sits past every failure: {:?}",
            settled.success_floor
        );
        assert_eq!(
            transition, None,
            "no retained failure falls inside the covered span: {settled:?}"
        );
    }

    #[test]
    fn a_degraded_route_restores_only_below_half_the_threshold() {
        let policy = policy();
        let health = degraded_route(&policy);

        // 2 failures, 8 successes = 20%: under the 25% threshold, over the
        // 12.5% hysteresis band.
        let (still, transition) = record_samples(&health, &[1_080, 1_090], &[], 1_150, &policy);
        assert_eq!(transition, None, "{still:?}");
        assert!(matches!(still.phase, Phase::Degraded { .. }), "{still:?}");

        // 2 failures, 18 successes = 10%, which clears the band.
        let more: Vec<u64> = (0..10).map(|i| 1_100 + i * 10).collect();
        let (restored, transition) = record_samples(&still, &more, &[], 1_250, &policy);
        assert!(
            matches!(transition, Some(Transition::Restored(_))),
            "{transition:?}"
        );
        assert_eq!(admission(&restored, 1_250, &policy), Admission::Allow);
    }

    #[test]
    fn a_degraded_route_restores_once_its_failures_age_out_of_the_window() {
        let policy = policy();
        let health = degraded_route(&policy);
        // 600s window: everything above is long gone by now.
        let (restored, transition) = record_samples(&health, &[2_000], &[], 2_000, &policy);
        assert!(
            matches!(transition, Some(Transition::Restored(_))),
            "{transition:?}"
        );
        assert!(restored.phase.is_healthy(), "{restored:?}");
    }

    #[test]
    fn a_degraded_route_still_opens_once_the_failures_reach_the_threshold() {
        let policy = policy();
        let health = degraded_route(&policy);
        let (opened, transition) = observe(
            &health,
            &err(ProviderErrorClass::Transport, 1_110),
            1_110,
            &policy,
        );
        assert!(matches!(opened.phase, Phase::Open { .. }), "{opened:?}");
        assert!(matches!(transition, Some(Transition::Opened(_))));
        assert!(admission(&opened, 1_110, &policy).denied().is_some());
    }

    #[test]
    fn a_completed_turn_alone_never_lifts_a_degradation() {
        let policy = policy();
        let health = degraded_route(&policy);
        let (next, transition) = record_success(&health, 1_110, &policy);
        assert_eq!(transition, None);
        assert!(matches!(next.phase, Phase::Degraded { .. }), "{next:?}");
    }

    #[test]
    fn latency_degrades_a_route_only_when_the_operator_set_a_threshold() {
        let samples: Vec<LatencySample> = (0..8)
            .map(|i| LatencySample {
                at: 1_000 + i * 10,
                ttft_ms: 30_000,
            })
            .collect();

        let off = policy();
        let (quiet, transition) =
            record_samples(&RouteHealth::default(), &[], &samples, 1_100, &off);
        assert_eq!(transition, None, "the latency signal is off by default");
        assert!(quiet.phase.is_healthy(), "{quiet:?}");

        let on = HealthPolicy {
            degrade_ttft_ms: Some(20_000),
            ..policy()
        };
        let (slow, transition) = record_samples(&RouteHealth::default(), &[], &samples, 1_100, &on);
        assert!(
            matches!(transition, Some(Transition::Degraded(_))),
            "{transition:?}"
        );
        assert_eq!(
            admission(&slow, 1_100, &on).degraded(),
            Some("first-token p50 30.0s over 8 turns"),
        );
        assert_eq!(
            admission(&slow, 1_100, &on).denied(),
            None,
            "latency never opens a circuit"
        );
    }

    #[test]
    fn a_latency_degraded_route_restores_under_three_quarters_of_the_threshold() {
        let policy = HealthPolicy {
            degrade_ttft_ms: Some(20_000),
            ..policy()
        };
        let slow: Vec<LatencySample> = (0..8)
            .map(|i| LatencySample {
                at: 1_000 + i * 10,
                ttft_ms: 30_000,
            })
            .collect();
        let (degraded, _) = record_samples(&RouteHealth::default(), &[], &slow, 1_100, &policy);
        assert!(matches!(degraded.phase, Phase::Degraded { .. }));

        let borderline: Vec<LatencySample> = (0..20)
            .map(|i| LatencySample {
                at: 1_200 + i * 10,
                ttft_ms: 16_000,
            })
            .collect();
        let (still, transition) = record_samples(&degraded, &[], &borderline, 1_400, &policy);
        assert_eq!(transition, None, "16s is still above 3/4 of 20s");
        assert!(matches!(still.phase, Phase::Degraded { .. }), "{still:?}");

        let fast: Vec<LatencySample> = (0..20)
            .map(|i| LatencySample {
                at: 1_500 + i * 10,
                ttft_ms: 2_000,
            })
            .collect();
        let (restored, transition) = record_samples(&still, &[], &fast, 1_700, &policy);
        assert!(
            matches!(transition, Some(Transition::Restored(_))),
            "{transition:?}"
        );
        assert!(restored.phase.is_healthy(), "{restored:?}");
    }

    #[test]
    fn turn_samples_are_deduplicated_by_their_own_row_time() {
        let policy = policy();
        let (once, _) = record_samples(
            &RouteHealth::default(),
            &[1_000, 1_010],
            &[],
            1_020,
            &policy,
        );
        let (twice, _) = record_samples(&once, &[1_000, 1_010], &[], 1_020, &policy);
        assert_eq!(
            twice.successes,
            vec![1_000, 1_010],
            "a re-parsed transcript must not inflate the denominator"
        );
    }

    #[test]
    fn the_sample_rings_are_bounded_and_window_pruned() {
        let policy = policy();
        let successes: Vec<u64> = (0..(MAX_SAMPLES as u64 * 3)).map(|i| 1_000 + i).collect();
        let latency: Vec<LatencySample> = (0..(MAX_OBSERVATIONS as u64 * 3))
            .map(|i| LatencySample {
                at: 1_000 + i,
                ttft_ms: 100,
            })
            .collect();
        let (full, _) = record_samples(
            &RouteHealth::default(),
            &successes,
            &latency,
            1_200,
            &policy,
        );
        assert_eq!(full.successes.len(), MAX_SAMPLES);
        assert_eq!(full.latency.len(), MAX_OBSERVATIONS);

        let (aged, _) = record_samples(&full, &[], &[], 9_000, &policy);
        assert!(aged.successes.is_empty(), "{aged:?}");
        assert!(aged.latency.is_empty(), "{aged:?}");
    }

    /// Slice C: a half-open route hands out exactly one trial.
    #[test]
    fn a_half_open_route_admits_one_trial_and_denies_the_second() {
        let policy = policy();
        let health = RouteHealth {
            phase: Phase::HalfOpen {
                since: 2_000,
                trial: None,
            },
            ..RouteHealth::default()
        };
        let (claimed, verdict, transition) = claim(&health, "sess-a", 2_010, &policy);
        assert_eq!(verdict, Admission::Trial);
        assert!(matches!(transition, Some(Transition::TrialClaimed(_))));

        let (_, second, transition) = claim(&claimed, "sess-b", 2_020, &policy);
        assert_eq!(
            second.denied(),
            Some(
                "half-open: recovery trial already in flight (sess-a); next attempt in ~4m \
                 (estimate)"
            )
        );
        assert_eq!(transition, None, "a lost race is not a phase change");
        assert_eq!(
            admission(&claimed, 2_020, &policy).denied(),
            Some(
                "half-open: recovery trial already in flight (sess-a); next attempt in ~4m \
                 (estimate)"
            )
        );
    }

    #[test]
    fn an_expired_trial_is_claimable_again() {
        let policy = policy();
        let health = RouteHealth {
            phase: Phase::HalfOpen {
                since: 2_000,
                trial: Some(Trial {
                    claim: "sess-a".to_string(),
                    at: 2_000,
                }),
            },
            ..RouteHealth::default()
        };
        let expired = 2_000 + policy.cooldown_secs;
        assert_eq!(admission(&health, expired, &policy), Admission::Trial);
        let (_, verdict, transition) = claim(&health, "sess-b", expired, &policy);
        assert_eq!(verdict, Admission::Trial);
        assert!(matches!(transition, Some(Transition::TrialClaimed(_))));
    }

    #[test]
    fn a_trial_that_succeeds_recovers_the_route_and_clears_the_claim() {
        let policy = policy();
        let health = RouteHealth {
            phase: Phase::HalfOpen {
                since: 2_000,
                trial: Some(Trial {
                    claim: "sess-a".to_string(),
                    at: 2_000,
                }),
            },
            ..RouteHealth::default()
        };
        let (next, transition) = record_success(&health, 2_010, &policy);
        assert!(next.phase.is_healthy(), "{next:?}");
        assert!(matches!(transition, Some(Transition::Recovered(_))));
    }

    #[test]
    fn a_trial_that_fails_reopens_the_route_and_clears_the_claim() {
        let policy = policy();
        let health = RouteHealth {
            phase: Phase::HalfOpen {
                since: 2_000,
                trial: Some(Trial {
                    claim: "sess-a".to_string(),
                    at: 2_000,
                }),
            },
            observations: vec![
                Observation {
                    class: ProviderErrorClass::Transport,
                    at: 1_990,
                },
                Observation {
                    class: ProviderErrorClass::Transport,
                    at: 1_995,
                },
            ],
            ..RouteHealth::default()
        };
        let (next, transition) = observe(
            &health,
            &err(ProviderErrorClass::Transport, 2_010),
            2_010,
            &policy,
        );
        assert!(matches!(next.phase, Phase::Open { .. }), "{next:?}");
        assert!(matches!(transition, Some(Transition::Opened(_))));
    }

    #[test]
    fn an_open_breaker_records_the_class_that_tripped_it() {
        let policy = policy();
        let (health, _) = observe_many(
            &[
                err(ProviderErrorClass::Server, 1_000),
                err(ProviderErrorClass::Server, 1_010),
                err(ProviderErrorClass::Server, 1_020),
            ],
            1_020,
            &policy,
        );
        assert!(
            matches!(
                health.phase,
                Phase::Open {
                    class: ProviderErrorClass::Server,
                    ..
                }
            ),
            "{health:?}"
        );
    }

    #[test]
    fn a_base_url_reduces_to_its_host_and_port_with_no_credentials() {
        assert_eq!(
            dependency_from_base_url("https://Gateway.Example.COM/v1/messages?beta=1"),
            Some("gateway.example.com".to_string())
        );
        assert_eq!(
            dependency_from_base_url("https://user:secret@gateway.example.com:8443/v1"),
            Some("gateway.example.com:8443".to_string()),
            "userinfo is dropped before the host is read"
        );
        assert_eq!(
            dependency_from_base_url("localhost:11434"),
            Some("localhost:11434".to_string()),
            "a scheme-less value still names a host, and two harnesses \
             configured with it really do share one"
        );
        // Malformed: nothing that could be a host survives the strip.
        assert_eq!(dependency_from_base_url("https://"), None);
        assert_eq!(dependency_from_base_url("https:///v1/messages"), None);
        assert_eq!(dependency_from_base_url(""), None);
    }

    #[test]
    fn three_transport_errors_inside_the_window_open_the_breaker() {
        let policy = policy();
        let (health, transition) = observe_many(
            &[
                err(ProviderErrorClass::Transport, 1_000),
                err(ProviderErrorClass::Transport, 1_010),
                err(ProviderErrorClass::Transport, 1_020),
            ],
            1_020,
            &policy,
        );
        assert!(matches!(health.phase, Phase::Open { .. }), "{health:?}");
        assert!(matches!(transition, Some(Transition::Opened(_))));
        assert!(
            matches!(admission(&health, 1_020, &policy), Admission::Deny { .. }),
            "an open breaker denies inside its cooldown"
        );
    }

    #[test]
    fn failures_outside_the_window_do_not_count_towards_opening() {
        let policy = policy();
        let (health, transition) = observe_many(
            &[
                err(ProviderErrorClass::Transport, 1_000),
                err(ProviderErrorClass::Transport, 1_010),
                // 600s window: the first two have aged out by now.
                err(ProviderErrorClass::Transport, 2_000),
            ],
            2_000,
            &policy,
        );
        assert!(
            matches!(health.phase, Phase::Suspect { failures: 1, .. }),
            "{health:?}"
        );
        assert_eq!(transition, None);
        assert_eq!(admission(&health, 2_000, &policy), Admission::Allow);
    }

    /// Finding 2: the window is judged on the ROW's own time. A poll that
    /// re-parses a transcript from offset 0 hands over rows whose real
    /// timestamps are long past, and those must not open a breaker just
    /// because the clock says "now".
    #[test]
    fn old_rows_replayed_by_a_full_reparse_never_open_the_breaker() {
        let policy = policy();
        let replayed = [
            err(ProviderErrorClass::Transport, 1_000),
            err(ProviderErrorClass::Transport, 1_001),
            err(ProviderErrorClass::Transport, 1_002),
        ];
        let mut health = RouteHealth::default();
        for observed in &replayed {
            // A day later: exactly the shape a version-bumped checkpoint
            // produces.
            let (next, _) = observe(&health, observed, 1_000 + 86_400, &policy);
            health = next;
        }
        assert!(health.phase.is_healthy(), "{health:?}");
        assert!(health.observations.is_empty(), "{health:?}");
    }

    /// Finding 4: the same row reaching two supervisors counts once.
    #[test]
    fn an_already_seen_row_id_is_ignored() {
        let policy = policy();
        let row = err(ProviderErrorClass::Transport, 1_000);
        let (once, _) = observe(&RouteHealth::default(), &row, 1_000, &policy);
        let (twice, transition) = observe(&once, &row, 1_001, &policy);
        assert_eq!(once, twice);
        assert_eq!(transition, None);
        assert_eq!(once.observations.len(), 1);
    }

    #[test]
    fn an_open_breaker_becomes_a_trial_once_its_cooldown_elapses() {
        let policy = policy();
        let (health, _) = observe_many(
            &[
                err(ProviderErrorClass::Server, 1_000),
                err(ProviderErrorClass::Server, 1_001),
                err(ProviderErrorClass::Server, 1_002),
            ],
            1_002,
            &policy,
        );
        assert!(matches!(
            admission(&health, 1_100, &policy),
            Admission::Deny { .. }
        ));
        assert_eq!(admission(&health, 1_002 + 300, &policy), Admission::Trial);
    }

    #[test]
    fn a_success_in_half_open_recovers_the_route() {
        let health = RouteHealth {
            phase: Phase::HalfOpen {
                since: 2_000,
                trial: None,
            },
            observations: vec![Observation {
                class: ProviderErrorClass::Transport,
                at: 1_900,
            }],
            seen_ids: vec!["row-1900".to_string()],
            ..RouteHealth::default()
        };
        let (next, transition) = record_success(&health, 2_010, &policy());
        assert!(next.phase.is_healthy(), "{next:?}");
        assert!(next.observations.is_empty(), "{next:?}");
        assert_eq!(
            next.seen_ids,
            vec!["row-1900".to_string()],
            "a heal clears the phase, never the seen-id ring"
        );
        assert!(matches!(transition, Some(Transition::Recovered(_))));
        assert_eq!(admission(&next, 2_010, &policy()), Admission::Allow);
    }

    #[test]
    fn an_auth_error_marks_the_route_unavailable_immediately() {
        let policy = policy();
        let (health, transition) = observe(
            &RouteHealth::default(),
            &err(ProviderErrorClass::Auth, 500),
            500,
            &policy,
        );
        assert!(
            matches!(health.phase, Phase::Unavailable { .. }),
            "{health:?}"
        );
        assert!(matches!(transition, Some(Transition::MarkedUnavailable(_))));
        assert!(admission(&health, 500, &policy).denied().is_some());
        // A second auth error is not a second transition.
        let (_, again) = observe(&health, &err(ProviderErrorClass::Auth, 600), 600, &policy);
        assert_eq!(again, None);
    }

    /// Finding 1: `Unavailable` used to be terminal, healable only by a
    /// success it also refused to admit -- a permanent deny. It now
    /// re-probes on the same cooldown `Open` does.
    #[test]
    fn an_unavailable_route_re_probes_after_the_cooldown_and_heals() {
        let policy = policy();
        let (health, _) = observe(
            &RouteHealth::default(),
            &err(ProviderErrorClass::Auth, 500),
            500,
            &policy,
        );
        assert!(admission(&health, 500 + 299, &policy).denied().is_some());
        assert_eq!(admission(&health, 500 + 300, &policy), Admission::Trial);

        let (healed, transition) = record_success(&health, 500 + 300, &policy);
        assert!(healed.phase.is_healthy(), "{healed:?}");
        assert!(matches!(transition, Some(Transition::Recovered(_))));
    }

    #[test]
    fn rate_limit_overflow_and_other_never_change_the_phase() {
        let policy = policy();
        for class in [
            ProviderErrorClass::RateLimit,
            ProviderErrorClass::Overflow,
            ProviderErrorClass::Other,
        ] {
            let (health, transition) = observe_many(
                &[err(class, 1_000), err(class, 1_010), err(class, 1_020)],
                1_020,
                &policy,
            );
            assert!(
                health.phase.is_healthy(),
                "{class:?} changed the phase: {health:?}"
            );
            assert_eq!(transition, None, "{class:?} produced a transition");
            assert_eq!(admission(&health, 1_020, &policy), Admission::Allow);
        }
    }

    #[test]
    fn a_disabled_policy_always_allows_and_never_transitions() {
        let policy = HealthPolicy {
            enabled: false,
            ..HealthPolicy::default()
        };
        let (health, transition) = observe_many(
            &[
                err(ProviderErrorClass::Transport, 1_000),
                err(ProviderErrorClass::Transport, 1_001),
                err(ProviderErrorClass::Transport, 1_002),
                err(ProviderErrorClass::Auth, 1_003),
            ],
            1_003,
            &policy,
        );
        assert!(health.phase.is_healthy(), "{health:?}");
        assert_eq!(transition, None);
        assert_eq!(admission(&health, 1_003, &policy), Admission::Allow);
        // A record already open on disk is still allowed while disabled.
        let open = RouteHealth {
            phase: Phase::Open {
                opened_at: 1,
                until: u64::MAX,
                reason: "stored".to_string(),
                class: ProviderErrorClass::Transport,
            },
            ..RouteHealth::default()
        };
        assert_eq!(admission(&open, 2, &policy), Admission::Allow);
    }

    #[test]
    fn the_observation_and_id_rings_are_bounded() {
        let policy = HealthPolicy {
            open_after_failures: u32::MAX,
            ..HealthPolicy::default()
        };
        let mut health = RouteHealth::default();
        for i in 0..(MAX_SAMPLES as u64 * 3) {
            for class in [ProviderErrorClass::Transport, ProviderErrorClass::Other] {
                let (next, _) = observe(&health, &err(class, 1_000 + i), 1_000 + i, &policy);
                health = next;
            }
        }
        assert_eq!(health.observations.len(), MAX_OBSERVATIONS);
        // Finding 8: the counting ring is bounded by the FAILURE ring's cap,
        // which is larger than the observation ring's -- forgetting a row id
        // whose TIME is still retained let a full re-parse recount it.
        assert_eq!(health.seen_ids.len(), MAX_SAMPLES);
        // R2: the non-counting classes are bounded separately, so they can
        // never evict a retained failure's id.
        assert_eq!(health.seen_other_ids.len(), MAX_OBSERVATIONS);
    }

    #[test]
    fn a_route_key_is_the_harness_alone_and_file_safe() {
        let key = RouteKey::new("Claude");
        assert_eq!(key.harness, "claude");
        assert_eq!(key.file_stem(), "claude");
        assert_eq!(key.label(), "claude");
        assert_eq!(RouteKey::new("gpt/6 astra").file_stem(), "gpt-6-astra");
    }
    /// Review round 2, finding 1: a dated Auth row from outside the window
    /// is history, not a live credential problem. Before this it denied the
    /// harness at the current clock, healed after the cooldown, and was
    /// marked again by the very next fresh-scorer iteration -- a route
    /// flapping every cycle on one long-fixed error.
    #[test]
    fn an_auth_row_older_than_the_window_changes_nothing() {
        let policy = policy();
        let stale = Observed::new(
            ProviderErrorClass::Auth,
            Some(1_000),
            Some("old-auth".to_string()),
        );
        // A week later, which is what a full re-parse hands over.
        let now = 1_000 + 7 * 86_400;
        let (health, transition) = observe(&RouteHealth::default(), &stale, now, &policy);
        assert!(health.phase.is_healthy(), "{health:?}");
        assert_eq!(transition, None);
        assert_eq!(admission(&health, now, &policy), Admission::Allow);

        // The same row inside the window still denies at once.
        let fresh = Observed::new(
            ProviderErrorClass::Auth,
            Some(now - 10),
            Some("new-auth".to_string()),
        );
        let (denied, transition) = observe(&RouteHealth::default(), &fresh, now, &policy);
        assert!(
            matches!(denied.phase, Phase::Unavailable { .. }),
            "{denied:?}"
        );
        assert!(matches!(transition, Some(Transition::MarkedUnavailable(_))));

        // An UNDATED row is still marked: the caller vets those (only an
        // incremental poll may hand one over), exactly as for Transport.
        let undated = Observed::new(ProviderErrorClass::Auth, None, None);
        let (marked, _) = observe(&RouteHealth::default(), &undated, now, &policy);
        assert!(
            matches!(marked.phase, Phase::Unavailable { .. }),
            "{marked:?}"
        );
    }

    /// Finding 1(b): the other half of that flap. A heal used to return
    /// `RouteHealth::default()`, wiping the seen-id ring, so the rows just
    /// folded in became eligible again and the next poll re-observed them.
    #[test]
    fn a_heal_keeps_rejecting_the_rows_it_already_folded_in() {
        let policy = policy();
        let rows = [
            err(ProviderErrorClass::Transport, 1_000),
            err(ProviderErrorClass::Transport, 1_001),
            err(ProviderErrorClass::Transport, 1_002),
        ];
        let (open, _) = observe_many(&rows, 1_002, &policy);
        assert!(matches!(open.phase, Phase::Open { .. }), "{open:?}");

        let (healed, _) = record_success(&open, 1_002 + 300, &policy);
        assert!(healed.phase.is_healthy(), "{healed:?}");

        // Replay every row a fresh scorer would hand over again.
        let mut replayed = healed.clone();
        for observed in &rows {
            let (next, transition) = observe(&replayed, observed, 1_002 + 301, &policy);
            assert_eq!(
                transition, None,
                "{observed:?} was re-observed after a heal"
            );
            replayed = next;
        }
        assert!(replayed.phase.is_healthy(), "{replayed:?}");
        assert!(replayed.observations.is_empty(), "{replayed:?}");
    }
}
