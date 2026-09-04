//! Issue #358 (task T6a): a harness-pool view for `zirv ctx status`.
//!
//! `pool::build` is the only I/O in this module -- it composes three
//! already-pure/already-I/O modules (`fallback::capacity_snapshot`,
//! `reservation::entries`/`outstanding`, `seat::load`) into one flat
//! [`PoolView`], the same "one snapshot, rendered several ways" shape
//! `status.rs`'s own report already follows for the sessions/work-group
//! sections. `render_text` is pure over that snapshot -- no fs/clock/env --
//! so its own shape is unit-tested without a state directory, and `status
//! --json` can hand the identical `PoolView` straight to
//! `serde_json::to_string_pretty` with no second read.

use serde::Serialize;

use super::allocator;
use super::config::CtxConfig;
use super::fallback;
use super::reservation;
use super::seat;
use super::sessions;
use super::state::StateDir;
use super::task;
use crate::style::{self, Tone};

/// One harness's row in the pool view -- `allocator::HarnessCapacity` plus
/// its binding window's own numbers, flattened so a JSON consumer (`status
/// --json`) never has to cross-reference a separate `providers` list to
/// learn what its own harness is currently reading.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarnessRow {
    pub name: String,
    pub provider: String,
    pub state: String,
    pub state_reason: String,
    pub used_pct: Option<f64>,
    pub headroom_pct: Option<f64>,
    pub projected_headroom_pct: Option<f64>,
    pub signal_age_secs: Option<u64>,
    pub signal_source: Option<String>,
    /// One of `"measured"`, `"estimated"`, `"assumed"`, `"stale"`,
    /// `"credit-covered"`, `"unknown"` -- see [`signal_quality_for`].
    pub signal_quality: String,
    pub active: u32,
    pub max_active: Option<u32>,
    /// Ready, unclaimed task cards for this repo -- repo-wide, not
    /// harness-specific (a card names no target harness), so every row in
    /// one [`PoolView`] carries the identical number. `0` when `build` was
    /// given no repo slug at all (no repo context).
    pub queued: u32,
    pub reserved_tokens: u64,
    pub resets_at: Option<u64>,
}

/// One provider's row: the reservation ledger's own view
/// (`reservation::entries`/`outstanding`), plus which window is currently
/// binding for it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderRow {
    pub provider: String,
    pub reserved_tokens: u64,
    pub reservations: usize,
    pub binding_window: Option<String>,
}

/// The orchestrator seat's own condensed status -- `seat::Seat` flattened to
/// what an operator reading `status` actually wants: is it mid-rollover, is
/// a rollover pending but not yet acted on, is it parked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeatView {
    pub short: String,
    pub agent: String,
    pub model: Option<String>,
    pub generation: u64,
    pub pinned: bool,
    /// `"idle"` / `"prepared"` / `"parked"` -- `seat::Phase`'s own three
    /// variants, lower-cased.
    pub phase: String,
    /// `seat::Seat::pending`'s own cause, described -- a trigger `decide`
    /// saw but has not yet acted on, distinct from an in-flight `Phase::
    /// Prepared` transaction (named separately via `successor`, below).
    pub rollover_pending: Option<String>,
    /// The successor agent named by an in-flight `Phase::Prepared`
    /// transaction, if any.
    pub successor: Option<String>,
    /// `Phase::Parked`'s own `until`, if the seat is currently parked.
    pub parked_until: Option<u64>,
}

/// The whole pool snapshot: every harness `cfg.fallback.order` (plus the
/// seat's own agent, or the requested one) names, every provider they sit
/// on, this orchestrator's own seat when one is registered, and the full
/// exclusion ledger `allocator::place` would produce for a nominal unit
/// requested on that same harness -- the same reasoning every OTHER
/// candidate was passed over for, not just which one currently wins.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PoolView {
    pub taken_at: u64,
    pub degraded: bool,
    pub seat: Option<SeatView>,
    pub harnesses: Vec<HarnessRow>,
    pub providers: Vec<ProviderRow>,
    pub exclusions: Vec<(String, String)>,
}

