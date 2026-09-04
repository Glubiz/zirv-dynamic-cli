use std::collections::VecDeque;

use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};

use super::config::ScoreConfig;
use super::event::{Capabilities, ModelChange, NormalizedEvent, ProviderErrorClass};

/// Leading characters a model tends to put before a reply prefix. Ported from
/// the shell canary's `^[ \t>*_`#~-]*` allowance.
const MARKER_LEAD: [char; 10] = [' ', '\t', '\n', '\r', '>', '*', '_', '`', '#', '~'];

/// Edit-like tool names, matched case-insensitively: Claude Code's own edit
/// tools plus codex's `apply_patch` (`src/commands/ctx/adapters/claude.rs`
/// passes tool-use block names through verbatim, so the real spelling is
/// exactly Claude's own tool names -- `"Edit"`, `"Write"`, `"MultiEdit"`,
/// `"NotebookEdit"` -- while `codex.rs`'s `parse_events` never emits
/// `ToolCall` at all today, so `"apply_patch"` is future-proofing, not yet
/// reachable). This is a deliberate, independent COPY of
/// `workflow::adoption::EDIT_LIKE_TOOLS`
/// (`src/commands/workflow/adoption.rs:13`), not a shared import: importing
/// from `workflow` would give this pure, fs/clock/env/net-free module a
/// dependency on a much less constrained module for a five-item list. Keep
/// the two lists in sync by hand; `adoption::EDIT_LIKE_TOOLS` is private to
/// its module, which is why this can only be a comment pointing at it rather
/// than a same-file cross-check test.
const EDIT_LIKE_TOOLS: &[&str] = &["edit", "write", "multiedit", "notebookedit", "apply_patch"];

