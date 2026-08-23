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
    /// Inside the soft-throttle band (`soft_percent` <= percent <
    /// `max_percent`): not a hard pause, but each cycle is delayed so the
    /// remaining budget spreads linearly over the time left in the window.
    Slow {
        delay_secs: u64,
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
        let band = cfg.max_percent - cfg.soft_percent;
        // Item 3: a nonzero `resets_at` at or before `now` means this
        // reading predates a completed reset -- the window genuinely rolled
        // over, so pacing against its old percentage (via the fallback
        // horizon below) would phantom-throttle a window that is actually
        // back near 0%. Only a truly unknown reset (0, "never reported")
        // still falls to the fallback branch.
        let reset_passed = window.resets_at != 0 && window.resets_at <= now;
        if band > 0.0 && window.used_percentage >= cfg.soft_percent && !reset_passed {
            let t_rem = if window.resets_at > now {
                window.resets_at - now
            } else {
                // Reset unknown (0): pace against the configured fallback
                // horizon.
                cfg.fallback_delay_secs
            };
            let frac = (window.used_percentage - cfg.soft_percent) / band;
            let delay_secs = (t_rem as f64 * frac) as u64;
            if delay_secs > 0 {
                return PaceDecision::Slow {
                    delay_secs,
                    window: name,
                    percent: window.used_percentage,
                    source,
                };
            }
        }
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
    match decision {
        PaceDecision::WaitUntil {
            reset_at, window, ..
        } => {
            let target = match reset_at {
                Some(at) if *at > now => *at,
                _ => now + cfg.fallback_delay_secs,
            };
            let jittered = apply_jitter(target, cfg.jitter_secs, seed);
            Some(jittered.min(now + wait_cap(window, cfg)))
        }
        // No jitter: the delay is already reading-derived, and the wait
        // loop's own monotonic min-tracking keeps it from creeping forward.
        PaceDecision::Slow { delay_secs, .. } => Some(now.saturating_add(*delay_secs)),
        _ => None,
    }
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
        PaceDecision::Slow {
            delay_secs,
            window,
            percent,
            source,
        } => format!(
            "{window} window at {percent:.1}% ({} data), throttling ~{delay_secs}s before the next run",
            source.as_str()
        ),
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

/// Operator-controlled inputs to the gate that are not part of `PaceConfig`
/// itself: resolved per call from the adapter's provider, not read from disk
/// or the environment inside `decide`/`wait_for_window`'s own logic.
#[derive(Clone, Copy)]
pub struct PaceGate<'a> {
    /// Operator declaration that this harness's vendor plan covers overage
    /// from credits: when true, the proactive throttle/pause is skipped
    /// entirely for this call. A vendor-reported limit hit (the caller's own
    /// `scan_for_limit`/`is_limit_hit` path) still parks -- that is a
    /// separate, untouched path.
    pub use_credits: bool,
    /// The active poll fallback for this call. `None` means `wait_for_window`
    /// never polls: polling disabled (`cfg.pace.poll_enabled == false`), or a
    /// caller on a path that must never make an HTTP call (`wrap` does not
    /// call `wait_for_window` at all today, so no caller currently supplies
    /// `None` for that reason -- but the option exists for one that must).
    /// `Some` lets `wait_for_window` fall back to it once the passive
    /// collector reading goes stale.
    pub poller: Option<&'a dyn super::poll::UsagePoller>,
}

/// Once-per-run announce latches for `wait_for_window`, owned by the caller
/// and threaded through every call across one run -- the same discipline the
/// wait loop's own internal `announced` local already follows *within* a
/// single call.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaceGateFlags {
    pub no_source_announced: bool,
    pub credits_announced: bool,
    /// Item 5: unix-seconds of the last codex rollout scan *attempt* made
    /// by `refresh_sources` (`0` means never), floored to
    /// `usage_window::CODEX_SCAN_FLOOR_SECS`.
    pub last_codex_scan: u64,
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
/// Whether `cfg` alone -- regardless of what the collector has ever
/// recorded -- means the estimator layer can contribute a decision: enabled,
/// and at least one window has a nonzero budget configured. Shared by
/// `current_windows` (whether to bother estimating at all) and
/// `wait_for_window`'s no-usage-source check (item 1): a collector-less
/// machine with estimator pacing configured must not read as "no usage
/// source" just because nothing has ever been observed passively.
fn estimator_configured(cfg: &PaceConfig) -> bool {
    cfg.estimator && (cfg.five_hour_budget_tokens > 0 || cfg.seven_day_budget_tokens > 0)
}

