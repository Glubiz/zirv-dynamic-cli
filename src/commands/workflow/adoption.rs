//! Pure workflow-adoption detector: whether a session has done "substantial"
//! edit work with no active `zirv workflow`, and what to say about it.
//!
//! Mirrors `rot::signals` (`src/commands/ctx/rot.rs:130-168`): every function
//! here is pure -- no fs/clock/env/net -- so identical events always produce
//! identical signals/decisions. Callers (`ctx::hook`) own persistence and
//! delivery; this module only counts and decides.

use crate::commands::ctx::event::NormalizedEvent;

/// Edit-like tool names, matched case-insensitively. `apply_patch` is codex's
/// own edit tool; the rest are claude's.
const EDIT_LIKE_TOOLS: &[&str] = &["edit", "write", "multiedit", "notebookedit", "apply_patch"];

/// Edit-call count at or above which work counts as substantial on its own.
///
/// Wrapper behaviour redesign (2026-09-01): raised from 5 to 12. The prior
/// threshold fired the nudge -- the only steering text zirv ever types into a
/// live session -- on ordinary bounded work well short of "substantial",
/// pushing toward more process regardless of diff size. See
/// `docs/superpowers/specs/2026-09-01-wrapper-behaviour-redesign.md`.
pub const SUBSTANTIAL_EDIT_CALLS: usize = 12;
/// Turn count above which even a single edit call counts as substantial.
///
/// Wrapper behaviour redesign (2026-09-01): raised from 12 to 25, alongside
/// [`SUBSTANTIAL_EDIT_CALLS`], for the same proportionality reason.
pub const SUBSTANTIAL_TURNS: usize = 25;
/// Minimum turn gap between one nudge and the next.
pub const NUDGE_EVERY_TURNS: usize = 5;

/// Adoption-relevant counts over a session's events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdoptionSignals {
    pub edit_like_calls: usize,
    pub turns: usize,
}

/// Counts edit-like tool calls and turns across `events`. Pure and cheap
/// enough to run on every Stop hook invocation.
pub fn signals(events: &[NormalizedEvent]) -> AdoptionSignals {
    let mut edit_like_calls = 0usize;
    let mut turns = 0usize;
    for event in events {
        match event {
            NormalizedEvent::TurnStart => turns += 1,
            NormalizedEvent::ToolCall { name, .. }
                if EDIT_LIKE_TOOLS
                    .iter()
                    .any(|tool| name.eq_ignore_ascii_case(tool)) =>
            {
                edit_like_calls += 1;
            }
            _ => {}
        }
    }
    AdoptionSignals {
        edit_like_calls,
        turns,
    }
}

/// Whether `s` describes "substantial" work: enough edit calls on its own, or
/// a long enough session that has done at least one edit.
pub fn is_substantial(s: &AdoptionSignals) -> bool {
    s.edit_like_calls >= SUBSTANTIAL_EDIT_CALLS
        || (s.turns >= SUBSTANTIAL_TURNS && s.edit_like_calls >= 1)
}

/// Operator-controlled strictness for workflow adoption, ordered
/// `Off < Advise < Nudge < Enforce`. Modeled on `deploy::DeployTier`
/// (`src/commands/workflow/deploy.rs:17-26`).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum AdoptionPolicy {
    Off,
    Advise,
    #[default]
    Nudge,
    Enforce,
}

impl std::fmt::Display for AdoptionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Advise => "advise",
            Self::Nudge => "nudge",
            Self::Enforce => "enforce",
        })
    }
}

/// Whether a nudge should fire right now: never below `Nudge`, never while a
/// workflow is already active, never for non-substantial work. The first
/// nudge fires immediately once substantial; after that, only every
/// [`NUDGE_EVERY_TURNS`] turns.
pub fn nudge_due(
    policy: AdoptionPolicy,
    substantial: bool,
    workflow_active: bool,
    turns_now: usize,
    last_nudged_turn: Option<usize>,
) -> bool {
    if policy < AdoptionPolicy::Nudge || !substantial || workflow_active {
        return false;
    }
    match last_nudged_turn {
        None => true,
        Some(last) => turns_now >= last + NUDGE_EVERY_TURNS,
    }
}

