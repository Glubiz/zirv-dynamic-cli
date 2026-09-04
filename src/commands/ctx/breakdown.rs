//! Window attribution (issue #312): where a session's context window
//! actually went, by bucket -- `system_and_layers` (the compiled prompt
//! prefix), `tool_schemas` (the harness roster `compile.rs` already
//! measures, the closest available proxy for the real per-tool API schema
//! bytes zirv cannot see), `tool_results_live`/`tool_results_stale` (tool
//! output, split on whether a later edit invalidated the path it described),
//! `assistant_text`, `user_text` and `thinking`.
//!
//! **Pure**, like `rot.rs`: [`BreakdownAccumulator::feed`] and
//! [`attribute_window`] read only `&NormalizedEvent`/plain numbers, never fs,
//! clock, env or net. The numbers `score.rs` feeds in
//! (`system_and_layers_bytes`, `tool_schema_bytes`, `total_tokens`) are
//! themselves already-computed I/O results, not fetched here.
//!
//! **Byte-identical tool results dedupe.** [`BreakdownAccumulator`] hashes
//! every tool result's full raw content (`NormalizedEvent::ToolResultSize`);
//! a later result with the SAME hash contributes zero additional bytes,
//! since it is not new window occupancy, it is the same content appearing
//! again.
//!
//! **Staleness is path-keyed.** A tool result correlated (via the adapter's
//! `NormalizedEvent::ToolCallPath` sibling) with a file path stays in
//! `tool_results_live` until a LATER call marks that same path modified, at
//! which point every not-yet-stale byte recorded for it moves to
//! `tool_results_stale`. A result with no known path (most `Bash` output,
//! for instance) can never go stale by this rule -- it is not a false
//! positive to leave it live, since zirv genuinely does not know what
//! invalidates it.
//!
//! **Tokens, not bytes, in the public summary.** [`BreakdownSummary`]'s
//! fields are already-apportioned tokens: the real total
//! (`total_tokens`, the ground truth from parsed usage) split across
//! buckets in proportion to their byte weight via [`apportion`]'s
//! largest-remainder allocation, which is exact, not approximate -- the
//! returned buckets always sum to `total_tokens` precisely (never merely
//! "close"), the one place this module fabricates a number is the harness
//! roster proxy for `tool_schemas`, and only when the caller actually
//! supplies one.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::event::NormalizedEvent;

/// One bucket's worth of a session's window, in tokens. Every field but
/// `tool_schemas` is always present; `tool_schemas` is `None` exactly when
/// the caller had no harness-roster byte figure to estimate it from --
/// never a fabricated zero standing in for "unknown".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BreakdownSummary {
    pub system_and_layers: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_schemas: Option<u64>,
    pub tool_results_live: u64,
    pub tool_results_stale: u64,
    pub assistant_text: u64,
    pub user_text: u64,
    pub thinking: u64,
    /// The real total this summary was scaled to match -- every other field
    /// sums to exactly this value.
    pub total_tokens: u64,
    /// The tool name that contributed the most bytes now sitting in
    /// `tool_results_stale`, when any byte is stale at all. Deterministic on
    /// a tie: the alphabetically-first name among the tied tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_source: Option<String>,
}

/// One `NormalizedEvent::ToolCall` awaiting the `NormalizedEvent::
/// ToolResultSize` that will consume it, carrying whatever `NormalizedEvent::
/// ToolCallPath` sibling (if any) named its file argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PendingCall {
    name: String,
    path: Option<String>,
}

