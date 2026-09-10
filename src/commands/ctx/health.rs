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
    Open {
        opened_at: u64,
        until: u64,
        reason: String,
    },
    HalfOpen {
        since: u64,
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
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            open_after_failures: 3,
            window_secs: 600,
            cooldown_secs: 300,
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
}

impl Transition {
    /// The `log::Decision::verdict` this transition is recorded under.
    pub fn verdict(&self) -> &'static str {
        match self {
            Self::Opened(_) => "health-open",
            Self::HalfOpened(_) => "health-half-open",
            Self::Recovered(_) => "health-recovered",
            Self::MarkedUnavailable(_) => "health-unavailable",
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Opened(reason)
            | Self::HalfOpened(reason)
            | Self::Recovered(reason)
            | Self::MarkedUnavailable(reason) => reason,
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
            phase: Phase::HalfOpen { since: now },
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
        && health.seen_ids.iter().any(|seen| seen == id)
    {
        return (health.clone(), None);
    }
    let class = observed.class;
    let at = observed.at.unwrap_or(now);
    let (promoted, _) = promote(health, now, policy);
    let mut next = RouteHealth {
        phase: promoted.phase.clone(),
        observations: prune(promoted.observations.clone(), now, policy),
        seen_ids: promoted.seen_ids.clone(),
    };
    next.observations.push(Observation { class, at });
    next.observations = prune(next.observations, now, policy);
    if let Some(id) = &observed.id {
        next.seen_ids.push(id.clone());
        if next.seen_ids.len() > MAX_OBSERVATIONS {
            let drop = next.seen_ids.len() - MAX_OBSERVATIONS;
            next.seen_ids.drain(..drop);
        }
    }

    if !policy.enabled {
        return (next, None);
    }

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
        next.phase = if failures == 0 {
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
        return (next, None);
    }

    let reason = open_reason(failures, class, policy);
    next.phase = Phase::Open {
        opened_at: now,
        until: now.saturating_add(policy.cooldown_secs),
        reason: reason.clone(),
    };
    (next, Some(Transition::Opened(reason)))
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
            },
            Some(Transition::Recovered(reason)),
        )
    };
    match &promoted.phase {
        Phase::Healthy => (promoted, None),
        Phase::Open { .. } => (promoted, None),
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
        Phase::HalfOpen { .. } => Admission::Trial,
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
            phase: Phase::HalfOpen { since: 2_000 },
            observations: vec![Observation {
                class: ProviderErrorClass::Transport,
                at: 1_900,
            }],
            seen_ids: vec!["row-1900".to_string()],
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
        for i in 0..(MAX_OBSERVATIONS as u64 * 3) {
            let (next, _) = observe(
                &health,
                &err(ProviderErrorClass::Other, 1_000 + i),
                1_000 + i,
                &policy,
            );
            health = next;
        }
        assert_eq!(health.observations.len(), MAX_OBSERVATIONS);
        assert_eq!(health.seen_ids.len(), MAX_OBSERVATIONS);
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
