//! Issue #358 (task 5): the automatic meta-orchestrator rollover driver.
//!
//! Task 4 (`seat.rs`) owns the seat record and the pure decision; task 2
//! (`allocator.rs`) owns the pure capacity plan; `fallback.rs` owns the only
//! I/O that builds a snapshot. This module is the thin layer that joins them
//! to the two live-swap seams issue #84 already established
//! (`wrap::perform_handover_swap`, `dash::pane::Pane::handover`): it decides,
//! opens the seat transaction, and hands the supervisors an ordinary
//! [`handover::HandoverRequest`] so an automatic rollover travels the exact
//! same code path an operator's own `zirv ctx handover` does. Nothing here
//! spawns a process or touches a pty.
//!
//! Everything this module logs goes to the ordinary decision log as a
//! [`PoolEvent`] payload -- capacity numbers, harness names and a reason,
//! never handoff text or transcript content: the decision log is a rotation
//! log an operator reads during an incident, not a second transcript (see
//! `log::SafetyDecision`'s own doc comment for the same rule applied to
//! command policy).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::adapters;
use super::allocator::{self, HarnessState};
use super::config::CtxConfig;
use super::fallback;
use super::handover::{self, HandoverRequest};
use super::log;
use super::pace;
use super::seat;
use super::state::StateDir;

/// A rollover transaction was opened: the successor is chosen and a
/// generation reserved, but nothing has been swapped yet.
pub const PREPARED: &str = "orchestrator-rollover-prepared";
/// The successor proved itself ready and now holds the seat.
pub const COMMITTED: &str = "orchestrator-rollover-committed";
/// A prepared rollover could not be completed; the seat kept its generation.
pub const FAILED: &str = "orchestrator-rollover-failed";
/// A rollover was decided against for a reason that will not resolve by
/// re-asking with the same evidence.
pub const REFUSED: &str = "orchestrator-rollover-refused";
/// No harness can take the seat right now; it is parked until a reset.
pub const EXHAUSTED: &str = "all-capacity-exhausted";
/// A parked seat's window elapsed and the seat is live again.
pub const RESUMED: &str = "capacity-resumed";
/// A harness in `fallback.order` entered [`HarnessState::Draining`].
pub const DRAINING: &str = "harness-draining";
/// A delegation was placed on a harness other than the requested one.
pub const DELEGATION_ROUTED: &str = "delegation-routed";
/// The ranked set of harnesses this seat could roll onto changed.
pub const PLAN_CHANGED: &str = "allocation-plan-changed";

/// The `Decision::verdict` family each action belongs to -- the same
/// coarse grouping `agent.rs`'s own `verdict: "reroute"` precedent uses, so
/// an operator grepping the decision log by verdict finds a whole family at
/// once rather than one action at a time.
fn verdict_for(action: &str) -> &'static str {
    match action {
        PREPARED | COMMITTED | FAILED | REFUSED => "rollover",
        EXHAUSTED | RESUMED | DRAINING => "capacity",
        DELEGATION_ROUTED => "reroute",
        _ => "allocation",
    }
}

/// One capacity-pool decision's evidence, serialized as a `Decision::detail`
/// payload. Numbers and harness names only: no handoff text, no transcript
/// content, nothing an operator would not want in a rotation log.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PoolEvent {
    pub snapshot_at: u64,
    pub binding_window: Option<String>,
    pub source_agent: String,
    pub source_headroom_pct: Option<f64>,
    pub target_agent: Option<String>,
    pub target_headroom_pct: Option<f64>,
    pub reserved_tokens: u64,
    pub task_budget: Option<u64>,
    pub tie_break: String,
    pub generation: Option<u64>,
    pub reason: String,
}

/// Appends one [`PoolEvent`] to the ordinary decision log. Best-effort, like
/// every other `log::append` call site: a pool decision that cannot be
/// recorded must never stop the decision itself from being carried out.
pub fn record(state: &StateDir, session: &str, verb: &str, action: &str, event: &PoolEvent) {
    let detail = serde_json::to_string(event).unwrap_or_else(|_| event.reason.clone());
    let _ = log::append(
        state,
        &log::Decision {
            ts: super::state::now_secs(),
            session,
            verb,
            verdict: verdict_for(action),
            score: 0,
            action,
            detail: &detail,
            observed_at: Some(event.snapshot_at),
        },
    );
}

/// What one [`evaluate`] call decided.
#[derive(Debug)]
pub enum Evaluation {
    /// Nothing to do, for a reason that is not itself a trigger.
    Skip(String),
    /// A trigger fired but this is not the moment to act on it (mid-turn, a
    /// cooldown). The cause is recorded on the seat via `seat::mark_pending`.
    Pending(seat::Cause),
    /// A rollover transaction is open: hand `request` to the same live-swap
    /// seam a manual `zirv ctx handover` uses, then `commit`/`fail` this
    /// module's own bookkeeping against `generation`.
    Rollover {
        request: HandoverRequest,
        generation: u64,
        cause: seat::Cause,
        candidates_tried: Vec<String>,
    },
    /// Nothing can take the seat; wait out `window` and call [`on_resume`].
    Park {
        until: u64,
        window: String,
        reason: String,
    },
}

