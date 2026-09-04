//! Cross-harness token fallback and predictive delegation steering (issue #186).
//!
//! This module owns *selection*, not launching. Callers remain responsible for
//! their existing session/group/sandbox machinery. That separation is
//! intentional: an in-flight session is never interrupted because a usage
//! estimate moved; new delegations consult this module before launch, while a
//! supervisor consults it only after the vendor has actually blocked the child.

use super::adapters;
use super::config::CtxConfig;
use super::handover;
use super::pace::{self, SpawnGate};
use super::state::StateDir;

pub const VISITED_ENV: &str = "ZIRV_CTX_FALLBACK_VISITED";
pub const DELEGATION_ENV: &str = "ZIRV_CTX_DELEGATION";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteReason {
    Exhausted,
    Predictive,
}

impl RouteReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exhausted => "usage exhausted",
            Self::Predictive => "low headroom",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub requested: String,
    pub selected: String,
    pub model: String,
    pub reason: RouteReason,
    pub requested_headroom_pct: Option<f64>,
    pub requested_age_secs: Option<u64>,
    pub requested_observed_at: Option<u64>,
    pub selected_headroom_pct: f64,
    pub selected_headroom_assumed: bool,
}

impl Route {
    pub fn detail(&self, seat: pace::Seat) -> String {
        let from = self
            .requested_headroom_pct
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let observed = match (self.requested_age_secs, self.requested_observed_at) {
            (Some(age), Some(observed_at)) => format!(
                ", observed {} ago, observed_at unix {observed_at}",
                crate::style::format_age(age)
            ),
            (Some(age), None) => format!(", observed {} ago", crate::style::format_age(age)),
            _ => String::new(),
        };
        let assumption = if self.selected_headroom_assumed {
            " assumed"
        } else {
            ""
        };
        format!(
            "{} -> {} ({}, source headroom {from}{observed}, target headroom \
             {:.1}%{assumption}, model {}; to override, {})",
            self.requested,
            self.selected,
            self.reason.label(),
            self.selected_headroom_pct,
            self.model,
            seat.override_hint()
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResetChoice {
    pub requested: String,
    pub selected: String,
    /// None only when waiting on the originally requested harness, where no
    /// cross-vendor model translation is needed.
    pub model: Option<String>,
    pub reset_at: u64,
    pub window: &'static str,
}

impl ResetChoice {
    pub fn is_cross_harness(&self) -> bool {
        self.selected != self.requested
    }

    pub fn detail(&self) -> String {
        format!(
            "{} -> {} (all admissible seats exhausted; earliest {} reset at {})",
            self.requested, self.selected, self.window, self.reset_at
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskBounds {
    pub tokens: Option<u64>,
    pub tool_calls: Option<u32>,
}

impl TaskBounds {
    pub fn is_small(self, cfg: &CtxConfig) -> bool {
        // A tool-call cap alone does not bound total work: a three-tool-call
        // brief can still spend an unbounded number of model tokens. Require
        // the total-token ceiling, and reject any additionally supplied tool
        // ceiling that is itself larger than the small-task policy.
        self.tokens
            .is_some_and(|tokens| tokens <= cfg.fallback.small_task_max_tokens)
            && self
                .tool_calls
                .is_none_or(|tools| tools <= cfg.fallback.small_task_max_tool_calls)
    }

    fn required_headroom_pct(self, cfg: &CtxConfig, window: &str) -> Option<f64> {
        let tokens = self.tokens?;
        let budget = match window {
            "five_hour" => cfg.pace.five_hour_budget_tokens,
            "seven_day" => cfg.pace.seven_day_budget_tokens,
            _ => 0,
        };
        (budget > 0).then(|| (tokens as f64 / budget as f64) * 100.0)
    }

    fn required_unknown_headroom_pct(self, cfg: &CtxConfig) -> f64 {
        ["five_hour", "seven_day"]
            .into_iter()
            .filter_map(|window| self.required_headroom_pct(cfg, window))
            .fold(0.0, f64::max)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RouteRequest<'a> {
    pub requested: &'a str,
    pub source_model: Option<&'a str>,
    pub source_model_explicit: bool,
    /// Delegations use the target harness's worker model policy unless the
    /// source model was explicitly pinned. Session continuations keep tier
    /// mirroring.
    pub delegation: bool,
    pub bounds: TaskBounds,
    pub now: u64,
    /// Issue #328 fix: a harness [`best_alternate`]/[`earliest_reset_choice`]
    /// must never select, compared case-insensitively -- an orchestrator
    /// seat's own harness, so a low-headroom `zirv agent <other-harness>`
    /// cannot be silently rerouted back onto the very same seat
    /// `same_harness_refusal` (agent.rs) already refuses through the front
    /// door. `None` for every caller with no such concept -- a running
    /// worker's own vendor-blocked reroute (`route_blocked_session`) is not
    /// an orchestrator-seat delegation and excludes nothing extra.
    pub exclude: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
struct CandidateHeadroom {
    pct: f64,
    assumed: bool,
}

fn candidate_headroom(
    state: &StateDir,
    cfg: &CtxConfig,
    name: &str,
    bounds: TaskBounds,
    now: u64,
) -> Option<CandidateHeadroom> {
    let provider = adapters::provider_for_agent_name(Some(name));
    let (collector, estimator) = pace::current_windows(state, &cfg.pace, now, provider);
    if matches!(
        pace::spawn_gate(&collector, estimator.as_ref(), now, &cfg.pace),
        SpawnGate::Refuse { .. }
    ) {
        return None;
    }
    if let Some(reading) = pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace) {
        let required = bounds
            .required_headroom_pct(cfg, reading.window)
            .unwrap_or(0.0);
        let floor = cfg.fallback.min_candidate_headroom_pct.max(required);
        return (reading.headroom_pct >= floor).then_some(CandidateHeadroom {
            pct: reading.headroom_pct,
            assumed: false,
        });
    }
    let pct = cfg.fallback.unknown_headroom_pct;
    let floor = cfg
        .fallback
        .min_candidate_headroom_pct
        .max(bounds.required_unknown_headroom_pct(cfg));
    (pct > 0.0 && pct >= floor).then_some(CandidateHeadroom { pct, assumed: true })
}

fn requested_reading(
    state: &StateDir,
    cfg: &CtxConfig,
    name: &str,
    now: u64,
) -> Option<pace::SpawnHeadroom> {
    let provider = adapters::provider_for_agent_name(Some(name));
    let (collector, estimator) = pace::current_windows(state, &cfg.pace, now, provider);
    pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace)
}

/// Pure policy question used by tests and by the runtime selector. A
/// capacity-small harness is admitted only for an explicitly bounded small
/// brief; merely having a short prompt is not evidence the work stays small.
pub fn candidate_allowed_by_capacity(cfg: &CtxConfig, name: &str, bounds: TaskBounds) -> bool {
    !cfg.agents.is_capacity_small(name) || bounds.is_small(cfg)
}

fn best_alternate(
    state: &StateDir,
    cfg: &CtxConfig,
    request: RouteRequest<'_>,
    excluded: &[String],
) -> Option<(String, String, CandidateHeadroom)> {
    let mut best: Option<(usize, String, String, CandidateHeadroom)> = None;
    for (order_index, name) in cfg.fallback.order.iter().enumerate() {
        if name == request.requested
            || excluded.iter().any(|seen| seen == name)
            || request
                .exclude
                .is_some_and(|excl| excl.eq_ignore_ascii_case(name))
            || !cfg.agents.is_enabled(name)
        {
            continue;
        }
        if !candidate_allowed_by_capacity(cfg, name, request.bounds) {
            continue;
        }
        // Selection is the canonical readiness + agent_bin compatibility gate.
        // A broken/absent alternate is skipped, never allowed to turn fallback
        // itself into a failed launch.
        let Ok(candidate_adapter) = adapters::select(Some(name), &[], cfg) else {
            continue;
        };
        if request.bounds.tool_calls.is_some() && !candidate_adapter.counts_tool_calls() {
            continue;
        }
        let Some(headroom) = candidate_headroom(state, cfg, name, request.bounds, request.now)
        else {
            continue;
        };
        let model = if request.delegation {
            handover::equivalent_delegation_model(
                request.requested,
                request.source_model,
                request.source_model_explicit,
                name,
                cfg,
            )
        } else {
            handover::equivalent_model(
                request.requested,
                request.source_model,
                request.source_model_explicit,
                name,
                cfg,
            )
        };
        let Some(model) = model else {
            continue;
        };
        let replace = match &best {
            None => true,
            Some((best_index, _, _, best_headroom)) => {
                headroom.pct > best_headroom.pct
                    || (headroom.pct == best_headroom.pct && order_index < *best_index)
            }
        };
        if replace {
            best = Some((order_index, name.clone(), model, headroom));
        }
    }
    best.map(|(_, name, model, headroom)| (name, model, headroom))
}

/// Routes a *new* delegation when the requested harness is already refused by
/// pacing, or predictively once its measured headroom reaches the configured
/// low-water mark. Unknown headroom does not trigger predictive steering:
/// uncertainty is treated conservatively and only affects whether an alternate
/// may be used after a real refusal/block.
pub fn route_new_delegation(
    state: &StateDir,
    cfg: &CtxConfig,
    request: RouteRequest<'_>,
    force: bool,
) -> Option<Route> {
    if !cfg.fallback.enabled || force {
        return None;
    }

    let provider = adapters::provider_for_agent_name(Some(request.requested));
    let (collector, estimator) = pace::current_windows(state, &cfg.pace, request.now, provider);
    let gate = pace::spawn_gate(&collector, estimator.as_ref(), request.now, &cfg.pace);
    let source_reading =
        pace::spawn_headroom(&collector, estimator.as_ref(), request.now, &cfg.pace);
    if source_reading.is_some_and(|reading| reading.overage_covered) {
        return None;
    }
    let source_headroom = source_reading.map(|reading| reading.headroom_pct);
    let task_will_not_fit = source_reading.is_some_and(|reading| {
        request
            .bounds
            .required_headroom_pct(cfg, reading.window)
            .is_some_and(|required| reading.headroom_pct < required)
    });
    let reason = match gate {
        SpawnGate::Refuse { .. } => RouteReason::Exhausted,
        _ if task_will_not_fit
            || source_headroom.is_some_and(|pct| pct <= cfg.fallback.predictive_headroom_pct) =>
        {
            RouteReason::Predictive
        }
        _ => return None,
    };

    let (selected, model, headroom) = best_alternate(state, cfg, request, &[])?;
    Some(Route {
        requested: request.requested.to_string(),
        selected,
        model,
        reason,
        requested_headroom_pct: source_headroom,
        requested_age_secs: source_reading.map(|reading| reading.age_secs),
        requested_observed_at: source_reading.map(|reading| reading.observed_at),
        selected_headroom_pct: headroom.pct,
        selected_headroom_assumed: headroom.assumed,
    })
}

/// When no admissible harness can run now, chooses the seat whose hard spawn
/// gate clears first. The requested harness remains a candidate even when its
/// model is unknown, because waiting on the same vendor requires no quality
/// translation. Alternate harnesses use the same roster, readiness, capacity,
/// tool-enforcement, and verified-tier checks as immediate fallback.
pub fn earliest_reset_choice(
    state: &StateDir,
    cfg: &CtxConfig,
    request: RouteRequest<'_>,
    excluded: &[String],
) -> Option<ResetChoice> {
    if !cfg.fallback.enabled {
        return None;
    }

    let requested_provider = adapters::provider_for_agent_name(Some(request.requested));
    let (collector, estimator) =
        pace::current_windows(state, &cfg.pace, request.now, requested_provider);
    let requested_reset =
        pace::spawn_reset(&collector, estimator.as_ref(), request.now, &cfg.pace)?;

    let mut best = ResetChoice {
        requested: request.requested.to_string(),
        selected: request.requested.to_string(),
        model: request.source_model.map(str::to_string),
        reset_at: requested_reset.reset_at,
        window: requested_reset.window,
    };
    let mut best_order = usize::MAX;

    for (order_index, name) in cfg.fallback.order.iter().enumerate() {
        if name == request.requested
            || excluded.iter().any(|seen| seen == name)
            || request
                .exclude
                .is_some_and(|excl| excl.eq_ignore_ascii_case(name))
            || !cfg.agents.is_enabled(name)
            || !candidate_allowed_by_capacity(cfg, name, request.bounds)
        {
            continue;
        }
        let Ok(candidate_adapter) = adapters::select(Some(name), &[], cfg) else {
            continue;
        };
        if request.bounds.tool_calls.is_some() && !candidate_adapter.counts_tool_calls() {
            continue;
        }
        let model = if request.delegation {
            handover::equivalent_delegation_model(
                request.requested,
                request.source_model,
                request.source_model_explicit,
                name,
                cfg,
            )
        } else {
            handover::equivalent_model(
                request.requested,
                request.source_model,
                request.source_model_explicit,
                name,
                cfg,
            )
        };
        let Some(model) = model else {
            continue;
        };
        let provider = candidate_adapter.provider();
        let (collector, estimator) = pace::current_windows(state, &cfg.pace, request.now, provider);
        let Some(reset) = pace::spawn_reset(&collector, estimator.as_ref(), request.now, &cfg.pace)
        else {
            continue;
        };
        // `best_order` stays `usize::MAX` while `best` is still the requested
        // harness, so a tie only hands the seat to an alternate once another
        // alternate is already ahead of it; an exact tie against the
        // requested harness itself must not dislodge it.
        if reset.reset_at < best.reset_at
            || (reset.reset_at == best.reset_at
                && best_order != usize::MAX
                && order_index < best_order)
        {
            best = ResetChoice {
                requested: request.requested.to_string(),
                selected: name.clone(),
                model: Some(model),
                reset_at: reset.reset_at,
                window: reset.window,
            };
            best_order = order_index;
        }
    }

    Some(best)
}

/// Selection after a vendor-confirmed block. Unlike predictive routing this
/// does not need the source usage collector to agree: the child's own limit
/// message is stronger evidence than a stale/missing passive reading.
pub fn route_blocked_session(
    state: &StateDir,
    cfg: &CtxConfig,
    request: RouteRequest<'_>,
    excluded: &[String],
) -> Option<Route> {
    if !cfg.fallback.enabled {
        return None;
    }
    let requested_reading = requested_reading(state, cfg, request.requested, request.now);
    let (selected, model, headroom) = best_alternate(state, cfg, request, excluded)?;
    Some(Route {
        requested: request.requested.to_string(),
        selected,
        model,
        reason: RouteReason::Exhausted,
        requested_headroom_pct: requested_reading.map(|reading| reading.headroom_pct),
        requested_age_secs: requested_reading.map(|reading| reading.age_secs),
        requested_observed_at: requested_reading.map(|reading| reading.observed_at),
        selected_headroom_pct: headroom.pct,
        selected_headroom_assumed: headroom.assumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_capacity_requires_a_bounded_small_token_budget() {
        let mut cfg = CtxConfig::default();
        cfg.fallback.small_task_max_tokens = 10_000;
        cfg.fallback.small_task_max_tool_calls = 10;
        assert!(
            TaskBounds {
                tokens: Some(1_000),
                tool_calls: None,
            }
            .is_small(&cfg)
        );
        assert!(
            TaskBounds {
                tokens: Some(1_000),
                tool_calls: Some(3),
            }
            .is_small(&cfg)
        );
        assert!(
            !TaskBounds {
                tokens: None,
                tool_calls: Some(3),
            }
            .is_small(&cfg),
            "a tool-call cap alone leaves model-token spend unbounded"
        );
        assert!(
            !TaskBounds {
                tokens: Some(1_000),
                tool_calls: Some(30),
            }
            .is_small(&cfg)
        );
        assert!(
            !TaskBounds {
                tokens: Some(20_000),
                tool_calls: Some(3),
            }
            .is_small(&cfg)
        );
    }

    #[test]
    fn token_budget_converts_to_required_window_headroom_when_capacity_is_known() {
        let mut cfg = CtxConfig::default();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.pace.seven_day_budget_tokens = 1_000_000;
        let bounds = TaskBounds {
            tokens: Some(25_000),
            tool_calls: None,
        };
        assert_eq!(bounds.required_headroom_pct(&cfg, "five_hour"), Some(25.0));
        assert_eq!(bounds.required_headroom_pct(&cfg, "seven_day"), Some(2.5));
    }

    fn test_cfg_with_ready_adapters() -> CtxConfig {
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
        cfg
    }

    fn store_usage(state: &StateDir, provider: &str, percent: f64, reset_at: u64, now: u64) {
        crate::commands::ctx::window::store_for(
            state,
            provider,
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: percent,
                    resets_at: reset_at,
                    observed_at: now,
                    overage_covered: false,
                }),
                seven_day: None,
            },
        )
        .expect("store provider usage");
    }

    #[test]
    fn all_exhausted_selects_the_admissible_harness_with_the_earliest_reset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 100.0, now + 3_600, now);
        store_usage(&state, "openai", 100.0, now + 600, now);

        let choice = earliest_reset_choice(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: None,
                    tool_calls: None,
                },
                now,
                exclude: None,
            },
            &[],
        )
        .expect("both seats are hard blocked");

        assert_eq!(choice.selected, "codex");
        assert_eq!(choice.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(choice.reset_at, now + 600);
    }

    #[test]
    fn predictive_routing_uses_the_task_budget_when_the_source_cannot_fit_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = test_cfg_with_ready_adapters();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.predictive_headroom_pct = 20.0;
        let now = 1_700_000_000;
        // 25% headroom is above the static 20% threshold, but a 30% task
        // budget cannot fit. The target has ample room.
        store_usage(&state, "anthropic", 75.0, now + 3_600, now);
        store_usage(&state, "openai", 20.0, now + 3_600, now);

        let route = route_new_delegation(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: Some(30_000),
                    tool_calls: None,
                },
                now,
                exclude: None,
            },
            false,
        )
        .expect("the bounded task does not fit the requested seat");