/// Which bucket a binding window's own reading falls into for a human
/// scanning the table, cheaper than re-deriving "measured vs. assumed" from
/// `WindowReading`'s own three separate booleans/strings at every call site.
/// Ordered by how much an operator should trust the number: a covered
/// overage is not usage pressure at all, a stale reading is usage pressure
/// that might already be wrong, a collector reading is the real thing, an
/// estimator reading is a model's best guess, and "no window at all" is
/// either a policy-configured assumption (`unknown_headroom_pct > 0`) or
/// genuinely unknown.
fn signal_quality_for(
    harness_state: allocator::HarnessState,
    binding: Option<&allocator::WindowReading>,
    unknown_headroom_pct: f64,
) -> String {
    let quality = match binding {
        Some(window) if window.overage_covered => "credit-covered",
        Some(window) if window.stale => "stale",
        Some(window) if window.source == "collector" => "measured",
        Some(window) if window.source == "estimator" => "estimated",
        Some(_) => "unknown",
        None if harness_state == allocator::HarnessState::Unknown && unknown_headroom_pct > 0.0 => {
            "assumed"
        }
        None => "unknown",
    };
    quality.to_string()
}

fn cause_label(cause: &seat::Cause) -> String {
    match cause {
        seat::Cause::Proactive { headroom_pct, .. } => {
            format!("proactive ({headroom_pct:.1}% headroom)")
        }
        seat::Cause::Reactive { detail, .. } => format!("reactive: {detail}"),
        seat::Cause::Manual => "manual".to_string(),
    }
}

fn seat_view_for(record: &seat::Seat) -> SeatView {
    let (phase, successor, parked_until) = match &record.phase {
        seat::Phase::Idle => ("idle".to_string(), None, None),
        seat::Phase::Prepared {
            successor_agent, ..
        } => ("prepared".to_string(), Some(successor_agent.clone()), None),
        seat::Phase::Parked { until, .. } => ("parked".to_string(), None, Some(*until)),
    };
    let rollover_pending = record.pending.as_ref().map(|p| cause_label(&p.cause));

    SeatView {
        short: record.short.clone(),
        agent: record.agent.clone(),
        model: record.model.clone(),
        generation: record.generation,
        pinned: record.pinned,
        phase,
        rollover_pending,
        successor,
        parked_until,
    }
}

/// Every candidate `allocator::place` would consider for a nominal,
/// unbounded unit requested on `requested` -- every eligible loser and every
/// disqualified candidate alike, each with its own `Exclusion::label()`
/// (`allocator::place`'s own doc comment: "explain every candidate it
/// considered, eligible losers included"). `None` when there is no harness
/// to plan against at all (no seat, no configured fallback order).
fn build_exclusions(
    snapshot: &allocator::CapacitySnapshot,
    cfg: &CtxConfig,
    requested: Option<&str>,
) -> Vec<(String, String)> {
    let Some(requested) = requested else {
        return Vec::new();
    };
    let unit = allocator::WorkUnit {
        id: "pool".to_string(),
        requested: requested.to_string(),
        bounds: allocator::TaskBounds {
            tokens: None,
            tool_calls: None,
        },
        expected_tokens: 0,
        needs_tool_call_counting: false,
        source_model: None,
        source_model_explicit: false,
        delegation: true,
    };
    let placement = allocator::place(snapshot, cfg, &unit, &[], &|_| Some("model".to_string()));
    placement
        .exclusions
        .into_iter()
        .map(|(name, exclusion)| (name, exclusion.label()))
        .collect()
}

/// Ready, unclaimed task cards for `repo_slug` -- `0` when `repo_slug` is
/// `None` (no repo context), never an error: a pool view degrades to "no
/// queue visibility" rather than failing the whole report.
fn queued_count(state: &StateDir, repo_slug: Option<&str>) -> u32 {
    let Some(slug) = repo_slug else {
        return 0;
    };
    task::load_cards(state, slug)
        .values()
        .filter(|card| card.state == task::State::Ready && card.claim.is_none())
        .count() as u32
}