impl Evaluation {
    /// One operator-facing line naming everything this verdict decided --
    /// what the supervisors write to the decision log whenever an
    /// evaluation was anything other than a plain `Skip`, so "why did my
    /// seat move / not move" is answerable after the fact from the log
    /// alone.
    pub fn summary(&self) -> String {
        match self {
            Self::Skip(reason) => format!("no rollover: {reason}"),
            Self::Pending(cause) => format!("rollover trigger seen but deferred: {cause:?}"),
            Self::Rollover {
                request,
                generation,
                cause,
                candidates_tried,
            } => format!(
                "rollover to {} (generation {generation}, {cause:?}); candidates considered: {}",
                request.target_agent,
                candidates_tried.join(", ")
            ),
            Self::Park {
                until,
                window,
                reason,
            } => format!("seat parked until unix {until} ({window}): {reason}"),
        }
    }
}

/// Drops everything this module persists for `seat_short` -- called when the
/// session at that address ends, since a seat outlives nothing: a short id
/// is derived from a session id, and a new launch gets a new one.
pub fn forget(state: &StateDir, seat_short: &str) {
    seat::remove(state, seat_short);
    let _ = std::fs::remove_file(pool_state_path(state, seat_short));
}

/// The persisted "what did this seat last see" record, purely so the pool
/// events below are edge-triggered rather than repeated on every tick: a
/// supervisor evaluates once per collector max-age, forever, and a harness
/// that stays `Draining` for an hour must not write an hour of identical log
/// lines. Tolerant-read like every other state-dir record (a missing or
/// malformed file is an empty history, never an error).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct PoolState {
    #[serde(default)]
    draining: Vec<String>,
    #[serde(default)]
    plan: Vec<String>,
}

fn pool_state_path(state: &StateDir, seat_short: &str) -> std::path::PathBuf {
    state
        .sessions()
        .join(format!("{seat_short}.pool-state.json"))
}

fn load_pool_state(state: &StateDir, seat_short: &str) -> PoolState {
    std::fs::read_to_string(pool_state_path(state, seat_short))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store_pool_state(state: &StateDir, seat_short: &str, pool: &PoolState) {
    let _ = super::state::create_private_dir_all(&state.sessions());
    if let Ok(json) = serde_json::to_string(pool) {
        let _ = super::state::write_private(&pool_state_path(state, seat_short), &json);
    }
}

/// Whether a freshly launched successor has earned the seat yet. Pure, so
/// both supervisors decide identically and the decision is testable without
/// a pty on either platform: `signal_seen` is "a turn signal arrived since
/// the swap" for a turn-signal-capable adapter, `quiescent` is
/// `wrap::signal_less_mail_ready`/`dash::pane::signal_less_quiescent`'s own
/// answer for one that reports no turns at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Waiting,
    TimedOut,
    Dead,
}

pub fn successor_readiness(
    alive: bool,
    turn_signal_capable: bool,
    signal_seen: bool,
    quiescent: bool,
    elapsed: Duration,
    timeout: Duration,
) -> Readiness {
    if !alive {
        return Readiness::Dead;
    }
    let ready = if turn_signal_capable {
        signal_seen
    } else {
        quiescent
    };
    if ready {
        Readiness::Ready
    } else if elapsed >= timeout {
        Readiness::TimedOut
    } else {
        Readiness::Waiting
    }
}

/// The reactive half of the trigger: a vendor block this supervisor can
/// actually corroborate against the provider's structured usage reading.
/// `None` -- never a rollover -- for an unconfirmed reading, which is
/// exactly `pace::confirm_limit_hit`'s own contract (a loose text match, a
/// stale collector and a missing reading all fail to confirm).
pub fn confirmed_block(
    state: &StateDir,
    cfg: &CtxConfig,
    now: u64,
    provider: &str,
) -> Option<String> {
    match pace::confirm_limit_hit(state, &cfg.pace, now, provider) {
        pace::LimitConfirmation::Confirmed { detail } => Some(detail),
        pace::LimitConfirmation::Unconfirmed { .. } => None,
    }
}

/// How often a supervisor should re-evaluate: the collector's own max age,
/// floored at a minute. Evaluating faster than the usage collector refreshes
/// can only ever re-read the same numbers.
pub fn evaluate_interval(cfg: &CtxConfig) -> Duration {
    Duration::from_secs(cfg.pace.collector_max_age_secs.max(60))
}