/// The running, UNBOUNDED fold behind [`attribute_window`]: unlike
/// `rot::RotState`'s windowed `segments`, this never prunes anything,
/// because a result's staleness can be decided by an edit arbitrarily many
/// turns later. Cheap in practice: its memory is proportional to the
/// session's distinct tool-result contents and distinct touched paths, not
/// to the number of turns.
///
/// `Default` is the empty accumulator a fresh session (or a fresh
/// `attribute_window` one-shot pass) starts from. `Serialize`/`Deserialize`
/// so a caller with an append-only transcript (issue #312's own
/// reclaim-gated advisory) can persist this across process restarts and
/// fold in only the newly appended events each time, the same incremental
/// idiom `hook.rs`'s `AdoptionRecord`/`CorrectionCheckpoint` already use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BreakdownAccumulator {
    assistant_bytes: u64,
    user_bytes: u64,
    thinking_bytes: u64,
    tool_live_bytes: u64,
    tool_stale_bytes: u64,
    /// Every `ToolResultSize::content_hash` seen so far -- a repeat means
    /// "already counted", never new occupancy.
    seen_hashes: BTreeSet<u64>,
    /// Bytes still live for a given path, summed across every not-yet-stale
    /// result recorded for it. Removed (and folded into `tool_stale_bytes`)
    /// the moment a modifying call names that same path.
    live_bytes_by_path: BTreeMap<String, u64>,
    /// Which tool most recently contributed to a path's live bytes, so a
    /// later staling event can attribute the move to a tool name.
    live_tool_by_path: BTreeMap<String, String>,
    stale_bytes_by_tool: BTreeMap<String, u64>,
    pending_calls: VecDeque<PendingCall>,
}

