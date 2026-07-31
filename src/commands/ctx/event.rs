// Consumed by verb entry points and adapters added in later tasks of this
// plan; nothing calls this yet, so dead_code is silenced module-wide until
// then.
#![allow(dead_code)]

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
}
