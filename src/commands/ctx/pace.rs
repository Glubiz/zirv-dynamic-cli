use super::config::PaceConfig;
use super::window::{FIVE_HOUR_SECS, SEVEN_DAY_SECS, UsageWindows, Window, age_secs};

/// Which data layer the decision rests on. Ordered by authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Collector,
    Estimator,
    None,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Collector => "collector",
            Source::Estimator => "estimator",
            Source::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaceDecision {
    Proceed {
        source: Source,
        worst_percent: f64,
    },
    WaitUntil {
        /// `None` when the window's reset time is unknown, which is when the
        /// configured fallback delay applies.
        reset_at: Option<u64>,
        window: &'static str,
        percent: f64,
        source: Source,
    },
    Unknown,
}

/// Exactly the three strings documented in
/// `docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md`, matched
/// case-insensitively on whole phrases. Deliberately narrow on both sides: a
/// false positive parks a healthy run, and an unverified guess is not a fact.
///
/// Candidates NOT shipped, pending empirical verification (see the follow-up in
/// that notes file). Add one only after observing it in real output:
///   "hit your sonnet limit"   plausible by symmetry with the Opus variant,
///                             but no documented occurrence
///   "hit your usage limit"    invented phrasing, no source at all
pub const LIMIT_HIT_PATTERNS: &[&str] = &[
    "hit your session limit",
    "hit your weekly limit",
    "hit your opus limit",
];

pub fn is_limit_hit(line: &str) -> bool {
    let lowered = line.to_lowercase();
    LIMIT_HIT_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
}

/// Words that, alongside "limit", suggest a possible usage-limit message even
/// when the exact phrasing does not match `LIMIT_HIT_PATTERNS`.
const LOOSE_LIMIT_HINTS: &[&str] = &["hit", "reached", "exceeded"];

/// Loose secondary signal for a possible usage-limit message: the word "limit"
/// alongside a verb suggesting it was hit, reached, or exceeded. Deliberately
/// wider than `LIMIT_HIT_PATTERNS` — firing here only leaves a breadcrumb via
/// `note_limit_wording_drift`, it never feeds into `is_limit_hit` or a pace
/// decision.
fn is_loose_limit_mention(line: &str) -> bool {
    let lowered = line.to_lowercase();
    lowered.contains("limit") && LOOSE_LIMIT_HINTS.iter().any(|hint| lowered.contains(hint))
}

/// Leaves a breadcrumb when `line` loosely resembles a usage-limit message that
/// the strict patterns did not recognize: a decision-log entry plus a single
/// stderr advisory, so a wording change upstream leaves a trail instead of
/// silently falling through to ordinary-failure handling. `strict` is the
/// result already computed by `is_limit_hit`; when it is true no breadcrumb is
/// needed, and this function never changes the proceed/wait/park decision by
/// itself. Fail-open: a broken state dir must not turn this into a hard
/// failure, so a logging error is swallowed like the rest of this file's
/// decision logging.
pub fn note_limit_wording_drift<W: Write>(
    line: &str,
    strict: bool,
    state: &StateDir,
    session: &str,
    verb: &'static str,
    stderr: &mut W,
) {
    if strict || !is_loose_limit_mention(line) {
        return;
    }
    let _ = writeln!(
        stderr,
        "zirv ctx {verb}: possible usage-limit message not recognized by known patterns"
    );
    let _ = log::append(
        state,
        &log::Decision {
            ts: now_secs(),
            session,
            verb,
            verdict: "n/a",
            score: 0,
            action: "limit-wording-drift",
            detail: line,
        },
    );
}

/// The supervisors' single reading of a batch of tapped agent output: whether
/// it announced a usage limit, plus a breadcrumb for every line that only
/// loosely resembles one. Only `is_limit_hit` decides the answer, so the
/// breadcrumb never parks a run on its own.
pub fn scan_for_limit<W: Write>(
    lines: &[String],
    state: &StateDir,
    session: &str,
    verb: &'static str,
    stderr: &mut W,
) -> bool {
    let mut hit = false;
    for line in lines {
        let strict = is_limit_hit(line);
        hit |= strict;
        note_limit_wording_drift(line, strict, state, session, verb, stderr);
    }
    hit
}

/// Whether a collector window may drive the decision.
///
/// A fresh observation always may. A stale one still may when it reported a full
/// window whose reset has not arrived: the percentage is out of date, but a
/// window cannot free up before its own reset time, so letting staleness clear
/// the park would resume straight into an exhausted window. A stale reading
/// below the ceiling is simply unknown and defers to the estimator.
fn binding<'a>(window: &'a Option<Window>, now: u64, cfg: &PaceConfig) -> Option<&'a Window> {
    let window = window.as_ref()?;
    if age_secs(window, now) <= cfg.collector_max_age_secs {
        return Some(window);
    }
    if window.used_percentage >= cfg.max_percent && window.resets_at > now {
        return Some(window);
    }
    None
}

