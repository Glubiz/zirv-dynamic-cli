use std::collections::VecDeque;

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

/// Share of `results` that failed. Zero for an empty window, which is what a
/// session that ran no tools should score.
fn failure_rate<I: Iterator<Item = bool>>(results: I) -> f64 {
    let mut total = 0usize;
    let mut errors = 0usize;
    for is_error in results {
        total += 1;
        errors += usize::from(is_error);
    }
    if total == 0 {
        return 0.0;
    }
    errors as f64 / total as f64
}

/// `(repetition_hits, max_repeat)` over identical `(tool, input)` pairs.
fn repetition<'a, I: Iterator<Item = (&'a str, u64)>>(
    calls: I,
    threshold: usize,
) -> (usize, usize) {
    let mut counts: HashMap<(&str, u64), usize> = HashMap::new();
    for key in calls {
        *counts.entry(key).or_insert(0) += 1;
    }
    let max_repeat = counts.values().copied().max().unwrap_or(0);
    let hits = counts.values().filter(|count| **count >= threshold).count();
    (hits, max_repeat)
}

/// Share of the already-windowed turn finals that are missing the marker.
/// Only called with a non-empty slice: an empty one means the marker signal is
/// inactive and no rate is reported at all.
fn miss_rate(marked: &[bool]) -> f64 {
    marked.iter().filter(|m| !**m).count() as f64 / marked.len() as f64
}

pub fn signals(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Signals {
    let finals = turn_final_texts(events);
    let turns = finals.len();

    let marker_ever = finals.iter().any(|t| has_marker(t, &cfg.marker));
    let marker_active =
        caps.marker_signal && !cfg.marker.is_empty() && marker_ever && turns >= cfg.min_turns;

    let marker_miss_rate = marker_active.then(|| {
        let marked: Vec<bool> = last_window(&finals, cfg.window)
            .iter()
            .map(|t| has_marker(t, &cfg.marker))
            .collect();
        miss_rate(&marked)
    });

    let tail = events_in_last_turns(events, cfg.window);

    let tool_failure_rate = failure_rate(tail.iter().filter_map(|e| match e {
        NormalizedEvent::ToolResult { is_error } => Some(*is_error),
        _ => None,
    }));

    let (repetition_hits, max_repeat) = repetition(
        tail.iter().filter_map(|e| match e {
            NormalizedEvent::ToolCall { name, input_hash } => Some((name.as_str(), *input_hash)),
            _ => None,
        }),
        cfg.repetition_threshold,
    );

    Signals {
        turns,
        tool_failure_rate,
        repetition_hits,
        max_repeat,
        marker_miss_rate,
    }
}

/// Tool activity of one turn segment: all the windowed signals need from the
/// events between two `TurnStart`s.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Segment {
    calls: Vec<(String, u64)>,
    results: Vec<bool>,
}

/// Exactly what `signals` and `context_tokens` read out of a full event
/// stream, maintained one event at a time so a growing transcript can be
/// folded in as it arrives instead of parsed from the start on every pass.
///
/// Bounded by `window`: only the turn segments and turn-final markers the
/// windowed signals can still reach are retained. A `window` of zero means "no
/// window at all", which is unbounded, so `new` refuses it and callers fall
/// back to a full parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotState {
    marker: String,
    window: usize,
    /// Turn finals already closed by a later `TurnStart`.
    closed_turns: usize,
    /// `has_marker` of the last `window` closed finals, oldest first.
    closed_markers: VecDeque<bool>,
    marker_seen: bool,
    in_turn: bool,
    /// `has_marker` of the text the still-open turn would contribute.
    open_marker: Option<bool>,
    last_tokens: u64,
    /// `segments[0]` is the run of events before the first `TurnStart`, kept
    /// only while the window still reaches back that far.
    segments: VecDeque<Segment>,
    turn_starts: usize,
}

impl RotState {
    /// `None` when the configured window is unbounded, which this state cannot
    /// represent in bounded memory.
    pub fn new(cfg: &ScoreConfig) -> Option<Self> {
        if cfg.window == 0 {
            return None;
        }
        Some(Self {
            marker: cfg.marker.clone(),
            window: cfg.window,
            closed_turns: 0,
            closed_markers: VecDeque::new(),
            marker_seen: false,
            in_turn: false,
            open_marker: None,
            last_tokens: 0,
            segments: VecDeque::from([Segment::default()]),
            turn_starts: 0,
        })
    }

    /// Whether this state was folded under the same rules `cfg` describes.
    /// Retention already discarded what a different window or marker would
    /// need, so a mismatch can only be answered by rebuilding.
    pub fn built_for(&self, cfg: &ScoreConfig) -> bool {
        self.window == cfg.window && self.marker == cfg.marker
    }

    pub fn feed_all(&mut self, events: &[NormalizedEvent]) {
        for event in events {
            self.feed(event);
        }
    }