/// Decides whether `seat_short`'s orchestrator seat should roll onto another
/// harness right now, and opens the transaction when it should.
///
/// `idle` is the supervisor's own verified-idle answer (`wrap::
/// handover_may_act`, `dash::pane::Pane::state() == Idle`) -- never inferred
/// here. `confirmed_block` is [`confirmed_block`]'s output: `Some` takes the
/// reactive path (issue #358 review, finding #14: it skips `rollover_
/// cooldown_secs`, but it never skips the idle boundary -- a hard block is
/// real evidence, but a turn this session has not yielded from is not
/// interrupted just because the account is blocked elsewhere; a mid-turn
/// reactive trigger is instead recorded as `Evaluation::Pending` and retried
/// once idle). `interactive` is the seat's own launch interactivity, carried
/// onto the request so the successor's permission posture matches the
/// predecessor's (`handover::resolve_swap_launch`).
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    state: &StateDir,
    cfg: &CtxConfig,
    verb: &str,
    seat_short: &str,
    now: u64,
    idle: bool,
    confirmed_block: Option<String>,
    interactive: bool,
) -> Evaluation {
    let Some(current) = seat::load(state, seat_short) else {
        return Evaluation::Skip(format!("no seat record for {seat_short}"));
    };
    if !cfg.fallback.enabled {
        return Evaluation::Skip("cross-harness fallback is disabled".to_string());
    }
    if !cfg.fallback.auto_orchestrator_rollover {
        return Evaluation::Skip("automatic orchestrator rollover is disabled".to_string());
    }
    if current.pinned {
        return Evaluation::Skip("seat is pinned".to_string());
    }
    if !matches!(current.phase, seat::Phase::Idle) {
        return Evaluation::Skip(format!("seat is not idle ({:?})", current.phase));
    }
    if cfg
        .fallback
        .order
        .iter()
        .filter(|name| cfg.agents.is_enabled(name))
        .count()
        < 2
    {
        return Evaluation::Skip(
            "fewer than two enabled harnesses; there is nowhere to roll over to".to_string(),
        );
    }

    let snapshot = fallback::capacity_snapshot(
        state,
        cfg,
        now,
        Some(current.session.as_str()),
        Some(current.agent.as_str()),
    );

    let source_provider = snapshot
        .harness(&current.agent)
        .map(|harness| harness.provider.clone())
        .unwrap_or_else(|| adapters::provider_for_agent_name(Some(&current.agent)).to_string());
    let source = snapshot.provider(&source_provider);
    // A stale reading, an `overage_covered` window and a provider with no
    // binding window at all are all "unknown" for this decision, never a
    // trigger: issue #358's own rule 5, and the same "never migrate on
    // missing data" convention `seat::decide` documents for itself.
    let fresh = source
        .and_then(|provider| provider.binding.and_then(|i| provider.windows.get(i)))
        .filter(|window| !window.stale && !window.overage_covered);
    let source_headroom_pct =
        fresh.and_then(|_| source.and_then(|p| allocator::projected_headroom(p, cfg, 0)));
    let source_observed_at = fresh.map(|window| window.observed_at).unwrap_or(now);
    let source_hard_blocked =
        confirmed_block.is_some() || (fresh.is_some() && source.is_some_and(|p| p.hard_refused));

    let mut candidates: Vec<seat::CandidateHeadroom> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for harness in &snapshot.harnesses {
        if harness.name.eq_ignore_ascii_case(&current.agent)
            || !cfg
                .fallback
                .order
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&harness.name))
        {
            continue;
        }
        if !matches!(harness.state, HarnessState::Ready | HarnessState::Unknown) {
            dropped.push(format!("{}: {}", harness.name, harness.state_reason));
            continue;
        }
        let Some(provider) = snapshot.provider(&harness.provider) else {
            dropped.push(format!("{}: no provider capacity", harness.name));
            continue;
        };
        let assumed = harness.state == HarnessState::Unknown;
        let projected_headroom_pct = if assumed {
            cfg.fallback.unknown_headroom_pct
        } else {
            allocator::projected_headroom(provider, cfg, 0).unwrap_or(0.0)
        };
        let Some(model) = handover::equivalent_model(
            &current.agent,
            current.model.as_deref(),
            current.model.is_some(),
            &harness.name,
            cfg,
        ) else {
            dropped.push(format!("{}: no equivalent model tier", harness.name));
            continue;
        };
        candidates.push(seat::CandidateHeadroom {
            agent: harness.name.clone(),
            model: Some(model),
            projected_headroom_pct,
            assumed,
            observed_at: provider
                .binding
                .and_then(|i| provider.windows.get(i))
                .map(|window| window.observed_at)
                .unwrap_or(now),
        });
    }

    let binding_window = fresh.map(|window| window.window.clone());
    let reserved_tokens = source.map(|provider| provider.reserved_tokens).unwrap_or(0);
    let base = PoolEvent {
        snapshot_at: snapshot.taken_at,
        binding_window,
        source_agent: current.agent.clone(),
        source_headroom_pct,
        reserved_tokens,
        ..PoolEvent::default()
    };
    note_pool_shape(state, cfg, verb, seat_short, &current, &snapshot, &base);

    let candidates_tried: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.agent.clone())
        .collect();
    let tie_break = if candidates.len() > 1 {
        "greatest projected headroom; ties by fallback.order position".to_string()
    } else {
        "sole eligible candidate".to_string()
    };

    let inputs = seat::RolloverInputs {
        seat: &current,
        now,
        source_headroom_pct,
        source_observed_at,
        source_hard_blocked,
        auto_enabled: cfg.fallback.auto_orchestrator_rollover,
        idle,
        candidates: &candidates,
    };
    let trigger_cause = if source_hard_blocked {
        Some(seat::Cause::Reactive {
            detail: confirmed_block
                .clone()
                .unwrap_or_else(|| "provider hard-refused a fresh reading".to_string()),
            observed_at: source_observed_at,
        })
    } else {
        source_headroom_pct
            .filter(|pct| *pct <= cfg.fallback.rollover_headroom_pct())
            .map(|headroom_pct| seat::Cause::Proactive {
                headroom_pct,
                observed_at: source_observed_at,
            })
    };

    match seat::decide(&inputs, cfg) {
        seat::RolloverDecision::Proceed {
            agent,
            model,
            cause,
        } => {
            let reactive = matches!(cause, seat::Cause::Reactive { .. });
            match seat::prepare(
                state,
                seat_short,
                &agent,
                model.as_deref(),
                cause.clone(),
                now,
            ) {
                Ok(generation) => {
                    record(
                        state,
                        &current.session,
                        verb,
                        PREPARED,
                        &PoolEvent {
                            target_agent: Some(agent.clone()),
                            target_headroom_pct: candidates
                                .iter()
                                .find(|candidate| candidate.agent == agent)
                                .map(|candidate| candidate.projected_headroom_pct),
                            tie_break,
                            generation: Some(generation),
                            reason: if reactive {
                                "source harness is hard-blocked".to_string()
                            } else {
                                "source harness is at or below the rollover headroom threshold"
                                    .to_string()
                            },
                            ..base.clone()
                        },
                    );
                    Evaluation::Rollover {
                        request: HandoverRequest {
                            target_agent: agent,
                            target_model: model,
                            // A hard-blocked seat cannot wait for a clean
                            // turn boundary that may never come; a proactive
                            // one already waited for `idle` above.
                            force: reactive,
                            requested_at: now,
                            interactive,
                            automatic: true,
                            generation: Some(generation),
                            // A blocked vendor cannot answer a distiller
                            // call either, so the reactive path never spends
                            // one: the structural packet is what actually
                            // survives an exhausted provider.
                            structural_only: reactive,
                        },
                        generation,
                        cause,
                        candidates_tried,
                    }
                }
                Err(e) => {
                    record(
                        state,
                        &current.session,
                        verb,
                        REFUSED,
                        &PoolEvent {
                            target_agent: Some(agent),
                            reason: e.to_string(),
                            ..base
                        },
                    );
                    Evaluation::Skip(e.to_string())
                }
            }
        }
        seat::RolloverDecision::Wait(reason) => match trigger_cause {
            Some(cause) => {
                let _ = seat::mark_pending(state, seat_short, cause.clone(), now);
                Evaluation::Pending(cause)
            }
            None => Evaluation::Skip(reason),
        },
        seat::RolloverDecision::Refuse(reason) => {
            if !source_hard_blocked {
                record(
                    state,
                    &current.session,
                    verb,
                    REFUSED,
                    &PoolEvent {
                        reason: reason.clone(),
                        ..base
                    },
                );
                return Evaluation::Skip(reason);
            }
            park_for_reset(state, cfg, verb, seat_short, &current, now, &reason, base)
        }
    }
}