/// Builds one [`PoolView`]: `session` (a full session id, `sessions::Record::
/// session`'s own shape) names this orchestrator's own seat when one is
/// registered, and is also excluded from every harness's live `active` count
/// the same way `fallback::capacity_snapshot`'s own `requester` parameter
/// already does for every other caller -- a session never counts its own
/// registry row as capacity already spent. `repo_slug` scopes [`queued_
/// count`]; `None` when the caller has no repo context (a dashboard
/// authority path, for instance).
pub fn build(
    state: &StateDir,
    cfg: &CtxConfig,
    now: u64,
    session: Option<&str>,
    repo_slug: Option<&str>,
) -> PoolView {
    let seat_record = session
        .map(sessions::short_id)
        .and_then(|short| seat::load(state, &short));

    let requested: Option<String> = seat_record
        .as_ref()
        .map(|s| s.agent.clone())
        .or_else(|| cfg.fallback.order.first().cloned());

    let snapshot = fallback::capacity_snapshot(state, cfg, now, session, requested.as_deref());
    let queued = queued_count(state, repo_slug);

    let providers: Vec<ProviderRow> = snapshot
        .providers
        .iter()
        .map(|provider| {
            let binding_window = provider
                .binding
                .and_then(|i| provider.windows.get(i))
                .map(|w| w.window.clone());
            ProviderRow {
                provider: provider.provider.clone(),
                reserved_tokens: reservation::outstanding(state, &provider.provider, now),
                reservations: reservation::entries(state, &provider.provider).len(),
                binding_window,
            }
        })
        .collect();

    let harnesses: Vec<HarnessRow> = snapshot
        .harnesses
        .iter()
        .map(|harness| {
            let provider_capacity = snapshot.provider(&harness.provider);
            let binding = provider_capacity.and_then(|p| p.binding.and_then(|i| p.windows.get(i)));
            let projected_headroom_pct =
                provider_capacity.and_then(|p| allocator::projected_headroom(p, cfg, 0));
            let reserved_tokens = providers
                .iter()
                .find(|p| p.provider == harness.provider)
                .map(|p| p.reserved_tokens)
                .unwrap_or(0);

            HarnessRow {
                name: harness.name.clone(),
                provider: harness.provider.clone(),
                state: harness.state.as_str().to_string(),
                state_reason: harness.state_reason.clone(),
                used_pct: binding.map(|w| w.used_pct),
                headroom_pct: binding.map(|w| w.headroom_pct),
                projected_headroom_pct,
                signal_age_secs: binding.map(|w| w.age_secs),
                signal_source: binding.map(|w| w.source.clone()),
                signal_quality: signal_quality_for(
                    harness.state,
                    binding,
                    cfg.fallback.unknown_headroom_pct,
                ),
                active: harness.active,
                max_active: harness.max_active,
                queued,
                reserved_tokens,
                resets_at: binding.map(|w| w.resets_at),
            }
        })
        .collect();

    let exclusions = build_exclusions(&snapshot, cfg, requested.as_deref());

    PoolView {
        taken_at: now,
        degraded: snapshot.degraded,
        seat: seat_record.as_ref().map(seat_view_for),
        harnesses,
        providers,
        exclusions,
    }
}

fn label(colour: bool, title: &str) -> String {
    style::paint(title, Tone::Emphasis, colour)
}

fn state_tone(state: &str) -> Tone {
    match state {
        "ready" => Tone::Ok,
        "draining" | "unknown" => Tone::Warn,
        "hard-blocked" | "disabled" => Tone::Err,
        _ => Tone::Plain,
    }
}

fn format_pct(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.0}%"),
        None => "--".to_string(),
    }
}

fn format_row(row: &HarnessRow, colour: bool) -> String {
    let usage = match (row.used_pct, row.headroom_pct) {
        (Some(used), Some(headroom)) => format!("{used:.0}% used / {headroom:.0}% headroom"),
        _ => "unknown".to_string(),
    };
    let signal = match (row.signal_age_secs, row.signal_source.as_deref()) {
        (Some(age), Some(source)) => format!(
            "{} ({source}, {} ago)",
            row.signal_quality,
            style::format_age(age)
        ),
        _ => row.signal_quality.clone(),
    };
    let target = row
        .max_active
        .map(|m| m.to_string())
        .unwrap_or_else(|| "--".to_string());
    let reset = row
        .resets_at
        .map(|r| r.to_string())
        .unwrap_or_else(|| "--".to_string());
    format!(
        "  {:<10} {:<10} {:<32} {:<28} {:>3}/{:<3}/{:<3} {:<12} {}",
        row.name,
        row.provider,
        usage,
        signal,
        row.active,
        target,
        row.queued,
        style::paint(&row.state, state_tone(&row.state), colour),
        reset,
    )
}