pub fn current_windows(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
) -> (UsageWindows, Option<UsageWindows>) {
    let collector = usage_window::load_for(state, provider).unwrap_or_default();

    if !estimator_configured(cfg) {
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

/// Best-effort refresh of both usage sources ahead of a pacing decision:
/// codex's passive rollout scan (`refresh_codex_usage`) plus the active poll
/// fallback (`maybe_poll`), when the caller supplied a poller via
/// `gate.poller`. Both refreshes are already cheap no-ops once the stored
/// reading is fresh -- `refresh_codex_usage`'s own staleness gate,
/// `maybe_poll`'s staleness check plus its `poll_min_interval_secs` floor --
/// so calling this on every loop iteration, not just once up front, costs
/// nothing on the common path where nothing has gone stale since the last
/// call. Never itself an error path: like the rest of this module, a failed
/// refresh just leaves the previously-stored reading in place.
///
/// The sessions directory is resolved here via `crate::utils::home_dir()`
/// (`HOME`/`USERPROFILE`) rather than left to `refresh_codex_usage`'s own
/// internal `dirs::home_dir()` fallback: on Windows, `dirs::home_dir()`
/// calls `SHGetKnownFolderPath` directly and ignores `HOME`/`USERPROFILE`
/// entirely, so a test's `HomeGuard` (env-var based, like every other
/// home-directory override in this crate) could never isolate it. Resolving
/// it the same way the rest of this crate already does keeps production
/// behavior identical on a normal machine (`USERPROFILE`/`HOME` always name
/// the real profile there) while making this call testable. Falling back to
/// `None` only when even that lookup fails, at which point
/// `refresh_codex_usage`'s own fallback would not have found anything usable
/// either.
///
/// Item 5: codex's scan is additionally floored to
/// `usage_window::CODEX_SCAN_FLOOR_SECS` between *attempts*
/// (`flags.last_codex_scan`), independent of `refresh_codex_usage`'s own
/// internal staleness gate (which floors how old a *stored* reading has to
/// be before a scan is worth doing at all, and does nothing for a provider
/// that has never stored a reading in the first place). Without this outer
/// floor, a parked codex session that writes no rollouts never satisfies
/// the inner gate either, so every 30s wait-loop iteration re-walks the
/// whole `~/.codex/sessions` tree for nothing. Shares the same constant
/// `wrap.rs`'s status-bar refresh uses, so the two floors cannot drift.
fn refresh_sources(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
    gate: &PaceGate,
    flags: &mut PaceGateFlags,
) {
    if provider == usage_window::CODEX_USAGE_PROVIDER
        && now.saturating_sub(flags.last_codex_scan) >= usage_window::CODEX_SCAN_FLOOR_SECS
    {
        flags.last_codex_scan = now;
        let sessions_dir = crate::utils::home_dir()
            .ok()
            .map(|h| h.join(".codex").join("sessions"));
        usage_window::refresh_codex_usage(
            state,
            sessions_dir.as_deref(),
            now,
            cfg.collector_max_age_secs,
        );
    }
    if let Some(poller) = gate.poller {
        super::poll::maybe_poll(state, cfg, now, provider, poller);
    }
}

/// Item 4: which `zirv ▸` announcement, if any, a decision should produce.
/// Pulled out of `wait_for_window`'s announce block as a pure mapping so it
/// is unit-testable on its own -- `Announcer::emit` always writes to real
/// stderr (see `Announcer::silent()`'s use in this crate's other tests), so
/// nothing downstream of it is easily assertable in a unit test.
fn pacing_event(decision: &PaceDecision) -> Option<super::announce::Event> {
    match decision {
        PaceDecision::WaitUntil {
            window, reset_at, ..
        } => Some(super::announce::Event::PacingWait {
            window: (*window).to_string(),
            reset_at: *reset_at,
        }),
        PaceDecision::Slow {
            window,
            delay_secs,
            percent,
            ..
        } => Some(super::announce::Event::PacingThrottled {
            window: (*window).to_string(),
            delay_secs: *delay_secs,
            percent: *percent,
        }),
        _ => None,
    }
}

/// T8 (fail-SAFE, not open): whether `decide()`'s `Unknown` verdict reflects
/// genuine blindness -- no binding collector reading and no configured
/// estimator -- rather than a stale-but-still-tracked `Slow` episode
/// surviving a recheck (item 2's own `slow.is_some()` case, which must keep
/// using its latched deadline and `wait_cap`, not this path). Pure and
/// table-testable on its own: this is exactly the branch condition that
/// decides whether a spend gate proceeds blind or applies the fail-safe
/// delay, so it must be checkable with no clock, no state dir, and no
/// `Announcer` involved.
fn is_blind(decision: &PaceDecision, slow_latched: bool) -> bool {
    matches!(decision, PaceDecision::Unknown) && !slow_latched
}

/// T8: what a genuinely blind gate does and tells the operator, shared by
/// the upfront no-source shortcut and the in-loop case where `decide()`
/// itself returns `Unknown` with no latched `Slow` episode to fall back on
/// (`is_blind`) -- a reading that existed once but has since gone stale
/// below the ceiling with nothing fresher reaches this same path, which the
/// upfront shortcut's own `has_no_usage_source` check cannot see on its own
/// (that check only knows "nothing was ever recorded", not "what was
/// recorded is now too old to trust").
///
/// This is the fix for the fail-open gap: previously both call sites simply
/// returned `Proceed` with zero delay, once per cycle, forever -- a
/// supervised loop with no usage data span at full speed with nothing
/// slowing it down. Now every call pays a bounded `cfg.blind_delay_secs`
/// safety delay (small next to `fallback_delay_secs`/`wait_slack_secs` by
/// design -- see `PaceConfig::blind_delay_secs`'s own doc comment), and the
/// operator is told once per run, not once per cycle (`flags.no_source_
/// announced`, the same latch discipline every other once-per-run line in
/// this module already follows) -- but the *delay* is not deduplicated: it
/// applies on every call, since that is the actual safety mechanism, not
/// just the narration of it.
///
/// A `writeln!`/`log::append` failure here degrades exactly like every
/// other decision-logging call in this module: this must never become a
/// hard error, since a spend gate that panics instead of pacing is strictly
/// worse than one that merely logs badly.
#[allow(clippy::too_many_arguments)]
fn blind_wait<W: Write>(
    w: &mut W,
    state: &StateDir,
    cfg: &PaceConfig,
    verb: &'static str,
    session: &str,
    provider: &str,
    announcer: Option<&super::announce::Announcer>,
    sleep_fn: &dyn Fn(Duration),
    flags: &mut PaceGateFlags,
    waited_so_far: u64,
) -> PaceOutcome {
    if !flags.no_source_announced {
        let _ = writeln!(
            w,
            "zirv ctx {verb}: pacing degraded: {provider} has no usage source; applying a \
             {}s safety delay per cycle until data is available (see `zirv ctx status`)",
            cfg.blind_delay_secs
        );
        if let Some(announcer) = announcer {
            announcer.emit(&super::announce::Event::PacingBlind {
                provider: provider.to_string(),
                delay_secs: cfg.blind_delay_secs,
            });
        }
        let _ = log::append(
            state,
            &log::Decision {
                ts: now_secs(),
                session,
                verb,
                verdict: "n/a",
                score: 0,
                action: "pacing-blind",
                detail: "no usage source; applying the blind-mode safety delay instead of \
                         proceeding unthrottled",
            },
        );
        flags.no_source_announced = true;
    }
    sleep_fn(Duration::from_secs(cfg.blind_delay_secs));
    PaceOutcome {
        waited_secs: waited_so_far + cfg.blind_delay_secs,
        source: Source::None,
    }
}

/// T10: what an INTERACTIVE launch (a human sitting in front of it, unlike
/// `wait_for_window`'s headless callers) should show and do, given the same
/// `PaceDecision` the headless gate already computes. A silent wait in front
/// of a person is its own failure -- worse than the spend problem it would
/// be solving -- so this never blocks without saying why, and every wait it
/// describes is one the human can shorten or refuse (never a bare sleep with
/// no way out). Pure: no I/O, no clock beyond what the caller already
/// resolved, so the exact wording and which decisions pause/refuse/launch
/// silently are table-testable without a terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractiveGate {
    /// Real data, comfortably below the soft band (or pacing off/use_credits
    /// declared): launch immediately, nothing shown.
    Launch,
    /// The soft band, or genuinely blind data: show `message`, then a
    /// skippable pause of `seconds` -- any keypress launches immediately,
    /// and the pause elapsing on its own also launches (this is advisory,
    /// not a refusal).
    Pause { message: String, seconds: u64 },
    /// At or above the hard ceiling: show `message` and refuse to launch by
    /// default. Only an explicit, deliberate confirmation launches anyway --
    /// this is not skippable by an idle timeout the way `Pause` is.
    Refuse { message: String },
}

/// The pure decision/message mapping `resolve_interactive_gate` (below)
/// hands off to once it has a `PaceDecision` in hand. Kept separate from the
/// `enabled`/`use_credits` short-circuits (which never reach here at all --
/// they resolve straight to `InteractiveGate::Launch`) so this function's
/// only job is "given a decision, what does a human need to see and do".
/// Named `gate_for_decision` (not `interactive_gate`) so the public
/// `interactive_gate` below -- which assembles the poller/gate/flags a call
/// site used to hand-build itself -- can have the name call sites actually
/// want.
fn gate_for_decision(decision: &PaceDecision, provider: &str, cfg: &PaceConfig) -> InteractiveGate {
    match decision {
        PaceDecision::Proceed { .. } => InteractiveGate::Launch,
        PaceDecision::Slow {
            delay_secs,
            window,
            percent,
            source,
        } => InteractiveGate::Pause {
            message: format!(
                "usage {percent:.1}% of the {window} window ({} data); pausing {delay_secs}s \
                 before launch to spread the remaining budget -- press any key to launch now, or \
                 pass --force-pace to skip this pause automatically",
                source.as_str()
            ),
            seconds: *delay_secs,
        },
        PaceDecision::WaitUntil {
            percent,
            window,
            source,
            reset_at,
        } => {
            let reset = match reset_at {
                Some(at) => format!("resets at unix {at}"),
                None => "reset time unknown".to_string(),
            };
            InteractiveGate::Refuse {
                message: format!(
                    "usage {percent:.1}% of the {window} window ({} data) is at or above the \
                     {:.0}% limit ({reset}); refusing to launch -- press 'y' then Enter to launch \
                     anyway, or pass --force-pace to skip this check",
                    source.as_str(),
                    cfg.max_percent
                ),
            }
        }
        // T8's blind fail-safe delay, made honest in front of a human: the
        // same reason (`usage_source_hint`) and the same bounded delay, but
        // shown and skippable rather than silently slept through.
        PaceDecision::Unknown => InteractiveGate::Pause {
            message: format!(
                "{} -- pausing {}s before launch as a precaution -- press any key to launch now, \
                 or pass --force-pace to skip this pause automatically",
                super::poll::usage_source_hint(provider),
                cfg.blind_delay_secs
            ),
            seconds: cfg.blind_delay_secs,
        },
    }
}

/// T10: resolves what an interactive launch should show/do. A one-shot
/// question, not a loop like `wait_for_window`'s own: an interactive launch
/// asks once, shows the human the answer, and lets *them* decide whether to
/// wait it out, skip it, or (at the ceiling) refuse -- it never loops or
/// blocks silently on its own. `flags` still threads through `refresh_
/// sources` (its `last_codex_scan` floor applies here exactly like it does
/// for the headless gate), but the announce-once latches (`no_source_
/// announced`/`credits_announced`) are deliberately not consulted for the
/// *message* shown here: a human deciding right now needs to see the reason
/// every time, not "trust me, I said this already" from a possibly
/// much-earlier restart.
pub fn resolve_interactive_gate(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
    gate: PaceGate,
    flags: &mut PaceGateFlags,
) -> InteractiveGate {
    if !cfg.enabled || gate.use_credits {
        return InteractiveGate::Launch;
    }
    refresh_sources(state, cfg, now, provider, &gate, flags);
    let (collector, estimated) = current_windows(state, cfg, now, provider);
    let decision = decide(&collector, estimated.as_ref(), now, cfg);
    gate_for_decision(&decision, provider, cfg)
}

/// Pure assembly of the `PaceGate` `interactive_gate` (below) resolves
/// against -- split out so the `poll` flag's effect on `gate.poller` is
/// directly testable without a state dir, a real decision, or a network
/// call. `http_poller` is always constructed by the caller (cheap: it only
/// stores a bool, see `HttpPoller::new`); `poll` and `cfg.poll_enabled`
/// together decide whether it is actually handed to the gate as a live
/// `UsagePoller` or left out entirely. This is where finding 1 (the
/// dashboard's mid-session spawn gate must never carry a live poller onto
/// the UI thread) is enforced, in one place rather than at each call site.
fn build_gate<'a>(
    cfg: &PaceConfig,
    provider: &str,
    poll: bool,
    http_poller: &'a super::poll::HttpPoller,
) -> PaceGate<'a> {
    PaceGate {
        use_credits: cfg.use_credits.for_provider(provider),
        poller: (poll && cfg.poll_enabled).then_some(http_poller as &dyn super::poll::UsagePoller),
    }
}