/// The window closest to its limit, with its name.
fn worst<'a>(
    five_hour: Option<&'a Window>,
    seven_day: Option<&'a Window>,
) -> Option<(&'static str, &'a Window)> {
    let candidates = [("five_hour", five_hour), ("seven_day", seven_day)];
    candidates
        .into_iter()
        .filter_map(|(name, window)| window.map(|w| (name, w)))
        .max_by(|a, b| {
            a.1.used_percentage
                .partial_cmp(&b.1.used_percentage)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Collector first when fresh, estimator second, nothing third. A fresher
/// lower-priority layer never overrides a fresh collector reading.
pub fn decide(
    collector: &UsageWindows,
    estimator: Option<&UsageWindows>,
    now: u64,
    cfg: &PaceConfig,
) -> PaceDecision {
    if !cfg.enabled {
        return PaceDecision::Proceed {
            source: Source::None,
            worst_percent: 0.0,
        };
    }

    let collector_worst = worst(
        binding(&collector.five_hour, now, cfg),
        binding(&collector.seven_day, now, cfg),
    );

    let (source, picked) = match collector_worst {
        Some(found) => (Source::Collector, Some(found)),
        None if cfg.estimator => (
            Source::Estimator,
            estimator
                .and_then(|windows| worst(windows.five_hour.as_ref(), windows.seven_day.as_ref())),
        ),
        None => (Source::None, None),
    };

    let Some((name, window)) = picked else {
        return PaceDecision::Unknown;
    };

    if window.used_percentage < cfg.max_percent {
        return PaceDecision::Proceed {
            source,
            worst_percent: window.used_percentage,
        };
    }

    PaceDecision::WaitUntil {
        reset_at: if window.resets_at == 0 {
            None
        } else {
            Some(window.resets_at)
        },
        window: name,
        percent: window.used_percentage,
        source,
    }
}

/// Deterministic spread so several supervisors on one machine do not all wake
/// in the same second. Not cryptographic, just decorrelating. A zero seed is
/// treated as "no entropy available" and adds no offset, exactly like a zero
/// `jitter_secs`: callers that do not care about jitter (most `wait_deadline`
/// call sites in tests) pass `0` and expect an exact, unperturbed value.
pub fn apply_jitter(until: u64, jitter_secs: u64, seed: u64) -> u64 {
    if jitter_secs == 0 || seed == 0 {
        return until;
    }
    let mixed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    until + (mixed >> 33) % jitter_secs
}

/// How long the named window itself lasts. An unrecognized name is treated as
/// the shorter window, so a future adapter cannot accidentally buy itself a
/// week-long wait.
pub fn window_length(window: &str) -> u64 {
    match window {
        "seven_day" => SEVEN_DAY_SECS,
        _ => FIVE_HOUR_SECS,
    }
}

/// Safety cap for waiting on a window: its own length plus head room, unless an
/// operator set an absolute override. Scaling to the window is the point: a
/// seven-day trip legitimately needs days, and a fixed cap would resume early
/// and spend tokens against a window that has not reset.
pub fn wait_cap(window: &str, cfg: &PaceConfig) -> u64 {
    cfg.max_wait_secs
        .unwrap_or_else(|| window_length(window) + cfg.wait_slack_secs)
}

/// Concrete wake-up time for a waiting decision: the reset when it is known and
/// still ahead, the fallback delay otherwise, jittered, and capped by
/// `wait_cap` for the window that tripped.
pub fn wait_deadline(
    decision: &PaceDecision,
    now: u64,
    cfg: &PaceConfig,
    seed: u64,
) -> Option<u64> {
    let PaceDecision::WaitUntil {
        reset_at, window, ..
    } = decision
    else {
        return None;
    };

    let target = match reset_at {
        Some(at) if *at > now => *at,
        _ => now + cfg.fallback_delay_secs,
    };
    let jittered = apply_jitter(target, cfg.jitter_secs, seed);
    Some(jittered.min(now + wait_cap(window, cfg)))
}

pub fn describe(decision: &PaceDecision) -> String {
    match decision {
        PaceDecision::Proceed {
            source,
            worst_percent,
        } => format!(
            "usage {worst_percent:.1}% of the limit ({} data), proceeding",
            source.as_str()
        ),
        PaceDecision::WaitUntil {
            reset_at,
            window,
            percent,
            source,
        } => {
            let reset = match reset_at {
                Some(at) => format!("resets at unix {at}"),
                None => "reset time unknown".to_string(),
            };
            format!(
                "{window} window at {percent:.1}% ({} data, {reset}), waiting before the next run",
                source.as_str()
            )
        }
        PaceDecision::Unknown => {
            "usage state unknown, no usage data from a fresh collector reading or a configured estimator budget, proceeding without pacing".to_string()
        }
    }
}

use std::io::Write;
use std::time::Duration;

use super::state::{StateDir, now_secs};
use super::{log, window as usage_window};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaceOutcome {
    pub waited_secs: u64,
    pub source: Source,
}

/// Longest single sleep, so a supervisor rechecks state (a live session may have
/// refreshed the collector) rather than sleeping blind for hours.
const SLEEP_CHUNK_SECS: u64 = 30;

/// Reads `provider`'s own collector file and, only when a budget is
/// configured, the estimator. Walking every transcript is not free, so it is
/// skipped whenever its result could not be used.
///
/// E: `provider`-scoped since 2026-08-15, via `window::load_for` rather than
/// the legacy unscoped `window::load` -- the pacing gate used to read the
/// single global `usage.json` regardless of which adapter's session it was
/// pacing, so a codex run was paced against whatever Anthropic data claude's
/// statusline tee happened to have written. `load_for` falls back to that
/// same legacy file for claude's own provider (`window::LEGACY_USAGE_
/// PROVIDER`), so this is a no-op for the common case; a provider with no
/// usage source at all (codex/openai today) now reads as "nothing known"
/// (`UsageWindows::default()`) rather than another provider's real numbers.
pub fn current_windows(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
) -> (UsageWindows, Option<UsageWindows>) {
    let collector = usage_window::load_for(state, provider).unwrap_or_default();

    let budgeted = cfg.five_hour_budget_tokens > 0 || cfg.seven_day_budget_tokens > 0;
    if !cfg.estimator || !budgeted {
        return (collector, None);
    }

    let estimated = usage_window::projects_root()
        .ok()
        .map(|root| usage_window::sum_transcripts(&root, now, cfg.count_cache_reads))
        .map(|sums| {
            usage_window::estimate_windows(
                &sums,
                now,
                cfg.five_hour_budget_tokens,
                cfg.seven_day_budget_tokens,
            )
        });
    (collector, estimated)
}

/// Blocks until the window has room, then returns. Never exits the process and
/// never returns an error: pacing failing closed would be worse than pacing not
/// happening, so every unknown proceeds.
///
/// `announcer` is `Some` where a session with a live chrome context is
/// running (`exec`, and by extension `agent`, which delegates to it) and
/// `None` where there is none to hand in (`loop`, which keeps today's plain
/// writer as its only channel): either way, `w` keeps receiving the same
/// text it always has, so nothing that already asserts on it breaks.
///
/// `provider` is the resolved adapter's own `AgentAdapter::provider()`
/// (`"anthropic"` for claude, `"openai"` for codex). E: when `provider` has
/// **no possible** usage source (`window::has_no_usage_source` -- any
/// provider but claude, since only claude has a real collector mechanism at
/// all today), the gate is skipped outright with one announcement rather
/// than silently entering the loop below and reading "nothing known" as if
/// it were a fresh, empty collector reading for *this* provider: there is no
/// collector for this provider at all, which is a materially different fact
/// than "the collector says 0%". Claude itself is exempt from this check
/// even before its first statusline tee: "not yet" is still true and
/// actionable there.
///
/// `announced_no_source` is owned by the caller and threaded through every
/// call across one run (`exec`'s own supervise loop calls this once per
/// cycle -- the pre-flight check and, on a usage-limit park, again -- and
/// `loop`'s cycle does the same), the same discipline the wait-loop's own
/// internal `announced` local already follows *within* a single call: the
/// no-source fact does not change cycle to cycle, so without this the skip
/// line and `PacingSkipped` would otherwise repeat on every single restart
/// of a long-running codex session, drowning out everything else on the
/// `zirv ▸` channel with a fact already stated once.
#[allow(clippy::too_many_arguments)]
pub fn wait_for_window<W: Write>(
    w: &mut W,
    state: &StateDir,
    cfg: &PaceConfig,
    verb: &'static str,
    session: &str,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
    announcer: Option<&super::announce::Announcer>,
    provider: &str,
    announced_no_source: &mut bool,
) -> PaceOutcome {
    if !cfg.enabled {
        return PaceOutcome {
            waited_secs: 0,
            source: Source::None,
        };
    }

    if usage_window::has_no_usage_source(state, provider) {
        if !*announced_no_source {
            let _ = writeln!(
                w,
                "zirv ctx {verb}: pacing off: {provider} has no usage source"
            );
            if let Some(announcer) = announcer {
                announcer.emit(&super::announce::Event::PacingSkipped {
                    provider: provider.to_string(),
                });
            }
            *announced_no_source = true;
        }
        return PaceOutcome {
            waited_secs: 0,
            source: Source::None,
        };
    }

    let started = now_fn();
    let mut announced: Option<(String, Option<u64>)> = None;

    loop {
        let now = now_fn();
        let (collector, estimated) = current_windows(state, cfg, now, provider);
        let decision = decide(&collector, estimated.as_ref(), now, cfg);

        let source = match &decision {
            PaceDecision::Proceed { source, .. } => *source,
            PaceDecision::WaitUntil { source, .. } => *source,
            PaceDecision::Unknown => Source::None,
        };

        let Some(deadline) = wait_deadline(&decision, now, cfg, std::process::id() as u64 ^ now)
        else {
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        };

        // The safety valve, scaled to the window that tripped: a seven-day trip
        // may legitimately wait days, a five-hour trip may not.
        let cap = match &decision {
            PaceDecision::WaitUntil { window, .. } => wait_cap(window, cfg),
            _ => 0,
        };
        if now.saturating_sub(started) >= cap {
            let _ = writeln!(
                w,
                "zirv ctx {verb}: usage still high after waiting {}s (cap {cap}s), proceeding anyway",
                now.saturating_sub(started)
            );
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        }

        // Announce once per distinct decision, not once per sleep chunk: a
        // seven-day park would otherwise write thousands of identical audit
        // lines and scroll the operator's terminal for days.
        let fingerprint = match &decision {
            PaceDecision::WaitUntil {
                window, reset_at, ..
            } => Some(((*window).to_string(), *reset_at)),
            _ => None,
        };
        if announced != fingerprint {
            announced = fingerprint;
            let _ = writeln!(w, "zirv ctx {verb}: {}", describe(&decision));
            if let (
                Some(announcer),
                PaceDecision::WaitUntil {
                    window, reset_at, ..
                },
            ) = (announcer, &decision)
            {
                announcer.emit(&super::announce::Event::PacingWait {
                    window: (*window).to_string(),
                    reset_at: *reset_at,
                });
            }
            let _ = log::append(
                state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb,
                    verdict: "paced",
                    score: 0,
                    action: "pace-wait",
                    detail: &describe(&decision),
                },
            );
        }

        let remaining = deadline.saturating_sub(now).min(cap);
        if remaining == 0 {
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        }
        sleep_fn(Duration::from_secs(remaining.min(SLEEP_CHUNK_SECS)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::PaceConfig;
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::window;
    use crate::commands::ctx::window::{UsageWindows, Window};
    use std::cell::RefCell;

    const NOW: u64 = 1_785_507_315;

    fn window(percent: f64, resets_at: u64, observed_at: u64) -> Option<Window> {
        Some(Window {
            used_percentage: percent,
            resets_at,
            observed_at,
        })
    }

    fn collector(percent: f64) -> UsageWindows {
        UsageWindows {
            five_hour: window(percent, NOW + 600, NOW - 10),
            seven_day: None,
        }
    }

    #[test]
    fn a_healthy_fresh_collector_reading_proceeds() {
        let decision = decide(&collector(42.0), None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::Proceed {
                source: Source::Collector,
                worst_percent: 42.0
            }
        );
    }

    #[test]
    fn at_the_ceiling_the_gate_waits_for_the_reset() {
        let decision = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 600),
                window: "five_hour",
                percent: 99.0,
                source: Source::Collector
            },
            "the default ceiling is inclusive"
        );
    }

    #[test]
    fn just_below_the_ceiling_still_proceeds() {
        let decision = decide(&collector(98.9), None, NOW, &PaceConfig::default());
        assert!(matches!(decision, PaceDecision::Proceed { .. }));
    }

    #[test]
    fn the_worst_window_decides() {
        let both = UsageWindows {
            five_hour: window(10.0, NOW + 100, NOW),
            seven_day: window(99.5, NOW + 90_000, NOW),
        };
        let decision = decide(&both, None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 90_000),
                window: "seven_day",
                percent: 99.5,
                source: Source::Collector
            }
        );
    }

    #[test]
    fn a_stale_collector_reading_is_ignored_in_favour_of_the_estimator() {
        let stale = UsageWindows {
            five_hour: window(5.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        let estimated = UsageWindows {
            five_hour: window(99.9, NOW + 300, NOW),
            seven_day: None,
        };
        let decision = decide(&stale, Some(&estimated), NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 300),
                window: "five_hour",
                percent: 99.9,
                source: Source::Estimator
            }
        );
    }

    #[test]
    fn a_fresh_collector_reading_always_beats_the_estimator() {
        let estimated = UsageWindows {
            five_hour: window(100.0, NOW + 300, NOW),
            seven_day: None,
        };
        let decision = decide(
            &collector(20.0),
            Some(&estimated),
            NOW,
            &PaceConfig::default(),
        );
        assert_eq!(
            decision,
            PaceDecision::Proceed {
                source: Source::Collector,
                worst_percent: 20.0
            },
            "server-authoritative data wins even when the approximation disagrees"
        );
    }

    #[test]
    fn nothing_known_is_unknown_not_zero() {
        let decision = decide(&UsageWindows::default(), None, NOW, &PaceConfig::default());
        assert_eq!(decision, PaceDecision::Unknown);

        let empty_estimate = UsageWindows::default();
        assert_eq!(
            decide(
                &UsageWindows::default(),
                Some(&empty_estimate),
                NOW,
                &PaceConfig::default()
            ),
            PaceDecision::Unknown,
            "an estimator with no configured budget contributes nothing"
        );
    }

    #[test]
    fn disabling_the_estimator_leaves_a_stale_collector_unknown() {
        // Stale and below the ceiling, so it carries no information at all.
        let stale = UsageWindows {
            five_hour: window(50.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        let estimated = UsageWindows {
            five_hour: window(99.9, NOW + 300, NOW),
            seven_day: None,
        };
        let cfg = PaceConfig {
            estimator: false,
            ..PaceConfig::default()
        };
        assert_eq!(
            decide(&stale, Some(&estimated), NOW, &cfg),
            PaceDecision::Unknown
        );
    }

    #[test]
    fn a_stale_full_window_keeps_binding_until_its_reset_arrives() {
        // Staleness must not clear a park: the percentage is old, but a window
        // cannot free up before its own reset, and resuming here would spend
        // tokens against a window that is still exhausted.
        let stale_but_full = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        assert_eq!(
            decide(&stale_but_full, None, NOW, &PaceConfig::default()),
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 600),
                window: "five_hour",
                percent: 100.0,
                source: Source::Collector
            }
        );
    }

    #[test]
    fn a_stale_full_window_stops_binding_once_its_reset_has_passed() {
        let expired = UsageWindows {
            five_hour: window(100.0, NOW - 1, NOW - 100_000),
            seven_day: None,
        };
        assert_eq!(
            decide(&expired, None, NOW, &PaceConfig::default()),
            PaceDecision::Unknown,
            "after the reset the old percentage says nothing about the new window"
        );
    }

    #[test]
    fn a_stale_full_window_still_loses_to_a_fresh_reading() {
        let mixed = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW - 100_000),
            seven_day: window(10.0, NOW + 90_000, NOW),
        };
        // The stale-but-full five hour window is the worse of the two and still
        // binds, so the gate waits on it rather than on the fresh healthy one.
        assert!(matches!(
            decide(&mixed, None, NOW, &PaceConfig::default()),
            PaceDecision::WaitUntil {
                window: "five_hour",
                ..
            }
        ));
    }

    #[test]
    fn pacing_disabled_always_proceeds() {
        let cfg = PaceConfig {
            enabled: false,
            ..PaceConfig::default()
        };
        assert_eq!(
            decide(&collector(100.0), None, NOW, &cfg),
            PaceDecision::Proceed {
                source: Source::None,
                worst_percent: 0.0
            }
        );
    }

    #[test]
    fn jitter_is_bounded_and_deterministic_for_a_seed() {
        for seed in [0_u64, 1, 12_345, u64::MAX] {
            let jittered = apply_jitter(NOW, 30, seed);
            assert!(
                (NOW..NOW + 30).contains(&jittered),
                "seed {seed} produced {jittered}"
            );
            assert_eq!(
                jittered,
                apply_jitter(NOW, 30, seed),
                "same seed, same answer"
            );
        }
        assert_eq!(apply_jitter(NOW, 0, 7), NOW, "zero jitter is exact");
    }

    #[test]
    fn a_known_reset_becomes_a_jittered_deadline() {
        let decision = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        let deadline =
            wait_deadline(&decision, NOW, &PaceConfig::default(), 7).expect("a deadline");
        assert!(
            (NOW + 600..NOW + 630).contains(&deadline),
            "reset plus jitter, got {deadline}"
        );
    }

    #[test]
    fn an_unknown_reset_uses_the_configured_fallback_delay() {
        let unknown = UsageWindows {
            five_hour: window(99.5, 0, NOW),
            seven_day: None,
        };
        let decision = decide(&unknown, None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: None,
                window: "five_hour",
                percent: 99.5,
                source: Source::Collector
            },
            "resets_at of zero means unknown, not epoch"
        );
        let deadline = wait_deadline(&decision, NOW, &PaceConfig::default(), 0).expect("deadline");
        assert_eq!(deadline, NOW + 900);
    }

    #[test]
    fn a_reset_already_in_the_past_uses_the_fallback_too() {
        let past = UsageWindows {
            five_hour: window(99.5, NOW - 5, NOW),
            seven_day: None,
        };
        let decision = decide(&past, None, NOW, &PaceConfig::default());
        let deadline = wait_deadline(&decision, NOW, &PaceConfig::default(), 0).expect("deadline");
        assert_eq!(
            deadline,
            NOW + 900,
            "a stale reset must not resolve instantly"
        );
    }

    #[test]
    fn the_cap_is_scaled_to_the_window_that_tripped() {
        let cfg = PaceConfig::default();
        assert_eq!(window_length("five_hour"), 18_000);
        assert_eq!(window_length("seven_day"), 604_800);
        assert_eq!(
            window_length("something_new"),
            18_000,
            "an unknown window name must not buy a week-long wait"
        );

        assert_eq!(wait_cap("five_hour", &cfg), 18_000 + 3600);
        assert_eq!(wait_cap("seven_day", &cfg), 604_800 + 3600);
    }

    #[test]
    fn a_seven_day_trip_may_wait_days_not_hours() {
        // The reset is five days out. A global six-hour valve would resume long
        // before the week reset and spend tokens against an exhausted window.
        let exhausted_week = UsageWindows {
            five_hour: None,
            seven_day: window(100.0, NOW + 432_000, NOW),
        };
        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let decision = decide(&exhausted_week, None, NOW, &cfg);
        assert_eq!(
            wait_deadline(&decision, NOW, &cfg, 0),
            Some(NOW + 432_000),
            "the real reset sits inside the seven-day cap, so it is honoured exactly"
        );
    }

    #[test]
    fn a_five_hour_trip_is_capped_near_five_hours() {
        // A bogus reset a year out must not park a supervisor for a year.
        let bogus = UsageWindows {
            five_hour: window(100.0, NOW + 31_000_000, NOW),
            seven_day: None,
        };
        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let decision = decide(&bogus, None, NOW, &cfg);
        assert_eq!(
            wait_deadline(&decision, NOW, &cfg, 0),
            Some(NOW + 18_000 + 3600),
            "capped at the window length plus slack"
        );
    }

    #[test]
    fn an_absolute_override_replaces_the_per_window_cap() {
        let far = UsageWindows {
            five_hour: None,
            seven_day: window(99.5, NOW + 500_000, NOW),
        };
        let cfg = PaceConfig {
            max_wait_secs: Some(60),
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        assert_eq!(
            wait_cap("seven_day", &cfg),
            60,
            "the override wins outright"
        );
        let decision = decide(&far, None, NOW, &cfg);
        assert_eq!(wait_deadline(&decision, NOW, &cfg, 0), Some(NOW + 60));
    }

    #[test]
    fn proceeding_and_unknown_have_no_deadline() {
        let cfg = PaceConfig::default();
        assert_eq!(wait_deadline(&PaceDecision::Unknown, NOW, &cfg, 0), None);
        assert_eq!(
            wait_deadline(
                &PaceDecision::Proceed {
                    source: Source::Collector,
                    worst_percent: 1.0
                },
                NOW,
                &cfg,
                0
            ),
            None
        );
    }

    #[test]
    fn descriptions_are_one_line_and_name_the_source() {
        let waiting = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        let text = describe(&waiting);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("five_hour"));
        assert!(text.contains("collector"));
        assert!(!text.contains('\u{2014}'));

        assert!(describe(&PaceDecision::Unknown).contains("unknown"));
        assert!(
            describe(&PaceDecision::Unknown).contains("approximation")
                || describe(&PaceDecision::Unknown).contains("no usage data"),
            "be honest when nothing is known: {}",
            describe(&PaceDecision::Unknown)
        );
    }

    #[test]
    fn the_documented_limit_strings_are_matched() {
        // Exactly the three shapes recorded in
        // docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md.
        assert!(is_limit_hit(
            "You've hit your session limit · resets 3:45pm"
        ));
        assert!(is_limit_hit(
            "You've hit your weekly limit · resets Mon 12:00am"
        ));
        assert!(is_limit_hit("You've hit your Opus limit · resets 3:45pm"));
        assert!(is_limit_hit(
            "  WARNING: you've HIT YOUR SESSION LIMIT now  "
        ));
    }

    #[test]
    fn only_the_documented_patterns_ship() {
        assert_eq!(
            LIMIT_HIT_PATTERNS.len(),
            3,
            "the notes file documents three strings; anything else needs verifying first"
        );
        // Plausible but unverified phrasings stay out until observed for real.
        assert!(!is_limit_hit(
            "You've hit your Sonnet limit · resets 3:45pm"
        ));
        assert!(!is_limit_hit("You've hit your usage limit"));
    }

    #[test]
    fn ordinary_output_is_not_a_limit_hit() {
        for line in [
            "",
            "rate limit headers look fine",
            "hit the ground running",
            "your session is limited to one file",
            "error: 429 too many requests",
        ] {
            assert!(!is_limit_hit(line), "false positive on {line:?}");
        }
    }

    #[test]
    fn the_loose_matcher_requires_limit_plus_a_hit_word() {
        assert!(is_loose_limit_mention("usage limit reached"));
        assert!(is_loose_limit_mention("You exceeded your limit"));
        assert!(is_loose_limit_mention("LIMIT HIT"));
        assert!(!is_loose_limit_mention("limit"), "needs a hit-word too");
        assert!(
            !is_loose_limit_mention("hit the ground running"),
            "needs the word limit too"
        );
        assert!(!is_loose_limit_mention(""));
    }

    #[test]
    fn a_reworded_limit_message_is_not_mistaken_for_the_documented_shape() {
        // Guards against silently mis-scoring wording drift as a strict hit:
        // this phrasing is plausible but not one of the three documented ones.
        assert!(!is_limit_hit(
            "You've reached your usage limit for this session"
        ));
    }

    #[test]
    fn loose_wording_drift_leaves_a_breadcrumb_without_changing_behavior() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut stderr = Vec::new();

        let line = "You've reached your usage limit for this session";
        note_limit_wording_drift(line, false, &state, "sess-1", "exec", &mut stderr);

        let printed = String::from_utf8(stderr).expect("utf8");
        assert!(
            printed.contains("possible usage-limit"),
            "advisory missing: {printed}"
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"limit-wording-drift\""),
            "got {log}"
        );
        assert!(log.contains("sess-1"), "got {log}");
    }

    #[test]
    fn a_recognized_strict_hit_needs_no_breadcrumb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut stderr = Vec::new();

        // Strict already matched (would also loosely match), so no breadcrumb.
        note_limit_wording_drift(
            "You've hit your session limit · resets 3:45pm",
            true,
            &state,
            "sess-1",
            "exec",
            &mut stderr,
        );

        assert!(String::from_utf8(stderr).expect("utf8").is_empty());
        assert!(
            !state.logs().join("decisions.jsonl").exists(),
            "a recognized hit needs no wording-drift breadcrumb"
        );
    }

    #[test]
    fn ordinary_output_leaves_no_breadcrumb_either() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut stderr = Vec::new();

        note_limit_wording_drift(
            "all systems normal",
            false,
            &state,
            "sess-1",
            "exec",
            &mut stderr,
        );

        assert!(String::from_utf8(stderr).expect("utf8").is_empty());
        assert!(!state.logs().join("decisions.jsonl").exists());
    }

    /// What the supervisors actually call: one pass over a batch of tapped
    /// lines answers the park question and leaves the breadcrumbs, and only
    /// the strict patterns decide the answer.
    #[test]
    fn scanning_a_batch_answers_strictly_and_notes_the_rest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut stderr = Vec::new();

        let loose_only = vec![
            "building the plan".to_string(),
            "You've reached your usage limit for this session".to_string(),
        ];
        assert!(
            !scan_for_limit(&loose_only, &state, "sess-1", "loop", &mut stderr),
            "loose wording must never park a run on its own"
        );
        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("limit-wording-drift"))
                .count(),
            1,
            "one breadcrumb, for the one line that drifted: {log}"
        );

        let with_strict = vec!["You've hit your weekly limit".to_string()];
        assert!(scan_for_limit(
            &with_strict,
            &state,
            "sess-1",
            "loop",
            &mut stderr
        ));
        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("limit-wording-drift"))
                .count(),
            1,
            "a recognized hit adds no breadcrumb: {log}"
        );
    }

    /// Fake clock: `now` advances by whatever the code sleeps, so a test can
    /// observe pacing without waiting for real time.
    struct FakeClock {
        now: RefCell<u64>,
        slept: RefCell<Vec<u64>>,
    }

    impl FakeClock {
        fn new(start: u64) -> Self {
            Self {
                now: RefCell::new(start),
                slept: RefCell::new(Vec::new()),
            }
        }
    }

    fn state_with(collector: UsageWindows) -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        window::store(&state, &collector).expect("store");
        (tmp, state)
    }

    #[test]
    fn a_healthy_window_does_not_wait() {
        let (_tmp, state) = state_with(collector(10.0));
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig::default(),
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| {
                clock.slept.borrow_mut().push(d.as_secs());
                *clock.now.borrow_mut() += d.as_secs();
            },
            None,
            "anthropic",
            &mut false,
        );

        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::Collector);
        assert!(clock.slept.borrow().is_empty(), "no sleeping when healthy");
    }

    #[test]
    fn an_exhausted_window_waits_past_the_reset_then_proceeds() {
        // Observed just now, at the ceiling, resetting in 10 minutes.
        let exhausted = UsageWindows {
            five_hour: window(99.5, NOW + 600, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let outcome = wait_for_window(
            &mut out,
            &state,
            &cfg,
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| {
                clock.slept.borrow_mut().push(d.as_secs());
                *clock.now.borrow_mut() += d.as_secs();
            },
            None,
            "anthropic",
            &mut false,
        );

        assert!(outcome.waited_secs >= 600, "waited {}", outcome.waited_secs);
        assert!(!clock.slept.borrow().is_empty(), "it must actually sleep");
        assert!(
            *clock.now.borrow() >= NOW + 600,
            "the clock advanced past the reset"
        );

        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("five_hour"),
            "explain the pause: {printed}"
        );
        assert!(printed.contains("waiting"), "got {printed}");
    }

    #[test]
    fn waiting_is_recorded_in_the_decision_log() {
        let exhausted = UsageWindows {
            five_hour: window(100.0, NOW + 120, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                jitter_secs: 0,
                ..PaceConfig::default()
            },
            "exec",
            "sess-1",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            &mut false,
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
        assert!(log.contains("sess-1"), "got {log}");
        assert_eq!(
            log.lines().filter(|l| l.contains("pace-wait")).count(),
            1,
            "one audit line per pause, not one per sleep chunk: {log}"
        );
    }

    #[test]
    fn an_absolute_override_bounds_the_total_wait() {
        let far = UsageWindows {
            five_hour: window(100.0, NOW + 10_000_000, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(far);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let cfg = PaceConfig {
            max_wait_secs: Some(120),
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let outcome = wait_for_window(
            &mut out,
            &state,
            &cfg,
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            &mut false,
        );

        assert!(
            outcome.waited_secs <= 150,
            "bounded by the override, waited {}",
            outcome.waited_secs
        );
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("proceeding"),
            "after the cap it proceeds rather than exiting: {printed}"
        );
    }

    #[test]
    fn a_bogus_five_hour_reset_is_bounded_by_the_window_not_by_six_hours() {
        // With no override, the cap comes from the window: 5h plus 1h slack.
        let bogus = UsageWindows {
            five_hour: window(100.0, NOW + 10_000_000, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(bogus);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                jitter_secs: 0,
                ..PaceConfig::default()
            },
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            &mut false,
        );

        assert!(
            (18_000..=21_700).contains(&outcome.waited_secs),
            "expected roughly the five-hour cap, waited {}",
            outcome.waited_secs
        );
    }

    #[test]
    fn an_exhausted_week_waits_for_the_real_reset_rather_than_resuming_early() {
        // Five days out. This is the case a fixed six-hour valve got wrong: it
        // would resume roughly twenty times before the week actually reset.
        let exhausted_week = UsageWindows {
            five_hour: None,
            seven_day: window(100.0, NOW + 432_000, NOW),
        };
        let (_tmp, state) = state_with(exhausted_week);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                jitter_secs: 0,
                ..PaceConfig::default()
            },
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            &mut false,
        );

        assert!(
            outcome.waited_secs >= 432_000,
            "it must wait out the week, waited {}",
            outcome.waited_secs
        );
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            !printed.contains("proceeding anyway"),
            "the real reset arrived inside the cap, so no valve message: {printed}"
        );
    }

    /// E: a provider with no usage source at all (no per-provider file, and
    /// not claude's own legacy-anthropic fallback) must be skipped outright
    /// -- naming the provider -- rather than entering the loop and reading
    /// "nothing known" as a fresh empty collector reading for this provider.
    /// A totally fresh state dir (no `state_with` at all): the point is that
    /// nothing has ever been written for "openai", not even an empty file.
    #[test]
    fn a_provider_with_no_usage_source_skips_the_gate_and_names_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut announced = false;

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig::default(),
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "openai",
            &mut announced,
        );

        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::None);
        let printed = String::from_utf8_lossy(&out).to_string();
        assert!(printed.contains("openai"), "got {printed}");
        assert!(printed.contains("no usage source"), "got {printed}");
        assert!(
            clock.slept.borrow().is_empty(),
            "must never enter the wait loop"
        );
        assert!(announced, "the latch is set so a caller's next cycle knows");

        // Item 10: a second cycle of the same run (the caller's own
        // `announced_no_source` threaded straight back in, exactly like
        // `exec`'s and `loop`'s own supervise loops do) must not repeat the
        // line or grow `out` at all -- the no-source fact was already stated
        // once for this run.
        let before = out.len();
        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig::default(),
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "openai",
            &mut announced,
        );
        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::None, "still skipped, just quietly");
        assert_eq!(
            out.len(),
            before,
            "a second cycle in the same run prints nothing new: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn unknown_usage_proceeds_without_waiting() {
        let (_tmp, state) = state_with(UsageWindows::default());
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig::default(),
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            &mut false,
        );

        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::None);
    }

    #[test]
    fn pacing_disabled_skips_the_gate_entirely() {
        let exhausted = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                enabled: false,
                ..PaceConfig::default()
            },
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            &mut false,
        );
        assert_eq!(outcome.waited_secs, 0);
        assert!(
            String::from_utf8_lossy(&out).is_empty(),
            "silent when disabled"
        );
    }

    #[test]
    fn the_estimator_is_only_consulted_when_a_budget_is_set() {
        let (_tmp, state) = state_with(UsageWindows::default());
        let cfg = PaceConfig::default();
        let (collector_windows, estimated) = current_windows(&state, &cfg, NOW, "anthropic");
        assert_eq!(collector_windows, UsageWindows::default());
        assert!(
            estimated.is_none(),
            "with both budgets at zero there is nothing to estimate against"
        );

        let with_budget = PaceConfig {
            five_hour_budget_tokens: 1000,
            ..PaceConfig::default()
        };
        let (_, estimated) = current_windows(&state, &with_budget, NOW, "anthropic");
        assert!(
            estimated.is_some(),
            "a configured budget turns the estimator on"
        );
    }
}