/// The reactive dead end: the seat is blocked and nothing can take it. Parks
/// the seat until the earliest window any admissible harness resets, so the
/// supervisor waits (keeping supervision, mail and workers alive) rather
/// than burning restart budget on a harness that cannot answer.
#[allow(clippy::too_many_arguments)]
fn park_for_reset(
    state: &StateDir,
    cfg: &CtxConfig,
    verb: &str,
    seat_short: &str,
    current: &seat::Seat,
    now: u64,
    reason: &str,
    base: PoolEvent,
) -> Evaluation {
    let visited: Vec<String> = current
        .visited
        .iter()
        .map(|visit| visit.agent.clone())
        .collect();
    let Some(choice) = fallback::earliest_reset_choice(
        state,
        cfg,
        fallback::RouteRequest {
            requested: &current.agent,
            source_model: current.model.as_deref(),
            source_model_explicit: current.model.is_some(),
            delegation: false,
            bounds: fallback::TaskBounds {
                tokens: None,
                tool_calls: None,
            },
            now,
            exclude: None,
        },
        &visited,
    ) else {
        record(
            state,
            &current.session,
            verb,
            REFUSED,
            &PoolEvent {
                reason: format!("{reason}; no reset time is known for any harness"),
                ..base
            },
        );
        return Evaluation::Skip(reason.to_string());
    };

    let window = choice.window.to_string();
    let detail = format!("{reason}; {}", choice.detail());
    if let Err(e) = seat::park(state, seat_short, choice.reset_at, &window, &detail, now) {
        return Evaluation::Skip(e.to_string());
    }
    record(
        state,
        &current.session,
        verb,
        EXHAUSTED,
        &PoolEvent {
            target_agent: Some(choice.selected.clone()),
            reason: detail.clone(),
            ..base
        },
    );
    Evaluation::Park {
        until: choice.reset_at,
        window,
        reason: detail,
    }
}

/// Edge-triggered pool telemetry: which harnesses in `fallback.order` are
/// draining right now, and what the ranked set of harnesses this seat could
/// move onto looks like. Both are written only when they differ from what
/// this seat last saw (`PoolState`), so a steady state logs nothing.
fn note_pool_shape(
    state: &StateDir,
    cfg: &CtxConfig,
    verb: &str,
    seat_short: &str,
    current: &seat::Seat,
    snapshot: &allocator::CapacitySnapshot,
    base: &PoolEvent,
) {
    let mut draining: Vec<String> = snapshot
        .harnesses
        .iter()
        .filter(|harness| {
            harness.state == HarnessState::Draining
                && cfg
                    .fallback
                    .order
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&harness.name))
        })
        .map(|harness| harness.name.clone())
        .collect();
    draining.sort();
    let mut plan: Vec<String> = snapshot
        .harnesses
        .iter()
        .filter(|harness| harness.state == HarnessState::Ready)
        .map(|harness| harness.name.clone())
        .collect();
    plan.sort();

    let previous = load_pool_state(state, seat_short);
    if previous.draining == draining && previous.plan == plan {
        return;
    }
    for name in draining
        .iter()
        .filter(|name| !previous.draining.contains(name))
    {
        let reason = snapshot
            .harness(name)
            .map(|harness| harness.state_reason.clone())
            .unwrap_or_else(|| "draining".to_string());
        record(
            state,
            &current.session,
            verb,
            DRAINING,
            &PoolEvent {
                target_agent: Some(name.clone()),
                reason,
                ..base.clone()
            },
        );
    }
    if previous.plan != plan {
        record(
            state,
            &current.session,
            verb,
            PLAN_CHANGED,
            &PoolEvent {
                reason: format!("ready harnesses: {}", plan.join(", ")),
                ..base.clone()
            },
        );
    }
    store_pool_state(state, seat_short, &PoolState { draining, plan });
}

