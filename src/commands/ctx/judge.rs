//! Issue #314: the pure completion-judge core for an objective-driven `zirv
//! ctx loop` run. Deterministic gates (`[objective] gates`, e.g. `zirv test
//! changed`/`zirv verify`) are always checked first and always win over the
//! judge -- a red gate short-circuits before a cheap model is ever asked
//! anything. Only once every gate is green does the judge get a turn to say
//! whether the objective itself is `done`, `blocked`, or needs to keep
//! going (`continue`) or pause (`wait`).
//!
//! Every type and function in this module is pure: no fs/clock/env/net,
//! mirroring `rot.rs`'s own discipline. The I/O -- spawning a gate as a
//! child process, computing `verification::change_fingerprint`, spawning the
//! judge model call, polling a `WaitOn` target -- lives in `run_loop.rs`,
//! the one caller.

use std::path::PathBuf;

/// What a `wait` verdict is waiting on, parsed straight out of the judge's
/// own `wait_on` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOn {
    Pid(u32),
    File(PathBuf),
    Seconds(u64),
}

/// The judge's own verdict, before any objective-status or evidence text is
/// folded in. `Continue` and `Done`/`Blocked` all carry no data of their
/// own here -- the human-readable reason the judge gave travels alongside
/// as a separate `String` (see [`parse_verdict`]), not inside this enum, so
/// this type stays a plain, comparable tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Done,
    Blocked,
    Continue,
    Wait(WaitOn),
}

/// Parses a judge answer that is supposed to be one JSON object:
/// `{"verdict": "done|blocked|continue|wait", "reason": "...", "wait_on":
/// {...}}` (`wait_on` required, and only meaningful, when `verdict` is
/// `"wait"`). Tolerates surrounding prose by taking the first `{` .. last
/// `}` slice before parsing -- a cheap model asked for JSON not
/// infrequently wraps it in a sentence or a fenced code block anyway.
/// Anything that fails to parse, is missing `verdict`, names an unknown
/// verdict, or claims `wait` with no usable `wait_on` target is `None`: a
/// caller must fail open (`Verdict` absent, not a fabricated one) rather
/// than guess.
pub fn parse_verdict(raw: &str) -> Option<(Verdict, String)> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let reason = value
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_string();
    let verdict = match value.get("verdict").and_then(|v| v.as_str())? {
        "done" => Verdict::Done,
        "blocked" => Verdict::Blocked,
        "continue" => Verdict::Continue,
        "wait" => Verdict::Wait(parse_wait_on(value.get("wait_on")?)?),
        _ => return None,
    };
    Some((verdict, reason))
}

fn parse_wait_on(value: &serde_json::Value) -> Option<WaitOn> {
    if let Some(pid) = value.get("pid").and_then(|v| v.as_u64()) {
        return u32::try_from(pid).ok().map(WaitOn::Pid);
    }
    if let Some(file) = value.get("file").and_then(|v| v.as_str()) {
        return Some(WaitOn::File(PathBuf::from(file)));
    }
    if let Some(secs) = value.get("seconds").and_then(|v| v.as_u64()) {
        return Some(WaitOn::Seconds(secs));
    }
    None
}

/// One configured gate's outcome for this cycle. `Skipped` is the #287-style
/// unchanged-workspace short circuit: the workspace has not moved since this
/// exact gate last failed, so re-running it would only reproduce the same
/// failure -- treated by [`next_step`] exactly like `Red`, just without
/// spending a child process on evidence the caller already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Green,
    Red {
        cmd: String,
        code: i32,
        tail: String,
    },
    Skipped,
}

/// The action `run_loop` takes for this cycle, once a gate outcome (and,
/// once the judge has actually run, its verdict) is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Every gate is green and the judge has not been asked yet this cycle.
    RunJudge,
    /// A gate is red (or skipped as still-red): inject this text into the
    /// next cycle's prompt exactly the way the soft-budget wrap-up text
    /// already is, and never call the judge.
    InjectFailure(String),
    /// The judge says the objective is done. The `String` is its reason,
    /// folded into the evidence recorded on close.
    Close(String),
    /// The judge says the objective is blocked. The `String` is its reason.
    StopBlocked(String),
    /// The judge says to pause on a concrete external event.
    Park(WaitOn),
    /// Keep cycling: the judge says more work remains (or is unavailable/
    /// disabled/mid-flight), and the no-progress backstop has not tripped.
    Continue,
    /// `max_cycles_without_progress` consecutive no-progress cycles: give up
    /// regardless of whether the judge is even enabled.
    StopNoProgress,
}

