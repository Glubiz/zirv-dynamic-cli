// Consumed by the `score` verb added in a later task of this plan; nothing
// calls this yet outside tests, so dead_code is silenced module-wide until
// then.
#![allow(dead_code)]

use hashbrown::HashMap;
use serde::Serialize;

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
}