/// Records one delegation reroute (`fallback::Route`) as a pool event, from
/// the two existing reroute sites (`agent.rs`, `dash::mod::spawn authority`)
/// so cross-harness delegation shows up in the same capacity story as an
/// orchestrator rollover.
pub fn record_route(
    state: &StateDir,
    session: &str,
    verb: &str,
    now: u64,
    route: &fallback::Route,
    task_budget: Option<u64>,
) {
    record(
        state,
        session,
        verb,
        DELEGATION_ROUTED,
        &PoolEvent {
            snapshot_at: route.requested_observed_at.unwrap_or(now),
            binding_window: route.binding_window.clone(),
            source_agent: route.requested.clone(),
            source_headroom_pct: route.requested_headroom_pct,
            target_agent: Some(route.selected.clone()),
            target_headroom_pct: Some(route.selected_headroom_pct),
            reserved_tokens: route.reserved_tokens,
            task_budget,
            tie_break: if route.selected_headroom_assumed {
                "assumed headroom for an unknown reading".to_string()
            } else {
                "greatest projected headroom; ties by fallback.order position".to_string()
            },
            generation: None,
            reason: route.reason.label().to_string(),
        },
    );
}

/// Commits a prepared rollover once the successor has proven itself ready --
/// the supervisors' half of [`Evaluation::Rollover`].
pub fn commit(
    state: &StateDir,
    verb: &str,
    seat_short: &str,
    generation: u64,
    successor_session: &str,
    now: u64,
) -> CtxResult<seat::Seat> {
    let committed = seat::commit(state, seat_short, generation, successor_session, now)?;
    record(
        state,
        &committed.session,
        verb,
        COMMITTED,
        &PoolEvent {
            snapshot_at: now,
            source_agent: committed.agent.clone(),
            target_agent: Some(committed.agent.clone()),
            generation: Some(committed.generation),
            reason: "successor is alive and answering".to_string(),
            ..PoolEvent::default()
        },
    );
    Ok(committed)
}

/// Aborts a prepared rollover: the successor never took the seat, and
/// `seat::abort` records a visit against it so [`evaluate`]'s next call at
/// the same epoch tries the NEXT candidate rather than this one again.
pub fn fail(
    state: &StateDir,
    verb: &str,
    seat_short: &str,
    generation: u64,
    reason: &str,
    now: u64,
) -> CtxResult<seat::Seat> {
    let aborted = seat::abort(state, seat_short, generation, now)?;
    record(
        state,
        &aborted.session,
        verb,
        FAILED,
        &PoolEvent {
            snapshot_at: now,
            source_agent: aborted.agent.clone(),
            generation: Some(generation),
            reason: reason.to_string(),
            ..PoolEvent::default()
        },
    );
    Ok(aborted)
}

/// A parked seat's window has elapsed. Returns the seat to `Idle` and, when
/// the best eligible harness is no longer the one the seat is sitting on,
/// opens a fresh rollover transaction onto it -- otherwise the seat simply
/// resumes where it is.
pub fn on_resume(
    state: &StateDir,
    cfg: &CtxConfig,
    verb: &str,
    seat_short: &str,
    now: u64,
    interactive: bool,
) -> Option<HandoverRequest> {
    let current = seat::load(state, seat_short)?;
    let seat::Phase::Parked { until, window, .. } = current.phase.clone() else {
        return None;
    };
    if until > now {
        return None;
    }

    let snapshot = fallback::capacity_snapshot(
        state,
        cfg,
        now,
        Some(current.session.as_str()),
        Some(current.agent.as_str()),
    );
    let unit = allocator::WorkUnit {
        id: "orchestrator-seat".to_string(),
        requested: current.agent.clone(),
        bounds: fallback::TaskBounds {
            tokens: None,
            tool_calls: None,
        },
        expected_tokens: 0,
        needs_tool_call_counting: false,
        source_model: current.model.clone(),
        source_model_explicit: current.model.is_some(),
        delegation: false,
    };
    let models = |name: &str| {
        handover::equivalent_model(
            &current.agent,
            current.model.as_deref(),
            current.model.is_some(),
            name,
            cfg,
        )
    };
    let placement = allocator::place(&snapshot, cfg, &unit, &[], &models);

    let _ = seat::resume(state, seat_short, now);
    let base = PoolEvent {
        snapshot_at: snapshot.taken_at,
        binding_window: Some(window),
        source_agent: current.agent.clone(),
        target_agent: placement
            .selected
            .as_ref()
            .map(|candidate| candidate.name.clone()),
        target_headroom_pct: placement
            .selected
            .as_ref()
            .map(|candidate| candidate.projected_headroom_pct),
        reason: "parked window elapsed".to_string(),
        ..PoolEvent::default()
    };
    record(state, &current.session, verb, RESUMED, &base);

    let candidate = placement
        .selected
        .filter(|candidate| !candidate.name.eq_ignore_ascii_case(&current.agent))?;
    let cause = seat::Cause::Reactive {
        detail: "capacity resumed on another harness".to_string(),
        observed_at: now,
    };
    let generation = seat::prepare(
        state,
        seat_short,
        &candidate.name,
        candidate.model.as_deref(),
        cause,
        now,
    )
    .ok()?;
    record(
        state,
        &current.session,
        verb,
        PREPARED,
        &PoolEvent {
            generation: Some(generation),
            reason: "the best eligible harness is no longer the parked seat's own".to_string(),
            ..base
        },
    );
    Some(HandoverRequest {
        target_agent: candidate.name,
        target_model: candidate.model,
        force: true,
        requested_at: now,
        interactive,
        automatic: true,
        generation: Some(generation),
        structural_only: true,
    })
}