/// The single decision point this module exists for. `gate` always wins: a
/// red or skipped gate short-circuits straight to [`Step::InjectFailure`]
/// with no regard for `verdict` at all, and the judge is consulted only once
/// every gate is green.
///
/// `verdict` is `None` in two situations the caller must distinguish only by
/// context, never by this function: "the judge has not run yet this cycle"
/// (leads to [`Step::RunJudge`]) and "the judge ran but its answer did not
/// parse, or it never answered at all" (the caller treats that failure as
/// [`Step::Continue`] directly, failing open, without calling this function
/// a second time).
///
/// `max_cycles_without_progress` is checked before the judge is ever
/// consulted (and again once its verdict is `continue`): the backstop must
/// stop the loop "regardless of judge availability", so it always takes
/// priority over asking a model to keep going.
pub fn next_step(
    gate: GateOutcome,
    judge_enabled: bool,
    verdict: Option<(Verdict, String)>,
    cycles_without_progress: u32,
    max_cycles_without_progress: u32,
) -> Step {
    match gate {
        GateOutcome::Red { cmd, code, tail } => {
            Step::InjectFailure(format!("gate red: {cmd} exited {code}\n{tail}"))
        }
        GateOutcome::Skipped => Step::InjectFailure(
            "gate skipped: the workspace is unchanged since this gate's last recorded failure"
                .to_string(),
        ),
        GateOutcome::Green => {
            if cycles_without_progress >= max_cycles_without_progress {
                return Step::StopNoProgress;
            }
            if !judge_enabled {
                return Step::Continue;
            }
            match verdict {
                None => Step::RunJudge,
                Some((Verdict::Done, reason)) => Step::Close(reason),
                Some((Verdict::Blocked, reason)) => Step::StopBlocked(reason),
                Some((Verdict::Wait(wait_on), _reason)) => Step::Park(wait_on),
                Some((Verdict::Continue, _reason)) => Step::Continue,
            }
        }
    }
}