        assert_eq!(route.reason, RouteReason::Predictive);
        assert_eq!(route.selected, "codex");
    }

    #[test]
    fn a_credit_covered_source_reading_never_routes_new_work_away() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        crate::commands::ctx::window::store_for(
            &state,
            "openai",
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 100.0,
                    resets_at: now + 600,
                    observed_at: now,
                    overage_covered: true,
                }),
                seven_day: None,
            },
        )
        .expect("store provider usage");

        let route = route_new_delegation(
            &state,
            &cfg,
            RouteRequest {
                requested: "codex",
                source_model: Some("gpt-5.6-terra"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: None,
                    tool_calls: None,
                },
                now,
                exclude: None,
            },
            false,
        );

        assert_eq!(route, None);
    }

    #[test]
    fn a_covered_seven_day_does_not_stop_a_reroute_when_the_five_hour_is_genuinely_exhausted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        crate::commands::ctx::window::store_for(
            &state,
            "openai",
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 96.0,
                    resets_at: now + 600,
                    observed_at: now,
                    overage_covered: false,
                }),
                seven_day: Some(crate::commands::ctx::window::Window {
                    used_percentage: 100.0,
                    resets_at: now + 3_600,
                    observed_at: now,
                    overage_covered: true,
                }),
            },
        )
        .expect("store source usage");
        store_usage(&state, "anthropic", 20.0, now + 3_600, now);

        let route = route_new_delegation(
            &state,
            &cfg,
            RouteRequest {
                requested: "codex",
                source_model: Some("gpt-5.6-terra"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: None,
                    tool_calls: None,
                },
                now,
                exclude: None,
            },
            false,
        )
        .expect("the uncovered hard refusal reroutes to the admissible alternate");

        assert_eq!(route.reason, RouteReason::Exhausted);
        assert_eq!(route.selected, "claude");
    }

    #[test]
    fn tied_reset_prefers_the_requested_harness_over_an_alternate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        // Both seats reset at exactly the same instant. Staying on the
        // requested harness requires no quality translation, so the tie
        // must not hand the seat to an alternate.
        store_usage(&state, "anthropic", 100.0, now + 3_600, now);
        store_usage(&state, "openai", 100.0, now + 3_600, now);

        let choice = earliest_reset_choice(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: None,
                    tool_calls: None,
                },
                now,
                exclude: None,
            },
            &[],
        )
        .expect("both seats are hard blocked");

        assert_eq!(
            choice.selected, "claude",
            "an exact tie should keep the requested harness"
        );
        assert_eq!(choice.reset_at, now + 3_600);
    }

    // -- RouteRequest::exclude (issue #328 back-door fix) -------------------

    /// Mirrors `predictive_routing_uses_the_task_budget_when_the_source_
    /// cannot_fit_it`'s exact fixture, but excludes the only viable
    /// alternate (`codex`) -- the shape an orchestrator seat's own
    /// same-harness exclusion produces when THAT seat's own harness is the
    /// one and only enabled alternate. No reroute must happen: rerouting
    /// back onto the excluded harness would be the exact same-harness
    /// zirv-supervised worker `same_harness_refusal` (agent.rs) already
    /// refuses through the front door.
    #[test]
    fn an_excluded_harness_is_never_selected_even_as_the_only_viable_alternate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = test_cfg_with_ready_adapters();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.predictive_headroom_pct = 20.0;
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 75.0, now + 3_600, now);
        store_usage(&state, "openai", 20.0, now + 3_600, now);

        let route = route_new_delegation(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: Some(30_000),
                    tool_calls: None,
                },
                now,
                exclude: Some("codex"),
            },
            false,
        );

        assert!(
            route.is_none(),
            "the only viable alternate is excluded, so no reroute should happen: {route:?}"
        );
    }

    /// `exclude` compares case-insensitively, the same as `same_harness_
    /// refusal`'s own own-harness comparison in agent.rs.
    #[test]
    fn exclude_matches_the_harness_name_case_insensitively() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = test_cfg_with_ready_adapters();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.predictive_headroom_pct = 20.0;
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 75.0, now + 3_600, now);
        store_usage(&state, "openai", 20.0, now + 3_600, now);

        let route = route_new_delegation(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: Some(30_000),
                    tool_calls: None,
                },
                now,
                exclude: Some("Codex"),
            },
            false,
        );

        assert!(
            route.is_none(),
            "exclude must match case-insensitively: {route:?}"
        );
    }

    /// The `earliest_reset_choice` fallback path (all seats hard-blocked)
    /// must honor the exclusion too: `codex` resets earliest, but with it
    /// excluded the requested harness (`claude`) must be kept even though
    /// it resets later.
    #[test]
    fn earliest_reset_choice_never_selects_an_excluded_harness() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 100.0, now + 3_600, now);
        store_usage(&state, "openai", 100.0, now + 600, now);

        let choice = earliest_reset_choice(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: None,
                    tool_calls: None,
                },
                now,
                exclude: Some("codex"),
            },
            &[],
        )
        .expect("the requested harness is itself still hard-blocked");

        assert_eq!(
            choice.selected, "claude",
            "codex resets earlier but is excluded, so the requested harness must be kept"
        );
    }

    /// Without an exclusion, routing behaves exactly as before -- the same
    /// fixture as `predictive_routing_uses_the_task_budget_when_the_source_
    /// cannot_fit_it` reroutes to `codex` when `exclude` is `None`.
    #[test]
    fn no_exclusion_reroutes_exactly_as_before() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = test_cfg_with_ready_adapters();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.predictive_headroom_pct = 20.0;
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 75.0, now + 3_600, now);
        store_usage(&state, "openai", 20.0, now + 3_600, now);

        let route = route_new_delegation(
            &state,
            &cfg,
            RouteRequest {
                requested: "claude",
                source_model: Some("sonnet"),
                source_model_explicit: false,
                delegation: true,
                bounds: TaskBounds {
                    tokens: Some(30_000),
                    tool_calls: None,
                },
                now,
                exclude: None,
            },
            false,
        )
        .expect("no exclusion is applied, so the usual reroute still happens");

        assert_eq!(route.selected, "codex");
    }

    #[test]
    fn reset_choice_detail_names_the_earliest_seat_and_reset() {
        let choice = ResetChoice {
            requested: "claude".into(),
            selected: "codex".into(),
            model: Some("gpt-5.6-terra".into()),
            reset_at: 1_700_000_600,
            window: "five_hour",
        };
        assert!(choice.is_cross_harness());
        let detail = choice.detail();
        assert!(detail.contains("claude -> codex"));
        assert!(detail.contains("1700000600"));
    }

    #[test]
    fn route_detail_discloses_assumed_headroom() {
        let route = Route {
            requested: "claude".into(),
            selected: "codex".into(),
            model: "gpt-5.6-terra".into(),
            reason: RouteReason::Exhausted,
            requested_headroom_pct: Some(0.0),
            requested_age_secs: Some(22 * 3600),
            requested_observed_at: Some(1_700_000_000),
            selected_headroom_pct: 25.0,
            selected_headroom_assumed: true,
        };
        let detail = route.detail(pace::Seat::Cli);
        assert!(detail.contains("claude -> codex"));
        assert!(detail.contains("observed 22h ago"));
        assert!(detail.contains("observed_at unix 1700000000"));
        assert!(detail.contains("25.0% assumed"));
        assert!(detail.contains("gpt-5.6-terra"));
        assert!(detail.contains("pass --force"));
    }

    #[test]
    fn delegation_reroutes_use_the_target_worker_model_unless_the_source_was_explicit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = test_cfg_with_ready_adapters();
        cfg.worker.codex = Some("gpt-5.6-sol".to_string());
        cfg.worker.claude = Some("claude-worker".to_string());
        let now = 1_700_000_000;
        store_usage(&state, "openai", 99.0, now + 3_600, now);
        store_usage(&state, "anthropic", 10.0, now + 3_600, now);

        let request = |source_model_explicit| RouteRequest {
            requested: "codex",
            source_model: Some("gpt-5.6-sol"),
            source_model_explicit,
            delegation: true,
            bounds: TaskBounds {
                tokens: None,
                tool_calls: None,
            },
            now,
            exclude: None,
        };
        let implicit = route_new_delegation(&state, &cfg, request(false), false)
            .expect("implicit worker model reroutes");
        assert_eq!(implicit.model, "claude-worker");

        cfg.worker.claude = None;
        let standard = route_new_delegation(&state, &cfg, request(false), false)
            .expect("implicit worker model reroutes to standard");
        assert_eq!(standard.model, "sonnet");

        let explicit = route_new_delegation(&state, &cfg, request(true), false)
            .expect("explicit worker model reroutes by tier");
        assert_eq!(explicit.model, "opus");
    }
}