    pub fn feed(&mut self, event: &NormalizedEvent) {
        match event {
            NormalizedEvent::TurnStart => {
                if self.in_turn
                    && let Some(marked) = self.open_marker
                {
                    self.close_turn(marked);
                }
                self.in_turn = true;
                self.open_marker = None;
                self.turn_starts += 1;
                self.segments.push_back(Segment::default());
                // Once the window no longer reaches the first turn, neither the
                // pre-turn prefix nor the older segments can be read again.
                if self.turn_starts > self.window {
                    while self.segments.len() > self.window {
                        self.segments.pop_front();
                    }
                }
            }
            NormalizedEvent::AssistantFinal { text, input_tokens } => {
                self.last_tokens = *input_tokens;
                if !text.trim().is_empty() {
                    self.open_marker = Some(has_marker(text, &self.marker));
                }
            }
            NormalizedEvent::ToolCall { name, input_hash } => {
                if let Some(segment) = self.segments.back_mut() {
                    segment.calls.push((name.clone(), *input_hash));
                }
            }
            NormalizedEvent::ToolResult { is_error } => {
                if let Some(segment) = self.segments.back_mut() {
                    segment.results.push(*is_error);
                }
            }
            NormalizedEvent::Compaction => {}
        }
    }

    fn close_turn(&mut self, marked: bool) {
        self.closed_turns += 1;
        self.marker_seen |= marked;
        self.closed_markers.push_back(marked);
        if self.closed_markers.len() > self.window {
            self.closed_markers.pop_front();
        }
    }

    /// `None` when `cfg` no longer matches the rules this state was folded
    /// under, which is the caller's cue to rebuild from a full parse.
    pub fn score(&self, caps: Capabilities, cfg: &ScoreConfig) -> Option<Score> {
        if !self.built_for(cfg) {
            return None;
        }
        Some(score_from(self.signals(caps, cfg), self.last_tokens, cfg))
    }