/// Whether `name` is one of [`EDIT_LIKE_TOOLS`], case-insensitively.
fn is_edit_like(name: &str) -> bool {
    EDIT_LIKE_TOOLS
        .iter()
        .any(|tool| name.eq_ignore_ascii_case(tool))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signals {
    pub turns: usize,
    pub tool_failure_rate: f64,
    pub repetition_hits: usize,
    pub max_repeat: usize,
    /// The longest run of consecutive identical (normalized) tool-result
    /// error texts within the window: three different fixes that all hit
    /// the SAME compiler/test error, not three attempts at different ones.
    /// Unlike `repetition_hits`/`max_repeat` (which key on `(tool,
    /// input_hash)`), this fires even though every attempt's input differs,
    /// since what repeats here is the ERROR, not the call. See
    /// `NormalizedEvent::ToolErrorText`.
    pub same_error_repeats: usize,
    pub provider_overflows: usize,
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
            NormalizedEvent::TurnStart { .. } => {
                if in_turn && let Some(text) = current.take() {
                    finals.push(text);
                }
                in_turn = true;
                current = None;
            }
            NormalizedEvent::AssistantFinal { text, .. } if !text.trim().is_empty() => {
                current = Some(text.clone());
            }
            NormalizedEvent::Compaction => {
                if in_turn && let Some(text) = current.take() {
                    finals.push(text);
                }
                in_turn = false;
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
        .filter(|(_, e)| matches!(e, NormalizedEvent::TurnStart { .. }))
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

/// `(repetition_hits, max_repeat)` over identical `(tool, input)` pairs,
/// interleave-aware: a repeat of `(name, input_hash)` only extends its streak
/// when no edit-like call (`is_edit_like`) happened since the previous
/// occurrence of that exact pair. This is what tells a healthy
/// edit -> rerun -> edit -> rerun TDD loop apart from a stuck agent
/// re-running the same passing check with nothing changed in between: the
/// TDD loop always has an edit between reruns, so it never builds a streak.
///
/// An edit-like call breaks EVERY key's in-flight streak at once, not just
/// the key it happens to share a name with: it sits between any pair of
/// calls made before and after it, for every key, so clearing every streak
/// on an edit is exactly the per-key rule applied uniformly. Edit-like calls
/// are themselves never tracked as a repeated call -- they are the healthy
/// action this signal exists to stop penalising, never the over-verification
/// it exists to catch.
fn repetition<'a, I: Iterator<Item = (&'a str, u64)>>(
    calls: I,
    threshold: usize,
) -> (usize, usize) {
    let mut streaks: HashMap<(&str, u64), usize> = HashMap::new();
    let mut hit: HashSet<(&str, u64)> = HashSet::new();
    let mut max_repeat = 0usize;
    for (name, hash) in calls {
        if is_edit_like(name) {
            streaks.clear();
            continue;
        }
        let count = streaks.entry((name, hash)).or_insert(0);
        *count += 1;
        max_repeat = max_repeat.max(*count);
        if *count >= threshold {
            hit.insert((name, hash));
        }
    }
    (hit.len(), max_repeat)
}

/// The longest run of consecutive identical hashes among `entries`, in
/// transcript order. `entries` carries one slot per `NormalizedEvent::
/// ToolResult`, in order: `Some(hash)` when that result was erroring and
/// immediately followed by its `ToolErrorText`, `None` for a successful
/// result or an erroring one with no extractable text (`result_error_entries`
/// is what builds this). A `None` DOES interrupt the streak -- a successful
/// result (or an error result whose text could not be extracted) between two
/// occurrences of the SAME error is a genuine break, unlike `repetition`'s
/// edit-interruption rule, which only edit-like tool CALLS reset. Only two
/// adjacent `Some` entries carrying the SAME hash extend a run; a `Some` with
/// a DIFFERENT hash resets it just like a `None` does.
fn longest_same_error_run<I: Iterator<Item = Option<u64>>>(entries: I) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    let mut last: Option<u64> = None;
    for entry in entries {
        match entry {
            Some(hash) if last == Some(hash) => current += 1,
            Some(hash) => {
                current = 1;
                last = Some(hash);
            }
            None => {
                current = 0;
                last = None;
            }
        }
        longest = longest.max(current);
    }
    longest
}

/// Pairs every `NormalizedEvent::ToolResult` in `events` with the
/// `ToolErrorText` hash that immediately follows it, if any -- one entry per
/// result, in order. `ToolErrorText` is only ever emitted directly after the
/// `ToolResult` it describes (see that variant's own doc comment), so
/// setting the LAST pushed entry is always setting the entry for the result
/// it belongs to, never an earlier one.
fn result_error_entries(events: &[NormalizedEvent]) -> Vec<Option<u64>> {
    let mut entries: Vec<Option<u64>> = Vec::new();
    for event in events {
        match event {
            NormalizedEvent::ToolResult { .. } => entries.push(None),
            NormalizedEvent::ToolErrorText { hash } => {
                if let Some(last) = entries.last_mut() {
                    *last = Some(*hash);
                }
            }
            _ => {}
        }
    }
    entries
}

/// Share of the already-windowed turn finals that are missing the marker.
/// Only called with a non-empty slice: an empty one means the marker signal is
/// inactive and no rate is reported at all.
fn miss_rate(marked: &[bool]) -> f64 {
    marked.iter().filter(|m| !**m).count() as f64 / marked.len() as f64
}

pub fn signals(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Signals {
    let turns = turn_final_texts(events).len();
    let boundary = events
        .iter()
        .rposition(|event| matches!(event, NormalizedEvent::Compaction))
        .map_or(events, |index| &events[index + 1..]);
    let finals = turn_final_texts(boundary);

    let marker_ever = finals.iter().any(|t| has_marker(t, &cfg.marker));
    let marker_active = caps.marker_signal
        && !cfg.marker.is_empty()
        && marker_ever
        && finals.len() >= cfg.min_turns;

    let marker_miss_rate = marker_active.then(|| {
        let marked: Vec<bool> = last_window(&finals, cfg.window)
            .iter()
            .map(|t| has_marker(t, &cfg.marker))
            .collect();
        miss_rate(&marked)
    });

    let tail = events_in_last_turns(boundary, cfg.window);

    let tool_failure_rate = failure_rate(tail.iter().filter_map(|e| match e {
        NormalizedEvent::ToolResult { is_error } => Some(*is_error),
        _ => None,
    }));

    let (repetition_hits, max_repeat) = repetition(
        tail.iter().filter_map(|e| match e {
            NormalizedEvent::ToolCall {
                name, input_hash, ..
            } => Some((name.as_str(), *input_hash)),
            _ => None,
        }),
        cfg.repetition_threshold,
    );
    let provider_overflows = tail
        .iter()
        .filter(|event| {
            matches!(
                event,
                NormalizedEvent::ProviderError {
                    class: ProviderErrorClass::Overflow
                }
            )
        })
        .count();

    let same_error_repeats = longest_same_error_run(result_error_entries(tail).into_iter());

    Signals {
        turns,
        tool_failure_rate,
        repetition_hits,
        max_repeat,
        same_error_repeats,
        provider_overflows,
        marker_miss_rate,
    }
}

/// Tool activity of one turn segment: all the windowed signals need from the
/// events between two `TurnStart`s.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Segment {
    calls: Vec<(String, u64)>,
    results: Vec<bool>,
    /// One entry per `NormalizedEvent::ToolResult` in this segment, in
    /// order, for `same_error_repeats`: `Some(hash)` when that result was
    /// erroring and immediately followed by its `ToolErrorText`, `None` for a
    /// successful result or an erroring one with no extractable text. See
    /// `longest_same_error_run`'s own doc comment for why `None` has to be
    /// preserved rather than simply omitted.
    result_errors: Vec<Option<u64>>,
    #[serde(default)]
    provider_overflows: usize,
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
    /// Closed turn finals since the latest compaction boundary.
    #[serde(default)]
    behavioral_closed_turns: usize,
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
            behavioral_closed_turns: 0,
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
            NormalizedEvent::TurnStart { .. } => {
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
            NormalizedEvent::AssistantFinal {
                text, input_tokens, ..
            } => {
                self.last_tokens = *input_tokens;
                if !text.trim().is_empty() {
                    self.open_marker = Some(has_marker(text, &self.marker));
                }
            }
            NormalizedEvent::ToolCall {
                name, input_hash, ..
            } => {
                if let Some(segment) = self.segments.back_mut() {
                    segment.calls.push((name.clone(), *input_hash));
                }
            }
            NormalizedEvent::ToolResult { is_error } => {
                if let Some(segment) = self.segments.back_mut() {
                    segment.results.push(*is_error);
                    // Placeholder until (and unless) the sibling
                    // `ToolErrorText` for THIS result arrives -- see
                    // `Segment::result_errors`'s own doc comment.
                    segment.result_errors.push(None);
                }
            }
            NormalizedEvent::ToolErrorText { hash } => {
                if let Some(segment) = self.segments.back_mut()
                    && let Some(last) = segment.result_errors.last_mut()
                {
                    *last = Some(*hash);
                }
            }
            NormalizedEvent::ProviderError {
                class: ProviderErrorClass::Overflow,
            } => {
                if let Some(segment) = self.segments.back_mut() {
                    segment.provider_overflows += 1;
                }
            }
            NormalizedEvent::ProviderError { .. }
            | NormalizedEvent::ModelId { .. }
            | NormalizedEvent::AssistantFirstText { .. }
            | NormalizedEvent::ToolResultTimestamp { .. }
            // Issue #312: window-attribution siblings. `RotState` scores
            // rot signals only -- the byte/staleness bookkeeping they carry
            // is folded separately, by `breakdown::BreakdownAccumulator`
            // (see `score.rs`'s own I/O-layer callers), never here.
            | NormalizedEvent::UserText { .. }
            | NormalizedEvent::AssistantThinking { .. }
            | NormalizedEvent::ToolResultSize { .. }
            | NormalizedEvent::ToolCallPath { .. } => {}
            NormalizedEvent::Compaction => self.reset_behavioral_window(),
        }
    }

    fn close_turn(&mut self, marked: bool) {
        self.closed_turns += 1;
        self.behavioral_closed_turns += 1;
        self.marker_seen |= marked;
        self.closed_markers.push_back(marked);
        if self.closed_markers.len() > self.window {
            self.closed_markers.pop_front();
        }
    }

    fn reset_behavioral_window(&mut self) {
        if self.in_turn && self.open_marker.is_some() {
            self.closed_turns += 1;
        }
        self.behavioral_closed_turns = 0;
        self.closed_markers.clear();
        self.marker_seen = false;
        self.in_turn = false;
        self.open_marker = None;
        self.segments = VecDeque::from([Segment::default()]);
    }

    /// `None` when `cfg` no longer matches the rules this state was folded
    /// under, which is the caller's cue to rebuild from a full parse.
    pub fn score(&self, caps: Capabilities, cfg: &ScoreConfig) -> Option<Score> {
        if !self.built_for(cfg) {
            return None;
        }
        Some(score_from(
            self.signals(caps, cfg),
            self.last_tokens,
            cfg,
            caps,
        ))
    }

    fn signals(&self, caps: Capabilities, cfg: &ScoreConfig) -> Signals {
        // The open turn's text is a turn final too: a full parse flushes it at
        // the end of the stream even though no later `TurnStart` closed it.
        let turns = self.closed_turns + usize::from(self.open_marker.is_some());
        let behavioral_turns =
            self.behavioral_closed_turns + usize::from(self.open_marker.is_some());
        let marker_ever = self.marker_seen || self.open_marker == Some(true);
        let marker_active = caps.marker_signal
            && !cfg.marker.is_empty()
            && marker_ever
            && behavioral_turns >= cfg.min_turns;

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
        let same_error_repeats = longest_same_error_run(
            self.segments
                .iter()
                .flat_map(|s| s.result_errors.iter().copied()),
        );
        let provider_overflows = self
            .segments
            .iter()
            .map(|segment| segment.provider_overflows)
            .sum();

        Signals {
            turns,
            tool_failure_rate,
            repetition_hits,
            max_repeat,
            same_error_repeats,
            provider_overflows,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_change: Option<ModelChange>,
    /// Issue #312: where this session's window went, by bucket. Attached
    /// post-hoc by `score.rs`, exactly like `model_change` -- `score_from`
    /// itself always leaves this `None`, so the incremental (`RotState`) and
    /// full-parse (`score_events`) scoring paths never have to agree on it
    /// for the equivalence tests both already uphold on every OTHER field.
    /// `None` when it was never computed for this `Score`, never a
    /// fabricated empty summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_breakdown: Option<super::breakdown::BreakdownSummary>,
}

/// Zero below the threshold, then a linear ramp that saturates at
/// `2 * threshold - 1` identical calls.
pub fn repetition_component(max_repeat: usize, threshold: usize) -> f64 {
    if threshold == 0 || max_repeat < threshold {
        return 0.0;
    }
    (((max_repeat + 1 - threshold) as f64) / threshold as f64).clamp(0.0, 1.0)
}

/// The absolute thresholds zirv shipped before capacity was knowable. Still
/// the answer whenever no capacity is available from anywhere -- codex today,
/// and any adapter that cannot honestly state one.
pub const FALLBACK_TOKEN_FLOOR: u64 = 100_000;
pub const FALLBACK_TOKEN_CEILING: u64 = 160_000;

/// The `(floor, ceiling)` this session's token gate actually uses.
///
/// Precedence, per field: an explicit `score.token_floor`/`token_ceiling`
/// wins outright -- an operator who pins a number gets that number.
/// Otherwise the ratio is applied to the resolved capacity:
/// `score.model_context_tokens` if the operator set one (they know their
/// seat; the adapter's default is a guess about it), else
/// `caps.context_window_tokens`. With no capacity from anywhere, the
/// absolute fallbacks apply, unchanged.
///
/// PURE, like everything else in this module: capacity arrives inside
/// `Capabilities`, which `score_events` and `RotState::score` already
/// receive, so no fs, clock, env or net access is added here.
///
/// The result is always ordered and non-zero: a ceiling at or below the
/// floor would make `verdict_for`'s two-stage gate meaningless, and a
/// misconfigured pair of ratios must degrade, never break rotation.
pub fn token_gates(cfg: &ScoreConfig, caps: Capabilities) -> (u64, u64) {
    let capacity = cfg.model_context_tokens.or(caps.context_window_tokens);
    let scaled = |ratio: f64, fallback: u64| -> u64 {
        match capacity {
            Some(capacity) if ratio > 0.0 => {
                ((capacity as f64) * ratio.clamp(0.0, 1.0)).round() as u64
            }
            _ => fallback,
        }
    };
    let mut floor = cfg
        .token_floor
        .unwrap_or_else(|| scaled(cfg.token_floor_ratio, FALLBACK_TOKEN_FLOOR))
        .max(1);
    let mut ceiling = cfg
        .token_ceiling
        .unwrap_or_else(|| scaled(cfg.token_ceiling_ratio, FALLBACK_TOKEN_CEILING));
    // Never inverted, never collapsed: `verdict_for`'s two-stage gate is
    // meaningless if the ceiling is at or below the floor, and a
    // misconfigured pair of ratios must degrade rather than break rotation.
    // Only a DERIVED value is moved -- a number the operator typed is never
    // silently rewritten, so a fully pinned inverted pair stands as written.
    if ceiling <= floor {
        match (cfg.token_floor, cfg.token_ceiling) {
            (None, Some(_)) => floor = ceiling.saturating_sub(1).max(1),
            (Some(_), None) | (None, None) => ceiling = floor.saturating_add(1),
            (Some(_), Some(_)) => {}
        }
    }
    (floor, ceiling)
}

/// The token gate is a gate, not a vote: below the floor nothing escalates, at
/// or above the ceiling the verdict is at least `compact`, and at the ceiling a
/// compact-level score becomes a restart.
pub fn verdict_for(score: u32, tokens: u64, cfg: &ScoreConfig, caps: Capabilities) -> Verdict {
    let (floor, ceiling) = token_gates(cfg, caps);
    if tokens < floor {
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

    if tokens < ceiling {
        return base;
    }
    if score >= cfg.compact_at {
        return Verdict::Restart;
    }
    base.max(Verdict::Compact)
}

pub fn score_events(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Score {
    score_from(
        signals(events, caps, cfg),
        context_tokens(events),
        cfg,
        caps,
    )
}

/// The weighted sum and the gate, shared by the full-parse and incremental
/// paths so the two can never drift apart.
pub fn score_from(signals: Signals, tokens: u64, cfg: &ScoreConfig, caps: Capabilities) -> Score {
    let raw = cfg.weight_tool_failure * signals.tool_failure_rate
        + cfg.weight_repetition
            * repetition_component(signals.max_repeat, cfg.repetition_threshold)
        + cfg.weight_marker * signals.marker_miss_rate.unwrap_or(0.0)
        + cfg.same_error_weight
            * repetition_component(signals.same_error_repeats, cfg.same_error_threshold);
    let score = raw.round().clamp(0.0, 100.0) as u32;

    let overflow_verdict = match signals.provider_overflows {
        0 => Verdict::Healthy,
        1 => Verdict::Compact,
        _ => Verdict::Restart,
    };

    Score {
        score,
        verdict: verdict_for(score, tokens, cfg, caps).max(overflow_verdict),
        signals,
        context_tokens: tokens,
        model_change: None,
        window_breakdown: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::ScoreConfig;
    use crate::commands::ctx::event::{
        Capabilities, NormalizedEvent, ProviderErrorClass, input_hash,
    };

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
            context_window_tokens: None,
        }
    }

    fn assistant(text: &str, tokens: u64) -> NormalizedEvent {
        NormalizedEvent::AssistantFinal {
            text: text.to_string(),
            input_tokens: tokens,
            at_ms: None,
        }
    }

    fn tool(name: &str, input: &str) -> NormalizedEvent {
        NormalizedEvent::ToolCall {
            name: name.to_string(),
            input_hash: input_hash(input),
            at_ms: None,
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
            NormalizedEvent::TurnStart { at_ms: None },
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
        open_turn.push(NormalizedEvent::TurnStart { at_ms: None });
        open_turn.push(assistant("still working", 130_000));

        // Review finding F1: a successful result, and separately a textless
        // error result, must each break a same-error streak the same way for
        // the incremental fold as for a full parse.
        let mut same_error_interrupted_by_success =
            vec![NormalizedEvent::TurnStart { at_ms: None }];
        same_error_interrupted_by_success.extend(erroring_tool_result("error A"));
        same_error_interrupted_by_success.push(NormalizedEvent::ToolResult { is_error: false });
        same_error_interrupted_by_success.extend(erroring_tool_result("error A"));
        same_error_interrupted_by_success.extend(erroring_tool_result("error A"));
        same_error_interrupted_by_success.extend(turns(3, "", "[zirv] ok", false, 120_000));

        let mut same_error_interrupted_by_textless_error =
            vec![NormalizedEvent::TurnStart { at_ms: None }];
        same_error_interrupted_by_textless_error.extend(erroring_tool_result("error A"));
        same_error_interrupted_by_textless_error
            .push(NormalizedEvent::ToolResult { is_error: true });
        same_error_interrupted_by_textless_error.extend(erroring_tool_result("error A"));
        same_error_interrupted_by_textless_error.extend(turns(3, "", "[zirv] ok", false, 120_000));

        vec![
            ("empty", Vec::new()),
            ("short", turns(3, "", "[zirv] ok", false, 120_000)),
            ("past the window", past_window),
            ("with a compaction", with_compaction),
            ("events before the first turn", before_first_turn),
            ("an open final turn", open_turn),
            ("no turn starts at all", vec![assistant("[zirv] hi", 9)]),
            (
                "same error interrupted by a success",
                same_error_interrupted_by_success,
            ),
            (
                "same error interrupted by a textless error",
                same_error_interrupted_by_textless_error,
            ),
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
            NormalizedEvent::TurnStart { at_ms: None },
            assistant("mid", 1),
            assistant("final one", 1),
            NormalizedEvent::TurnStart { at_ms: None },
            assistant("", 1),
            NormalizedEvent::TurnStart { at_ms: None },
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
        let events = vec![
            NormalizedEvent::TurnStart { at_ms: None },
            assistant("[zirv] hi", 120_000),
        ];
        let s = signals(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(s.tool_failure_rate, 0.0);
    }

    #[test]
    fn identical_tool_calls_are_counted_and_distinct_ones_are_not() {
        let cfg = ScoreConfig::default();
        let mut repeated = vec![NormalizedEvent::TurnStart { at_ms: None }];
        for _ in 0..4 {
            repeated.push(tool("Bash", "{\"command\":\"ls\"}"));
        }
        let s = signals(&repeated, full_caps(), &cfg);
        assert_eq!(s.max_repeat, 4);
        assert_eq!(s.repetition_hits, 1);

        let mut distinct = vec![NormalizedEvent::TurnStart { at_ms: None }];
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
            NormalizedEvent::TurnStart { at_ms: None },
            tool("Read", "{\"file_path\":\"/a\"}"),
            tool("Write", "{\"file_path\":\"/a\"}"),
            tool("Edit", "{\"file_path\":\"/a\"}"),
        ];
        let s = signals(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(s.max_repeat, 1);
    }

    /// A repeat separated by an edit-like call is exactly the healthy
    /// edit -> rerun -> edit -> rerun TDD loop, not over-verification: it
    /// must never build a streak.
    #[test]
    fn a_repeat_interrupted_by_an_edit_like_call_does_not_count() {
        let cfg = ScoreConfig::default(); // repetition_threshold: 3
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        for _ in 0..4 {
            events.push(tool("Bash", "{\"command\":\"cargo test\"}"));
            events.push(tool("Edit", "{\"file_path\":\"/a.rs\"}"));
        }
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(
            s.max_repeat, 1,
            "an edit between every pair of reruns breaks the streak each time"
        );
        assert_eq!(s.repetition_hits, 0);
    }

    /// A non-edit-like call between repeats (a Read, or a Bash with a
    /// different command) never breaks the streak: only edit-like calls do.
    #[test]
    fn a_repeat_interleaved_with_only_non_edit_calls_still_counts() {
        let cfg = ScoreConfig::default(); // repetition_threshold: 3
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        for i in 0..4 {
            events.push(tool("Bash", "{\"command\":\"cargo test\"}"));
            // Distinct each time, so only the Bash repetition is under test.
            events.push(tool("Read", &format!("{{\"file_path\":\"/a{i}.rs\"}}")));
        }
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(
            s.max_repeat, 4,
            "non-edit-like calls between reruns do not interrupt the streak"
        );
        assert_eq!(s.repetition_hits, 1);
    }

    /// Edit-like calls are never themselves tracked as a repeated call, even
    /// when the exact same edit is made twice with nothing else in between --
    /// they are the healthy action this signal exists to stop penalising.
    #[test]
    fn edit_like_calls_are_never_tracked_as_a_repetition_themselves() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        for _ in 0..5 {
            events.push(tool("Edit", "{\"file_path\":\"/a.rs\"}"));
        }
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.max_repeat, 0);
        assert_eq!(s.repetition_hits, 0);
    }

    /// Tool names normalize case-sensitively off the wire (Claude Code's
    /// block names pass through verbatim), but the classification into
    /// edit-like must not care about casing.
    #[test]
    fn edit_like_matching_is_case_insensitive() {
        assert!(is_edit_like("Edit"));
        assert!(is_edit_like("EDIT"));
        assert!(is_edit_like("write"));
        assert!(is_edit_like("MultiEdit"));
        assert!(is_edit_like("NotebookEdit"));
        assert!(is_edit_like("apply_patch"));
        assert!(!is_edit_like("Bash"));
        assert!(!is_edit_like("Read"));
    }

    /// `ToolResult { is_error: true }` immediately followed by the hashed,
    /// normalized error text -- exactly what `ClaudeAdapter::parse_events`
    /// emits for an erroring tool result it can extract text from.
    fn erroring_tool_result(error_text: &str) -> Vec<NormalizedEvent> {
        vec![
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolErrorText {
                hash: crate::commands::ctx::event::error_text_hash(error_text),
            },
        ]
    }

    /// Three different fixes (different tool inputs each time), same
    /// underlying error every time: `same_error_repeats` must fire even
    /// though `max_repeat`/`repetition_hits` -- keyed on `(tool,
    /// input_hash)` -- do not.
    #[test]
    fn identical_error_text_across_different_tool_inputs_builds_a_streak() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        for i in 0..4 {
            events.push(tool(
                "Bash",
                &format!("{{\"command\":\"cargo test mod{i}\"}}"),
            ));
            events.extend(erroring_tool_result(
                "error[E0433]: failed to resolve at src/foo.rs:42",
            ));
            events.push(tool("Edit", &format!("{{\"file_path\":\"/a{i}.rs\"}}")));
        }
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.same_error_repeats, 4, "same normalized error every time");
        assert_eq!(
            s.max_repeat, 1,
            "distinct tool inputs must not trip the ordinary repetition signal"
        );
    }

    #[test]
    fn a_different_error_resets_the_same_error_streak() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        events.extend(erroring_tool_result("error A"));
        events.extend(erroring_tool_result("error A"));
        events.extend(erroring_tool_result("error B"));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(
            s.same_error_repeats, 2,
            "the run resets at the different error"
        );
    }

    /// Review finding F1: a successful tool result between two occurrences of
    /// the SAME error must interrupt the streak -- "error A, success, error
    /// A, error A" is a run of 2, not 3, because the intervening success
    /// proves the fix landed at least once.
    #[test]
    fn a_successful_result_between_same_errors_breaks_the_streak() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        events.extend(erroring_tool_result("error A"));
        events.push(NormalizedEvent::ToolResult { is_error: false });
        events.extend(erroring_tool_result("error A"));
        events.extend(erroring_tool_result("error A"));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(
            s.same_error_repeats, 2,
            "the successful result in between breaks the streak"
        );
    }

    /// Review finding F1: an erroring result with no extractable text (a
    /// `ToolResult { is_error: true }` never followed by `ToolErrorText`)
    /// also interrupts the streak -- it is a result boundary, not simply
    /// absent from the stream the way it was before this fix.
    #[test]
    fn an_error_with_no_extractable_text_breaks_the_streak() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        events.extend(erroring_tool_result("error A"));
        events.push(NormalizedEvent::ToolResult { is_error: true });
        events.extend(erroring_tool_result("error A"));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(
            s.same_error_repeats, 1,
            "an error result with no extractable text still breaks the streak"
        );
    }

    /// Three occurrences of the same error with no intervening result at all
    /// still build a run of three -- this fix must not regress the ordinary
    /// case.
    #[test]
    fn three_same_errors_with_no_intervening_results_run_three() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        events.extend(erroring_tool_result("error A"));
        events.extend(erroring_tool_result("error A"));
        events.extend(erroring_tool_result("error A"));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.same_error_repeats, 3);
    }

    #[test]
    fn same_error_repeats_normalizes_digits_and_paths_that_differ_between_attempts() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        events.extend(erroring_tool_result(
            "error[E0433]: failed to resolve at /tmp/build123/src/foo.rs:42:10",
        ));
        events.extend(erroring_tool_result(
            "error[E0433]: failed to resolve at /tmp/build987/src/foo.rs:57:3",
        ));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(
            s.same_error_repeats, 2,
            "a randomized temp dir and differing line/col must not defeat the match"
        );
    }

    /// The shipped default weight is 0: this signal must never move an
    /// existing verdict fixture until an operator raises it deliberately.
    #[test]
    fn default_same_error_weight_is_zero_and_does_not_move_the_score() {
        let cfg = ScoreConfig::default();
        assert_eq!(cfg.same_error_weight, 0.0, "shipped default");

        let base = Signals {
            turns: 12,
            tool_failure_rate: 0.0,
            repetition_hits: 0,
            max_repeat: 1,
            same_error_repeats: 0,
            provider_overflows: 0,
            marker_miss_rate: Some(0.0),
        };
        let heavy_repeat = Signals {
            same_error_repeats: 50,
            ..base.clone()
        };

        let a = score_from(base, 120_000, &cfg, full_caps());
        let b = score_from(heavy_repeat, 120_000, &cfg, full_caps());
        assert_eq!(
            a.score, b.score,
            "same_error_repeats must not move the score until an operator sets a weight"
        );
    }

    /// The incremental fold has to agree with a full parse on the new
    /// signal too, across turn boundaries and chunk sizes -- the same
    /// contract `folding_events_in_chunks_matches_a_full_parse` already
    /// holds for every other signal.
    #[test]
    fn same_error_repeats_folds_incrementally_the_same_as_a_full_parse() {
        let cfg = ScoreConfig::default();
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        for i in 0..5 {
            events.push(tool(
                "Bash",
                &format!("{{\"command\":\"cargo test mod{i}\"}}"),
            ));
            events.extend(erroring_tool_result("error[E0433]: unresolved import"));
            events.push(NormalizedEvent::TurnStart { at_ms: None });
        }
        let expected = score_events(&events, full_caps(), &cfg);
        for chunk in [1, 3, 7] {
            let mut state = RotState::new(&cfg).expect("bounded window");
            for part in events.chunks(chunk) {
                state.feed_all(part);
            }
            assert_eq!(
                state.score(full_caps(), &cfg),
                Some(expected.clone()),
                "chunks of {chunk}"
            );
        }
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
            context_window_tokens: None,
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
        let caps = Capabilities::default();
        let tokens = 120_000;
        assert_eq!(verdict_for(0, tokens, &cfg, caps), Verdict::Healthy);
        assert_eq!(verdict_for(39, tokens, &cfg, caps), Verdict::Healthy);
        assert_eq!(verdict_for(40, tokens, &cfg, caps), Verdict::Advise);
        assert_eq!(verdict_for(59, tokens, &cfg, caps), Verdict::Advise);
        assert_eq!(verdict_for(60, tokens, &cfg, caps), Verdict::Compact);
        assert_eq!(verdict_for(79, tokens, &cfg, caps), Verdict::Compact);
        assert_eq!(verdict_for(80, tokens, &cfg, caps), Verdict::Restart);
        assert_eq!(verdict_for(100, tokens, &cfg, caps), Verdict::Restart);
    }

    #[test]
    fn below_the_token_floor_the_verdict_is_always_healthy() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities::default();
        assert_eq!(verdict_for(100, 99_999, &cfg, caps), Verdict::Healthy);
        assert_eq!(verdict_for(0, 0, &cfg, caps), Verdict::Healthy);
        assert_eq!(
            verdict_for(100, 100_000, &cfg, caps),
            Verdict::Restart,
            "floor is inclusive"
        );
    }

    #[test]
    fn at_the_ceiling_the_verdict_is_at_least_compact() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities::default();
        assert_eq!(verdict_for(0, 160_000, &cfg, caps), Verdict::Compact);
        assert_eq!(verdict_for(45, 200_000, &cfg, caps), Verdict::Compact);
    }

    #[test]
    fn at_the_ceiling_a_compact_level_score_escalates_to_restart() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities::default();
        assert_eq!(verdict_for(60, 160_000, &cfg, caps), Verdict::Restart);
        assert_eq!(verdict_for(70, 170_000, &cfg, caps), Verdict::Restart);
        assert_eq!(verdict_for(59, 170_000, &cfg, caps), Verdict::Compact);
    }

    /// Issue #155, Phase 6(b): with no capacity known, the gates are EXACTLY
    /// today's absolute defaults. This is the compatibility floor: codex
    /// reports no capacity, and its rotation behaviour must not move at all.
    #[test]
    fn an_unknown_capacity_keeps_todays_absolute_thresholds() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities::default();
        assert_eq!(
            token_gates(&cfg, caps),
            (FALLBACK_TOKEN_FLOOR, FALLBACK_TOKEN_CEILING)
        );
        assert_eq!(token_gates(&cfg, caps), (100_000, 160_000));
    }

    /// A known capacity makes the gates RATIOS of it. On a 1M seat the old
    /// absolute ceiling fired at 16% of capacity and restarted a session with
    /// 840k tokens of headroom -- discarding a warm cache to rebuild one,
    /// which is the most expensive possible response to a size signal.
    #[test]
    fn a_known_capacity_scales_the_gates_to_it() {
        let cfg = ScoreConfig::default();
        let million = Capabilities {
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };
        assert_eq!(token_gates(&cfg, million), (500_000, 800_000));

        let small = Capabilities {
            context_window_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(
            token_gates(&cfg, small),
            (100_000, 160_000),
            "the shipped ratios reproduce the old absolutes on a 200k seat"
        );
    }

    /// An explicit absolute wins outright: an operator who pins a number gets
    /// that number, capacity or not. Where ordering has to be repaired, the
    /// DERIVED side moves -- zirv never silently rewrites a number the
    /// operator typed.
    #[test]
    fn an_explicit_absolute_overrides_the_ratio() {
        let million = Capabilities {
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };

        let ceiling_only = ScoreConfig {
            token_ceiling: Some(900_000),
            ..ScoreConfig::default()
        };
        assert_eq!(token_gates(&ceiling_only, million), (500_000, 900_000));

        let floor_only = ScoreConfig {
            token_floor: Some(120_000),
            ..ScoreConfig::default()
        };
        assert_eq!(token_gates(&floor_only, million), (120_000, 800_000));

        let both = ScoreConfig {
            token_floor: Some(10),
            token_ceiling: Some(20),
            ..ScoreConfig::default()
        };
        assert_eq!(
            token_gates(&both, million),
            (10, 20),
            "a fully pinned pair is used verbatim"
        );
    }

    /// The operator's own capacity override beats the adapter's reported
    /// one: an adapter's conservative default is a guess about the seat, and
    /// the operator knows their seat.
    #[test]
    fn the_configured_capacity_overrides_the_adapters_reported_one() {
        let cfg = ScoreConfig {
            model_context_tokens: Some(1_000_000),
            ..ScoreConfig::default()
        };
        let conservative = Capabilities {
            context_window_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(token_gates(&cfg, conservative), (500_000, 800_000));
    }

    /// The gates must never invert or collapse, whatever ratios are
    /// configured: a ceiling at or below the floor would make `verdict_for`'s
    /// two-stage gate meaningless.
    #[test]
    fn the_gates_are_always_ordered_and_nonzero() {
        let inverted = ScoreConfig {
            token_floor_ratio: 0.9,
            token_ceiling_ratio: 0.1,
            ..ScoreConfig::default()
        };
        let caps = Capabilities {
            context_window_tokens: Some(200_000),
            ..Default::default()
        };
        let (floor, ceiling) = token_gates(&inverted, caps);
        assert!(floor < ceiling, "got ({floor}, {ceiling})");

        let zeroed = ScoreConfig {
            token_floor_ratio: 0.0,
            token_ceiling_ratio: 0.0,
            ..ScoreConfig::default()
        };
        let (floor, ceiling) = token_gates(&zeroed, caps);
        assert!(floor > 0 && ceiling > floor, "got ({floor}, {ceiling})");
    }

    /// And the gate still behaves the same way around those thresholds --
    /// this is a change of WHERE the gate sits, never of what it does.
    #[test]
    fn the_verdict_gate_behaves_identically_at_the_scaled_thresholds() {
        let cfg = ScoreConfig::default();
        let million = Capabilities {
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };
        assert_eq!(verdict_for(100, 499_999, &cfg, million), Verdict::Healthy);
        assert_eq!(verdict_for(100, 500_000, &cfg, million), Verdict::Restart);
        assert_eq!(verdict_for(0, 800_000, &cfg, million), Verdict::Compact);
        assert_eq!(verdict_for(60, 800_000, &cfg, million), Verdict::Restart);
        assert_eq!(verdict_for(59, 850_000, &cfg, million), Verdict::Compact);
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
            context_window_tokens: None,
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
            context_window_tokens: None,
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
    fn provider_overflow_escalates_even_below_the_token_floor() {
        let cfg = ScoreConfig::default();
        let overflow = NormalizedEvent::ProviderError {
            class: ProviderErrorClass::Overflow,
        };

        let once = score_events(std::slice::from_ref(&overflow), full_caps(), &cfg);
        assert_eq!(once.score, 0);
        assert_eq!(once.context_tokens, 0);
        assert_eq!(once.signals.provider_overflows, 1);
        assert_eq!(once.verdict, Verdict::Compact);

        let twice = score_events(&[overflow.clone(), overflow], full_caps(), &cfg);
        assert_eq!(twice.signals.provider_overflows, 2);
        assert_eq!(twice.verdict, Verdict::Restart);
    }

    #[test]
    fn non_overflow_provider_events_and_model_ids_do_not_change_the_verdict() {
        let cfg = ScoreConfig::default();
        let baseline = score_events(&[], full_caps(), &cfg);
        let events = [
            NormalizedEvent::ProviderError {
                class: ProviderErrorClass::RateLimit,
            },
            NormalizedEvent::ProviderError {
                class: ProviderErrorClass::Other,
            },
            NormalizedEvent::ModelId {
                id: "claude-sonnet-5".to_string(),
            },
        ];

        assert_eq!(score_events(&events, full_caps(), &cfg), baseline);
    }

    #[test]
    fn compaction_clears_behavioral_state_but_preserves_lifetime_turns_and_tokens() {
        let cfg = ScoreConfig::default();
        let mut events = looping_turns(3, "", "[zirv] ok", true, 120_000);
        events.extend([
            NormalizedEvent::ProviderError {
                class: ProviderErrorClass::Overflow,
            },
            NormalizedEvent::ProviderError {
                class: ProviderErrorClass::Overflow,
            },
        ]);
        let before = score_events(&events, full_caps(), &cfg);
        assert_eq!(before.signals.turns, 3);
        assert_eq!(before.signals.max_repeat, 3);
        assert_eq!(before.signals.tool_failure_rate, 1.0);
        assert_eq!(before.signals.provider_overflows, 2);

        events.push(NormalizedEvent::Compaction);
        let after = score_events(&events, full_caps(), &cfg);
        assert_eq!(after.signals.turns, 3, "turn count is lifetime state");
        assert_eq!(after.signals.max_repeat, 0);
        assert_eq!(after.signals.tool_failure_rate, 0.0);
        assert_eq!(after.signals.marker_miss_rate, None);
        assert_eq!(after.signals.provider_overflows, 0);
        assert_eq!(after.context_tokens, 120_000, "tokens self-correct later");

        let mut state = RotState::new(&cfg).expect("bounded state");
        state.feed_all(&events);
        assert_eq!(state.score(full_caps(), &cfg), Some(after));
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

    /// Issue #293's own acceptance criterion: adding wall-clock timestamps to
    /// an event stream must never move an existing verdict. Built from the
    /// same fixtures `equivalence_fixtures` already exercises for the
    /// incremental fold, stamped with a strictly increasing `at_ms` on every
    /// `TurnStart`/`AssistantFinal`/`ToolCall`, plus a genuine
    /// `AssistantFirstText`/`ToolResultTimestamp` sprinkled in (the two new
    /// variants `rot.rs` treats as pure no-ops) -- `rot.rs` must score the
    /// stamped and unstamped streams identically -- `Score` itself carries
    /// no speed field at all (that lives in `score.rs`'s own
    /// `derive_speed_metrics`, deliberately outside `Score` so the
    /// incremental-fold-equals-full-parse contract every other `Score`
    /// field already upholds is never put at risk by a signal that is
    /// legitimately allowed to differ between a bounded poll and a full
    /// parse).
    #[test]
    fn timestamps_never_move_a_verdict() {
        fn stamp(events: &[NormalizedEvent]) -> Vec<NormalizedEvent> {
            let mut clock = 1_700_000_000_000u64;
            let mut out = Vec::with_capacity(events.len() * 2);
            for event in events {
                clock += 1_000;
                match event.clone() {
                    NormalizedEvent::TurnStart { .. } => {
                        out.push(NormalizedEvent::TurnStart { at_ms: Some(clock) });
                    }
                    NormalizedEvent::AssistantFinal {
                        text, input_tokens, ..
                    } => {
                        if !text.trim().is_empty() {
                            out.push(NormalizedEvent::AssistantFirstText { at_ms: Some(clock) });
                        }
                        out.push(NormalizedEvent::AssistantFinal {
                            text,
                            input_tokens,
                            at_ms: Some(clock),
                        });
                    }
                    NormalizedEvent::ToolCall {
                        name, input_hash, ..
                    } => {
                        out.push(NormalizedEvent::ToolCall {
                            name,
                            input_hash,
                            at_ms: Some(clock),
                        });
                    }
                    NormalizedEvent::ToolResult { is_error } => {
                        out.push(NormalizedEvent::ToolResult { is_error });
                        out.push(NormalizedEvent::ToolResultTimestamp { at_ms: Some(clock) });
                    }
                    other => out.push(other),
                }
            }
            out
        }

        let cfg = ScoreConfig::default();
        for (name, events) in equivalence_fixtures() {
            let unstamped = score_events(&events, full_caps(), &cfg);
            let stamped = score_events(&stamp(&events), full_caps(), &cfg);
            assert_eq!(
                stamped.verdict, unstamped.verdict,
                "{name}: timestamps must never move the verdict"
            );
            assert_eq!(
                stamped.score, unstamped.score,
                "{name}: timestamps must never move the score"
            );
            assert_eq!(
                stamped.signals, unstamped.signals,
                "{name}: timestamps must never move a signal"
            );
        }
    }

    /// `rot.rs`'s production code must never read a clock: time only ever
    /// arrives as data already carried on an event (issue #293, mirroring
    /// `rot_stays_pure_of_pace_and_window_data` above).
    #[test]
    fn rot_stays_pure_of_a_clock() {
        const THIS_FILE: &str = include_str!("rot.rs");
        let production_code = THIS_FILE
            .split("#[cfg(test)]")
            .next()
            .expect("this file always has a #[cfg(test)] module");
        for needle in ["SystemTime", "Instant::now", "std::time::Instant"] {
            assert!(
                !production_code.contains(needle),
                "rot.rs's production code must never call a clock -- found `{needle}`"
            );
        }
    }
}
