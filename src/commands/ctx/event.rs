use std::path::PathBuf;

/// FNV-1a 64. Hand-rolled rather than `DefaultHasher` because the rot engine
/// must be deterministic across compiler versions.
pub fn input_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct SessionRef {
    pub id: SessionId,
    pub cwd: PathBuf,
}

/// Cumulative token usage read from a harness transcript, in the four RAW
/// classes the provider bills separately. Adapters return `None` when their
/// transcript does not expose a verified usage shape, and `0` for a class
/// their transcript genuinely does not report (codex's cumulative
/// `TokenCount` totals carry no cache classes) -- never a guess.
///
/// Recorded raw, not pre-summed (issue #155, 2026-08-26): cache CREATION is
/// expensive and written once, cache READ is cheap and dominant in a healthy
/// session, and folding them together at the adapter boundary made the
/// cache-hit ratio -- the one number that says whether prompt-shape work
/// helped -- uncomputable anywhere downstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl TranscriptUsage {
    /// The combined "real context size" figure this type carried in
    /// `input_tokens` before 2.34.0: uncached input plus both cache classes.
    /// Every caller that genuinely wants ONE context-size number -- rot's
    /// token gate, status display -- calls this. Saturating, like every other
    /// token arithmetic in this crate.
    pub fn context_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

/// The only currency the rot engine and supervisors understand.
///
/// `AssistantFinal` is emitted for every assistant message: `text` holds the
/// concatenated text blocks and is empty for tool-only or thinking-only
/// messages. The marker signal groups by turn and takes the last non-empty
/// text; the token gate takes the most recent event's `input_tokens`
/// regardless of text, so mid-turn token growth is visible.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedEvent {
    TurnStart,
    AssistantFinal { text: String, input_tokens: u64 },
    ToolCall { name: String, input_hash: u64 },
    ToolResult { is_error: bool },
    Compaction,
}

/// Which rot signals an adapter can actually feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub marker_signal: bool,
    pub token_usage: bool,
    pub turn_signal: bool,
    /// Whether this agent has a verified per-run system-prompt mechanism.
    pub system_prompt: bool,
    /// Whether `AgentAdapter::parse_events`/`structural_context` can ever
    /// produce real data for this agent, as opposed to the empty/default
    /// stub every "no verified mechanism" adapter ships (codex today, see
    /// issue #11). `false` here is not "unhealthy" -- `rot.rs` stays pure
    /// and never reads this field at all, since a session this adapter
    /// cannot parse is not zero signals of health, it is *no data*. The
    /// scoring callers that build a `Score`/verdict from real events
    /// (`score.rs`'s `full_score`/`IncrementalScorer::poll`) are what read
    /// it, to report "no data" instead of a false `Healthy`/`0` built from
    /// an always-empty parse -- the same distinction `score::cached_score`'s
    /// own doc comment already draws for a missing transcript.
    pub events: bool,
    /// Whether this agent's own composer folds a same-write trailing `\r`
    /// into pasted text, so an injection into it must write the text and its
    /// submitting `\r` as two genuinely separate writes rather than one
    /// burst (issue #118). Verified `true` for codex against its ratatui
    /// composer (issue #114, fixed for the dashboard in PR #116); `false`
    /// for claude, whose composer submits a same-burst trailing `\r`
    /// correctly. See `wrap::write_mail_advisory` and
    /// `dash::pane::inject_visible` for the two callers this actually
    /// changes behavior for.
    pub defer_injection_submit: bool,
    /// The model's usable context window, when the adapter can state one
    /// (issue #155). `None` means "unknown", which `rot::token_gates` reads
    /// as "use the absolute `score.token_floor`/`token_ceiling` defaults" --
    /// never as a guess. Delivered inside `Capabilities` deliberately: this
    /// struct is already an input to `rot::score_events` and
    /// `RotState::score`, so capacity reaches the rot engine without adding
    /// a single fs, clock or env read to a module that must stay pure.
    pub context_window_tokens: Option<u64>,
}

/// Raw material for handoffs, extracted per-agent because it needs fields the
/// normalized stream deliberately drops (message text, tool inputs).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StructuralContext {
    pub user_messages: Vec<String>,
    pub assistant_texts: Vec<String>,
    pub files_touched: Vec<String>,
    pub tool_errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hash_is_fnv1a_64_and_stable() {
        assert_eq!(input_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(input_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(input_hash("foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(
            input_hash("{\"command\":\"ls\"}"),
            input_hash("{\"command\":\"ls\"}")
        );
        assert_ne!(
            input_hash("{\"command\":\"ls\"}"),
            input_hash("{\"command\":\"ls -l\"}")
        );
    }

    #[test]
    fn session_ids_are_unique_uuids() {
        let a = SessionId::new_v4();
        let b = SessionId::new_v4();
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str().len(), 36, "canonical hyphenated uuid");
        assert_eq!(a.to_string(), a.as_str());
    }

    #[test]
    fn capabilities_default_to_nothing_available() {
        let caps = Capabilities::default();
        assert!(!caps.marker_signal);
        assert!(!caps.token_usage);
        assert!(!caps.turn_signal);
    }

    /// Issue #155, Phase 6(a): capacity is a CAPABILITY, delivered inside the
    /// struct `rot.rs` already receives. That is what lets rotation
    /// thresholds become ratios of a real window without adding any fs,
    /// clock or env access to a module that must stay pure.
    #[test]
    fn capabilities_default_to_an_unknown_context_window() {
        assert_eq!(Capabilities::default().context_window_tokens, None);
    }

    #[test]
    fn events_compare_by_value() {
        assert_eq!(
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolResult { is_error: true }
        );
        assert_ne!(
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolResult { is_error: false }
        );
    }

    /// Issue #155, Phase 2: the four categories are recorded RAW. Before
    /// this, claude's adapter summed input + cache_creation + cache_read into
    /// `input_tokens` at the adapter boundary, which made a cache-hit ratio
    /// -- the one number that says whether prompt-shape work helped --
    /// uncomputable anywhere downstream.
    #[test]
    fn transcript_usage_keeps_the_cache_classes_apart_and_can_still_combine_them() {
        let usage = TranscriptUsage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 8_000,
            cache_read_input_tokens: 91_000,
            output_tokens: 500,
        };
        assert_eq!(
            usage.context_total(),
            100_000,
            "the pre-2.34.0 combined number"
        );
        assert_eq!(TranscriptUsage::default().context_total(), 0);
    }
}