/// Crash recovery, called by each supervisor right after it registers its
/// seat: a rollover left `Prepared` by a supervisor that died mid-swap is
/// committed (the successor is alive) or aborted (it is not), so the seat
/// can never be wedged out of every future `seat::prepare`.
pub fn on_startup(
    state: &StateDir,
    seat_short: &str,
    successor_alive: &dyn Fn(u64) -> Option<String>,
) -> Option<seat::Seat> {
    let before = seat::load(state, seat_short)?;
    let seat::Phase::Prepared {
        successor_agent,
        generation,
        ..
    } = before.phase.clone()
    else {
        return None;
    };
    let now = super::state::now_secs();
    let recovered = seat::recover(state, seat_short, successor_alive, now)
        .ok()
        .flatten()?;
    let committed = recovered.generation == generation;
    record(
        state,
        &recovered.session,
        "startup",
        if committed { COMMITTED } else { FAILED },
        &PoolEvent {
            snapshot_at: now,
            source_agent: before.agent.clone(),
            target_agent: Some(successor_agent),
            generation: Some(generation),
            reason: "interrupted rollover recovered at supervisor startup".to_string(),
            ..PoolEvent::default()
        },
    );
    Some(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::window;

    const NOW: u64 = 1_700_000_000;
    const SHORT: &str = "abcd1234";
    const SESSION: &str = "session-a";

    fn temp_state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        (dir, state)
    }

    fn cfg() -> CtxConfig {
        let mut cfg = CtxConfig {
            agent_bin: Some(
                std::env::current_exe()
                    .expect("current test executable")
                    .display()
                    .to_string(),
            ),
            ..CtxConfig::default()
        };
        cfg.pace.estimator = false;
        cfg.fallback.auto_orchestrator_rollover = true;
        cfg.fallback.orchestrator_rollover_headroom_pct = Some(20.0);
        cfg.fallback.min_candidate_headroom_pct = 10.0;
        cfg
    }

    fn store_usage(state: &StateDir, provider: &str, used_pct: f64, observed_at: u64) {
        window::store_for(
            state,
            provider,
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: used_pct,
                    resets_at: NOW + 3_600,
                    observed_at,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store provider usage");
    }

    fn register_seat(state: &StateDir) {
        seat::register(
            state,
            SHORT,
            SESSION,
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            NOW,
        )
        .expect("register seat");
    }

    fn evaluate_now(
        state: &StateDir,
        cfg: &CtxConfig,
        idle: bool,
        blocked: Option<String>,
    ) -> Evaluation {
        evaluate(state, cfg, "wrap", SHORT, NOW, idle, blocked, true)
    }

    #[test]
    fn a_proactive_threshold_at_an_idle_boundary_prepares_a_rollover() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 85.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        let Evaluation::Rollover {
            request,
            generation,
            candidates_tried,
            ..
        } = evaluate_now(&state, &cfg, true, None)
        else {
            panic!("a source below the rollover threshold at an idle boundary must roll over");
        };
        assert_eq!(request.target_agent, "codex");
        assert!(request.automatic);
        assert!(
            !request.structural_only,
            "the proactive path still distills"
        );
        assert_eq!(request.generation, Some(generation));
        assert_eq!(generation, 2);
        assert!(candidates_tried.contains(&"codex".to_string()));

        let seat = seat::load(&state, SHORT).expect("seat");
        assert!(matches!(seat.phase, seat::Phase::Prepared { .. }));
    }

    #[test]
    fn a_proactive_trigger_mid_turn_is_pending_not_a_rollover() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 85.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        let Evaluation::Pending(cause) = evaluate_now(&state, &cfg, false, None) else {
            panic!("a mid-turn proactive trigger must be remembered, not acted on");
        };
        assert!(matches!(cause, seat::Cause::Proactive { .. }));
        let seat = seat::load(&state, SHORT).expect("seat");
        assert!(seat.pending.is_some());
        assert!(matches!(seat.phase, seat::Phase::Idle));
    }

    #[test]
    fn a_stale_source_reading_never_triggers_a_rollover() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        let stale = NOW - cfg.pace.collector_max_age_secs - 60;
        store_usage(&state, "anthropic", 85.0, stale);
        store_usage(&state, "openai", 5.0, NOW);

        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));
    }

    #[test]
    fn an_unconfirmed_limit_text_never_triggers_a_rollover() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        // Healthy structured readings everywhere: a supervisor that saw
        // limit-shaped text but could not corroborate it hands `None`.
        store_usage(&state, "anthropic", 10.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));
    }

    /// Finding #14 (issue #358 review) narrowed this test's own scope: the
    /// reactive path still ignores the COOLDOWN, but no longer the idle
    /// boundary -- `idle: true` here is load-bearing (see the companion
    /// `a_confirmed_block_mid_turn_is_pending_not_a_rollover` for the
    /// mid-turn case, which this test used to also cover, incorrectly, by
    /// asserting a `Rollover`).
    #[test]
    fn a_confirmed_block_rolls_over_even_inside_the_cooldown_once_idle() {
        let (_dir, state) = temp_state();
        let mut cfg = cfg();
        cfg.fallback.rollover_cooldown_secs = 86_400;
        register_seat(&state);
        // Below the hard spawn ceiling, so nothing but the caller's own
        // confirmation makes this reactive -- and inside a day-long
        // cooldown, which the proactive path would refuse.
        store_usage(&state, "anthropic", 92.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);
        let mut seat = seat::load(&state, SHORT).expect("seat");
        seat.last_rollover_at = Some(NOW - 10);
        seat::store(&state, &seat).expect("store seat");

        let Evaluation::Rollover { request, .. } = evaluate_now(
            &state,
            &cfg,
            true,
            Some("provider=anthropic, five_hour reached=true".to_string()),
        ) else {
            panic!("a confirmed block ignores the cooldown once idle");
        };
        assert_eq!(request.target_agent, "codex");
        assert!(request.force);
        assert!(
            request.structural_only,
            "a blocked vendor cannot answer a distiller call"
        );
    }

    /// Finding #14 (issue #358 review): a confirmed block must NOT roll over
    /// mid-turn. A fresh, account-wide hard-ceiling reading can report the
    /// provider blocked at any moment, entirely independent of whether THIS
    /// child ever yielded -- swapping the pty out from under a turn that has
    /// not is exactly the session-worsening move supervision may never make.
    /// It is remembered (`Evaluation::Pending`) exactly like a mid-turn
    /// proactive trigger already was, and retried once idle.
    #[test]
    fn a_confirmed_block_mid_turn_is_pending_not_a_rollover() {
        let (_dir, state) = temp_state();
        let mut cfg = cfg();
        cfg.fallback.rollover_cooldown_secs = 86_400;
        register_seat(&state);
        store_usage(&state, "anthropic", 92.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        let Evaluation::Pending(cause) = evaluate_now(
            &state,
            &cfg,
            false,
            Some("provider=anthropic, five_hour reached=true".to_string()),
        ) else {
            panic!("a mid-turn confirmed block must be remembered, not acted on");
        };
        assert!(matches!(cause, seat::Cause::Reactive { .. }));
        let seat = seat::load(&state, SHORT).expect("seat");
        assert!(seat.pending.is_some());
        assert!(matches!(seat.phase, seat::Phase::Idle));
    }

    #[test]
    fn a_candidate_already_visited_at_this_epoch_is_not_retried() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 85.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        let mut seat = seat::load(&state, SHORT).expect("seat");
        seat.visited.push(seat::Visit {
            agent: "codex".to_string(),
            epoch: NOW,
            at: NOW,
        });
        seat::store(&state, &seat).expect("store seat");

        assert!(
            matches!(evaluate_now(&state, &cfg, true, None), Evaluation::Skip(_)),
            "the only candidate was already tried against this exact evidence"
        );
    }

    #[test]
    fn a_blocked_seat_with_no_candidate_parks_until_the_earliest_reset() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 100.0, NOW);
        store_usage(&state, "openai", 100.0, NOW);

        let Evaluation::Park { until, window, .. } = evaluate_now(
            &state,
            &cfg,
            true,
            Some("provider=anthropic, five_hour reached=true".to_string()),
        ) else {
            panic!("every harness is exhausted, so the seat must park");
        };
        assert_eq!(until, NOW + 3_600);
        assert_eq!(window, "five_hour");

        let seat = seat::load(&state, SHORT).expect("seat");
        assert!(matches!(seat.phase, seat::Phase::Parked { .. }));
        let logged = std::fs::read_to_string(state.logs().join(log::LOG_FILE)).expect("log");
        assert!(logged.contains(EXHAUSTED));
    }

    #[test]
    fn a_failed_successor_returns_the_seat_to_idle_and_is_not_retried_at_the_same_epoch() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 100.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);
        let blocked = Some("provider=anthropic, five_hour reached=true".to_string());

        let Evaluation::Rollover { generation, .. } =
            evaluate_now(&state, &cfg, true, blocked.clone())
        else {
            panic!("a confirmed block with a healthy alternate must roll over");
        };

        fail(&state, "wrap", SHORT, generation, "launch failed", NOW).expect("abort");
        let seat = seat::load(&state, SHORT).expect("seat");
        assert!(matches!(seat.phase, seat::Phase::Idle));
        assert_eq!(seat.generation, 1, "the successor never took the seat");
        assert!(seat.visited.iter().any(|visit| visit.agent == "codex"));
        assert!(seat::no_flap_invariant(&seat));
        let logged = std::fs::read_to_string(state.logs().join(log::LOG_FILE)).expect("log");
        assert!(logged.contains(FAILED));

        assert!(
            matches!(
                evaluate_now(&state, &cfg, true, blocked),
                Evaluation::Park { .. }
            ),
            "the only candidate already failed against this evidence, so the seat parks"
        );
    }

    #[test]
    fn on_resume_moves_to_a_better_harness_and_resumes_in_place_otherwise() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 100.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);
        seat::park(&state, SHORT, NOW, "five_hour", "exhausted", NOW).expect("park");

        let request = on_resume(&state, &cfg, "wrap", SHORT, NOW, true)
            .expect("a healthier harness is available");
        assert_eq!(request.target_agent, "codex");
        assert!(request.automatic);

        let (_dir2, state2) = temp_state();
        register_seat(&state2);
        store_usage(&state2, "anthropic", 5.0, NOW);
        store_usage(&state2, "openai", 90.0, NOW);
        seat::park(&state2, SHORT, NOW, "five_hour", "exhausted", NOW).expect("park");
        assert!(
            on_resume(&state2, &cfg, "wrap", SHORT, NOW, true).is_none(),
            "the seat's own harness is still the best one"
        );
        let seat = seat::load(&state2, SHORT).expect("seat");
        assert!(matches!(seat.phase, seat::Phase::Idle));

        // Finding #15 (issue #358 review): the in-place branch (no
        // `HandoverRequest` to hand a live-swap seam -- the still-running
        // child simply continues) must still leave the SAME durable trail a
        // real rollover would: `seat::resume` (just asserted above via
        // `Phase::Idle`) and a `capacity-resumed` `PoolEvent` on the
        // ordinary decision log, so an operator reading `zirv ctx status`/
        // the decision log can see the seat came back rather than silently
        // wondering why a parked seat's own dashboard entry ever changed at
        // all.
        let log = std::fs::read_to_string(state2.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains(&format!("\"action\":\"{RESUMED}\"")),
            "the in-place resume must still record a capacity-resumed event: {log}"
        );
    }

    #[test]
    fn on_startup_commits_or_aborts_an_interrupted_prepared_rollover() {
        let (_dir, state) = temp_state();
        register_seat(&state);
        let generation = seat::prepare(
            &state,
            SHORT,
            "codex",
            Some("gpt-5.6-terra"),
            seat::Cause::Manual,
            NOW,
        )
        .expect("prepare");
        let recovered = on_startup(&state, SHORT, &|_| Some("session-b".to_string()))
            .expect("a prepared seat is recovered");
        assert_eq!(recovered.generation, generation);
        assert_eq!(recovered.agent, "codex");

        let (_dir2, state2) = temp_state();
        register_seat(&state2);
        seat::prepare(&state2, SHORT, "codex", None, seat::Cause::Manual, NOW).expect("prepare");
        let aborted = on_startup(&state2, SHORT, &|_| None).expect("a prepared seat is recovered");
        assert_eq!(aborted.generation, 1, "the successor never took the seat");
        assert_eq!(aborted.agent, "claude");
        assert!(aborted.visited.iter().any(|visit| visit.agent == "codex"));
    }

    #[test]
    fn a_pool_event_carries_only_capacity_evidence() {
        let event = PoolEvent {
            snapshot_at: NOW,
            source_agent: "claude".to_string(),
            target_agent: Some("codex".to_string()),
            reason: "source harness is hard-blocked".to_string(),
            ..PoolEvent::default()
        };
        let json = serde_json::to_string(&event).expect("serialize");
        for field in [
            "snapshot_at",
            "source_agent",
            "target_agent",
            "reserved_tokens",
            "tie_break",
            "reason",
        ] {
            assert!(json.contains(field), "{field} must be reported");
        }
        assert!(
            !json.contains("handoff") && !json.contains("transcript"),
            "a pool event must never carry handoff or transcript text: {json}"
        );
    }

    #[test]
    fn a_disabled_or_pinned_seat_is_skipped() {
        let (_dir, state) = temp_state();
        let mut cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 85.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        cfg.fallback.auto_orchestrator_rollover = false;
        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));

        cfg.fallback.auto_orchestrator_rollover = true;
        cfg.fallback.enabled = false;
        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));

        cfg.fallback.enabled = true;
        let mut seat = seat::load(&state, SHORT).expect("seat");
        seat.pinned = true;
        seat::store(&state, &seat).expect("store seat");
        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));
    }

    #[test]
    fn a_single_enabled_harness_is_skipped() {
        let (_dir, state) = temp_state();
        let mut cfg = cfg();
        cfg.fallback.order = vec!["claude".to_string()];
        register_seat(&state);
        store_usage(&state, "anthropic", 85.0, NOW);

        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));
    }

    #[test]
    fn an_absent_seat_is_skipped() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        assert!(matches!(
            evaluate_now(&state, &cfg, true, None),
            Evaluation::Skip(_)
        ));
    }

    #[test]
    fn successor_readiness_waits_then_times_out_and_never_commits_a_dead_child() {
        let timeout = Duration::from_secs(30);
        assert_eq!(
            successor_readiness(false, true, true, true, Duration::ZERO, timeout),
            Readiness::Dead
        );
        assert_eq!(
            successor_readiness(true, true, true, false, Duration::ZERO, timeout),
            Readiness::Ready
        );
        assert_eq!(
            successor_readiness(true, true, false, true, Duration::ZERO, timeout),
            Readiness::Waiting,
            "a turn-signal-capable successor is not judged by quiet time"
        );
        assert_eq!(
            successor_readiness(true, false, false, true, Duration::ZERO, timeout),
            Readiness::Ready,
            "a signal-less successor proves itself by going quiet"
        );
        assert_eq!(
            successor_readiness(true, true, false, false, timeout, timeout),
            Readiness::TimedOut
        );
    }

    #[test]
    fn draining_and_plan_events_are_edge_triggered() {
        let (_dir, state) = temp_state();
        let cfg = cfg();
        register_seat(&state);
        store_usage(&state, "anthropic", 85.0, NOW);
        store_usage(&state, "openai", 5.0, NOW);

        let _ = evaluate_now(&state, &cfg, true, None);
        let first = std::fs::read_to_string(state.logs().join(log::LOG_FILE)).expect("log");
        let plan_lines = first.matches(PLAN_CHANGED).count();
        assert_eq!(plan_lines, 1, "the first evaluation records the plan once");

        seat::abort(&state, SHORT, 2, NOW).expect("abort the prepared rollover");
        let _ = evaluate_now(&state, &cfg, true, None);
        let second = std::fs::read_to_string(state.logs().join(log::LOG_FILE)).expect("log");
        assert_eq!(
            second.matches(PLAN_CHANGED).count(),
            1,
            "an unchanged plan must not be logged again"
        );
    }
}
