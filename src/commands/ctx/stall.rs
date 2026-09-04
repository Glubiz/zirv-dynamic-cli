//! Progress-based liveness (issue #310, 3a): `sessions::Liveness` is a bare
//! `kill(pid, 0)` probe, so a hung agent that is still scheduling CPU but
//! producing nothing -- or wedged inside a tool call -- reads as `Live`
//! forever. This module turns the progress signals zirv already observes
//! (PTY output, a turn-signal/transcript-growth boundary, mail activity)
//! into a stall verdict and a once-only latch/nudge/terminate decision.
//!
//! Pure end to end, mirroring `rot.rs`'s own discipline: every function here
//! takes `now`/`signals` as parameters and never reads a clock, the
//! filesystem, or the environment. I/O -- reading the real signals, writing
//! the latch marker, sending the steering nudge, terminating the child --
//! lives entirely in the caller (`exec.rs`'s `supervise_run`).

use std::time::{Duration, Instant};

/// The most recent moment each progress signal this module knows about last
/// advanced. `None` means "never observed yet", which must never be read as
/// evidence of a stall on its own -- see [`ProgressSignals::baseline`].
#[derive(Debug, Clone, Copy)]
pub struct ProgressSignals {
    /// When this boot's own supervision started -- the fallback baseline for
    /// a session that has not yet produced a single signal of any kind (a
    /// slow launch, or an adapter with no turn-signal mechanism that also
    /// has not written a byte yet). Never `None`: every boot has a start.
    pub started_at: Instant,
    /// Last time new PTY/stdout bytes were observed.
    pub last_output: Option<Instant>,
    /// Last time a turn boundary (Stop hook / transcript growth signal) for
    /// THIS session was observed.
    pub last_turn: Option<Instant>,
    /// Last time this session's mailbox activity changed (new mail arrived
    /// or was consumed).
    pub last_mail: Option<Instant>,
}

impl ProgressSignals {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            last_output: None,
            last_turn: None,
            last_mail: None,
        }
    }

    /// The latest of every signal that has ever fired, or `started_at` when
    /// none has. This is the instant the stall clock is measured from --
    /// using the boot's own start rather than treating "no signal yet" as
    /// "just progressed" is what lets a session that never produces a single
    /// byte still trip `idle_no_tool` instead of living forever.
    pub fn baseline(&self) -> Instant {
        [self.last_output, self.last_turn, self.last_mail]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(self.started_at)
    }
}

/// Whether the session is inside a tool call right now, which selects
/// between the two configured thresholds: a stuck tool call is expected to
/// run longer than idle "thinking" time before it counts as a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    // Not yet constructed by any production caller: `exec.rs` has no live
    // tool-call boundary tracking (`IncrementalScorer` does not expose its
    // parsed events), so it always passes `InTool`, the conservative
    // direction -- see its own tick closure doc comment. Exercised directly
    // by this module's own unit tests below, which is what this feature's
    // pure decision core is tested against regardless of which caller wires
    // it up.
    #[allow(dead_code)]
    Idle,
    InTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressVerdict {
    Progressing,
    Stalled,
}

/// Pure: has the progress clock run past its threshold. Never fires while
/// any signal has advanced within the window -- `signals.baseline()` is
/// always the latest of every signal this module knows about, so a session
/// producing steady output (or turn signals, or mail activity) never trips
/// this regardless of total runtime.
pub fn evaluate_progress(
    signals: &ProgressSignals,
    tool_state: ToolState,
    now: Instant,
    idle_no_tool: Duration,
    in_tool: Duration,
) -> ProgressVerdict {
    let threshold = match tool_state {
        ToolState::Idle => idle_no_tool,
        ToolState::InTool => in_tool,
    };
    if now.saturating_duration_since(signals.baseline()) >= threshold {
        ProgressVerdict::Stalled
    } else {
        ProgressVerdict::Progressing
    }
}