/// Progress = the cycle's transcript digest changed since the previous
/// cycle, or a gate flipped from red to green. `prev_digest` is `None` only
/// for the very first cycle evaluated, which by construction is never
/// "unchanged from nothing" -- callers should treat a first cycle as
/// progress on its own terms rather than calling this at all, but this
/// still returns `true` for it (a missing previous digest can never equal
/// the current one).
pub fn progress_made(
    prev_digest: Option<u64>,
    curr_digest: u64,
    prev_gate_red: bool,
    curr_gate_green: bool,
) -> bool {
    prev_digest != Some(curr_digest) || (prev_gate_red && curr_gate_green)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_reads_a_bare_json_object() {
        let (verdict, reason) =
            parse_verdict(r#"{"verdict":"done","reason":"shipped it"}"#).expect("parses");
        assert_eq!(verdict, Verdict::Done);
        assert_eq!(reason, "shipped it");
    }

    #[test]
    fn parse_verdict_tolerates_surrounding_prose() {
        let raw =
            "Sure, here you go:\n{\"verdict\": \"blocked\", \"reason\": \"no creds\"}\nthanks!";
        let (verdict, reason) = parse_verdict(raw).expect("parses");
        assert_eq!(verdict, Verdict::Blocked);
        assert_eq!(reason, "no creds");
    }

    #[test]
    fn parse_verdict_reads_every_wait_on_shape() {
        let (pid, _) = parse_verdict(r#"{"verdict":"wait","wait_on":{"pid":123}}"#).expect("pid");
        assert_eq!(pid, Verdict::Wait(WaitOn::Pid(123)));

        let (file, _) = parse_verdict(r#"{"verdict":"wait","wait_on":{"file":"/tmp/done.flag"}}"#)
            .expect("file");
        assert_eq!(
            file,
            Verdict::Wait(WaitOn::File(PathBuf::from("/tmp/done.flag")))
        );

        let (secs, _) =
            parse_verdict(r#"{"verdict":"wait","wait_on":{"seconds":90}}"#).expect("seconds");
        assert_eq!(secs, Verdict::Wait(WaitOn::Seconds(90)));
    }

    #[test]
    fn parse_verdict_rejects_a_wait_with_no_usable_target() {
        assert_eq!(parse_verdict(r#"{"verdict":"wait","wait_on":{}}"#), None);
        assert_eq!(parse_verdict(r#"{"verdict":"wait"}"#), None);
    }

    #[test]
    fn parse_verdict_rejects_an_unknown_verdict() {
        assert_eq!(parse_verdict(r#"{"verdict":"maybe"}"#), None);
    }

    #[test]
    fn parse_verdict_rejects_plain_prose_with_no_json_at_all() {
        assert_eq!(parse_verdict("looks fine to me"), None);
    }

    #[test]
    fn parse_verdict_rejects_malformed_json() {
        assert_eq!(parse_verdict(r#"{"verdict": "done""#), None);
    }

    #[test]
    fn parse_verdict_defaults_a_missing_reason_to_empty() {
        let (verdict, reason) = parse_verdict(r#"{"verdict":"continue"}"#).expect("parses");
        assert_eq!(verdict, Verdict::Continue);
        assert_eq!(reason, "");
    }

    fn red() -> GateOutcome {
        GateOutcome::Red {
            cmd: "zirv test changed".to_string(),
            code: 1,
            tail: "1 test failed".to_string(),
        }
    }

    #[test]
    fn a_red_gate_short_circuits_before_the_judge_no_matter_the_verdict() {
        let step = next_step(red(), true, None, 0, 5);
        assert_eq!(
            step,
            Step::InjectFailure("gate red: zirv test changed exited 1\n1 test failed".to_string())
        );
        // Even a stray `Some(verdict)` (the judge should never have been
        // asked) still loses to the gate.
        let step = next_step(red(), true, Some((Verdict::Done, "done".to_string())), 0, 5);
        assert!(matches!(step, Step::InjectFailure(_)));
    }

    #[test]
    fn a_skipped_gate_also_short_circuits_without_running_the_judge() {
        let step = next_step(GateOutcome::Skipped, true, None, 0, 5);
        assert!(matches!(step, Step::InjectFailure(_)));
    }

    #[test]
    fn a_green_gate_with_no_verdict_yet_runs_the_judge() {
        assert_eq!(
            next_step(GateOutcome::Green, true, None, 0, 5),
            Step::RunJudge
        );
    }

    #[test]
    fn a_green_gate_never_runs_the_judge_when_it_is_disabled() {
        assert_eq!(
            next_step(GateOutcome::Green, false, None, 0, 5),
            Step::Continue
        );
    }

    #[test]
    fn done_closes_and_blocked_stops_and_wait_parks() {
        assert_eq!(
            next_step(
                GateOutcome::Green,
                true,
                Some((Verdict::Done, "ok".into())),
                0,
                5
            ),
            Step::Close("ok".to_string())
        );
        assert_eq!(
            next_step(
                GateOutcome::Green,
                true,
                Some((Verdict::Blocked, "need creds".into())),
                0,
                5
            ),
            Step::StopBlocked("need creds".to_string())
        );
        assert_eq!(
            next_step(
                GateOutcome::Green,
                true,
                Some((Verdict::Wait(WaitOn::Seconds(30)), "napping".into())),
                0,
                5
            ),
            Step::Park(WaitOn::Seconds(30))
        );
        assert_eq!(
            next_step(
                GateOutcome::Green,
                true,
                Some((Verdict::Continue, "more to do".into())),
                0,
                5
            ),
            Step::Continue
        );
    }

    #[test]
    fn the_no_progress_backstop_wins_over_every_other_green_path() {
        // Reached the cap: never RunJudge, never Close/Park, even with a
        // verdict already in hand and the judge enabled.
        assert_eq!(
            next_step(GateOutcome::Green, true, None, 5, 5),
            Step::StopNoProgress
        );
        assert_eq!(
            next_step(
                GateOutcome::Green,
                true,
                Some((Verdict::Done, "ok".into())),
                5,
                5
            ),
            Step::StopNoProgress
        );
        assert_eq!(
            next_step(GateOutcome::Green, false, None, 9, 5),
            Step::StopNoProgress,
            "the backstop applies regardless of judge availability"
        );
        // A red gate never trips the backstop -- InjectFailure always wins.
        assert!(matches!(
            next_step(red(), true, None, 5, 5),
            Step::InjectFailure(_)
        ));
    }

    #[test]
    fn progress_is_a_changed_digest_or_a_red_to_green_flip() {
        assert!(progress_made(Some(1), 2, false, false), "digest changed");
        assert!(!progress_made(Some(1), 1, false, false), "nothing changed");
        assert!(
            progress_made(Some(1), 1, true, true),
            "same digest but the gate flipped red -> green"
        );
        assert!(
            !progress_made(Some(1), 1, false, true),
            "green -> green with an unchanged digest is not new progress"
        );
        assert!(
            progress_made(None, 1, false, false),
            "no previous digest at all"
        );
    }
}