fn format_seat_line(seat: &SeatView, colour: bool) -> String {
    let model = seat.model.as_deref().unwrap_or("--");
    let pin = if seat.pinned { " pinned" } else { "" };
    let mut line = format!(
        "  {} {} {} gen {}{pin} phase {}",
        label(colour, "seat:"),
        seat.agent,
        model,
        seat.generation,
        seat.phase,
    );
    if let Some(cause) = &seat.rollover_pending {
        line.push_str(&format!(
            "; rollover pending: {cause} (waiting for idle boundary)"
        ));
    }
    line
}

/// The full, multi-line table form -- no leading `pool:` header of its own
/// (the caller, `status::render_report`, already prints one via its shared
/// `header()` helper, exactly as every other section does).
fn render_full(view: &PoolView, colour: bool) -> String {
    let mut lines = Vec::new();
    if view.degraded {
        lines.push(format!(
            "  {}",
            style::paint("degraded: provider readings disagree", Tone::Warn, colour)
        ));
    }
    lines.push(format!(
        "  {:<10} {:<10} {:<32} {:<28} {:>11} {:<12} {}",
        "harness", "provider", "usage/headroom", "signal", "active/tgt/q", "state", "reset"
    ));
    for row in &view.harnesses {
        lines.push(format_row(row, colour));
    }
    if let Some(seat) = &view.seat {
        lines.push(format_seat_line(seat, colour));
        if let Some(successor) = &seat.successor {
            lines.push(format!("  successor: {successor}"));
        }
        if let Some(until) = seat.parked_until {
            lines.push(format!("  parked until unix {until}"));
        }
    }
    for provider in &view.providers {
        let binding = provider
            .binding_window
            .as_deref()
            .map(|w| format!(", binding {w}"))
            .unwrap_or_default();
        lines.push(format!(
            "  provider {}: reserved {} tokens across {} reservation(s){binding}",
            provider.provider, provider.reserved_tokens, provider.reservations
        ));
    }
    for (name, reason) in &view.exclusions {
        lines.push(format!("  excluded {name}: {reason}"));
    }
    lines.join("\n")
}

/// The one-line brief form, e.g. `pool: claude ready 62% | codex draining
/// 8% | seat claude gen 3` -- carries its own `pool:` label, unlike
/// [`render_full`], matching every other `--brief` line `status.rs` renders
/// (`sessions:`, `agents:`, ...).
fn render_brief(view: &PoolView, colour: bool) -> String {
    let mut parts: Vec<String> = view
        .harnesses
        .iter()
        .map(|row| {
            format!(
                "{} {} {}",
                row.name,
                style::paint(&row.state, state_tone(&row.state), colour),
                format_pct(row.headroom_pct)
            )
        })
        .collect();
    if let Some(seat) = &view.seat {
        parts.push(format!("seat {} gen {}", seat.agent, seat.generation));
    }
    if parts.is_empty() {
        parts.push("(no harnesses configured)".to_string());
    }
    format!("{} {}", label(colour, "pool:"), parts.join(" | "))
}