/// The once-only stalled latch: armed the instant the progress clock first
/// trips, and remembering the progress baseline AT that moment so a later
/// tick can tell genuine progress apart from the latch simply persisting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallLatch {
    pub latched_at: Instant,
    /// `ProgressSignals::baseline()` as of the moment this latch armed. The
    /// latch clears only when a STRICTLY NEWER baseline is observed later --
    /// never merely because a signal reads `None` again (a data gap is not
    /// evidence anything moved) and never because time alone passed.
    pub baseline_at_latch: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallAction {
    /// No latch, no stall: ordinary supervision continues.
    Continue,
    /// Not previously latched, but the progress clock just tripped: arm the
    /// latch and send exactly one steering nudge.
    LatchAndNudge,
    /// Already latched, no newer progress observed, and the grace period has
    /// not elapsed yet: keep waiting.
    AwaitGrace,
    /// Already latched, no newer progress observed, and the grace period has
    /// elapsed: terminate the session and record the outcome.
    Terminate,
    /// Already latched, but a strictly newer progress baseline was observed
    /// since the latch armed: clear the latch and resume ordinary
    /// supervision.
    ClearLatch,
}

/// Pure decision core for the whole stall lifecycle. `latch` is `None` until
/// [`StallAction::LatchAndNudge`] is acted on by the caller and `Some` from
/// then on until [`StallAction::ClearLatch`] or [`StallAction::Terminate`] is
/// acted on.
pub fn decide(
    latch: Option<StallLatch>,
    signals: &ProgressSignals,
    tool_state: ToolState,
    now: Instant,
    idle_no_tool: Duration,
    in_tool: Duration,
    stall_grace: Duration,
) -> StallAction {
    match latch {
        None => match evaluate_progress(signals, tool_state, now, idle_no_tool, in_tool) {
            ProgressVerdict::Progressing => StallAction::Continue,
            ProgressVerdict::Stalled => StallAction::LatchAndNudge,
        },
        Some(latch) => {
            if signals.baseline() > latch.baseline_at_latch {
                StallAction::ClearLatch
            } else if now.saturating_duration_since(latch.latched_at) >= stall_grace {
                StallAction::Terminate
            } else {
                StallAction::AwaitGrace
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> (Duration, Duration, Duration) {
        (
            Duration::from_secs(450),
            Duration::from_secs(1200),
            Duration::from_secs(120),
        )
    }

    #[test]
    fn steady_output_never_trips_idle_no_tool_regardless_of_total_runtime() {
        let (idle, in_tool, _) = thresholds();
        let start = Instant::now();
        let mut signals = ProgressSignals::new(start);
        // Ten "polls", each 400s apart (under the 450s threshold), stretched
        // over a long total runtime.
        let mut now = start;
        for _ in 0..10 {
            now += Duration::from_secs(400);
            signals.last_output = Some(now);
            assert_eq!(
                evaluate_progress(&signals, ToolState::Idle, now, idle, in_tool),
                ProgressVerdict::Progressing
            );
        }
    }

    #[test]
    fn a_session_with_zero_signals_ever_is_measured_from_its_own_start() {
        let (idle, in_tool, _) = thresholds();
        let start = Instant::now();
        let signals = ProgressSignals::new(start);
        assert_eq!(
            evaluate_progress(
                &signals,
                ToolState::Idle,
                start + Duration::from_secs(449),
                idle,
                in_tool
            ),
            ProgressVerdict::Progressing
        );
        assert_eq!(
            evaluate_progress(
                &signals,
                ToolState::Idle,
                start + Duration::from_secs(450),
                idle,
                in_tool
            ),
            ProgressVerdict::Stalled,
            "the boundary itself is inclusive"
        );
    }

    #[test]
    fn idle_no_tool_boundary_is_exact() {
        let (idle, in_tool, _) = thresholds();
        let start = Instant::now();
        let signals = ProgressSignals::new(start);
        assert_eq!(
            evaluate_progress(
                &signals,
                ToolState::Idle,
                start + idle - Duration::from_millis(1),
                idle,
                in_tool
            ),
            ProgressVerdict::Progressing
        );
        assert_eq!(
            evaluate_progress(&signals, ToolState::Idle, start + idle, idle, in_tool),
            ProgressVerdict::Stalled
        );
    }

    #[test]
    fn in_tool_uses_the_longer_threshold() {
        let (idle, in_tool, _) = thresholds();
        let start = Instant::now();
        let signals = ProgressSignals::new(start);
        let past_idle_not_in_tool = start + idle + Duration::from_secs(1);
        assert_eq!(
            evaluate_progress(
                &signals,
                ToolState::InTool,
                past_idle_not_in_tool,
                idle,
                in_tool
            ),
            ProgressVerdict::Progressing,
            "a session inside a tool call gets the longer fuse"
        );
        assert_eq!(
            evaluate_progress(&signals, ToolState::InTool, start + in_tool, idle, in_tool),
            ProgressVerdict::Stalled
        );
    }

    #[test]
    fn decide_latches_and_nudges_exactly_once_when_the_clock_trips() {
        let (idle, in_tool, grace) = thresholds();
        let start = Instant::now();
        let signals = ProgressSignals::new(start);
        let now = start + idle;
        assert_eq!(
            decide(None, &signals, ToolState::Idle, now, idle, in_tool, grace),
            StallAction::LatchAndNudge
        );
    }

    #[test]
    fn decide_awaits_grace_then_terminates_when_nothing_moves() {
        let (idle, in_tool, grace) = thresholds();
        let start = Instant::now();
        let signals = ProgressSignals::new(start);
        let latched_at = start + idle;
        let latch = StallLatch {
            latched_at,
            baseline_at_latch: signals.baseline(),
        };

        assert_eq!(
            decide(
                Some(latch),
                &signals,
                ToolState::Idle,
                latched_at + grace - Duration::from_millis(1),
                idle,
                in_tool,
                grace
            ),
            StallAction::AwaitGrace
        );
        assert_eq!(
            decide(
                Some(latch),
                &signals,
                ToolState::Idle,
                latched_at + grace,
                idle,
                in_tool,
                grace
            ),
            StallAction::Terminate
        );
    }

    /// The latch clears ONLY on a strictly newer progress baseline -- never
    /// merely because time passed, and never because a signal happens to
    /// read `None` again (a data gap is not recovery).
    #[test]
    fn the_latch_clears_only_on_observed_progress_never_on_a_data_gap() {
        let (idle, in_tool, grace) = thresholds();
        let start = Instant::now();
        let mut signals = ProgressSignals::new(start);
        let latched_at = start + idle;
        let latch = StallLatch {
            latched_at,
            baseline_at_latch: signals.baseline(),
        };

        // No signal changed at all: still held, not cleared, even mid-grace.
        assert_eq!(
            decide(
                Some(latch),
                &signals,
                ToolState::Idle,
                latched_at + Duration::from_secs(1),
                idle,
                in_tool,
                grace
            ),
            StallAction::AwaitGrace,
            "an unchanged (unknown) signal must never read as recovery"
        );

        // Real progress: a fresh output byte strictly after the latch's own
        // baseline.
        signals.last_output = Some(latched_at + Duration::from_secs(1));
        assert_eq!(
            decide(
                Some(latch),
                &signals,
                ToolState::Idle,
                latched_at + Duration::from_secs(2),
                idle,
                in_tool,
                grace
            ),
            StallAction::ClearLatch
        );
    }

    /// Sanity: a `ProgressSignals` value used for equality-style latch
    /// comparisons behaves symmetrically regardless of which channel
    /// advanced (output, turn, or mail all count as progress equally).
    #[test]
    fn any_one_of_the_three_signal_channels_clears_the_latch() {
        let (idle, in_tool, grace) = thresholds();
        let start = Instant::now();
        let signals = ProgressSignals::new(start);
        let latched_at = start + idle;
        let latch = StallLatch {
            latched_at,
            baseline_at_latch: signals.baseline(),
        };

        for pick in 0..3 {
            let mut signals = signals;
            let progressed_at = latched_at + Duration::from_secs(1);
            match pick {
                0 => signals.last_output = Some(progressed_at),
                1 => signals.last_turn = Some(progressed_at),
                _ => signals.last_mail = Some(progressed_at),
            }
            assert_eq!(
                decide(
                    Some(latch),
                    &signals,
                    ToolState::Idle,
                    latched_at + Duration::from_secs(2),
                    idle,
                    in_tool,
                    grace
                ),
                StallAction::ClearLatch,
                "channel {pick} must clear the latch"
            );
        }
    }
}