impl BreakdownAccumulator {
    /// Folds one event. `NormalizedEvent::ToolCallPath` is always emitted
    /// directly after the `NormalizedEvent::ToolCall` it describes (see that
    /// variant's own doc comment), so updating `pending_calls`'s last entry
    /// is always updating the entry it belongs to.
    pub fn feed(&mut self, event: &NormalizedEvent) {
        match event {
            NormalizedEvent::AssistantFinal { text, .. } => {
                self.assistant_bytes = self.assistant_bytes.saturating_add(text.len() as u64);
            }
            NormalizedEvent::UserText { byte_len } => {
                self.user_bytes = self.user_bytes.saturating_add(*byte_len);
            }
            NormalizedEvent::AssistantThinking { byte_len } => {
                self.thinking_bytes = self.thinking_bytes.saturating_add(*byte_len);
            }
            NormalizedEvent::ToolCall { name, .. } => {
                self.pending_calls.push_back(PendingCall {
                    name: name.clone(),
                    path: None,
                });
            }
            NormalizedEvent::ToolCallPath {
                path,
                is_modification,
            } => {
                if let Some(pending) = self.pending_calls.back_mut() {
                    pending.path = Some(path.clone());
                }
                if *is_modification && let Some(bytes) = self.live_bytes_by_path.remove(path) {
                    self.tool_live_bytes = self.tool_live_bytes.saturating_sub(bytes);
                    self.tool_stale_bytes = self.tool_stale_bytes.saturating_add(bytes);
                    let tool = self
                        .live_tool_by_path
                        .remove(path)
                        .unwrap_or_else(|| "tool".to_string());
                    *self.stale_bytes_by_tool.entry(tool).or_insert(0) += bytes;
                }
            }
            NormalizedEvent::ToolResultSize {
                byte_len,
                content_hash,
            } => {
                let pending = self.pending_calls.pop_front();
                if self.seen_hashes.insert(*content_hash) {
                    self.tool_live_bytes = self.tool_live_bytes.saturating_add(*byte_len);
                    if let Some(PendingCall {
                        name,
                        path: Some(path),
                    }) = pending
                    {
                        *self.live_bytes_by_path.entry(path.clone()).or_insert(0) += byte_len;
                        self.live_tool_by_path.insert(path, name);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn feed_all(&mut self, events: &[NormalizedEvent]) {
        for event in events {
            self.feed(event);
        }
    }

    fn dominant_stale_tool(&self) -> Option<String> {
        let mut best: Option<(&str, u64)> = None;
        for (name, bytes) in &self.stale_bytes_by_tool {
            if *bytes == 0 {
                continue;
            }
            // `BTreeMap` iterates in ascending key order; a strict `>` here
            // keeps the alphabetically-first name on a tie, deterministically.
            if best.is_none_or(|(_, best_bytes)| *bytes > best_bytes) {
                best = Some((name.as_str(), *bytes));
            }
        }
        best.map(|(name, _)| name.to_string())
    }

    /// Turns the running byte totals into a [`BreakdownSummary`] whose
    /// fields are tokens summing to exactly `total_tokens` (see
    /// [`apportion`]). `system_and_layers_bytes`/`tool_schema_bytes` are
    /// supplied by the caller (`score.rs`'s own compile-context read),
    /// never computed here.
    pub fn materialize(
        &self,
        total_tokens: u64,
        system_and_layers_bytes: u64,
        tool_schema_bytes: Option<u64>,
    ) -> BreakdownSummary {
        let weights = [
            system_and_layers_bytes,
            tool_schema_bytes.unwrap_or(0),
            self.tool_live_bytes,
            self.tool_stale_bytes,
            self.assistant_bytes,
            self.user_bytes,
            self.thinking_bytes,
        ];
        let shares = apportion(total_tokens, &weights);
        BreakdownSummary {
            system_and_layers: shares[0],
            tool_schemas: tool_schema_bytes.map(|_| shares[1]),
            tool_results_live: shares[2],
            tool_results_stale: shares[3],
            assistant_text: shares[4],
            user_text: shares[5],
            thinking: shares[6],
            total_tokens,
            stale_source: self.dominant_stale_tool(),
        }
    }
}

/// Largest-remainder (Hamilton) apportionment of `total` across `weights`,
/// proportional to each weight's share of the sum. Guarantees the returned
/// values sum to EXACTLY `total` when `weights` is not all-zero -- never
/// merely "close" -- by rounding every share down, then handing the leftover
/// units one each to the buckets with the largest fractional remainder,
/// breaking a tie by ascending index for determinism.
///
/// `total == 0` returns all zeros. An all-zero `weights` with `total > 0`
/// (nothing at all was attributable, yet the real usage total is non-zero)
/// puts the WHOLE total on `weights[0]` -- by this module's own convention,
/// always `system_and_layers_bytes` -- rather than fabricating a
/// distribution across buckets with no evidence behind them: the compiled
/// prefix is the most plausible place unaccounted-for context came from.
fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    if total == 0 {
        return vec![0; weights.len()];
    }
    let weight_sum: u64 = weights.iter().sum();
    if weight_sum == 0 {
        let mut out = vec![0u64; weights.len()];
        if let Some(first) = out.first_mut() {
            *first = total;
        }
        return out;
    }
    let total_f = total as f64;
    let weight_sum_f = weight_sum as f64;
    let mut floors = vec![0u64; weights.len()];
    let mut remainders: Vec<(usize, f64)> = Vec::with_capacity(weights.len());
    for (i, &w) in weights.iter().enumerate() {
        let exact = total_f * (w as f64) / weight_sum_f;
        let floor = exact.floor();
        floors[i] = floor as u64;
        remainders.push((i, exact - floor));
    }
    let floor_sum: u64 = floors.iter().sum();
    let mut remainder = total.saturating_sub(floor_sum);
    remainders.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    for (i, _) in remainders {
        if remainder == 0 {
            break;
        }
        floors[i] += 1;
        remainder -= 1;
    }
    floors
}

/// One-shot pure attribution pass over a full event slice: builds a fresh
/// [`BreakdownAccumulator`], folds every event into it, and materializes the
/// result. `score.rs`'s incremental reclaim-gated advisory instead keeps a
/// persisted `BreakdownAccumulator` alive across calls and folds only newly
/// appended events into it -- see that struct's own doc comment -- but both
/// paths call the exact same [`BreakdownAccumulator::feed`]/[`BreakdownAccumulator::materialize`],
/// so a one-shot pass and an incrementally-folded one over the SAME events
/// always agree.
pub fn attribute_window(
    events: &[NormalizedEvent],
    total_tokens: u64,
    system_and_layers_bytes: u64,
    tool_schema_bytes: Option<u64>,
) -> BreakdownSummary {
    let mut acc = BreakdownAccumulator::default();
    acc.feed_all(events);
    acc.materialize(total_tokens, system_and_layers_bytes, tool_schema_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_text(text: &str) -> NormalizedEvent {
        NormalizedEvent::AssistantFinal {
            text: text.to_string(),
            input_tokens: 0,
            at_ms: None,
        }
    }

    fn tool_call(name: &str) -> NormalizedEvent {
        NormalizedEvent::ToolCall {
            name: name.to_string(),
            input_hash: 0,
            at_ms: None,
        }
    }

    fn tool_result(byte_len: u64, content_hash: u64) -> NormalizedEvent {
        NormalizedEvent::ToolResultSize {
            byte_len,
            content_hash,
        }
    }

    fn tool_call_path(path: &str, is_modification: bool) -> NormalizedEvent {
        NormalizedEvent::ToolCallPath {
            path: path.to_string(),
            is_modification,
        }
    }

    /// Every field must sum to exactly `total_tokens` for any summary this
    /// module produces -- the one property every other test in this module
    /// leans on implicitly, checked explicitly here across several shapes.
    fn assert_sums_to_total(summary: &BreakdownSummary) {
        let sum = summary.system_and_layers
            + summary.tool_schemas.unwrap_or(0)
            + summary.tool_results_live
            + summary.tool_results_stale
            + summary.assistant_text
            + summary.user_text
            + summary.thinking;
        assert_eq!(
            sum, summary.total_tokens,
            "buckets must sum to the real total: {summary:?}"
        );
    }

    #[test]
    fn hand_computed_buckets_with_no_dedup_or_staleness() {
        let events = vec![
            assistant_text("0123456789"), // 10 bytes
            NormalizedEvent::UserText { byte_len: 5 },
            NormalizedEvent::AssistantThinking { byte_len: 15 },
            tool_call("Bash"),
            tool_result(20, 111),
        ];
        // total bytes: 30 (system) + 20 (schema) + 20 (tool live) + 10
        // (assistant) + 5 (user) + 15 (thinking) = 100, so a total of 100
        // tokens apportions 1:1 with no rounding ambiguity.
        let summary = attribute_window(&events, 100, 30, Some(20));
        assert_eq!(summary.system_and_layers, 30);
        assert_eq!(summary.tool_schemas, Some(20));
        assert_eq!(summary.tool_results_live, 20);
        assert_eq!(summary.tool_results_stale, 0);
        assert_eq!(summary.assistant_text, 10);
        assert_eq!(summary.user_text, 5);
        assert_eq!(summary.thinking, 15);
        assert_eq!(summary.total_tokens, 100);
        assert_sums_to_total(&summary);
    }

    #[test]
    fn byte_identical_tool_results_dedupe() {
        let events = vec![
            tool_call("Read"),
            tool_result(50, 42),
            tool_call("Read"),
            tool_result(50, 42),
        ];
        // 50 (system) + 50 (tool live, counted ONCE) = 100.
        let summary = attribute_window(&events, 100, 50, None);
        assert_eq!(
            summary.tool_results_live, 50,
            "a byte-identical repeat must not double-count"
        );
        assert_eq!(summary.tool_schemas, None);
        assert_sums_to_total(&summary);
    }

    #[test]
    fn a_later_edit_moves_the_earlier_read_of_the_same_path_to_stale() {
        let events = vec![
            tool_call("Read"),
            tool_call_path("/a.rs", false),
            tool_result(40, 1),
            tool_call("Edit"),
            tool_call_path("/a.rs", true),
            tool_result(5, 2),
        ];
        // 5 (system) + 5 (Edit's own live result) + 40 (staled Read) = 50.
        let summary = attribute_window(&events, 50, 5, None);
        assert_eq!(summary.tool_results_live, 5, "only the Edit's own result");
        assert_eq!(summary.tool_results_stale, 40, "the invalidated Read");
        assert_eq!(summary.stale_source.as_deref(), Some("Read"));
        assert_sums_to_total(&summary);
    }

    #[test]
    fn a_read_with_no_known_path_never_goes_stale() {
        let events = vec![
            tool_call("Bash"),
            tool_result(30, 7),
            tool_call("Edit"),
            tool_call_path("/unrelated.rs", true),
            tool_result(2, 8),
        ];
        let summary = attribute_window(&events, 40, 8, None);
        assert_eq!(summary.tool_results_stale, 0);
        assert_eq!(summary.tool_results_live, 32);
        assert_eq!(summary.stale_source, None);
        assert_sums_to_total(&summary);
    }

    #[test]
    fn tool_schemas_is_absent_rather_than_a_fabricated_zero() {
        let summary = attribute_window(&[], 10, 10, None);
        assert_eq!(summary.tool_schemas, None);
        assert_eq!(summary.system_and_layers, 10);
        assert_sums_to_total(&summary);
    }

    #[test]
    fn buckets_sum_exactly_even_when_weights_do_not_divide_evenly() {
        let events = vec![
            assistant_text("a"),
            NormalizedEvent::UserText { byte_len: 1 },
            NormalizedEvent::AssistantThinking { byte_len: 1 },
            tool_call("Bash"),
            tool_result(1, 1),
        ];
        // Seven equal-weight buckets (system=1, schema=1, live=1, stale=0,
        // assistant=1, user=1, thinking=1) sharing a total that does not
        // divide evenly by their count.
        let summary = attribute_window(&events, 10, 1, Some(1));
        assert_sums_to_total(&summary);
        assert_eq!(summary.total_tokens, 10);
    }

    #[test]
    fn nothing_attributable_puts_the_whole_total_on_system_and_layers() {
        let summary = attribute_window(&[], 50, 0, None);
        assert_eq!(summary.system_and_layers, 50);
        assert_sums_to_total(&summary);
    }

    #[test]
    fn zero_total_tokens_is_all_zero_buckets() {
        let events = vec![assistant_text("hello")];
        let summary = attribute_window(&events, 0, 10, Some(5));
        assert_eq!(summary.total_tokens, 0);
        assert_eq!(summary.system_and_layers, 0);
        assert_eq!(summary.tool_schemas, Some(0));
        assert_sums_to_total(&summary);
    }

    #[test]
    fn dominant_stale_tool_breaks_a_tie_alphabetically() {
        let mut acc = BreakdownAccumulator::default();
        acc.feed_all(&[
            tool_call("Zebra"),
            tool_call_path("/z.rs", false),
            tool_result(10, 1),
            tool_call("Alpha"),
            tool_call_path("/a.rs", false),
            tool_result(10, 2),
            tool_call("Edit"),
            tool_call_path("/z.rs", true),
            tool_result(1, 3),
            tool_call("Edit"),
            tool_call_path("/a.rs", true),
            tool_result(1, 4),
        ]);
        // Both "Zebra" and "Alpha" contributed exactly 10 stale bytes --
        // "Alpha" must win the tie.
        assert_eq!(acc.dominant_stale_tool().as_deref(), Some("Alpha"));
    }

    #[test]
    fn accumulator_incremental_fold_matches_a_one_shot_pass() {
        let events = vec![
            assistant_text("hello there"),
            NormalizedEvent::UserText { byte_len: 4 },
            tool_call("Read"),
            tool_call_path("/a.rs", false),
            tool_result(12, 1),
            tool_call("Edit"),
            tool_call_path("/a.rs", true),
            tool_result(3, 2),
        ];
        let one_shot = attribute_window(&events, 100, 20, Some(10));

        let mut incremental = BreakdownAccumulator::default();
        for chunk in events.chunks(2) {
            incremental.feed_all(chunk);
        }
        let folded = incremental.materialize(100, 20, Some(10));

        assert_eq!(
            one_shot, folded,
            "chunked incremental folding must match a full pass"
        );
    }
}