/// Renders `view` as either the full table (`brief = false`) or the single
/// summary line (`brief = true`). Pure over `view` -- no fs/clock/env -- so
/// every shape is exercised directly against a hand-built [`PoolView`].
pub fn render_text(view: &PoolView, brief: bool, colour: bool) -> String {
    if brief {
        render_brief(view, colour)
    } else {
        render_full(view, colour)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(source: &str, headroom: f64) -> allocator::WindowReading {
        allocator::WindowReading {
            window: "five_hour".to_string(),
            used_pct: 100.0 - headroom,
            headroom_pct: headroom,
            resets_at: 1_700_003_600,
            observed_at: 1_700_000_000,
            age_secs: 12,
            source: source.to_string(),
            stale: false,
            limit_reached: false,
            overage_covered: false,
        }
    }

    fn sample_row(name: &str, provider: &str, state: &str, headroom: Option<f64>) -> HarnessRow {
        HarnessRow {
            name: name.to_string(),
            provider: provider.to_string(),
            state: state.to_string(),
            state_reason: "ready".to_string(),
            used_pct: headroom.map(|h| 100.0 - h),
            headroom_pct: headroom,
            projected_headroom_pct: headroom,
            signal_age_secs: Some(12),
            signal_source: Some("collector".to_string()),
            signal_quality: "measured".to_string(),
            active: 1,
            max_active: Some(3),
            queued: 2,
            reserved_tokens: 1_000,
            resets_at: Some(1_700_003_600),
        }
    }

    fn sample_view() -> PoolView {
        PoolView {
            taken_at: 1_700_000_000,
            degraded: false,
            seat: Some(SeatView {
                short: "abcd1234".to_string(),
                agent: "claude".to_string(),
                model: Some("opus".to_string()),
                generation: 3,
                pinned: false,
                phase: "idle".to_string(),
                rollover_pending: None,
                successor: None,
                parked_until: None,
            }),
            harnesses: vec![
                sample_row("claude", "anthropic", "ready", Some(62.0)),
                sample_row("codex", "openai", "draining", Some(8.0)),
            ],
            providers: vec![ProviderRow {
                provider: "anthropic".to_string(),
                reserved_tokens: 5_000,
                reservations: 2,
                binding_window: Some("five_hour".to_string()),
            }],
            exclusions: vec![("gemini".to_string(), "disabled".to_string())],
        }
    }

    #[test]
    fn render_full_lists_every_harness_the_seat_and_exclusions() {
        let view = sample_view();
        let text = render_text(&view, false, false);
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("codex"), "got {text}");
        assert!(text.contains("ready"), "got {text}");
        assert!(text.contains("draining"), "got {text}");
        assert!(text.contains("seat:"), "got {text}");
        assert!(text.contains("gen 3"), "got {text}");
        assert!(text.contains("excluded gemini: disabled"), "got {text}");
        assert!(
            text.contains("provider anthropic: reserved 5000 tokens across 2 reservation(s)"),
            "got {text}"
        );
        assert!(
            !text.starts_with("pool:"),
            "the full form carries no header of its own: {text}"
        );
    }

    #[test]
    fn render_brief_is_one_line_with_the_pool_label() {
        let view = sample_view();
        let text = render_text(&view, true, false);
        assert_eq!(text.lines().count(), 1, "got {text}");
        assert!(text.starts_with("pool: "), "got {text}");
        assert!(text.contains("claude ready 62%"), "got {text}");
        assert!(text.contains("codex draining 8%"), "got {text}");
        assert!(text.contains("seat claude gen 3"), "got {text}");
    }

    #[test]
    fn render_full_names_a_parked_seat() {
        let mut view = sample_view();
        view.seat = Some(SeatView {
            short: "abcd1234".to_string(),
            agent: "codex".to_string(),
            model: None,
            generation: 4,
            pinned: true,
            phase: "parked".to_string(),
            rollover_pending: None,
            successor: None,
            parked_until: Some(1_700_010_000),
        });
        let text = render_text(&view, false, false);
        assert!(text.contains("phase parked"), "got {text}");
        assert!(text.contains("pinned"), "got {text}");
        assert!(text.contains("parked until unix 1700010000"), "got {text}");
    }

    #[test]
    fn render_full_names_a_pending_rollover_and_its_successor() {
        let mut view = sample_view();
        view.seat = Some(SeatView {
            short: "abcd1234".to_string(),
            agent: "claude".to_string(),
            model: Some("opus".to_string()),
            generation: 3,
            pinned: false,
            phase: "prepared".to_string(),
            rollover_pending: Some("proactive (4.0% headroom)".to_string()),
            successor: Some("codex".to_string()),
            parked_until: None,
        });
        let text = render_text(&view, false, false);
        assert!(
            text.contains(
                "rollover pending: proactive (4.0% headroom) (waiting for idle boundary)"
            ),
            "got {text}"
        );
        assert!(text.contains("successor: codex"), "got {text}");
    }

    #[test]
    fn render_brief_with_no_harnesses_still_names_the_seat() {
        let view = PoolView {
            taken_at: 0,
            degraded: false,
            seat: Some(SeatView {
                short: "abcd1234".to_string(),
                agent: "claude".to_string(),
                model: None,
                generation: 1,
                pinned: false,
                phase: "idle".to_string(),
                rollover_pending: None,
                successor: None,
                parked_until: None,
            }),
            harnesses: Vec::new(),
            providers: Vec::new(),
            exclusions: Vec::new(),
        };
        let text = render_text(&view, true, false);
        assert_eq!(text, "pool: seat claude gen 1");
    }

    // -- signal_quality_for -----------------------------------------------

    #[test]
    fn signal_quality_fresh_collector_reading_is_measured() {
        let w = window("collector", 50.0);
        assert_eq!(
            signal_quality_for(allocator::HarnessState::Ready, Some(&w), 20.0),
            "measured"
        );
    }

    #[test]
    fn signal_quality_estimator_reading_is_estimated() {
        let w = window("estimator", 50.0);
        assert_eq!(
            signal_quality_for(allocator::HarnessState::Ready, Some(&w), 20.0),
            "estimated"
        );
    }

    #[test]
    fn signal_quality_stale_reading_is_stale_even_if_it_came_from_the_collector() {
        let mut w = window("collector", 50.0);
        w.stale = true;
        assert_eq!(
            signal_quality_for(allocator::HarnessState::Ready, Some(&w), 20.0),
            "stale"
        );
    }

    #[test]
    fn signal_quality_overage_covered_reading_is_credit_covered_even_when_stale() {
        let mut w = window("collector", 0.0);
        w.overage_covered = true;
        w.stale = true;
        assert_eq!(
            signal_quality_for(allocator::HarnessState::Ready, Some(&w), 20.0),
            "credit-covered"
        );
    }

    #[test]
    fn signal_quality_no_window_with_unknown_headroom_configured_is_assumed() {
        assert_eq!(
            signal_quality_for(allocator::HarnessState::Unknown, None, 20.0),
            "assumed"
        );
    }

    #[test]
    fn signal_quality_no_window_and_opted_out_is_unknown() {
        assert_eq!(
            signal_quality_for(allocator::HarnessState::Unknown, None, 0.0),
            "unknown"
        );
    }

    // -- build --------------------------------------------------------------

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
        cfg.fallback.order = vec!["claude".to_string(), "codex".to_string()];
        cfg
    }

    fn store_usage(state: &StateDir, provider: &str, percent: f64, reset_at: u64, now: u64) {
        super::super::window::store_for(
            state,
            provider,
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: percent,
                    resets_at: reset_at,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store provider usage");
    }

    #[test]
    fn build_yields_two_harness_rows_with_the_right_states() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 10.0, now + 3_600, now);
        store_usage(&state, "openai", 95.0, now + 600, now);

        let view = build(&state, &cfg, now, None, None);
        assert_eq!(view.harnesses.len(), 2);

        let claude = view
            .harnesses
            .iter()
            .find(|h| h.name == "claude")
            .expect("claude row");
        assert_eq!(claude.state, "ready");
        assert_eq!(claude.signal_quality, "measured");

        let codex = view
            .harnesses
            .iter()
            .find(|h| h.name == "codex")
            .expect("codex row");
        assert_ne!(codex.state, "ready");
    }

    #[test]
    fn build_reports_the_queued_count_from_a_ready_unclaimed_card() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 10.0, now + 3_600, now);
        store_usage(&state, "openai", 10.0, now + 3_600, now);

        task::append_event(
            &state,
            "repo-a",
            &task::Event::Created {
                id: "t1".to_string(),
                repo_slug: "repo-a".to_string(),
                title: "do the thing".to_string(),
                brief: "brief".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: now,
            },
        )
        .expect("create card");
        task::append_event(
            &state,
            "repo-a",
            &task::Event::Readied {
                id: "t1".to_string(),
                at: now,
            },
        )
        .expect("ready card");

        let view = build(&state, &cfg, now, None, Some("repo-a"));
        assert!(!view.harnesses.is_empty());
        for row in &view.harnesses {
            assert_eq!(row.queued, 1, "one ready unclaimed card for repo-a");
        }

        let view_no_repo = build(&state, &cfg, now, None, None);
        for row in &view_no_repo.harnesses {
            assert_eq!(row.queued, 0, "no repo context reports zero queued");
        }
    }

    #[test]
    fn build_names_the_registered_seat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = test_cfg_with_ready_adapters();
        let now = 1_700_000_000;
        store_usage(&state, "anthropic", 10.0, now + 3_600, now);
        store_usage(&state, "openai", 10.0, now + 3_600, now);

        let short = sessions::short_id("session-a");
        seat::register(
            &state,
            &short,
            "session-a",
            "claude",
            Some("opus"),
            "claude",
            "orchestrator",
            false,
            now,
        )
        .expect("register seat");

        let view = build(&state, &cfg, now, Some("session-a"), None);
        let seat_view = view.seat.expect("seat present");
        assert_eq!(seat_view.agent, "claude");
        assert_eq!(seat_view.generation, 1);
    }
}
