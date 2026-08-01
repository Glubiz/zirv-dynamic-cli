use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use super::config::ScoreConfig;
use super::event::{Capabilities, NormalizedEvent};

/// Leading characters a model tends to put before a reply prefix. Ported from
/// the shell canary's `^[ \t>*_`#~-]*` allowance.
const MARKER_LEAD: [char; 10] = [' ', '\t', '\n', '\r', '>', '*', '_', '`', '#', '~'];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signals {
    pub turns: usize,
    pub tool_failure_rate: f64,
    pub repetition_hits: usize,
    pub max_repeat: usize,
    /// `None` means the signal is unavailable, not that it scored zero.
    pub marker_miss_rate: Option<f64>,
}

pub fn has_marker(text: &str, marker: &str) -> bool {
    if marker.is_empty() {
        return false;
    }
    text.trim_start_matches(|c| MARKER_LEAD.contains(&c) || c == '-')
        .starts_with(marker)
}

/// One entry per turn that produced any assistant text, holding that turn's
/// last text message. Mid-turn notes are deliberately discarded: they are
/// missing the marker even in healthy sessions.
pub fn turn_final_texts(events: &[NormalizedEvent]) -> Vec<String> {
    let mut finals = Vec::new();
    let mut current: Option<String> = None;
    let mut in_turn = false;

    for event in events {
        match event {
            NormalizedEvent::TurnStart => {
                if in_turn && let Some(text) = current.take() {
                    finals.push(text);
                }
                in_turn = true;
                current = None;
            }
            NormalizedEvent::AssistantFinal { text, .. } if !text.trim().is_empty() => {
                current = Some(text.clone());
            }
            _ => {}
        }
    }
    if let Some(text) = current {
        finals.push(text);
    }
    finals
}

pub fn context_tokens(events: &[NormalizedEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
            _ => None,
        })
        .unwrap_or(0)
}

fn last_window<T>(items: &[T], window: usize) -> &[T] {
    if window == 0 || items.len() <= window {
        return items;
    }
    &items[items.len() - window..]
}

/// Events belonging to the last `window` turns, so tool signals share the
/// marker signal's horizon.
fn events_in_last_turns(events: &[NormalizedEvent], window: usize) -> &[NormalizedEvent] {
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, NormalizedEvent::TurnStart))
        .map(|(i, _)| i)
        .collect();

    if window == 0 || starts.len() <= window {
        return events;
    }
    &events[starts[starts.len() - window]..]
}