/// The nudge message itself. `kind` is the classified workflow kind (`None`
/// falls back to `"feature"`); under [`AdoptionPolicy::Enforce`] an extra
/// sentence names the delegation gate this policy also applies
/// (`ctx::agent::run_with`).
///
/// Wrapper behaviour redesign (2026-09-01): the wording is now proportional
/// -- it names a workflow as something to start "if it spans several areas
/// or carries real risk" and says outright that a bounded change may finish
/// without one, rather than unconditionally telling the session to start one
/// now. See `docs/superpowers/specs/2026-09-01-wrapper-behaviour-redesign.md`.
pub fn nudge_text(signals: &AdoptionSignals, kind: Option<&str>, policy: AdoptionPolicy) -> String {
    let kind = kind.unwrap_or("feature");
    let mut text = format!(
        "[zirv workflow] this has grown into substantial work ({} edit calls over {} turns) \
         with no active zirv workflow. If it spans several areas or carries real risk, start \
         one now: zirv workflow start {kind} --task \"<summary>\". A bounded change may finish \
         without one.",
        signals.edit_like_calls, signals.turns
    );
    if policy == AdoptionPolicy::Enforce {
        // ASCII double-hyphen, not a real em dash: this text rides the Stop
        // hook's `systemMessage` and `UserPromptSubmit`'s `additionalContext`,
        // both held to the same "no em dashes in user-facing copy" rule every
        // other hook-adjacent string in this crate is tested against (see
        // e.g. `hook.rs`'s `an_advisory_verdict_prints_a_non_blocking_
        // system_message`).
        text.push_str(
            " -- workflow.adoption = enforce: zirv agent delegation is held until a workflow is \
             active.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> NormalizedEvent {
        NormalizedEvent::ToolCall {
            name: name.to_string(),
            input_hash: 0,
        }
    }

    #[test]
    fn signals_count_edit_like_tools_case_insensitively() {
        let events = vec![
            tool("Edit"),
            tool("WRITE"),
            tool("multiEdit"),
            tool("NotebookEdit"),
            tool("apply_patch"),
            tool("Bash"),
            tool("Read"),
        ];
        let s = signals(&events);
        assert_eq!(s.edit_like_calls, 5);
        assert_eq!(s.turns, 0);
    }

    #[test]
    fn signals_ignore_non_tool_events_and_count_turns() {
        let events = vec![
            NormalizedEvent::TurnStart,
            tool("Edit"),
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 0,
            },
            NormalizedEvent::ToolResult { is_error: false },
            NormalizedEvent::Compaction,
            NormalizedEvent::TurnStart,
        ];
        let s = signals(&events);
        assert_eq!(s.edit_like_calls, 1);
        assert_eq!(s.turns, 2);
    }

    #[test]
    fn substantial_by_edit_count_alone() {
        let s = AdoptionSignals {
            edit_like_calls: SUBSTANTIAL_EDIT_CALLS,
            turns: 1,
        };
        assert!(is_substantial(&s));
        let below = AdoptionSignals {
            edit_like_calls: SUBSTANTIAL_EDIT_CALLS - 1,
            turns: 1,
        };
        assert!(!is_substantial(&below));
    }

    #[test]
    fn substantial_by_turns_needs_at_least_one_edit() {
        let s = AdoptionSignals {
            edit_like_calls: 1,
            turns: SUBSTANTIAL_TURNS,
        };
        assert!(is_substantial(&s));

        let no_edits = AdoptionSignals {
            edit_like_calls: 0,
            turns: SUBSTANTIAL_TURNS + 10,
        };
        assert!(
            !is_substantial(&no_edits),
            "turns alone, with no edits, is not substantial"
        );

        let below_turns = AdoptionSignals {
            edit_like_calls: 1,
            turns: SUBSTANTIAL_TURNS - 1,
        };
        assert!(!is_substantial(&below_turns));
    }

    #[test]
    fn adoption_policy_orders_by_strictness() {
        assert!(AdoptionPolicy::Off < AdoptionPolicy::Advise);
        assert!(AdoptionPolicy::Advise < AdoptionPolicy::Nudge);
        assert!(AdoptionPolicy::Nudge < AdoptionPolicy::Enforce);
        assert_eq!(AdoptionPolicy::default(), AdoptionPolicy::Nudge);
    }

    #[test]
    fn nudge_due_requires_at_least_nudge_policy() {
        assert!(!nudge_due(AdoptionPolicy::Off, true, false, 20, None));
        assert!(!nudge_due(AdoptionPolicy::Advise, true, false, 20, None));
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 20, None));
        assert!(nudge_due(AdoptionPolicy::Enforce, true, false, 20, None));
    }

    #[test]
    fn nudge_due_requires_substantial_and_no_active_workflow() {
        assert!(!nudge_due(AdoptionPolicy::Nudge, false, false, 20, None));
        assert!(!nudge_due(AdoptionPolicy::Nudge, true, true, 20, None));
    }

    #[test]
    fn nudge_due_fires_immediately_then_every_nudge_every_turns() {
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 12, None));

        // Just nudged at turn 12: not due again until turn 17.
        assert!(!nudge_due(AdoptionPolicy::Nudge, true, false, 16, Some(12)));
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 17, Some(12)));
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 25, Some(12)));
    }

    #[test]
    fn nudge_text_names_the_kind_and_counts() {
        let s = AdoptionSignals {
            edit_like_calls: 7,
            turns: 9,
        };
        let text = nudge_text(&s, Some("bugfix"), AdoptionPolicy::Nudge);
        assert!(text.contains("7 edit calls over 9 turns"), "{text}");
        assert!(text.contains("zirv workflow start bugfix"), "{text}");
        assert!(!text.contains("enforce"), "{text}");
    }

    #[test]
    fn nudge_text_falls_back_to_feature_when_kind_is_unknown() {
        let s = AdoptionSignals {
            edit_like_calls: 5,
            turns: 5,
        };
        let text = nudge_text(&s, None, AdoptionPolicy::Advise);
        assert!(text.contains("zirv workflow start feature"), "{text}");
    }

    #[test]
    fn nudge_does_not_fire_at_old_thresholds_but_does_at_new_ones() {
        // Old thresholds (5 edits / 12 turns) no longer count as substantial.
        let old = AdoptionSignals {
            edit_like_calls: 5,
            turns: 12,
        };
        assert!(!is_substantial(&old));
        assert!(!nudge_due(
            AdoptionPolicy::Nudge,
            is_substantial(&old),
            false,
            12,
            None
        ));

        // New thresholds (12 edits / 25 turns) do.
        let new_by_edits = AdoptionSignals {
            edit_like_calls: SUBSTANTIAL_EDIT_CALLS,
            turns: 1,
        };
        assert!(is_substantial(&new_by_edits));
        let new_by_turns = AdoptionSignals {
            edit_like_calls: 1,
            turns: SUBSTANTIAL_TURNS,
        };
        assert!(is_substantial(&new_by_turns));
    }

    #[test]
    fn nudge_text_under_enforce_names_the_delegation_gate() {
        let s = AdoptionSignals {
            edit_like_calls: 5,
            turns: 5,
        };
        let text = nudge_text(&s, Some("feature"), AdoptionPolicy::Enforce);
        assert!(text.contains("workflow.adoption = enforce"), "{text}");
        assert!(text.contains("zirv agent delegation is held"), "{text}");
    }
}