    fn signals(&self, caps: Capabilities, cfg: &ScoreConfig) -> Signals {
        // The open turn's text is a turn final too: a full parse flushes it at
        // the end of the stream even though no later `TurnStart` closed it.
        let turns = self.closed_turns + usize::from(self.open_marker.is_some());
        let marker_ever = self.marker_seen || self.open_marker == Some(true);
        let marker_active =
            caps.marker_signal && !cfg.marker.is_empty() && marker_ever && turns >= cfg.min_turns;

        let marker_miss_rate = marker_active.then(|| {
            let mut marked: Vec<bool> = self.closed_markers.iter().copied().collect();
            marked.extend(self.open_marker);
            if marked.len() > self.window {
                marked.drain(..marked.len() - self.window);
            }
            miss_rate(&marked)
        });

        let tool_failure_rate =
            failure_rate(self.segments.iter().flat_map(|s| s.results.iter().copied()));
        let (repetition_hits, max_repeat) = repetition(
            self.segments
                .iter()
                .flat_map(|s| s.calls.iter().map(|(name, hash)| (name.as_str(), *hash))),
            cfg.repetition_threshold,
        );

        Signals {
            turns,
            tool_failure_rate,
            repetition_hits,
            max_repeat,
            marker_miss_rate,
        }
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
    score_from(signals(events, caps, cfg), context_tokens(events), cfg)
}

/// The weighted sum and the gate, shared by the full-parse and incremental
/// paths so the two can never drift apart.
pub fn score_from(signals: Signals, tokens: u64, cfg: &ScoreConfig) -> Score {
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

    /// Issue #155, Phase 6(c): `rot.rs` stays pure -- no filesystem, clock,
    /// env, network, AND no visibility into quota/usage data (`pace`/
    /// `window`), so a session 97% through its five-hour window scores
    /// exactly like one at 3%. Quota pressure gates NEW spawns only
    /// (`pace::spawn_gate`, consulted from `agent.rs`/`dash/mod.rs`) --
    /// restarting a session because it is expensive would discard a warm
    /// cache and re-read the whole context, the single most expensive
    /// possible reaction to a cost signal. This scans this file's own
    /// PRODUCTION code (everything before the `#[cfg(test)]` marker that
    /// starts this very module) for a reference to either module, rather
    /// than merely asserting the invariant in prose, so a future `use
    /// super::pace` or a `window::`-qualified path compiles clean and then
    /// silently reintroduces the coupling this test exists to forbid.
    /// Scoped to the PRODUCTION half of the file on purpose: this test's own
    /// needles below would otherwise trip on themselves once included, and
    /// the bare word "window" is legitimately this module's own vocabulary
    /// (`ScoreConfig::window`, `RotState`'s own bounded turn window) used
    /// dozens of times, which a whole-file substring search would misfire
    /// on.
    #[test]
    fn rot_stays_pure_of_pace_and_window_data() {
        const THIS_FILE: &str = include_str!("rot.rs");
        let production_code = THIS_FILE
            .split("#[cfg(test)]")
            .next()
            .expect("this file always has a #[cfg(test)] module");
        for needle in [
            "use super::pace",
            "use super::window",
            "super::pace::",
            "super::window::",
            "pace::",
            "window::",
            "UsageWindows",
        ] {
            assert!(
                !production_code.contains(needle),
                "rot.rs's production code must never read quota/usage data -- found `{needle}`; \
                 a scheduling gate on quota belongs in pace.rs's spawn_gate, never in rot's own \
                 scoring"
            );
        }
    }

    fn full_caps() -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: false,
            events: true,
            defer_injection_submit: false,
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

    /// Every fixture shape the incremental state has to survive: an empty
    /// stream, events before the first turn, an open final turn, a stream
    /// longer than the window, and one that compacts mid-way.
    fn equivalence_fixtures() -> Vec<(&'static str, Vec<NormalizedEvent>)> {
        let mut past_window = looping_turns(4, "", "[zirv] ok", true, 120_000);
        past_window.extend(turns(20, "mid", "sloppy", false, 165_000));

        let mut with_compaction = turns(12, "", "[zirv] ok", false, 170_000);
        with_compaction.push(NormalizedEvent::Compaction);
        with_compaction.extend(turn_with(
            "{\"command\":\"p\"}",
            "",
            "[zirv] ok",
            false,
            12_000,
        ));

        let mut before_first_turn = vec![
            assistant("orphan text", 5_000),
            tool("Bash", "{\"command\":\"ls\"}"),
            NormalizedEvent::ToolResult { is_error: true },
        ];
        before_first_turn.extend(turns(3, "", "[zirv] ok", false, 120_000));

        let mut open_turn = turns(11, "", "[zirv] ok", false, 120_000);
        open_turn.push(NormalizedEvent::TurnStart);
        open_turn.push(assistant("still working", 130_000));

        vec![
            ("empty", Vec::new()),
            ("short", turns(3, "", "[zirv] ok", false, 120_000)),
            ("past the window", past_window),
            ("with a compaction", with_compaction),
            ("events before the first turn", before_first_turn),
            ("an open final turn", open_turn),
            ("no turn starts at all", vec![assistant("[zirv] hi", 9)]),
        ]
    }

    /// The whole point of the incremental path: folding the same events in any
    /// number of chunks has to land on the byte-identical score a single full
    /// parse produces.
    #[test]
    fn folding_events_in_chunks_matches_a_full_parse() {
        for cfg in [
            ScoreConfig::default(),
            ScoreConfig {
                window: 3,
                min_turns: 2,
                ..ScoreConfig::default()
            },
        ] {
            for (name, events) in equivalence_fixtures() {
                let expected = score_events(&events, full_caps(), &cfg);
                for chunk in [1, 2, 5, 97] {
                    let mut state = RotState::new(&cfg).expect("bounded window");
                    for part in events.chunks(chunk) {
                        state.feed_all(part);
                    }
                    assert_eq!(
                        state.score(full_caps(), &cfg),
                        Some(expected.clone()),
                        "{name} in chunks of {chunk} (window {})",
                        cfg.window
                    );
                }
            }
        }
    }

    /// A prefix is not a special case: the state has to be right after every
    /// single event, because that is what a per-turn scoring pass reads.
    #[test]
    fn every_prefix_of_a_stream_scores_like_a_full_parse_of_that_prefix() {
        let cfg = ScoreConfig::default();
        let mut events = looping_turns(3, "", "[zirv] ok", true, 120_000);
        events.extend(turns(14, "note", "sloppy", false, 170_000));

        let mut state = RotState::new(&cfg).expect("bounded window");
        for (index, event) in events.iter().enumerate() {
            state.feed(event);
            assert_eq!(
                state.score(full_caps(), &cfg),
                Some(score_events(&events[..=index], full_caps(), &cfg)),
                "prefix of {} events",
                index + 1
            );
        }
    }

    #[test]
    fn an_unbounded_window_has_no_incremental_state() {
        let cfg = ScoreConfig {
            window: 0,
            ..ScoreConfig::default()
        };
        assert!(
            RotState::new(&cfg).is_none(),
            "an unbounded window cannot be folded in bounded memory"
        );
    }

    #[test]
    fn state_folded_under_other_rules_refuses_to_score() {
        let cfg = ScoreConfig::default();
        let mut state = RotState::new(&cfg).expect("bounded window");
        state.feed_all(&turns(12, "", "[zirv] ok", false, 120_000));

        let other_marker = ScoreConfig {
            marker: "[other]".to_string(),
            ..cfg.clone()
        };
        let other_window = ScoreConfig {
            window: 4,
            ..cfg.clone()
        };
        assert!(state.score(full_caps(), &other_marker).is_none());
        assert!(state.score(full_caps(), &other_window).is_none());
        assert!(state.score(full_caps(), &cfg).is_some());
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
            system_prompt: false,
            events: true,
            defer_injection_submit: false,
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
            system_prompt: false,
            events: true,
            defer_injection_submit: false,
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
            system_prompt: false,
            events: true,
            defer_injection_submit: false,
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