/// Builds the `HttpPoller`/`PaceGate`/fresh `PaceGateFlags` and resolves the
/// interactive gate in one call. Introduced so the three call sites that
/// used to hand-assemble this block individually (`wrap::run_with`, and
/// `dash/mod.rs`'s two spawn points -- `fulfill_spawn_request` and
/// `run_dashboard`'s own first-pane spawn) cannot drift out of sync with
/// each other again. `poll` is the one difference between call sites: pass
/// `false` on a call site that must never block on a synchronous HTTP/
/// keychain round trip (the dashboard's mid-session spawn path, which runs
/// on its single UI thread) and `true` everywhere a blocking wait is
/// already the point (`wrap`'s own pre-spawn gate, the dashboard's first
/// pane before raw mode is entered).
pub fn interactive_gate(
    state: &StateDir,
    cfg: &super::config::CtxConfig,
    provider: &str,
    poll: bool,
) -> InteractiveGate {
    let http_poller = super::poll::HttpPoller::new(cfg.chrome.events);
    let gate = build_gate(&cfg.pace, provider, poll, &http_poller);
    resolve_interactive_gate(
        state,
        &cfg.pace,
        now_secs(),
        provider,
        gate,
        &mut PaceGateFlags::default(),
    )
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
/// (`"anthropic"` for claude, `"openai"` for codex). Both usage sources are
/// refreshed (`refresh_sources`, below) immediately before the no-source
/// check -- codex's passive rollout scan, plus a poll through `gate.poller`
/// when the caller supplied one -- so a first-ever run can still acquire
/// data before deciding it has none, and again once per loop iteration so a
/// stale reading can be topped up mid-wait. When nothing has been recorded
/// for `provider` (`window::has_no_usage_source`) even after that refresh,
/// the gate is skipped outright with one announcement rather than silently
/// entering the loop below and reading "nothing known" as if it were a
/// fresh, empty collector reading.
///
/// `flags` is owned by the caller and threaded through every call across one
/// run (`exec`'s own supervise loop calls this once per cycle -- the
/// pre-flight check and, on a usage-limit park, again -- and `loop`'s cycle
/// does the same), the same discipline the wait-loop's own internal
/// `announced` local already follows *within* a single call: neither the
/// no-source fact nor the use_credits setting changes cycle to cycle, so
/// without this the skip lines would otherwise repeat on every single
/// restart of a long-running session, drowning out everything else on the
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
    gate: PaceGate<'_>,
    flags: &mut PaceGateFlags,
) -> PaceOutcome {
    if !cfg.enabled {
        return PaceOutcome {
            waited_secs: 0,
            source: Source::None,
        };
    }

    // Short-circuits before any source refresh or decision: an operator
    // declaring that this harness's vendor plan covers overage from credits
    // means the proactive throttle/pause never applies to it. A
    // vendor-reported limit hit still parks via a separate, untouched path.
    if gate.use_credits {
        if !flags.credits_announced {
            let _ = writeln!(
                w,
                "zirv ctx {verb}: pacing: use_credits enabled for this harness, gate skipped"
            );
            let _ = log::append(
                state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb,
                    verdict: "n/a",
                    score: 0,
                    action: "use-credits-skip",
                    detail: "use_credits enabled; throttle/pause skipped for this harness",
                },
            );
            flags.credits_announced = true;
        }
        return PaceOutcome {
            waited_secs: 0,
            source: Source::None,
        };
    }

    refresh_sources(state, cfg, now_fn(), provider, &gate, flags);

    // Item 1: a provider with nothing in the collector is only truly
    // pacing-blind when the estimator cannot fill in either -- otherwise
    // this early return would skip `current_windows` entirely and silently
    // disable an operator's estimator-only pacing setup (no statusline tee,
    // no working poll, but a configured budget).
    if usage_window::has_no_usage_source(state, provider) && !estimator_configured(cfg) {
        return blind_wait(
            w, state, cfg, verb, session, provider, announcer, sleep_fn, flags, 0,
        );
    }

    let started = now_fn();
    let mut announced: Option<(String, Option<u64>)> = None;
    // Item 2: the monotonic Slow deadline, latched together with the window
    // that produced it. Latching the window alongside the deadline (rather
    // than just the deadline alone) is what lets a later staleness-driven
    // `Unknown` recheck keep using the right `wait_cap` too -- see below.
    let mut slow: Option<(u64, &'static str)> = None;

    loop {
        let now = now_fn();
        refresh_sources(state, cfg, now, provider, &gate, flags);
        let (collector, estimated) = current_windows(state, cfg, now, provider);
        let decision = decide(&collector, estimated.as_ref(), now, cfg);

        // T8: a reading that existed when the upfront shortcut ran (so it
        // did not fire) can still go stale *during* the wait -- `binding()`
        // then reads it as unusable, `decide()` returns `Unknown`, and
        // without this check the loop below would fall through to `deadline
        // = None` and return `Proceed` at zero delay, silently reopening the
        // exact fail-open gap the upfront shortcut exists to close. Skipped
        // only when a `Slow` episode is already latched (`slow.is_some()`):
        // that is item 2's own "stale-but-still-tracked" case, which must
        // keep its latched deadline, not fall back to this one.
        if is_blind(&decision, slow.is_some()) {
            return blind_wait(
                w,
                state,
                cfg,
                verb,
                session,
                provider,
                announcer,
                sleep_fn,
                flags,
                now.saturating_sub(started),
            );
        }

        let source = match &decision {
            PaceDecision::Proceed { source, .. } => *source,
            PaceDecision::WaitUntil { source, .. } => *source,
            PaceDecision::Slow { source, .. } => *source,
            PaceDecision::Unknown => Source::None,
        };

        let seed = std::process::id() as u64 ^ now;

        // `WaitUntil`'s deadline is absolute (the window's own reset), so
        // re-deriving it each chunk is stable. A re-derived `Slow` deadline
        // creeps forward every chunk instead (`now + (resets_at - now) *
        // frac` grows as `now` does), which would stretch a soft-band
        // throttle into a full park -- so its deadline is tracked
        // monotonically, and a later, larger recheck may never push it out,
        // only a smaller one may pull it in.
        //
        // Item 2: once a Slow deadline has latched, a recheck that reads as
        // `Unknown` purely because the stored reading crossed
        // `collector_max_age_secs` mid-wait carries no new information --
        // it must not be read as "nothing to wait for" and exit the gate
        // hours before the announced deadline. The latch holds until a
        // `WaitUntil` supersedes it outright (a hard pause is a stronger
        // signal than a soft throttle) or fresh data recomputes a smaller
        // `Slow` deadline via the min rule above.
        let deadline = match &decision {
            PaceDecision::Slow { window, .. } => {
                let cand = wait_deadline(&decision, now, cfg, seed);
                let resolved = match (slow, cand) {
                    (Some((prev, _)), Some(c)) => prev.min(c),
                    (None, Some(c)) => c,
                    (Some((prev, _)), None) => prev,
                    // Unreachable in practice: `wait_deadline` always
                    // returns `Some` for a `Slow` decision.
                    (None, None) => now,
                };
                slow = Some((resolved, window));
                Some(resolved)
            }
            PaceDecision::Unknown if slow.is_some() => slow.map(|(deadline, _)| deadline),
            _ => {
                slow = None;
                wait_deadline(&decision, now, cfg, seed)
            }
        };
        let Some(deadline) = deadline else {
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        };

        // The safety valve, scaled to the window that tripped: a seven-day trip
        // may legitimately wait days, a five-hour trip may not.
        let cap = match &decision {
            PaceDecision::WaitUntil { window, .. } | PaceDecision::Slow { window, .. } => {
                wait_cap(window, cfg)
            }
            // A latched Slow episode surviving a stale-data `Unknown`
            // recheck (item 2) still needs the cap of the window that
            // latched it, not the bare `0` a plain `Unknown` gets --
            // otherwise the very next line's cap check would immediately
            // read "already over cap" and exit early regardless of the
            // deadline fix above.
            PaceDecision::Unknown if slow.is_some() => {
                wait_cap(slow.map(|(_, window)| window).unwrap_or("five_hour"), cfg)
            }
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
        // lines and scroll the operator's terminal for days. `Slow` is keyed
        // on the window alone (not the shrinking delay/percent), so one
        // throttle episode announces once rather than on every recheck. A
        // latched-but-currently-`Unknown` recheck (item 2) reuses the same
        // `slow:<window>` fingerprint it had while still reading `Slow`, so
        // it is silently absorbed by the `announced == fingerprint` check
        // below rather than re-announcing (or misreporting the episode as
        // "usage state unknown, proceeding").
        let fingerprint = match &decision {
            PaceDecision::WaitUntil {
                window, reset_at, ..
            } => Some(((*window).to_string(), *reset_at)),
            PaceDecision::Slow { window, .. } => Some((format!("slow:{window}"), None)),
            PaceDecision::Unknown if slow.is_some() => {
                slow.map(|(_, window)| (format!("slow:{window}"), None))
            }
            _ => None,
        };
        if announced != fingerprint {
            announced = fingerprint;
            let _ = writeln!(w, "zirv ctx {verb}: {}", describe(&decision));
            // Item 4: `Slow` used to be invisible on the `zirv ▸` channel --
            // only `WaitUntil` ever reached `announcer.emit`, so a
            // potentially hours-long soft throttle produced no announcement
            // at all. See `pacing_event` for the actual mapping.
            if let (Some(announcer), Some(event)) = (announcer, pacing_event(&decision)) {
                announcer.emit(&event);
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

    fn collector_with_reset(percent: f64, resets_at: u64) -> UsageWindows {
        UsageWindows {
            five_hour: window(percent, resets_at, NOW - 10),
            seven_day: None,
        }
    }

    #[test]
    fn below_soft_percent_proceeds_unthrottled() {
        let d = decide(&collector(79.0), None, NOW, &PaceConfig::default());
        assert!(matches!(d, PaceDecision::Proceed { .. }));
    }

    #[test]
    fn inside_the_band_slows_proportionally_to_time_left() {
        // soft 80, max 99: at 90% the band fraction is 10/19. Reset in 1900s
        // -> delay = 1900 * 10/19 = 1000.
        let w = collector_with_reset(90.0, NOW + 1900);
        let d = decide(&w, None, NOW, &PaceConfig::default());
        assert_eq!(
            d,
            PaceDecision::Slow {
                delay_secs: 1000,
                window: "five_hour",
                percent: 90.0,
                source: Source::Collector
            }
        );
    }

    #[test]
    fn near_reset_the_slow_delay_shrinks_toward_zero() {
        let w = collector_with_reset(90.0, NOW + 1); // 1s left: delay rounds to 0 -> Proceed
        assert!(matches!(
            decide(&w, None, NOW, &PaceConfig::default()),
            PaceDecision::Proceed { .. }
        ));
    }

    #[test]
    fn at_max_percent_the_hard_pause_still_wins() {
        let d = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        assert!(matches!(d, PaceDecision::WaitUntil { .. }));
    }

    #[test]
    fn unknown_reset_time_slows_by_the_fallback_delay() {
        // resets_at == 0 inside the band: t_rem stands in as fallback_delay_secs (900)
        // at 90%: 900 * 10/19 = 473.
        let w = collector_with_reset(90.0, 0);
        let d = decide(&w, None, NOW, &PaceConfig::default());
        assert!(matches!(
            d,
            PaceDecision::Slow {
                delay_secs: 473,
                ..
            }
        ));
    }

    #[test]
    fn an_empty_band_disables_the_throttle() {
        let cfg = PaceConfig {
            soft_percent: 99.0,
            ..PaceConfig::default()
        }; // == max
        assert!(matches!(
            decide(&collector(98.0), None, NOW, &cfg),
            PaceDecision::Proceed { .. }
        ));
    }

    #[test]
    fn use_credits_skips_the_gate_entirely() {
        let exhausted = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();

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
            PaceGate {
                use_credits: true,
                poller: None,
            },
            &mut flags,
        );

        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::None);
        assert!(
            clock.slept.borrow().is_empty(),
            "use_credits must never enter the wait loop"
        );
    }

    #[test]
    fn a_slow_wait_does_not_extend_itself_across_rechecks() {
        // 90% with soft 80/max 99: first computed delay is 1000s (see
        // inside_the_band_slows_proportionally_to_time_left). Re-deriving the
        // decision every 30s chunk must not stretch the wait toward the full
        // reset -- the monotonic slow_deadline must hold the first value.
        let slow = collector_with_reset(90.0, NOW + 1900);
        let (_tmp, state) = state_with(slow);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();

        let cfg = PaceConfig {
            jitter_secs: 0,
            // Item 2: the stored reading (observed 10s before `NOW`, see
            // `collector_with_reset`) must stay fresh for the whole ~1000s
            // wait, or the staleness gate rescues this test at ~890s and it
            // stops proving anything about the monotonic rule at all --
            // which is exactly what the masked version of this test used
            // to do.
            collector_max_age_secs: 3000,
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut flags,
        );

        let total_slept: u64 = clock.slept.borrow().iter().sum();
        assert!(
            total_slept <= 1000 + SLEEP_CHUNK_SECS,
            "slept {total_slept}s, wanted <= 1000 + {SLEEP_CHUNK_SECS}"
        );
        assert!(
            outcome.waited_secs <= 1000 + SLEEP_CHUNK_SECS,
            "waited {}s",
            outcome.waited_secs
        );
        assert!(
            outcome.waited_secs >= 1000,
            "the wait must reach the latched deadline in full now that the \
             reading stays fresh, waited {}s",
            outcome.waited_secs
        );
    }

    /// Item 2: once a Slow deadline has latched, a reading that goes stale
    /// mid-wait (crossing `collector_max_age_secs`, which `decide` then
    /// reads as `Unknown`) must not truncate the wait to roughly
    /// `collector_max_age_secs` -- the gate must keep waiting until the
    /// latched deadline, exactly as if the reading had stayed fresh.
    #[test]
    fn a_latched_slow_deadline_survives_the_reading_going_stale_mid_wait() {
        // Same 90%/reset-in-1900s shape as the test above (delay_secs =
        // 1000), but with `collector_max_age_secs` tight enough that the
        // stored reading goes stale well before that deadline arrives.
        let slow = collector_with_reset(90.0, NOW + 1900);
        let (_tmp, state) = state_with(slow);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();

        let cfg = PaceConfig {
            jitter_secs: 0,
            collector_max_age_secs: 200,
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut flags,
        );

        assert!(
            outcome.waited_secs >= 1000,
            "the latched deadline must survive the reading going stale \
             mid-wait, waited only {}s (~collector_max_age_secs would be \
             the old, truncated behavior)",
            outcome.waited_secs
        );
        assert!(
            outcome.waited_secs <= 1000 + SLEEP_CHUNK_SECS,
            "waited {}s",
            outcome.waited_secs
        );
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
        // With the default soft-throttle band (soft 80, max 99) 98.9% falls
        // inside the band, so this is now a throttled `Slow`, not a flat
        // `Proceed` -- but it is still not the hard pause `WaitUntil` gives
        // at/above the ceiling.
        let decision = decide(&collector(98.9), None, NOW, &PaceConfig::default());
        assert!(matches!(decision, PaceDecision::Slow { .. }));
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

    /// Item 3: a fresh reading inside the soft-throttle band whose own
    /// `resets_at` has already passed must not phantom-throttle -- the
    /// window genuinely rolled over, so this stale-looking percentage no
    /// longer describes the current window.
    #[test]
    fn a_reading_after_a_genuine_reset_does_not_phantom_throttle() {
        let w = collector_with_reset(95.0, NOW - 120);
        let d = decide(&w, None, NOW, &PaceConfig::default());
        assert_eq!(
            d,
            PaceDecision::Proceed {
                source: Source::Collector,
                worst_percent: 95.0
            },
            "a completed reset must not pace against its own stale percentage"
        );
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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

    /// Item 4: `Slow` used to be invisible on the `zirv ▸` channel -- only
    /// `WaitUntil` ever mapped to an announcement. This is the direct,
    /// mutation-sensitive proof of the fix: `pacing_event`'s mapping, with
    /// no need to capture real stderr.
    #[test]
    fn pacing_event_maps_wait_until_and_slow_and_nothing_else() {
        let wait = PaceDecision::WaitUntil {
            reset_at: Some(NOW + 600),
            window: "five_hour",
            percent: 99.0,
            source: Source::Collector,
        };
        assert_eq!(
            pacing_event(&wait),
            Some(crate::commands::ctx::announce::Event::PacingWait {
                window: "five_hour".to_string(),
                reset_at: Some(NOW + 600),
            })
        );

        let slow = PaceDecision::Slow {
            delay_secs: 473,
            window: "five_hour",
            percent: 90.0,
            source: Source::Collector,
        };
        assert_eq!(
            pacing_event(&slow),
            Some(crate::commands::ctx::announce::Event::PacingThrottled {
                window: "five_hour".to_string(),
                delay_secs: 473,
                percent: 90.0,
            })
        );

        assert_eq!(pacing_event(&PaceDecision::Unknown), None);
        assert_eq!(
            pacing_event(&PaceDecision::Proceed {
                source: Source::Collector,
                worst_percent: 1.0
            }),
            None
        );
    }

    /// T8 (fail-SAFE, not open): the exact branch condition deciding whether
    /// a genuinely data-less gate degrades to the bounded safety delay or
    /// falls through to item 2's own latched-`Slow`-survives-staleness path.
    /// Pure and table-testable with no clock, state dir, or `Announcer`.
    #[test]
    fn is_blind_is_true_only_for_unknown_with_no_slow_episode_latched() {
        assert!(
            is_blind(&PaceDecision::Unknown, false),
            "no binding data and nothing latched: genuinely blind"
        );
        assert!(
            !is_blind(&PaceDecision::Unknown, true),
            "item 2's own case: a latched Slow surviving a stale recheck must \
             keep its own deadline, not fall back to the blind delay"
        );
        assert!(
            !is_blind(
                &PaceDecision::Proceed {
                    source: Source::Collector,
                    worst_percent: 10.0
                },
                false
            ),
            "real data exists: not blind"
        );
        assert!(
            !is_blind(
                &PaceDecision::WaitUntil {
                    reset_at: Some(NOW + 600),
                    window: "five_hour",
                    percent: 99.0,
                    source: Source::Collector,
                },
                false
            ),
            "a hard pause is real data, not blindness"
        );
        assert!(
            !is_blind(
                &PaceDecision::Slow {
                    delay_secs: 100,
                    window: "five_hour",
                    percent: 90.0,
                    source: Source::Collector,
                },
                false
            ),
            "a soft throttle is real data, not blindness"
        );
    }

    /// T10: the pure decision/message mapping an interactive launch uses --
    /// no terminal, no I/O, just the same `PaceDecision` the headless gate
    /// already computes. Pins the exact override wording verbatim, since a
    /// human reading this message is the whole point of the fix.
    #[test]
    fn interactive_gate_maps_each_decision_to_the_right_shape_and_names_the_override() {
        let cfg = PaceConfig::default();

        assert_eq!(
            gate_for_decision(
                &PaceDecision::Proceed {
                    source: Source::Collector,
                    worst_percent: 10.0
                },
                "anthropic",
                &cfg
            ),
            InteractiveGate::Launch,
            "healthy usage launches with nothing shown"
        );

        let slow = PaceDecision::Slow {
            delay_secs: 42,
            window: "five_hour",
            percent: 85.0,
            source: Source::Collector,
        };
        match gate_for_decision(&slow, "anthropic", &cfg) {
            InteractiveGate::Pause { message, seconds } => {
                assert_eq!(seconds, 42);
                assert!(message.contains("85.0%"), "got {message}");
                assert!(message.contains("five_hour"), "got {message}");
                assert!(message.contains("press any key"), "got {message}");
                assert!(message.contains("--force-pace"), "got {message}");
            }
            other => panic!("soft band must Pause, got {other:?}"),
        }

        let wait = PaceDecision::WaitUntil {
            reset_at: Some(1_785_507_915),
            window: "seven_day",
            percent: 99.5,
            source: Source::Collector,
        };
        match gate_for_decision(&wait, "anthropic", &cfg) {
            InteractiveGate::Refuse { message } => {
                assert!(message.contains("99.5%"), "got {message}");
                assert!(message.contains("seven_day"), "got {message}");
                assert!(message.contains("refusing to launch"), "got {message}");
                assert!(message.contains("press 'y' then Enter"), "got {message}");
                assert!(message.contains("--force-pace"), "got {message}");
                assert!(message.contains("1785507915"), "got {message}");
            }
            other => panic!("the hard ceiling must Refuse, got {other:?}"),
        }

        match gate_for_decision(&PaceDecision::Unknown, "anthropic", &cfg) {
            InteractiveGate::Pause { message, seconds } => {
                assert_eq!(seconds, cfg.blind_delay_secs);
                assert!(
                    message.contains("zirv ctx status") || message.contains("statusline tee"),
                    "reuses usage_source_hint's own reason/remedy: {message}"
                );
                assert!(message.contains("press any key"), "got {message}");
                assert!(message.contains("--force-pace"), "got {message}");
            }
            other => {
                panic!("blind data must Pause (not silently inherit the delay), got {other:?}")
            }
        }
    }

    /// T10: `resolve_interactive_gate`'s own short-circuits -- disabled
    /// pacing and an operator's `use_credits` declaration both launch with
    /// nothing shown, exactly like the headless gate's own early returns,
    /// without ever reaching `decide()` at all.
    #[test]
    fn resolve_interactive_gate_launches_silently_when_disabled_or_credits_cover_it() {
        let exhausted = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);

        let disabled = resolve_interactive_gate(
            &state,
            &PaceConfig {
                enabled: false,
                ..PaceConfig::default()
            },
            NOW,
            "anthropic",
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
        );
        assert_eq!(disabled, InteractiveGate::Launch);

        let credits = resolve_interactive_gate(
            &state,
            &PaceConfig::default(),
            NOW,
            "anthropic",
            PaceGate {
                use_credits: true,
                poller: None,
            },
            &mut PaceGateFlags::default(),
        );
        assert_eq!(
            credits,
            InteractiveGate::Launch,
            "an operator's own use_credits declaration is never second-guessed"
        );
    }

    /// Finding 1 (review): the dashboard's mid-session spawn gate
    /// (`fulfill_spawn_request`) must never carry a live `HttpPoller` onto
    /// the dashboard's single UI thread -- a synchronous ureq request or a
    /// macOS Keychain shell-out would freeze every pane and all input.
    /// `build_gate(.., poll: false, ..)` is how that call site enforces it;
    /// pinned here directly against the `PaceGate` it produces, without a
    /// state dir or a real decision.
    #[test]
    fn build_gate_carries_no_poller_for_the_dashboards_mid_session_spawn_path() {
        let cfg = PaceConfig::default();
        let http_poller = crate::commands::ctx::poll::HttpPoller::new(false);

        let gate = build_gate(&cfg, "anthropic", false, &http_poller);
        assert!(
            gate.poller.is_none(),
            "poll: false must never hand a live poller to the gate"
        );
    }

    /// The mirror case: a call site that passes `poll: true` (`wrap`'s own
    /// pre-spawn gate, the dashboard's first pane before raw mode) still
    /// gets a live poller, as long as `cfg.poll_enabled` allows it -- so
    /// `poll: false` above is a deliberate per-call-site choice, not
    /// `build_gate` silently dropping every poller.
    #[test]
    fn build_gate_carries_a_poller_when_poll_is_true_and_polling_is_enabled() {
        let cfg = PaceConfig {
            poll_enabled: true,
            ..PaceConfig::default()
        };
        let http_poller = crate::commands::ctx::poll::HttpPoller::new(false);

        let gate = build_gate(&cfg, "anthropic", true, &http_poller);
        assert!(gate.poller.is_some());

        let disabled_cfg = PaceConfig {
            poll_enabled: false,
            ..PaceConfig::default()
        };
        let gate = build_gate(&disabled_cfg, "anthropic", true, &http_poller);
        assert!(
            gate.poller.is_none(),
            "cfg.poll_enabled = false must still win even when the call site asks for poll: true"
        );
    }

    /// Item 4: the plain writer `w` and the announcer are gated by the
    /// exact same `announced != fingerprint` check, so counting the
    /// writer's "throttling" lines is an honest proxy for "the announce arm
    /// fired once, not once per 30s recheck" without needing to capture
    /// real stderr (this crate's other announcer tests use
    /// `Announcer::silent()` for the same reason -- see `wrap.rs`'s
    /// supervision tests).
    #[test]
    fn a_slow_pass_announces_once_not_per_recheck() {
        let slow = collector_with_reset(90.0, NOW + 1900);
        let (_tmp, state) = state_with(slow);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();
        let announcer = crate::commands::ctx::announce::Announcer::silent();

        let cfg = PaceConfig {
            jitter_secs: 0,
            max_wait_secs: Some(60),
            ..PaceConfig::default()
        };
        let _outcome = wait_for_window(
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
            Some(&announcer),
            "anthropic",
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut flags,
        );

        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(
            printed.lines().filter(|l| l.contains("throttling")).count(),
            1,
            "one throttle announcement per latched episode, not once per recheck: {printed}"
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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
    /// nothing has ever been written for this provider, not even an empty
    /// file.
    ///
    /// Uses a synthetic provider name rather than the real
    /// `CODEX_USAGE_PROVIDER` ("openai"): since Task 6, `wait_for_window`
    /// refreshes codex's source via `refresh_codex_usage(state, None, ...)`
    /// before this very check, and that `None` falls all the way through to
    /// `dirs::home_dir()` -- a live Win32 `SHGetKnownFolderPath` call on
    /// Windows that ignores `HOME`/`USERPROFILE` overrides entirely (unlike
    /// `crate::utils::home_dir()`, used elsewhere in this crate specifically
    /// for testability). A real `~/.codex/sessions` on the machine running
    /// this test would make the premise -- "nothing has ever been recorded"
    /// -- false out from under it. `poller: None` plus a provider that is
    /// neither the codex nor the legacy-anthropic name keeps this
    /// deterministic regardless of what the real machine has on disk, while
    /// still exercising the same generic skip path a real unknown provider
    /// would take.
    /// T8 (fail-SAFE, not open): a provider with nothing ever recorded no
    /// longer skips the gate at zero delay -- that was the fail-open bug.
    /// It now pays `cfg.blind_delay_secs` every cycle (the actual safety
    /// mechanism) while the *narration* still dedupes to once per run (item
    /// 10's own discipline, unchanged).
    #[test]
    fn a_provider_with_no_usage_source_pays_the_blind_delay_every_cycle_and_names_it_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();
        let cfg = PaceConfig::default();

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
            "example-vendor",
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut flags,
        );

        assert_eq!(outcome.waited_secs, cfg.blind_delay_secs);
        assert_eq!(outcome.source, Source::None);
        let printed = String::from_utf8_lossy(&out).to_string();
        assert!(printed.contains("example-vendor"), "got {printed}");
        assert!(printed.contains("no usage source"), "got {printed}");
        assert!(
            printed.contains("zirv ctx status"),
            "the remedy is pointed at: {printed}"
        );
        assert!(
            !printed.to_lowercase().contains("pacing off"),
            "must read as degraded, not off: {printed}"
        );
        assert_eq!(
            clock.slept.borrow().as_slice(),
            &[cfg.blind_delay_secs],
            "the fail-safe delay must actually be slept, not just claimed"
        );
        assert!(
            flags.no_source_announced,
            "the latch is set so a caller's next cycle knows"
        );

        // Item 10: a second cycle of the same run (the caller's own
        // `flags` threaded straight back in, exactly like `exec`'s and
        // `loop`'s own supervise loops do) must not repeat the *line*, but
        // must still pay the delay -- narration dedupes, the safety
        // mechanism itself does not.
        let before = out.len();
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
            "example-vendor",
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut flags,
        );
        assert_eq!(outcome.waited_secs, cfg.blind_delay_secs, "still throttled");
        assert_eq!(outcome.source, Source::None);
        assert_eq!(
            out.len(),
            before,
            "a second cycle in the same run prints nothing new: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(
            clock.slept.borrow().as_slice(),
            &[cfg.blind_delay_secs, cfg.blind_delay_secs],
            "every cycle pays the delay even once the narration has gone quiet"
        );
    }

    /// Item 1: a machine with no collector at all (no statusline tee, no
    /// working poll) but a configured estimator budget must not take the
    /// no-usage-source early return -- that would silently disable
    /// estimator-only pacing and, via `exec.rs`'s vendor-limit park, spin
    /// the caller in a zero-wait hot loop. `estimator_configured` is what
    /// carves the exemption; this proves it reaches all the way through
    /// `wait_for_window`.
    #[test]
    fn estimator_only_pacing_engages_when_the_collector_has_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let home = tempfile::tempdir().expect("home tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        // A recent transcript whose usage alone, against a configured
        // budget, pushes the five_hour estimate to (and past) the ceiling --
        // with nothing at all recorded in the collector.
        let event_ts = "2026-08-16T20:00:00Z";
        let event_time = window::parse_iso8601_utc(event_ts).expect("test timestamp parses");
        let now = event_time + 10;

        let projects_dir = home.path().join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&projects_dir).expect("mkdir");
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{event_ts}","message":{{"usage":{{"input_tokens":2000}}}}}}"#
        );
        std::fs::write(projects_dir.join("t.jsonl"), format!("{line}\n")).expect("write");

        let clock = FakeClock::new(now);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();

        let cfg = PaceConfig {
            jitter_secs: 0,
            max_wait_secs: Some(60),
            five_hour_budget_tokens: 1000,
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut flags,
        );

        assert!(
            !flags.no_source_announced,
            "estimator pacing configured must not take the no-source skip path"
        );
        let printed = String::from_utf8_lossy(&out).to_string();
        assert!(
            !printed.contains("no usage source"),
            "must not print the skip line: {printed}"
        );
        assert!(
            outcome.waited_secs > 0,
            "estimator-only pacing must actually pace, not zero-wait hot loop, got {}",
            outcome.waited_secs
        );
    }

    /// T8: a recorded-but-empty `UsageWindows` (a file exists, so the
    /// upfront `has_no_usage_source` shortcut does not fire, but neither
    /// sub-window has ever been observed) still reaches `decide()`'s own
    /// `Unknown` verdict inside the loop -- this is the path the upfront
    /// shortcut structurally cannot see (it only knows "nothing was ever
    /// recorded", not "what exists has nothing usable in it"), and it must
    /// degrade the same fail-SAFE way, not proceed at zero delay.
    #[test]
    fn genuinely_unknown_usage_pays_the_blind_delay_rather_than_proceeding_free() {
        let (_tmp, state) = state_with(UsageWindows::default());
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let cfg = PaceConfig::default();

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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
        );

        assert_eq!(outcome.waited_secs, cfg.blind_delay_secs);
        assert_eq!(outcome.source, Source::None);
        assert_eq!(clock.slept.borrow().as_slice(), &[cfg.blind_delay_secs]);
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
            PaceGate {
                use_credits: false,
                poller: None,
            },
            &mut PaceGateFlags::default(),
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

    struct StubPoller {
        calls: RefCell<u32>,
        reading: Option<crate::commands::ctx::poll::PollReading>,
    }

    impl crate::commands::ctx::poll::UsagePoller for StubPoller {
        fn poll(&self, _provider: &str) -> Option<crate::commands::ctx::poll::PollReading> {
            *self.calls.borrow_mut() += 1;
            self.reading.clone()
        }
    }

    #[test]
    fn the_gate_polls_only_when_the_stored_reading_is_stale() {
        let cfg = PaceConfig::default();

        // Stale stored reading (well past collector_max_age_secs) and above
        // the ceiling, so a failure to refresh would still park: the poller
        // returns a fresh below-soft reading and the gate proceeds without
        // waiting -- proof the poll's result actually drove the decision.
        let stale = UsageWindows {
            five_hour: window(95.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        let (_tmp, state) = state_with(stale);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();
        let poller = StubPoller {
            calls: RefCell::new(0),
            reading: Some(crate::commands::ctx::poll::PollReading {
                windows: UsageWindows {
                    five_hour: window(10.0, NOW + 600, NOW),
                    seven_day: None,
                },
                vendor_credits_enabled: None,
            }),
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
            PaceGate {
                use_credits: false,
                poller: Some(&poller),
            },
            &mut flags,
        );

        assert_eq!(
            *poller.calls.borrow(),
            1,
            "the stale reading triggers exactly one poll"
        );
        assert_eq!(
            outcome.waited_secs, 0,
            "the poll's fresh below-soft reading lets the gate proceed"
        );
        assert!(clock.slept.borrow().is_empty());

        // Fresh stored reading: the poller must never be consulted at all.
        let fresh = UsageWindows {
            five_hour: window(10.0, NOW + 600, NOW - 10),
            seven_day: None,
        };
        let (_tmp2, state2) = state_with(fresh);
        let clock2 = FakeClock::new(NOW);
        let poller2 = StubPoller {
            calls: RefCell::new(0),
            reading: None,
        };
        let mut flags2 = PaceGateFlags::default();
        let mut out2 = Vec::new();

        let outcome2 = wait_for_window(
            &mut out2,
            &state2,
            &cfg,
            "loop",
            "sess",
            &|| *clock2.now.borrow(),
            &|d| *clock2.now.borrow_mut() += d.as_secs(),
            None,
            "anthropic",
            PaceGate {
                use_credits: false,
                poller: Some(&poller2),
            },
            &mut flags2,
        );

        assert_eq!(
            *poller2.calls.borrow(),
            0,
            "a fresh stored reading needs no poll"
        );
        assert_eq!(outcome2.waited_secs, 0);
    }

    #[test]
    fn a_failing_poller_leaves_the_gate_on_passive_data() {
        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        // Stale, but at/above the ceiling with a still-future reset: this
        // binds via the existing `binding` rule regardless of staleness (see
        // `a_stale_full_window_keeps_binding_until_its_reset_arrives`), so a
        // poller that can never produce data must not change the outcome.
        let stale_but_full = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        let (_tmp, state) = state_with(stale_but_full);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();
        let mut flags = PaceGateFlags::default();
        let poller = StubPoller {
            calls: RefCell::new(0),
            reading: None,
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
            PaceGate {
                use_credits: false,
                poller: Some(&poller),
            },
            &mut flags,
        );

        assert!(
            *poller.calls.borrow() >= 1,
            "the failing poller was actually consulted"
        );
        assert!(
            outcome.waited_secs >= 600,
            "still parks on the stale-but-binding reading exactly as today, waited {}",
            outcome.waited_secs
        );
    }

    /// Item 5: `refresh_sources` floors codex scan *attempts* to
    /// `window::CODEX_SCAN_FLOOR_SECS`, independent of
    /// `refresh_codex_usage`'s own internal staleness gate. `cfg
    /// .collector_max_age_secs` is set to `1` here specifically so that
    /// inner gate never blocks a rescan on its own -- isolating the outer
    /// floor as the only thing that can explain a skipped scan in this
    /// test.
    #[test]
    fn refresh_sources_floors_codex_scan_attempts_to_the_shared_constant() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let home = tempfile::tempdir().expect("home tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let sessions_dir = home.path().join(".codex").join("sessions");
        let day = sessions_dir.join("2026").join("03").join("01");
        std::fs::create_dir_all(&day).expect("mkdir");

        let ts1 = "2026-03-01T10:00:00Z";
        let ts2 = "2026-03-01T10:05:00Z";
        let t1 = window::parse_iso8601_utc(ts1).expect("ts1 parses");
        let t2 = window::parse_iso8601_utc(ts2).expect("ts2 parses");

        std::fs::write(
            day.join("a.jsonl"),
            format!(
                r#"{{"timestamp":"{ts1}","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"primary":{{"used_percent":12.0,"window_minutes":300,"resets_at":{}}}}}}}}}
"#,
                t1 + 100_000
            ),
        )
        .expect("write a");

        let cfg = PaceConfig {
            collector_max_age_secs: 1,
            ..PaceConfig::default()
        };
        let gate = PaceGate {
            use_credits: false,
            poller: None,
        };
        let mut flags = PaceGateFlags::default();
        let first_now = t1 + 60;

        // Call 1: nothing scanned yet (`last_codex_scan == 0`), so the floor
        // is open regardless of how large `now` is.
        refresh_sources(
            &state,
            &cfg,
            first_now,
            window::CODEX_USAGE_PROVIDER,
            &gate,
            &mut flags,
        );
        let after_first = window::load_for(&state, window::CODEX_USAGE_PROVIDER)
            .and_then(|w| w.five_hour)
            .expect("stored after first scan");
        assert_eq!(after_first.used_percentage, 12.0, "first scan finds file a");
        assert_eq!(flags.last_codex_scan, first_now);

        // A second, fresher rollout now appears on disk -- but the next
        // call lands only 30s later, under the 60s floor.
        std::fs::write(
            day.join("b.jsonl"),
            format!(
                r#"{{"timestamp":"{ts2}","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"primary":{{"used_percent":99.0,"window_minutes":300,"resets_at":{}}}}}}}}}
"#,
                t2 + 100_000
            ),
        )
        .expect("write b");

        refresh_sources(
            &state,
            &cfg,
            first_now + 30,
            window::CODEX_USAGE_PROVIDER,
            &gate,
            &mut flags,
        );
        let after_second = window::load_for(&state, window::CODEX_USAGE_PROVIDER)
            .and_then(|w| w.five_hour)
            .expect("still stored");
        assert_eq!(
            after_second.used_percentage, 12.0,
            "30s < the 60s floor: the second file must not have been picked up yet"
        );
        assert_eq!(
            flags.last_codex_scan, first_now,
            "a floored attempt must not move the last-scan clock"
        );

        // A third call, 90s after the first (>= the 60s floor since the
        // last actual scan), is due again and picks up the fresher file.
        refresh_sources(
            &state,
            &cfg,
            first_now + 90,
            window::CODEX_USAGE_PROVIDER,
            &gate,
            &mut flags,
        );
        let after_third = window::load_for(&state, window::CODEX_USAGE_PROVIDER)
            .and_then(|w| w.five_hour)
            .expect("still stored");
        assert_eq!(
            after_third.used_percentage, 99.0,
            "once the floor has elapsed, the newer file is picked up"
        );
        assert_eq!(flags.last_codex_scan, first_now + 90);
    }
}
