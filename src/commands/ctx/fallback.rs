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
    pub selected_headroom_pct: f64,
    pub selected_headroom_assumed: bool,
}

impl Route {
    pub fn detail(&self) -> String {
        let from = self
            .requested_headroom_pct
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let assumption = if self.selected_headroom_assumed {
            " assumed"
        } else {
            ""
        };
        format!(
            "{} -> {} ({}, source headroom {from}, target headroom {:.1}%{assumption}, model {})",
            self.requested,
            self.selected,
            self.reason.label(),
            self.selected_headroom_pct,
            self.model
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
    pub bounds: TaskBounds,
    pub now: u64,
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

fn requested_headroom(state: &StateDir, cfg: &CtxConfig, name: &str, now: u64) -> Option<f64> {
    let provider = adapters::provider_for_agent_name(Some(name));
    let (collector, estimator) = pace::current_windows(state, &cfg.pace, now, provider);
    pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace)
        .map(|reading| reading.headroom_pct)
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
        let Some(headroom) =
            candidate_headroom(state, cfg, name, request.bounds, request.now)
        else {
            continue;
        };
        let Some(model) = handover::equivalent_model(
            request.requested,
            request.source_model,
            request.source_model_explicit,
            name,
            cfg,
        ) else {
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
        selected_headroom_pct: headroom.pct,
        selected_headroom_assumed: headroom.assumed,
    })
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
    let (selected, model, headroom) = best_alternate(state, cfg, request, excluded)?;
    Some(Route {
        requested: request.requested.to_string(),
        selected,
        model,
        reason: RouteReason::Exhausted,
        requested_headroom_pct: requested_headroom(state, cfg, request.requested, request.now),
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

    #[test]
    fn route_detail_discloses_assumed_headroom() {
        let route = Route {
            requested: "claude".into(),
            selected: "codex".into(),
            model: "gpt-5.6-terra".into(),
            reason: RouteReason::Exhausted,
            requested_headroom_pct: Some(0.0),
            selected_headroom_pct: 25.0,
            selected_headroom_assumed: true,
        };
        let detail = route.detail();
        assert!(detail.contains("claude -> codex"));
        assert!(detail.contains("25.0% assumed"));
        assert!(detail.contains("gpt-5.6-terra"));
    }
}