pub fn signals(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Signals {
    let finals = turn_final_texts(events);
    let turns = finals.len();

    let marker_ever = finals.iter().any(|t| has_marker(t, &cfg.marker));
    let marker_active =
        caps.marker_signal && !cfg.marker.is_empty() && marker_ever && turns >= cfg.min_turns;

    let marker_miss_rate = if marker_active {
        let recent = last_window(&finals, cfg.window);
        let misses = recent
            .iter()
            .filter(|t| !has_marker(t, &cfg.marker))
            .count();
        Some(misses as f64 / recent.len() as f64)
    } else {
        None
    };

    let tail = events_in_last_turns(events, cfg.window);

    let results: Vec<bool> = tail
        .iter()
        .filter_map(|e| match e {
            NormalizedEvent::ToolResult { is_error } => Some(*is_error),
            _ => None,
        })
        .collect();
    let tool_failure_rate = if results.is_empty() {
        0.0
    } else {
        results.iter().filter(|e| **e).count() as f64 / results.len() as f64
    };

    let mut counts: HashMap<(&str, u64), usize> = HashMap::new();
    for event in tail {
        if let NormalizedEvent::ToolCall { name, input_hash } = event {
            *counts.entry((name.as_str(), *input_hash)).or_insert(0) += 1;
        }
    }
    let max_repeat = counts.values().copied().max().unwrap_or(0);
    let repetition_hits = counts
        .values()
        .filter(|count| **count >= cfg.repetition_threshold)
        .count();

    Signals {
        turns,
        tool_failure_rate,
        repetition_hits,
        max_repeat,
        marker_miss_rate,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Healthy,
    Advise,
    Compact,
    Restart,
}

impl Verdict {
    /// Human-readable form for the decision log and advisories; the JSON path
    /// uses `Serialize` instead.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::Advise => "advise",
            Verdict::Compact => "compact",
            Verdict::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Score {
    pub score: u32,
    pub verdict: Verdict,
    pub signals: Signals,
    pub context_tokens: u64,
}

/// Zero below the threshold, then a linear ramp that saturates at
/// `2 * threshold - 1` identical calls.
pub fn repetition_component(max_repeat: usize, threshold: usize) -> f64 {
    if threshold == 0 || max_repeat < threshold {
        return 0.0;
    }
    (((max_repeat + 1 - threshold) as f64) / threshold as f64).clamp(0.0, 1.0)
}

/// The token gate is a gate, not a vote: below the floor nothing escalates, at
/// or above the ceiling the verdict is at least `compact`, and at the ceiling a
/// compact-level score becomes a restart.
pub fn verdict_for(score: u32, tokens: u64, cfg: &ScoreConfig) -> Verdict {
    if tokens < cfg.token_floor {
        return Verdict::Healthy;
    }

    let base = if score >= cfg.restart_at {
        Verdict::Restart
    } else if score >= cfg.compact_at {
        Verdict::Compact
    } else if score >= cfg.advise_at {
        Verdict::Advise
    } else {
        Verdict::Healthy
    };

    if tokens < cfg.token_ceiling {
        return base;
    }
    if score >= cfg.compact_at {
        return Verdict::Restart;
    }
    base.max(Verdict::Compact)
}

pub fn score_events(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Score {
    let signals = signals(events, caps, cfg);
    let tokens = context_tokens(events);

    let raw = cfg.weight_tool_failure * signals.tool_failure_rate
        + cfg.weight_repetition
            * repetition_component(signals.max_repeat, cfg.repetition_threshold)
        + cfg.weight_marker * signals.marker_miss_rate.unwrap_or(0.0);
    let score = raw.round().clamp(0.0, 100.0) as u32;

    Score {
        score,
        verdict: verdict_for(score, tokens, cfg),
        signals,
        context_tokens: tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::ScoreConfig;
    use crate::commands::ctx::event::{Capabilities, NormalizedEvent, input_hash};

    fn full_caps() -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
        }
    }

    fn assistant(text: &str, tokens: u64) -> NormalizedEvent {
        NormalizedEvent::AssistantFinal {
            text: text.to_string(),
            input_tokens: tokens,
        }
    }

    fn tool(name: &str, input: &str) -> NormalizedEvent {
        NormalizedEvent::ToolCall {
            name: name.to_string(),
            input_hash: input_hash(input),
        }
    }

    /// One turn: prompt, a mid-turn tool-only assistant message, a tool call,
    /// its result, then the turn-final text. Mirrors the canary's synthetic
    /// turn builder. `tool_input` decides whether the turn feeds the repetition
    /// signal, so every test states that choice explicitly.
    fn turn_with(
        tool_input: &str,
        mid_text: &str,
        final_text: &str,
        is_error: bool,
        tokens: u64,
    ) -> Vec<NormalizedEvent> {
        vec![
            NormalizedEvent::TurnStart,
            assistant(mid_text, tokens),
            tool("Bash", tool_input),
            NormalizedEvent::ToolResult { is_error },
            assistant(final_text, tokens),
        ]
    }

    /// Distinct tool input per turn: the repetition signal stays at zero, so
    /// these fixtures isolate the marker and tool-failure signals.
    fn turns(
        count: usize,
        mid: &str,
        fin: &str,
        is_error: bool,
        tokens: u64,
    ) -> Vec<NormalizedEvent> {
        (0..count)
            .flat_map(|i| {
                turn_with(
                    &format!("{{\"command\":\"ls {i}\"}}"),
                    mid,
                    fin,
                    is_error,
                    tokens,
                )
            })
            .collect()
    }

    /// Identical tool input every turn: this is what a repetition loop looks
    /// like, so the repetition signal fires.
    fn looping_turns(
        count: usize,
        mid: &str,
        fin: &str,
        is_error: bool,
        tokens: u64,
    ) -> Vec<NormalizedEvent> {
        (0..count)
            .flat_map(|_| turn_with("{\"command\":\"ls\"}", mid, fin, is_error, tokens))
            .collect()
    }

    #[test]
    fn marker_detection_tolerates_leading_markdown() {
        assert!(has_marker("[zirv] done", "[zirv]"));
        assert!(has_marker("  > **[zirv]** done", "[zirv]"));
        assert!(has_marker("- [zirv] done", "[zirv]"));
        assert!(!has_marker("done [zirv]", "[zirv]"));
        assert!(!has_marker("done", "[zirv]"));
    }

    #[test]
    fn turn_finals_take_the_last_text_per_turn_and_skip_textless_turns() {
        let events = vec![
            NormalizedEvent::TurnStart,
            assistant("mid", 1),
            assistant("final one", 1),
            NormalizedEvent::TurnStart,
            assistant("", 1),
            NormalizedEvent::TurnStart,
            assistant("final two", 1),
        ];
        assert_eq!(turn_final_texts(&events), vec!["final one", "final two"]);
    }

    #[test]
    fn context_tokens_come_from_the_most_recent_assistant_event() {
        let events = vec![
            assistant("a", 10),
            assistant("", 55_000),
            assistant("b", 120_000),
        ];
        assert_eq!(context_tokens(&events), 120_000);
        assert_eq!(context_tokens(&[]), 0);
    }

    #[test]
    fn tool_failure_rate_is_measured_over_the_trailing_window() {
        let cfg = ScoreConfig {
            window: 2,
            ..ScoreConfig::default()
        };
        let mut events = turns(3, "", "[zirv] ok", false, 120_000);
        events.extend(turn_with(
            "{\"command\":\"a\"}",
            "",
            "[zirv] ok",
            true,
            120_000,
        ));
        events.extend(turn_with(
            "{\"command\":\"b\"}",
            "",
            "[zirv] ok",
            true,
            120_000,
        ));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.tool_failure_rate, 1.0, "only the last two turns count");
    }

    #[test]
    fn no_tool_results_means_no_failures() {
        let events = vec![NormalizedEvent::TurnStart, assistant("[zirv] hi", 120_000)];
        let s = signals(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(s.tool_failure_rate, 0.0);
    }

    #[test]
    fn identical_tool_calls_are_counted_and_distinct_ones_are_not() {
        let cfg = ScoreConfig::default();
        let mut repeated = vec![NormalizedEvent::TurnStart];
        for _ in 0..4 {
            repeated.push(tool("Bash", "{\"command\":\"ls\"}"));
        }
        let s = signals(&repeated, full_caps(), &cfg);
        assert_eq!(s.max_repeat, 4);
        assert_eq!(s.repetition_hits, 1);

        let mut distinct = vec![NormalizedEvent::TurnStart];
        for i in 0..4 {
            distinct.push(tool("Bash", &format!("{{\"command\":\"ls {i}\"}}")));
        }
        let s = signals(&distinct, full_caps(), &cfg);
        assert_eq!(s.max_repeat, 1);
        assert_eq!(s.repetition_hits, 0);
    }

    #[test]
    fn same_input_different_tool_is_not_a_repetition() {
        let events = vec![
            NormalizedEvent::TurnStart,
            tool("Read", "{\"file_path\":\"/a\"}"),
            tool("Write", "{\"file_path\":\"/a\"}"),
            tool("Edit", "{\"file_path\":\"/a\"}"),
        ];
        let s = signals(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(s.max_repeat, 1);
    }

    #[test]
    fn marker_miss_rate_is_measured_over_the_last_window_of_turn_finals() {
        let cfg = ScoreConfig::default();
        let mut events = turns(2, "", "[zirv] ok", false, 120_000);
        events.extend(turns(10, "", "sloppy", false, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.turns, 12);
        assert_eq!(s.marker_miss_rate, Some(1.0), "last 10 finals all miss");
    }

    #[test]
    fn half_missing_markers_is_a_half_rate() {
        let cfg = ScoreConfig::default();
        let mut events = turns(6, "", "[zirv] ok", false, 120_000);
        events.extend(turns(4, "", "sloppy", false, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, Some(0.4));
    }

    #[test]
    fn mid_turn_notes_never_count_against_the_marker() {
        let cfg = ScoreConfig::default();
        let events = turns(12, "no prefix here", "[zirv] ok", false, 120_000);
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, Some(0.0));
    }

    #[test]
    fn marker_signal_is_inactive_for_immature_sessions() {
        let cfg = ScoreConfig::default();
        let mut events = turns(1, "", "[zirv] ok", false, 120_000);
        events.extend(turns(7, "", "sloppy", false, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.turns, 8);
        assert_eq!(s.marker_miss_rate, None, "8 turns is below min_turns");
    }

    #[test]
    fn marker_signal_is_inactive_when_the_marker_never_appears() {
        let cfg = ScoreConfig::default();
        let events = turns(12, "", "no marker anywhere", false, 120_000);
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, None, "the hook is not installed");
    }

    #[test]
    fn marker_signal_is_inactive_without_the_capability() {
        let cfg = ScoreConfig::default();
        let mut events = turns(2, "", "[zirv] ok", false, 120_000);
        events.extend(turns(10, "", "sloppy", false, 120_000));
        let caps = Capabilities {
            marker_signal: false,
            token_usage: true,
            turn_signal: true,
        };
        assert_eq!(signals(&events, caps, &cfg).marker_miss_rate, None);
    }

    #[test]
    fn marker_signal_is_inactive_when_configured_empty() {
        let cfg = ScoreConfig {
            marker: String::new(),
            ..ScoreConfig::default()
        };
        let events = turns(12, "", "anything", false, 120_000);
        assert_eq!(signals(&events, full_caps(), &cfg).marker_miss_rate, None);
    }

    #[test]
    fn signals_are_reported_even_below_the_token_floor() {
        let cfg = ScoreConfig::default();
        let mut events = turns(2, "", "[zirv] ok", false, 1_000);
        events.extend(turns(10, "", "sloppy", true, 1_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, Some(1.0));
        assert_eq!(s.tool_failure_rate, 1.0);
    }

    #[test]
    fn repetition_component_ramps_from_the_threshold() {
        assert_eq!(repetition_component(0, 3), 0.0);
        assert_eq!(repetition_component(2, 3), 0.0);
        assert!((repetition_component(3, 3) - 1.0 / 3.0).abs() < 1e-9);
        assert!((repetition_component(4, 3) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(repetition_component(5, 3), 1.0);
        assert_eq!(repetition_component(50, 3), 1.0, "clamped");
        assert_eq!(
            repetition_component(5, 0),
            0.0,
            "a zero threshold disables the signal"
        );
    }

    #[test]
    fn thresholds_map_scores_to_verdicts_above_the_floor() {
        let cfg = ScoreConfig::default();
        let tokens = 120_000;
        assert_eq!(verdict_for(0, tokens, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(39, tokens, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(40, tokens, &cfg), Verdict::Advise);
        assert_eq!(verdict_for(59, tokens, &cfg), Verdict::Advise);
        assert_eq!(verdict_for(60, tokens, &cfg), Verdict::Compact);
        assert_eq!(verdict_for(79, tokens, &cfg), Verdict::Compact);
        assert_eq!(verdict_for(80, tokens, &cfg), Verdict::Restart);
        assert_eq!(verdict_for(100, tokens, &cfg), Verdict::Restart);
    }

    #[test]
    fn below_the_token_floor_the_verdict_is_always_healthy() {
        let cfg = ScoreConfig::default();
        assert_eq!(verdict_for(100, 99_999, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(0, 0, &cfg), Verdict::Healthy);
        assert_eq!(
            verdict_for(100, 100_000, &cfg),
            Verdict::Restart,
            "floor is inclusive"
        );
    }

    #[test]
    fn at_the_ceiling_the_verdict_is_at_least_compact() {
        let cfg = ScoreConfig::default();
        assert_eq!(verdict_for(0, 160_000, &cfg), Verdict::Compact);
        assert_eq!(verdict_for(45, 200_000, &cfg), Verdict::Compact);
    }

    #[test]
    fn at_the_ceiling_a_compact_level_score_escalates_to_restart() {
        let cfg = ScoreConfig::default();
        assert_eq!(verdict_for(60, 160_000, &cfg), Verdict::Restart);
        assert_eq!(verdict_for(70, 170_000, &cfg), Verdict::Restart);
        assert_eq!(verdict_for(59, 170_000, &cfg), Verdict::Compact);
    }

    #[test]
    fn verdicts_are_ordered_for_escalation_comparisons() {
        assert!(Verdict::Restart > Verdict::Compact);
        assert!(Verdict::Compact > Verdict::Advise);
        assert!(Verdict::Advise > Verdict::Healthy);
    }

    #[test]
    fn verdict_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Verdict::Restart).expect("serialize"),
            "\"restart\""
        );
        assert_eq!(Verdict::Compact.as_str(), "compact");
    }

    #[test]
    fn a_tool_failure_spike_alone_reaches_advise() {
        let cfg = ScoreConfig::default();
        let events = turns(12, "", "[zirv] ok", true, 120_000);
        let result = score_events(&events, full_caps(), &cfg);
        assert_eq!(result.signals.tool_failure_rate, 1.0);
        assert_eq!(result.score, 40);
        assert_eq!(result.verdict, Verdict::Advise);
    }

    #[test]
    fn tool_failures_plus_repetition_reach_compact() {
        let cfg = ScoreConfig::default();
        // Same tool and input every turn, every result an error, marker intact.
        let events = looping_turns(12, "", "[zirv] ok", true, 120_000);
        let result = score_events(&events, full_caps(), &cfg);
        // 40 (failures) + 30 (repetition maxed) + 0 (marker clean) = 70
        assert_eq!(result.signals.max_repeat, 10, "window bounded");
        assert_eq!(result.signals.marker_miss_rate, Some(0.0));
        assert_eq!(result.score, 70);
        assert_eq!(result.verdict, Verdict::Compact);
    }

    #[test]
    fn all_three_signals_together_reach_restart() {
        let cfg = ScoreConfig::default();
        let mut events = looping_turns(2, "", "[zirv] ok", true, 120_000);
        events.extend(looping_turns(10, "", "sloppy", true, 120_000));
        let result = score_events(&events, full_caps(), &cfg);
        assert_eq!(result.score, 100);
        assert_eq!(result.verdict, Verdict::Restart);
    }

    #[test]
    fn without_the_marker_signal_behavior_alone_caps_at_seventy() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities {
            marker_signal: false,
            token_usage: true,
            turn_signal: true,
        };
        let mut events = looping_turns(2, "", "[zirv] ok", true, 120_000);
        events.extend(looping_turns(10, "", "sloppy", true, 120_000));
        let result = score_events(&events, caps, &cfg);
        assert_eq!(result.score, 70, "weights are not redistributed");
        assert_eq!(
            result.verdict,
            Verdict::Compact,
            "never restart on behavior alone"
        );
    }

    #[test]
    fn without_the_marker_signal_the_ceiling_still_forces_a_restart() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities {
            marker_signal: false,
            token_usage: true,
            turn_signal: true,
        };
        let events = looping_turns(12, "", "sloppy", true, 175_000);
        let result = score_events(&events, caps, &cfg);
        assert_eq!(result.score, 70);
        assert_eq!(result.context_tokens, 175_000);
        assert_eq!(result.verdict, Verdict::Restart);
    }

    #[test]
    fn scoring_is_deterministic() {
        let cfg = ScoreConfig::default();
        let mut events = looping_turns(2, "", "[zirv] ok", true, 165_000);
        events.extend(looping_turns(10, "", "sloppy", true, 165_000));
        let first = score_events(&events, full_caps(), &cfg);
        for _ in 0..20 {
            assert_eq!(score_events(&events, full_caps(), &cfg), first);
        }
    }

    #[test]
    fn an_empty_transcript_is_healthy() {
        let result = score_events(&[], full_caps(), &ScoreConfig::default());
        assert_eq!(result.score, 0);
        assert_eq!(result.verdict, Verdict::Healthy);
        assert_eq!(result.context_tokens, 0);
        assert_eq!(result.signals.turns, 0);
    }

    #[test]
    fn compaction_drops_the_reported_context_size() {
        let cfg = ScoreConfig::default();
        let mut events = turns(12, "", "[zirv] ok", false, 170_000);
        events.push(NormalizedEvent::Compaction);
        events.extend(turn_with(
            "{\"command\":\"post\"}",
            "",
            "[zirv] ok",
            false,
            12_000,
        ));
        let result = score_events(&events, full_caps(), &cfg);
        assert_eq!(result.context_tokens, 12_000);
        assert_eq!(
            result.verdict,
            Verdict::Healthy,
            "post-compaction sessions are healthy again"
        );
    }

    // The eight cases from ~/.claude/hooks/canary-check.test.sh, ported. The
    // canary's warn tier maps to `advise` and its block tier to `restart`, but
    // the verdicts below follow zirv's own gate rules, which weight the noisy
    // marker signal far lower than the canary did. Case 7 (the stop_hook_active
    // guard) is not a scoring case and is covered in Task A15.
    #[test]
    fn ported_canary_case_1_bimodal_healthy() {
        let events = turns(12, "", "[zirv] ok", false, 120_000);
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, Some(0.0));
        assert_eq!(result.verdict, Verdict::Healthy);
    }

    #[test]
    fn ported_canary_case_2_young_sloppy_session() {
        let mut events = turns(1, "", "[zirv] ok", false, 120_000);
        events.extend(turns(7, "", "sloppy", false, 120_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, None);
        assert_eq!(result.score, 0);
        assert_eq!(result.verdict, Verdict::Healthy);
    }

    #[test]
    fn ported_canary_case_3_sustained_misses_below_the_floor() {
        let mut events = turns(2, "", "[zirv] ok", false, 90_000);
        events.extend(turns(10, "", "sloppy", false, 90_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(
            result.signals.marker_miss_rate,
            Some(1.0),
            "signal still reported"
        );
        assert_eq!(result.score, 30);
        assert_eq!(result.verdict, Verdict::Healthy, "the floor gate wins");
    }

    #[test]
    fn ported_canary_case_4_sustained_misses_above_the_ceiling() {
        let mut events = turns(2, "", "[zirv] ok", false, 170_000);
        events.extend(turns(10, "", "sloppy", false, 170_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.score, 30);
        assert_eq!(
            result.verdict,
            Verdict::Compact,
            "marker misses alone never restart"
        );
    }

    #[test]
    fn ported_canary_case_5_egregious_but_low_context_is_never_escalated() {
        let mut events = looping_turns(2, "", "[zirv] ok", true, 40_000);
        events.extend(looping_turns(10, "", "sloppy", true, 40_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.score, 100, "every signal is firing");
        assert_eq!(
            result.verdict,
            Verdict::Healthy,
            "and it still must not intervene"
        );
    }

    #[test]
    fn ported_canary_case_6_marker_never_loaded() {
        let events = turns(12, "", "no marker at all", false, 170_000);
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, None);
        assert_eq!(result.score, 0);
        assert_eq!(
            result.verdict,
            Verdict::Compact,
            "the ceiling gate still applies"
        );
    }

    #[test]
    fn ported_canary_case_8_half_missing_stays_below_advise() {
        let mut events = turns(6, "", "[zirv] ok", false, 120_000);
        events.extend(turns(4, "", "sloppy", false, 120_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, Some(0.4));
        assert_eq!(result.score, 12);
        assert_eq!(result.verdict, Verdict::Healthy);
    }
}
