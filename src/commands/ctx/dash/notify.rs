//! Issue #354 phase 5: attention-transition notifications.
//!
//! The approved design (`docs/superpowers/specs/2026-09-05-354-dashboard-
//! design.md`, "Behaviour contract" -> Notifications) asks for exactly one
//! compact notice when a session's *cached* status transitions into a state
//! that owes the operator something -- `Blocked`, `Failed` or `DoneUnread` --
//! suppressed for the pane that already has the keyboard, and deduped per
//! `(session, attention episode)`.
//!
//! Everything here is pure. The reducer never reads the clock, the filesystem
//! or the config: `now` is injected by the caller (`dash::mod`'s event loop,
//! on the `FactsCache` cadence and only there), so an identical sequence of
//! samples always produces an identical sequence of notices. The one impure
//! thing a notice does -- landing in the header's transient notice channel --
//! happens at the call site through the existing `push_notice`, so there is
//! no new UI surface: no toast, no popup, no second row.

use std::collections::HashMap;

use super::super::attention::{Attention, Projection};
use crate::style;

/// The three projections worth interrupting an operator for, and the glyph
/// each one is announced with -- the same glyph the sidebar's own column
/// draws for that state, so a notice and the row it names never disagree.
///
/// Deliberately NOT every projection: `Working`, `IdleSeen` and `Unknown` are
/// states nobody is waiting on, and a dashboard that announced them would be
/// a dashboard nobody reads.
const fn notice_glyph(projection: Projection) -> Option<&'static str> {
    match projection {
        Projection::Blocked(_) => Some("\u{25b2}"),
        Projection::Failed => Some("\u{2717}"),
        Projection::DoneUnread => Some("\u{25c6}"),
        Projection::Working | Projection::IdleSeen | Projection::Unknown => None,
    }
}

/// Pure: the words a notice says after the short id.
///
/// `Blocked` spells out its own [`Attention`] payload -- "needs approval" is
/// actionable where "blocked" is not. `Failed` and `DoneUnread` lead with the
/// state and append the recorded evidence in parentheses when there is any
/// (`attention::reason` is what the caller passes in, which is that evidence
/// when it exists and a generic sentence when it does not); the generic
/// sentences are dropped rather than repeated back as if they were evidence.
fn notice_words(projection: Projection, reason: &str) -> String {
    let reason = reason.trim();
    match projection {
        Projection::Blocked(attention) => blocked_words(attention).to_string(),
        Projection::Failed => decorate("failed", reason, "exited"),
        Projection::DoneUnread => decorate("done", reason, "finished, not yet acknowledged"),
        // Unreachable in practice: `notice_glyph` has already refused these.
        Projection::Working | Projection::IdleSeen | Projection::Unknown => {
            projection.label().to_string()
        }
    }
}

/// The fixed phrase for each [`Attention`] variant. A stable, human sentence
/// per variant rather than `{:?}`, which would change shape the moment a
/// variant is renamed.
const fn blocked_words(attention: Attention) -> &'static str {
    match attention {
        Attention::None => "is waiting",
        Attention::Approval => "needs approval",
        Attention::Question => "needs an answer",
        Attention::Permission => "needs permission",
        Attention::Quota => "hit a quota limit",
        Attention::WorkflowGate => "waits on a workflow gate",
        Attention::WriterConflict => "waits on the writer permit",
        Attention::VerificationFailure => "failed verification",
        Attention::Stalled => "looks stalled",
        Attention::Unknown => "needs attention",
    }
}

/// `word` on its own, or `word (reason)` when `reason` says something the
/// generic fallback does not.
fn decorate(word: &str, reason: &str, generic: &str) -> String {
    if reason.is_empty() || reason == generic {
        word.to_string()
    } else {
        format!("{word} ({reason})")
    }
}

/// One attention-cache sample for one session, as the reducer sees it.
///
/// `previous` is the projection the PREVIOUS cache refresh saw, `None` for a
/// session this reducer has never sampled (the dashboard's very first tick,
/// or a pane that has just appeared). Both projections come from the same
/// `attention::project` the sidebar's glyph column uses, so a notice can
/// never describe a state the row does not show.
#[derive(Debug, Clone, Copy)]
pub struct AttentionSample<'a> {
    pub short: &'a str,
    pub previous: Option<Projection>,
    pub next: Projection,
    /// `attention::reason` for the new status.
    pub reason: &'a str,
    pub revision: u64,
    pub last_transition: u64,
    /// This session is the pane that currently has the keyboard.
    pub focused: bool,
}

/// What one session's currently-announced episode is keyed by.
///
/// An "episode" starts at the transition that produced the state (its
/// `revision`, plus the `last_transition` clock the composer stamped) and
/// ends when the projection leaves that state. Two samples that carry the
/// same key are the same episode however many times they are observed, which
/// is what makes the reducer idempotent under the once-a-second cadence it is
/// driven at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Episode {
    revision: u64,
    started: u64,
}

