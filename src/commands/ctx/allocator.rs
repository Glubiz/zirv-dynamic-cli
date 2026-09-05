//! Issue #358 (task 2): pure cross-harness capacity planning.
//!
//! `fallback.rs` owns the only I/O (`capacity_snapshot`) and hands the result
//! here as a plain [`CapacitySnapshot`]. Everything in this module is a
//! function of that snapshot and `CtxConfig` alone -- no fs/clock/env/net,
//! the same purity precedent `rot.rs` documents for itself: identical inputs
//! give identical placements, every time, so a plan can be replayed,
//! diffed, or serialized for `zirv ctx status` without re-reading any state.

use serde::Serialize;

use super::config::CtxConfig;
pub use super::fallback::TaskBounds;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HarnessState {
    Ready,
    Draining,
    HardBlocked,
    Unknown,
    Disabled,
}

impl HarnessState {
    /// Not yet called from production code: the `zirv ctx status` surface
    /// this feeds (issue #358, a later task) lands after this one. Kept
    /// `pub` and exercised by this module's own tests now, the same
    /// task-ordering shape `FallbackConfig::rollover_headroom_pct` already
    /// documents for itself.
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::HardBlocked => "hard-blocked",
            Self::Unknown => "unknown",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowReading {
    pub window: String,
    pub used_pct: f64,
    pub headroom_pct: f64,
    pub resets_at: u64,
    pub observed_at: u64,
    pub age_secs: u64,
    pub source: String,
    pub stale: bool,
    pub limit_reached: bool,
    pub overage_covered: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderCapacity {
    pub provider: String,
    pub windows: Vec<WindowReading>,
    /// Index into `windows` of the most restrictive (binding) reading, per
    /// `pace`'s own binding rule -- `None` when no window is currently
    /// usable.
    pub binding: Option<usize>,
    pub hard_refused: bool,
    pub reserved_tokens: u64,
    pub degraded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarnessCapacity {
    pub name: String,
    pub provider: String,
    pub enabled: bool,
    pub ready: bool,
    pub unready_reason: Option<String>,
    pub capacity_small: bool,
    pub counts_tool_calls: bool,
    pub active: u32,
    pub max_active: Option<u32>,
    pub reserve_headroom_pct: f64,
    pub state: HarnessState,
    pub state_reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapacitySnapshot {
    pub taken_at: u64,
    pub providers: Vec<ProviderCapacity>,
    pub harnesses: Vec<HarnessCapacity>,
    pub degraded: bool,
}

impl CapacitySnapshot {
    pub fn provider(&self, name: &str) -> Option<&ProviderCapacity> {
        self.providers
            .iter()
            .find(|p| p.provider.eq_ignore_ascii_case(name))
    }

    pub fn harness(&self, name: &str) -> Option<&HarnessCapacity> {
        self.harnesses
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkUnit {
    pub id: String,
    pub requested: String,
    pub bounds: TaskBounds,
    pub expected_tokens: u64,
    pub needs_tool_call_counting: bool,
    pub source_model: Option<String>,
    pub source_model_explicit: bool,
    pub delegation: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Exclusion {
    Disabled,
    Unready(String),
    CapacitySmall,
    NoToolCallCounting,
    HardBlocked,
    Draining(String),
    AtMaxActive {
        active: u32,
        max: u32,
    },
    InsufficientHeadroom {
        have: f64,
        need: f64,
    },
    UnknownHeadroomOptedOut,
    NoEquivalentModel,
    Visited,
    Excluded,
    /// This candidate cleared every eligibility check but lost to `by`,
    /// whose own projected headroom (`projected_headroom_pct`) was greater
    /// (or tied and earlier in `cfg.fallback.order`). Issue #358 follow-up:
    /// `Placement.exclusions` must name a reason for every candidate it
    /// considered, eligible losers included, not just the disqualified
    /// ones.
    Outranked {
        by: String,
        projected_headroom_pct: f64,
    },
}

impl Exclusion {
    /// Same task-ordering note as `HarnessState::as_str`: the human-facing
    /// surface this labels for lands in a later issue #358 task.
    #[allow(dead_code)]
    pub fn label(&self) -> String {
        match self {
            Self::Disabled => "disabled".to_string(),
            Self::Unready(reason) => format!("not ready: {reason}"),
            Self::CapacitySmall => "capacity-limited harness cannot take this task".to_string(),
            Self::NoToolCallCounting => "does not count tool calls".to_string(),
            Self::HardBlocked => "hard blocked by usage".to_string(),
            Self::Draining(reason) => format!("draining: {reason}"),
            Self::AtMaxActive { active, max } => format!("at max_active ({active}/{max})"),
            Self::InsufficientHeadroom { have, need } => {
                format!("insufficient headroom ({have:.1}% < {need:.1}%)")
            }
            Self::UnknownHeadroomOptedOut => {
                "unknown headroom opted out (unknown_headroom_pct=0)".to_string()
            }
            Self::NoEquivalentModel => "no equivalent model available".to_string(),
            Self::Visited => "already visited in this order".to_string(),
            Self::Excluded => "explicitly excluded".to_string(),
            Self::Outranked {
                by,
                projected_headroom_pct,
            } => format!("outranked by {by} ({projected_headroom_pct:.1}% projected headroom)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub name: String,
    pub model: Option<String>,
    pub headroom_pct: f64,
    pub projected_headroom_pct: f64,
    pub assumed: bool,
    pub binding_window: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Placement {
    pub unit: String,
    pub selected: Option<Candidate>,
    pub keep_requested: bool,
    pub exclusions: Vec<(String, Exclusion)>,
}

/// The state ladder: disabled/unready first (the harness cannot run at all),
/// then a confirmed hard refusal (it can run, but usage says not now), then
/// draining (it could accept work, but not enough is left for a new unit or
/// it is already at its concurrency cap), then unknown (no usable reading to
/// judge by), and only then ready.
pub fn classify(
    harness: &HarnessCapacity,
    provider: &ProviderCapacity,
    cfg: &CtxConfig,
) -> (HarnessState, String) {
    if !harness.enabled {
        return (HarnessState::Disabled, "harness disabled".to_string());
    }
    if !harness.ready {
        let reason = harness
            .unready_reason
            .clone()
            .unwrap_or_else(|| "not ready".to_string());
        return (HarnessState::Disabled, reason);
    }
    if provider.hard_refused {
        return (
            HarnessState::HardBlocked,
            "binding window at or above the hard spawn ceiling".to_string(),
        );
    }
    if let Some(max) = harness.max_active
        && harness.active >= max
    {
        return (
            HarnessState::Draining,
            format!("at max_active ({}/{max})", harness.active),
        );
    }
    let projected = projected_headroom(provider, cfg, 0);
    if let Some(p) = projected
        && p <= harness.reserve_headroom_pct
    {
        return (
            HarnessState::Draining,
            format!(
                "projected headroom {p:.1}% at or below reserve {:.1}%",
                harness.reserve_headroom_pct
            ),
        );
    }
    if provider.binding.is_none() {
        return (
            HarnessState::Unknown,
            "no usable binding usage reading".to_string(),
        );
    }
    (HarnessState::Ready, "ready".to_string())
}

fn window_budget(window_name: &str, cfg: &CtxConfig) -> u64 {
    match window_name {
        "five_hour" => cfg.pace.five_hour_budget_tokens,
        "seven_day" => cfg.pace.seven_day_budget_tokens,
        _ => 0,
    }
}

fn window_projected_headroom(
    window: &WindowReading,
    cfg: &CtxConfig,
    reserved: u64,
    extra: u64,
) -> f64 {
    let budget = window_budget(&window.window, cfg);
    let headroom = if budget > 0 {
        window.headroom_pct - ((reserved + extra) as f64 / budget as f64) * 100.0
    } else {
        window.headroom_pct
    };
    headroom.clamp(0.0, 100.0)
}

/// The binding window's headroom, minus this provider's already-reserved
/// tokens plus `extra_tokens`, expressed as a percentage of that window's
/// configured token budget. `None` when the provider has no binding window
/// at all. When the binding window has no configured budget, the raw
/// headroom is returned unchanged (there is nothing to convert tokens into).
pub fn projected_headroom(
    provider: &ProviderCapacity,
    cfg: &CtxConfig,
    extra_tokens: u64,
) -> Option<f64> {
    let idx = provider.binding?;
    let window = provider.windows.get(idx)?;
    Some(window_projected_headroom(
        window,
        cfg,
        provider.reserved_tokens,
        extra_tokens,
    ))
}

fn binding_headroom(provider: &ProviderCapacity) -> (f64, Option<String>) {
    match provider.binding.and_then(|i| provider.windows.get(i)) {
        Some(w) => (w.headroom_pct, Some(w.window.clone())),
        None => (0.0, None),
    }
}

/// Every window that has a configured budget must have enough projected
/// headroom for `bounds`, not merely the single binding window -- a task
/// that fits the five-hour window can still be rejected by a tighter
/// seven-day ceiling.
fn fits_all_windows(
    provider: &ProviderCapacity,
    bounds: &TaskBounds,
    cfg: &CtxConfig,
    extra_tokens: u64,
) -> bool {
    provider
        .windows
        .iter()
        .all(|w| match bounds.required_headroom_pct(cfg, &w.window) {
            Some(required) => {
                window_projected_headroom(w, cfg, provider.reserved_tokens, extra_tokens)
                    >= required
            }
            None => true,
        })
}

/// The single worst-shortfall window, for a human-readable exclusion reason:
/// the window with the largest gap between what `bounds` requires and what
/// is actually projected to be left.
fn worst_window_shortfall(
    provider: &ProviderCapacity,
    bounds: &TaskBounds,
    cfg: &CtxConfig,
    extra_tokens: u64,
) -> (f64, f64) {
    provider
        .windows
        .iter()
        .filter_map(|w| {
            let required = bounds.required_headroom_pct(cfg, &w.window)?;
            let have = window_projected_headroom(w, cfg, provider.reserved_tokens, extra_tokens);
            Some((have, required, required - have))
        })
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(have, need, _)| (have, need))
        .unwrap_or((0.0, 0.0))
}

/// Why the requested harness itself did not qualify under rule (a) of
/// [`place`], recorded so a `Placement` with no selection still explains
/// every candidate it considered -- including the one that was asked for.
fn requested_unfit_reason(
    harness: &HarnessCapacity,
    provider: &ProviderCapacity,
    cfg: &CtxConfig,
    unit: &WorkUnit,
) -> Exclusion {
    match harness.state {
        HarnessState::Disabled => Exclusion::Disabled,
        HarnessState::HardBlocked => Exclusion::HardBlocked,
        HarnessState::Draining => Exclusion::Draining(harness.state_reason.clone()),
        HarnessState::Unknown => {
            if cfg.fallback.unknown_headroom_pct <= 0.0 {
                Exclusion::UnknownHeadroomOptedOut
            } else {
                Exclusion::InsufficientHeadroom {
                    have: cfg.fallback.unknown_headroom_pct,
                    need: cfg
                        .fallback
                        .min_candidate_headroom_pct
                        .max(unit.bounds.required_unknown_headroom_pct(cfg)),
                }
            }
        }
        HarnessState::Ready => {
            let (have, need) =
                worst_window_shortfall(provider, &unit.bounds, cfg, unit.expected_tokens);
            Exclusion::InsufficientHeadroom { have, need }
        }
    }
}

/// Places one [`WorkUnit`], deterministically: identical `snapshot`/`cfg`/
/// `unit`/`exclude` inputs always produce the identical `Placement`.
///
/// Rule (a): the requested harness is kept when it is `Ready` and fits every
/// budgeted window with `unit.expected_tokens` added to whatever this
/// provider already has reserved -- this reproduces today's "no reroute"
/// outcome exactly.
///
/// Rule (b): otherwise `cfg.fallback.order` is walked in order, skipping the
/// requested harness (already tried) and anything in `exclude`; the
/// candidate with the greatest projected headroom wins, ties broken by
/// `order` position. An `Unknown` harness may still win, using the
/// configured `unknown_headroom_pct` as an assumed headroom (never reduced
/// by reservations, since there is no window to compute a projection
/// against); `unknown_headroom_pct <= 0` opts every `Unknown` harness out.
pub fn place(
    snapshot: &CapacitySnapshot,
    cfg: &CtxConfig,
    unit: &WorkUnit,
    exclude: &[&str],
    models: &dyn Fn(&str) -> Option<String>,
) -> Placement {
    let mut exclusions: Vec<(String, Exclusion)> = Vec::new();

    if let Some(requested) = snapshot.harness(&unit.requested) {
        if let Some(provider) = snapshot.provider(&requested.provider) {
            if requested.state == HarnessState::Ready
                && fits_all_windows(provider, &unit.bounds, cfg, unit.expected_tokens)
            {
                let (headroom_pct, binding_window) = binding_headroom(provider);
                let projected =
                    projected_headroom(provider, cfg, unit.expected_tokens).unwrap_or(headroom_pct);
                return Placement {
                    unit: unit.id.clone(),
                    selected: Some(Candidate {
                        name: requested.name.clone(),
                        model: models(&requested.name),
                        headroom_pct,
                        projected_headroom_pct: projected,
                        assumed: false,
                        binding_window,
                    }),
                    keep_requested: true,
                    exclusions,
                };
            }
            exclusions.push((
                requested.name.clone(),
                requested_unfit_reason(requested, provider, cfg, unit),
            ));
        } else {
            exclusions.push((
                requested.name.clone(),
                Exclusion::Unready("no provider capacity for this harness".to_string()),
            ));
        }
    } else {
        exclusions.push((
            unit.requested.clone(),
            Exclusion::Unready("harness not in this capacity snapshot".to_string()),
        ));
    }

    let mut seen: hashbrown::HashSet<String> = hashbrown::HashSet::new();
    seen.insert(unit.requested.to_lowercase());
    // Every candidate that clears every check, in `cfg.fallback.order`
    // position order -- kept whole (not just the running best) so every
    // eligible loser can be recorded as `Exclusion::Outranked` once the
    // winner is known, not only the disqualified ones.
    let mut eligible: Vec<(usize, Candidate)> = Vec::new();

    for (order_index, name) in cfg.fallback.order.iter().enumerate() {
        if name.eq_ignore_ascii_case(&unit.requested) {
            continue;
        }
        if exclude.iter().any(|excl| excl.eq_ignore_ascii_case(name)) {
            exclusions.push((name.clone(), Exclusion::Excluded));
            continue;
        }
        if !seen.insert(name.to_lowercase()) {
            exclusions.push((name.clone(), Exclusion::Visited));
            continue;
        }

        let Some(harness) = snapshot.harness(name) else {
            exclusions.push((
                name.clone(),
                Exclusion::Unready("harness not in this capacity snapshot".to_string()),
            ));
            continue;
        };
        if !harness.enabled {
            exclusions.push((name.clone(), Exclusion::Disabled));
            continue;
        }
        if !harness.ready {
            exclusions.push((
                name.clone(),
                Exclusion::Unready(
                    harness
                        .unready_reason
                        .clone()
                        .unwrap_or_else(|| "not ready".to_string()),
                ),
            ));
            continue;
        }
        if harness.capacity_small && !unit.bounds.is_small(cfg) {
            exclusions.push((name.clone(), Exclusion::CapacitySmall));
            continue;
        }
        if unit.needs_tool_call_counting && !harness.counts_tool_calls {
            exclusions.push((name.clone(), Exclusion::NoToolCallCounting));
            continue;
        }
        match harness.state {
            HarnessState::HardBlocked => {
                exclusions.push((name.clone(), Exclusion::HardBlocked));
                continue;
            }
            HarnessState::Draining => {
                exclusions.push((
                    name.clone(),
                    Exclusion::Draining(harness.state_reason.clone()),
                ));
                continue;
            }
            _ => {}
        }
        if let Some(max) = harness.max_active
            && harness.active >= max
        {
            exclusions.push((
                name.clone(),
                Exclusion::AtMaxActive {
                    active: harness.active,
                    max,
                },
            ));
            continue;
        }

        let Some(provider) = snapshot.provider(&harness.provider) else {
            exclusions.push((
                name.clone(),
                Exclusion::Unready("no provider capacity for this harness".to_string()),
            ));
            continue;
        };

        let (headroom_pct, assumed, binding_window) = if harness.state == HarnessState::Unknown {
            let pct = cfg.fallback.unknown_headroom_pct;
            if pct <= 0.0 {
                exclusions.push((name.clone(), Exclusion::UnknownHeadroomOptedOut));
                continue;
            }
            (pct, true, None)
        } else {
            let (raw, window_name) = binding_headroom(provider);
            (raw, false, window_name)
        };

        if !assumed && !fits_all_windows(provider, &unit.bounds, cfg, unit.expected_tokens) {
            let (have, need) =
                worst_window_shortfall(provider, &unit.bounds, cfg, unit.expected_tokens);
            exclusions.push((name.clone(), Exclusion::InsufficientHeadroom { have, need }));
            continue;
        }

        let projected = if assumed {
            headroom_pct
        } else {
            projected_headroom(provider, cfg, unit.expected_tokens).unwrap_or(headroom_pct)
        };
        let required = if assumed {
            cfg.fallback
                .min_candidate_headroom_pct
                .max(unit.bounds.required_unknown_headroom_pct(cfg))
        } else {
            cfg.fallback.min_candidate_headroom_pct.max(
                binding_window
                    .as_deref()
                    .and_then(|w| unit.bounds.required_headroom_pct(cfg, w))
                    .unwrap_or(0.0),
            )
        };
        if projected < required {
            exclusions.push((
                name.clone(),
                Exclusion::InsufficientHeadroom {
                    have: projected,
                    need: required,
                },
            ));
            continue;
        }

        let Some(model) = models(name) else {
            exclusions.push((name.clone(), Exclusion::NoEquivalentModel));
            continue;
        };

        eligible.push((
            order_index,
            Candidate {
                name: name.clone(),
                model: Some(model),
                headroom_pct,
                projected_headroom_pct: projected,
                assumed,
                binding_window,
            },
        ));
    }

    // The same "greatest projected headroom, ties by order position" rule
    // as before, just applied over the whole `eligible` set at once instead
    // of tracked incrementally, so the loser(s) can still be identified.
    let winner_index =
        eligible
            .iter()
            .enumerate()
            .min_by(|(_, (a_order, a)), (_, (b_order, b))| {
                b.projected_headroom_pct
                    .partial_cmp(&a.projected_headroom_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a_order.cmp(b_order))
            });

    let selected = winner_index.map(|(i, _)| eligible[i].1.clone());
    if let Some((winner_i, _)) = winner_index {
        let winner_name = eligible[winner_i].1.name.clone();
        let winner_projected = eligible[winner_i].1.projected_headroom_pct;
        for (i, (_, candidate)) in eligible.iter().enumerate() {
            if i == winner_i {
                continue;
            }
            exclusions.push((
                candidate.name.clone(),
                Exclusion::Outranked {
                    by: winner_name.clone(),
                    projected_headroom_pct: winner_projected,
                },
            ));
        }
    }

    Placement {
        unit: unit.id.clone(),
        selected,
        keep_requested: false,
        exclusions,
    }
}

/// Plans every unit in order against one scratch copy of `snapshot`: each
/// admitted unit's `expected_tokens` is added to its provider's `reserved_
/// tokens` and its harness's `active` count is incremented before the next
/// unit is placed, so later units see the capacity the earlier ones already
/// claimed. `O(units * harnesses)`: each unit does one `place` call plus a
/// bounded scratch update, no unit ever re-scans earlier units.
///
/// Not yet called from production code: the multi-unit scheduling call site
/// (issue #358, a later task) lands after this one. Kept `pub` and exercised
/// by this module's own tests now, the same task-ordering shape
/// `FallbackConfig::rollover_headroom_pct` already documents for itself.
#[allow(dead_code)]
pub fn plan(
    snapshot: &CapacitySnapshot,
    cfg: &CtxConfig,
    units: &[WorkUnit],
    models: &dyn Fn(&WorkUnit, &str) -> Option<String>,
) -> Vec<Placement> {
    let mut scratch = snapshot.clone();
    let mut placements = Vec::with_capacity(units.len());

    for unit in units {
        let placement = place(&scratch, cfg, unit, &[], &|name: &str| models(unit, name));

        if let Some(candidate) = &placement.selected {
            let provider_name = scratch.harness(&candidate.name).map(|h| h.provider.clone());
            if let Some(provider_name) = provider_name {
                if let Some(provider) = scratch
                    .providers
                    .iter_mut()
                    .find(|p| p.provider.eq_ignore_ascii_case(&provider_name))
                {
                    provider.reserved_tokens = provider
                        .reserved_tokens
                        .saturating_add(unit.expected_tokens);
                }
                if let Some(harness) = scratch
                    .harnesses
                    .iter_mut()
                    .find(|h| h.name.eq_ignore_ascii_case(&candidate.name))
                {
                    harness.active = harness.active.saturating_add(1);
                }
                // Finding #8 (issue #358 review): `reserved_tokens` just
                // moved on the WHOLE provider, not just the harness that was
                // placed -- every sibling harness sharing this provider has
                // stale `state`/`state_reason` the moment that happens (a
                // second harness on the same provider can flip Ready ->
                // Draining purely from a sibling's admission, with no
                // capacity change of its own). Reclassify every harness on
                // this provider, not only `candidate.name`, so the NEXT
                // unit's own `place` call sees an accurate snapshot.
                if let Some(provider) = scratch.provider(&provider_name).cloned() {
                    let siblings: Vec<String> = scratch
                        .harnesses
                        .iter()
                        .filter(|h| h.provider.eq_ignore_ascii_case(&provider_name))
                        .map(|h| h.name.clone())
                        .collect();
                    for sibling_name in siblings {
                        let Some(harness) = scratch.harness(&sibling_name).cloned() else {
                            continue;
                        };
                        let (state, reason) = classify(&harness, &provider, cfg);
                        if let Some(harness_mut) = scratch
                            .harnesses
                            .iter_mut()
                            .find(|h| h.name.eq_ignore_ascii_case(&sibling_name))
                        {
                            harness_mut.state = state;
                            harness_mut.state_reason = reason;
                        }
                    }
                }
            }
        }

        placements.push(placement);
    }

    placements
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(name: &str, headroom: f64) -> WindowReading {
        WindowReading {
            window: name.to_string(),
            used_pct: 100.0 - headroom,
            headroom_pct: headroom,
            resets_at: 1_700_003_600,
            observed_at: 1_700_000_000,
            age_secs: 0,
            source: "collector".to_string(),
            stale: false,
            limit_reached: false,
            overage_covered: false,
        }
    }

    fn provider(
        name: &str,
        windows: Vec<WindowReading>,
        binding: Option<usize>,
    ) -> ProviderCapacity {
        ProviderCapacity {
            provider: name.to_string(),
            windows,
            binding,
            hard_refused: false,
            reserved_tokens: 0,
            degraded: false,
        }
    }

    fn harness(
        name: &str,
        provider: &str,
        active: u32,
        max_active: Option<u32>,
    ) -> HarnessCapacity {
        HarnessCapacity {
            name: name.to_string(),
            provider: provider.to_string(),
            enabled: true,
            ready: true,
            unready_reason: None,
            capacity_small: false,
            counts_tool_calls: true,
            active,
            max_active,
            reserve_headroom_pct: 10.0,
            state: HarnessState::Unknown,
            state_reason: String::new(),
        }
    }

    fn classify_all(
        cfg: &CtxConfig,
        providers: Vec<ProviderCapacity>,
        mut harnesses: Vec<HarnessCapacity>,
    ) -> CapacitySnapshot {
        for h in &mut harnesses {
            let p = providers
                .iter()
                .find(|p| p.provider == h.provider)
                .expect("provider present");
            let (state, reason) = classify(h, p, cfg);
            h.state = state;
            h.state_reason = reason;
        }
        CapacitySnapshot {
            taken_at: 1_700_000_000,
            providers,
            harnesses,
            degraded: false,
        }
    }

    fn base_cfg() -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.fallback.order = vec!["claude".to_string(), "codex".to_string()];
        cfg
    }

    fn unit(id: &str, requested: &str, expected_tokens: u64) -> WorkUnit {
        WorkUnit {
            id: id.to_string(),
            requested: requested.to_string(),
            bounds: TaskBounds {
                tokens: None,
                tool_calls: None,
            },
            expected_tokens,
            needs_tool_call_counting: false,
            source_model: None,
            source_model_explicit: false,
            delegation: true,
        }
    }

    fn always_model(_: &str) -> Option<String> {
        Some("model".to_string())
    }

    #[test]
    fn placing_the_same_snapshot_twice_is_identical() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let first = place(&snapshot, &cfg, &unit, &[], &always_model);
        let second = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert_eq!(first, second);
    }

    #[test]
    fn requested_ready_harness_is_kept() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 50.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert!(placement.keep_requested);
        assert_eq!(placement.selected.expect("selected").name, "claude");
    }

    #[test]
    fn draining_requested_moves_to_the_best_projected_headroom() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 60.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert!(!placement.keep_requested);
        let selected = placement.selected.expect("an alternate was selected");
        assert_eq!(selected.name, "codex");
    }

    #[test]
    fn a_tie_in_projected_headroom_is_broken_by_order() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 40.0)], Some(0)),
                provider("third", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
                harness("gemini", "third", 0, None),
            ],
        );
        let mut cfg = cfg;
        cfg.fallback.order = vec![
            "claude".to_string(),
            "codex".to_string(),
            "gemini".to_string(),
        ];
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        let selected = placement.selected.expect("a tie still selects one");
        assert_eq!(
            selected.name, "codex",
            "codex precedes gemini in fallback.order"
        );
    }

    /// Follow-up to issue #358 task 2: every eligible-but-not-chosen
    /// candidate must carry a reason too, not just the disqualified ones --
    /// `zirv ctx status` needs to explain every candidate it considered.
    #[test]
    fn an_eligible_loser_carries_outranked() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 60.0)], Some(0)),
                provider("third", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
                harness("gemini", "third", 0, None),
            ],
        );
        let mut cfg = cfg;
        cfg.fallback.order = vec![
            "claude".to_string(),
            "codex".to_string(),
            "gemini".to_string(),
        ];
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        let selected = placement
            .selected
            .expect("codex has the greater projected headroom");
        assert_eq!(selected.name, "codex");

        let gemini_reason = placement
            .exclusions
            .iter()
            .find(|(name, _)| name == "gemini")
            .map(|(_, reason)| reason.clone());
        assert_eq!(
            gemini_reason,
            Some(Exclusion::Outranked {
                by: "codex".to_string(),
                projected_headroom_pct: 60.0,
            })
        );
    }

    #[test]
    fn max_active_is_respected_across_a_plan_of_fifteen_units() {
        let mut cfg = base_cfg();
        cfg.fallback.min_candidate_headroom_pct = 0.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 90.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, Some(5)),
                harness("codex", "openai", 0, Some(5)),
            ],
        );
        let units: Vec<WorkUnit> = (0..15)
            .map(|i| unit(&format!("u{i}"), "claude", 0))
            .collect();
        let placements = plan(&snapshot, &cfg, &units, &|_, _| Some("model".to_string()));
        assert_eq!(placements.len(), 15);

        let mut counts = std::collections::HashMap::new();
        for placement in &placements {
            if let Some(candidate) = &placement.selected {
                *counts.entry(candidate.name.clone()).or_insert(0u32) += 1;
            } else {
                assert!(
                    !placement.exclusions.is_empty(),
                    "an unplaced unit must explain every candidate it considered"
                );
            }
        }
        for (_, count) in counts {
            assert!(count <= 5, "no harness may exceed its max_active cap");
        }
    }

    #[test]
    fn reservations_reduce_projected_headroom_and_can_flip_a_candidate_to_draining() {
        let mut cfg = base_cfg();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.min_candidate_headroom_pct = 5.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 20.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        // 15% of the 100k budget: enough to clear codex's raw 20% headroom
        // for the first unit, but the second unit's reservation should push
        // the projected headroom under the 5% floor.
        let units = vec![unit("u1", "claude", 15_000), unit("u2", "claude", 15_000)];
        let placements = plan(&snapshot, &cfg, &units, &|_, _| Some("model".to_string()));
        let first = placements[0].selected.as_ref().expect("first unit placed");
        assert_eq!(first.name, "codex");
        assert!(
            placements[1].selected.is_none(),
            "the second unit should now be excluded"
        );
        let codex_reason = placements[1]
            .exclusions
            .iter()
            .find(|(name, _)| name == "codex")
            .map(|(_, reason)| reason.clone());
        assert!(matches!(
            codex_reason,
            Some(Exclusion::InsufficientHeadroom { .. }) | Some(Exclusion::Draining(_))
        ));
    }

    #[test]
    fn two_harnesses_on_one_provider_share_reserved_tokens() {
        let mut cfg = base_cfg();
        cfg.fallback.order = vec![
            "claude".to_string(),
            "claude-worker".to_string(),
            "codex".to_string(),
        ];
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.min_candidate_headroom_pct = 5.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 30.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("claude-worker", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let units = vec![
            unit("u1", "claude-worker", 15_000),
            unit("u2", "claude", 15_000),
        ];
        let placements = plan(&snapshot, &cfg, &units, &|_, _| Some("model".to_string()));
        // Both units target harnesses on the shared "anthropic" provider;
        // the second unit's own requested-harness projection must already
        // reflect the first unit's reservation.
        let second_provider = placements[1]
            .selected
            .as_ref()
            .map(|c| c.projected_headroom_pct);
        assert!(second_provider.is_some());
        assert!(second_provider.unwrap() < 30.0);
    }

    /// Finding #8 (issue #358 review): `plan()` used to reclassify only the
    /// harness it had just placed after moving `reserved_tokens` onto the
    /// shared provider -- a SIBLING harness on that same provider kept its
    /// stale `state`, so `place()`'s own requested-harness fast path (`state
    /// == Ready`) would keep handing it out long after the provider's real
    /// projected headroom had crossed below its own `reserve_headroom_pct`.
    /// Two harnesses share one provider here; the first unit's own
    /// reservation alone (600 of the provider's 1000-token five-hour budget,
    /// against 15% raw headroom) is enough to push projected headroom to 0%,
    /// under BOTH harnesses' 10% reserve floor -- so the second unit's
    /// requested harness must already read Draining, not a stale Ready.
    #[test]
    fn a_sibling_harness_is_reclassified_after_a_plan_admission_on_its_shared_provider() {
        let mut cfg = base_cfg();
        cfg.fallback.order = vec!["claude-a".to_string(), "claude-b".to_string()];
        cfg.pace.five_hour_budget_tokens = 1_000;
        let snapshot = classify_all(
            &cfg,
            vec![provider(
                "anthropic",
                vec![window("five_hour", 15.0)],
                Some(0),
            )],
            vec![
                harness("claude-a", "anthropic", 0, None),
                harness("claude-b", "anthropic", 0, None),
            ],
        );
        let units = vec![unit("u1", "claude-a", 600), unit("u2", "claude-b", 0)];
        let placements = plan(&snapshot, &cfg, &units, &|_, name| always_model(name));

        assert_eq!(
            placements[0].selected.as_ref().map(|c| c.name.as_str()),
            Some("claude-a"),
            "sanity: the first unit lands on the harness it requested"
        );

        assert!(
            !placements[1].keep_requested,
            "claude-b must no longer be kept as a fresh Ready candidate once the shared \
             provider's headroom has drained below its own reserve: {:?}",
            placements[1]
        );
        assert!(
            placements[1].selected.is_none(),
            "no other harness is eligible either (claude-a is also draining): {:?}",
            placements[1]
        );
        let claude_b_reason = placements[1]
            .exclusions
            .iter()
            .find(|(name, _)| name == "claude-b")
            .map(|(_, reason)| reason.clone());
        assert!(
            matches!(claude_b_reason, Some(Exclusion::Draining(_))),
            "got {claude_b_reason:?} in {:?}",
            placements[1]
        );
    }

    #[test]
    fn unknown_headroom_opted_out_excludes_the_candidate() {
        let mut cfg = base_cfg();
        cfg.fallback.unknown_headroom_pct = 0.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![], None),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        assert_eq!(
            snapshot.harness("codex").expect("codex present").state,
            HarnessState::Unknown
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        let codex_reason = placement
            .exclusions
            .iter()
            .find(|(name, _)| name == "codex")
            .map(|(_, reason)| reason.clone());
        assert_eq!(codex_reason, Some(Exclusion::UnknownHeadroomOptedOut));
    }

    #[test]
    fn a_seven_day_shortfall_rejects_a_unit_the_five_hour_window_would_accept() {
        let mut cfg = base_cfg();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.pace.seven_day_budget_tokens = 1_000_000;
        cfg.fallback.min_candidate_headroom_pct = 0.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider(
                    "openai",
                    vec![window("five_hour", 50.0), window("seven_day", 1.0)],
                    Some(1),
                ),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let bounds = TaskBounds {
            tokens: Some(25_000),
            tool_calls: None,
        };
        let mut unit = unit("u1", "claude", 25_000);
        unit.bounds = bounds;
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert!(
            placement.selected.is_none(),
            "codex's seven_day window cannot fit the bounded task even though five_hour could: {placement:?}"
        );
    }

    #[test]
    fn a_limit_reached_window_hard_blocks_the_provider() {
        let cfg = base_cfg();
        let mut blocked = window("five_hour", 0.0);
        blocked.limit_reached = true;
        let anthropic = ProviderCapacity {
            hard_refused: true,
            ..provider("anthropic", vec![blocked], Some(0))
        };
        let claude = harness("claude", "anthropic", 0, None);
        let (state, _) = classify(&claude, &anthropic, &cfg);
        assert_eq!(state, HarnessState::HardBlocked);
    }

    #[test]
    fn overage_covered_never_hard_blocks() {
        let cfg = base_cfg();
        let mut covered = window("five_hour", 0.0);
        covered.overage_covered = true;
        covered.used_pct = 100.0;
        let anthropic = provider("anthropic", vec![covered], Some(0));
        let claude = harness("claude", "anthropic", 0, None);
        let (state, _) = classify(&claude, &anthropic, &cfg);
        assert_ne!(state, HarnessState::HardBlocked);
    }

    #[test]
    fn classify_state_ladder_order() {
        let mut cfg = base_cfg();
        cfg.fallback.min_candidate_headroom_pct = 50.0;

        // Disabled beats everything else, even a hard refusal.
        let mut h = harness("claude", "anthropic", 0, None);
        h.enabled = false;
        let p = ProviderCapacity {
            hard_refused: true,
            ..provider("anthropic", vec![window("five_hour", 0.0)], Some(0))
        };
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Disabled);

        // HardBlocked beats draining/unknown.
        let h = harness("claude", "anthropic", 10, Some(1));
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::HardBlocked);

        // Draining (at max_active) beats unknown.
        let h = harness("claude", "anthropic", 1, Some(1));
        let p = provider("anthropic", vec![], None);
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Draining);

        // No binding reading at all -> Unknown.
        let h = harness("claude", "anthropic", 0, None);
        let p = provider("anthropic", vec![], None);
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Unknown);

        // Everything clears -> Ready.
        cfg.fallback.min_candidate_headroom_pct = 0.0;
        let h = harness("claude", "anthropic", 0, None);
        let p = provider("anthropic", vec![window("five_hour", 90.0)], Some(0));
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Ready);
    }
}