/// The notice reducer's own memory: the last episode announced (or observed
/// while suppressed) per session short id, and the projection that episode
/// was in.
#[derive(Debug, Default)]
pub struct NoticeReducer {
    seen: HashMap<String, Episode>,
}

impl NoticeReducer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pure: the notice this sample earns, if any, clamped to `max_cols`
    /// display columns.
    ///
    /// Four rules, in this order:
    ///
    /// 1. The new projection must be one somebody is waiting on
    ///    (`notice_glyph`); anything else ends the session's episode and says
    ///    nothing.
    /// 2. It must be a *transition* as the cache observed it: `previous` must
    ///    exist and differ. A session already blocked when the dashboard
    ///    started never transitioned in front of the operator, and a flap that
    ///    happened entirely between two cache refreshes is not two events --
    ///    the reducer only ever sees `Blocked -> Blocked` for it, which is
    ///    silence.
    /// 3. The pane must not be the focused one: the operator is looking at it.
    /// 4. The episode must be one this session has not announced yet.
    ///
    /// `now` is used only when the composed status carries no
    /// `last_transition` at all (a status written by a build that predates
    /// it): the sample's own observation time then stands in as the episode's
    /// start, so a genuinely new transition is still a new episode.
    pub fn observe(
        &mut self,
        sample: &AttentionSample<'_>,
        now: u64,
        max_cols: usize,
    ) -> Option<String> {
        let Some(glyph) = notice_glyph(sample.next) else {
            // Left the state: the episode is over, and the next entry into it
            // is a new one.
            self.seen.remove(sample.short);
            return None;
        };
        let episode = Episode {
            revision: sample.revision,
            started: if sample.last_transition == 0 {
                now
            } else {
                sample.last_transition
            },
        };
        let transitioned = sample.previous.is_some_and(|prev| prev != sample.next);
        let already = self.seen.get(sample.short) == Some(&episode);
        self.seen.insert(sample.short.to_string(), episode);
        if !transitioned || already || sample.focused {
            return None;
        }
        let text = format!(
            "{glyph} {} {}",
            sample.short,
            notice_words(sample.next, sample.reason)
        );
        Some(style::truncate_display(&text, max_cols).into_owned())
    }

    /// Drops every session the roster no longer draws, so a short id that is
    /// reused after a reap cannot inherit a stale episode. Called on the same
    /// cadence the reducer is fed at.
    pub fn retain(&mut self, shorts: &[String]) {
        self.seen
            .retain(|short, _| shorts.iter().any(|s| s == short));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample<'a>(
        short: &'a str,
        previous: Option<Projection>,
        next: Projection,
        reason: &'a str,
    ) -> AttentionSample<'a> {
        AttentionSample {
            short,
            previous,
            next,
            reason,
            revision: 1,
            last_transition: 100,
            focused: false,
        }
    }

    /// The three states worth announcing each produce exactly one notice on
    /// the transition into them, in the approved shape.
    #[test]
    fn a_transition_into_each_attention_state_emits_one_notice() {
        let mut reducer = NoticeReducer::new();
        assert_eq!(
            reducer.observe(
                &sample(
                    "a0000002",
                    Some(Projection::Working),
                    Projection::Blocked(Attention::Approval),
                    "Approval: workflow gate",
                ),
                1_000,
                80
            ),
            Some("\u{25b2} a0000002 needs approval".to_string())
        );
        let mut reducer = NoticeReducer::new();
        assert_eq!(
            reducer.observe(
                &sample(
                    "a0000003",
                    Some(Projection::Working),
                    Projection::Failed,
                    "exit 1",
                ),
                1_000,
                80
            ),
            Some("\u{2717} a0000003 failed (exit 1)".to_string())
        );
        let mut reducer = NoticeReducer::new();
        assert_eq!(
            reducer.observe(
                &sample(
                    "a0000004",
                    Some(Projection::Working),
                    Projection::DoneUnread,
                    "finished, not yet acknowledged",
                ),
                1_000,
                80
            ),
            Some("\u{25c6} a0000004 done".to_string())
        );
    }

    /// The focused pane is the one the operator is already looking at, so it
    /// never earns a notice -- but its episode is still recorded, so
    /// unfocusing it later does not replay the transition.
    #[test]
    fn the_focused_pane_is_suppressed_and_stays_suppressed() {
        let mut reducer = NoticeReducer::new();
        let mut s = sample(
            "a0000002",
            Some(Projection::Working),
            Projection::Blocked(Attention::Question),
            "",
        );
        s.focused = true;
        assert_eq!(reducer.observe(&s, 1_000, 80), None);
        s.focused = false;
        s.previous = Some(Projection::Blocked(Attention::Question));
        assert_eq!(reducer.observe(&s, 1_001, 80), None);
    }

    /// A steady state says nothing, and a flap that happens entirely between
    /// two cache refreshes -- which the cache can only ever show as the same
    /// projection with a moved revision -- is one episode, not two.
    #[test]
    fn a_repeat_or_a_flap_inside_one_cadence_is_deduped() {
        let mut reducer = NoticeReducer::new();
        let first = sample(
            "a0000002",
            Some(Projection::Working),
            Projection::Blocked(Attention::Approval),
            "",
        );
        assert!(reducer.observe(&first, 1_000, 80).is_some());
        // Same status, observed again a second later.
        let mut steady = first;
        steady.previous = Some(Projection::Blocked(Attention::Approval));
        assert_eq!(reducer.observe(&steady, 1_001, 80), None);
        // A flap: the session left and re-entered between two refreshes, so
        // revision/last_transition moved but the cache only ever saw blocked.
        let mut flapped = steady;
        flapped.revision = 9;
        flapped.last_transition = 140;
        assert_eq!(
            reducer.observe(&flapped, 1_002, 80),
            None,
            "a flap the cache never observed is not a second event"
        );
    }

    /// Leaving the state and coming back -- both observed -- is a new
    /// episode, and announces again.
    #[test]
    fn a_new_episode_after_leaving_the_state_emits_again() {
        let mut reducer = NoticeReducer::new();
        let first = sample(
            "a0000002",
            Some(Projection::Working),
            Projection::Blocked(Attention::Approval),
            "",
        );
        assert!(reducer.observe(&first, 1_000, 80).is_some());
        let left = sample(
            "a0000002",
            Some(Projection::Blocked(Attention::Approval)),
            Projection::Working,
            "",
        );
        assert_eq!(reducer.observe(&left, 1_001, 80), None);
        let mut again = first;
        again.previous = Some(Projection::Working);
        again.revision = 7;
        again.last_transition = 180;
        assert_eq!(
            reducer.observe(&again, 1_002, 80),
            Some("\u{25b2} a0000002 needs approval".to_string())
        );
    }

    /// A status with no recorded transition clock still separates episodes,
    /// on the injected `now` alone.
    #[test]
    fn a_status_without_a_transition_clock_falls_back_to_the_injected_now() {
        let mut reducer = NoticeReducer::new();
        let mut first = sample(
            "a0000002",
            Some(Projection::Working),
            Projection::Failed,
            "",
        );
        first.last_transition = 0;
        assert!(reducer.observe(&first, 1_000, 80).is_some());
        let mut again = first;
        again.previous = Some(Projection::Working);
        assert!(
            reducer.observe(&again, 1_050, 80).is_some(),
            "a later observation with no transition clock is a later episode"
        );
    }

    /// The three quiet projections never announce anything, whatever they
    /// came from.
    #[test]
    fn quiet_projections_never_emit() {
        let mut reducer = NoticeReducer::new();
        for next in [
            Projection::Working,
            Projection::IdleSeen,
            Projection::Unknown,
        ] {
            assert_eq!(
                reducer.observe(
                    &sample(
                        "a0000002",
                        Some(Projection::Blocked(Attention::None)),
                        next,
                        ""
                    ),
                    1_000,
                    80
                ),
                None,
                "{next:?} must never raise a notice"
            );
        }
    }

    /// The first sample for a session is not a transition anybody watched.
    #[test]
    fn the_first_sample_for_a_session_is_never_a_transition() {
        let mut reducer = NoticeReducer::new();
        assert_eq!(
            reducer.observe(
                &sample(
                    "a0000002",
                    None,
                    Projection::Blocked(Attention::Approval),
                    ""
                ),
                1_000,
                80
            ),
            None
        );
    }

    /// The message is clamped to the header middle's own budget, by display
    /// columns, and never splits a character.
    #[test]
    fn the_message_is_clamped_to_the_width_it_is_given() {
        let mut reducer = NoticeReducer::new();
        let long = "\u{5efa}".repeat(80);
        let text = reducer
            .observe(
                &sample(
                    "a0000002",
                    Some(Projection::Working),
                    Projection::Failed,
                    &long,
                ),
                1_000,
                24,
            )
            .expect("a notice");
        assert!(style::display_width(&text) <= 24, "{text:?}");
        assert!(text.starts_with("\u{2717} a0000002"));
    }

    /// A short id the roster no longer draws is forgotten, so the id cannot
    /// inherit an episode if it is ever reused.
    #[test]
    fn retain_drops_sessions_the_roster_no_longer_has() {
        let mut reducer = NoticeReducer::new();
        let first = sample(
            "a0000002",
            Some(Projection::Working),
            Projection::Blocked(Attention::Approval),
            "",
        );
        assert!(reducer.observe(&first, 1_000, 80).is_some());
        reducer.retain(&["a0000009".to_string()]);
        assert!(reducer.seen.is_empty());
    }
}
