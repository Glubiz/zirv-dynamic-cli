use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::super::CtxResult;
use super::super::event::input_hash;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, TurnSignalSetup};

fn text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Real context size is `input_tokens` plus both cache fields; the bare
/// `input_tokens` field is near zero once prompt caching kicks in.
pub fn context_tokens_of(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
    .sum()
}

pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent> {
    let mut events = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let results: Vec<&Value> = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| {
                                b.get("type").and_then(Value::as_str) == Some("tool_result")
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                if results.is_empty() {
                    events.push(NormalizedEvent::TurnStart);
                    continue;
                }
                for block in results {
                    events.push(NormalizedEvent::ToolResult {
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
            }
            Some("assistant") => {
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let input_tokens = message.get("usage").map(context_tokens_of).unwrap_or(0);
                events.push(NormalizedEvent::AssistantFinal {
                    text: text_of(&message),
                    input_tokens,
                });

                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                        events.push(NormalizedEvent::ToolCall {
                            name,
                            input_hash: input_hash(&raw),
                        });
                    }
                }
            }
            Some("system")
                if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                events.push(NormalizedEvent::Compaction);
            }
            _ => {}
        }
    }

    events
}

// Scaffold only: Task A8 replaces this struct with the real one, which splits a
// multi-word bin into `program` plus `bin_args`.
#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    bin: String,
}

impl ClaudeAdapter {
    pub fn new(bin: Option<&str>) -> Self {
        Self {
            bin: bin.unwrap_or("claude").to_string(),
        }
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn ready(&self) -> CtxResult<()> {
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "claude")
            .unwrap_or(false)
    }

    fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
        Command::new(&self.bin)
    }

    fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
        Command::new(&self.bin)
    }

    fn distiller_cmd(&self, _model: &str) -> Command {
        Command::new(&self.bin)
    }

    fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
        PathBuf::new()
    }

    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        parse_events(jsonl)
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    fn quit_sequence(&self) -> &'static str {
        "/exit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
        }
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::{NormalizedEvent, input_hash};

    pub(crate) fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn recorded_fixture_carries_no_personal_data() {
        let text = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture must be committed");
        for needle in ["jonathansolskov", "/Users/", "sk-ant", "ghp_", "Bearer "] {
            assert!(
                !text.contains(needle),
                "fixture leaks '{needle}'; re-run scripts/record-claude-fixture.py"
            );
        }
        assert!(
            text.contains("compact_boundary"),
            "fixture must include a compaction"
        );
        assert!(
            text.lines().count() >= 50,
            "fixture is too small to be representative"
        );
    }

    #[test]
    fn context_tokens_sum_the_cache_fields() {
        // Verified against a real transcript: input_tokens alone is 2 in a
        // 110k-token session, so the cache fields carry the real size.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(context_tokens_of(&usage), 108_886);
    }

    #[test]
    fn a_real_prompt_starts_a_turn_but_a_tool_result_does_not() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n"
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::TurnStart,
                NormalizedEvent::ToolResult { is_error: false },
            ]
        );
    }

    #[test]
    fn missing_is_error_counts_as_success() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![NormalizedEvent::ToolResult { is_error: false }]
        );
    }

    #[test]
    fn assistant_yields_text_tokens_and_tool_calls() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":["#,
            r#"{"type":"thinking","thinking":"hmm"},"#,
            r#"{"type":"text","text":"[zirv] on it"},"#,
            r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#,
            r#"],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] on it".to_string(),
                    input_tokens: 100,
                },
                NormalizedEvent::ToolCall {
                    name: "Bash".to_string(),
                    input_hash: input_hash("{\"command\":\"ls\"}"),
                },
            ]
        );
    }

    #[test]
    fn tool_only_assistant_messages_still_report_tokens() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":5}}}"#;
        let events = parse_events(jsonl);
        assert_eq!(
            events[0],
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 5
            }
        );
    }

    #[test]
    fn compact_boundary_becomes_a_compaction_event() {
        let jsonl =
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#;
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::Compaction]);
    }

    #[test]
    fn sidechain_meta_and_garbage_lines_are_skipped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"content":"hook noise"}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"type":"pr-link","prNumber":7}"#,
            "\n",
            r#"{"type":"user","message":{"content":"real prompt"}}"#,
            "\n"
        );
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::TurnStart]);
    }

    #[test]
    fn real_fixture_matches_recorded_expectations() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");

        let events = parse_events(&jsonl);
        let count = |pred: &dyn Fn(&NormalizedEvent) -> bool| {
            events.iter().filter(|e| pred(e)).count() as u64
        };
        let want = |key: &str| {
            expected[key]
                .as_u64()
                .unwrap_or_else(|| panic!("{key} missing"))
        };

        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::TurnStart)),
            want("turn_start")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::AssistantFinal { .. })),
            want("assistant")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolCall { .. })),
            want("tool_call")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: true })),
            want("tool_result_error")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: false })),
            want("tool_result_ok")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::Compaction)),
            want("compaction")
        );

        let last_tokens = events
            .iter()
            .rev()
            .find_map(|e| match e {
                NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
                _ => None,
            })
            .expect("fixture has assistant events");
        assert_eq!(last_tokens, want("last_context_tokens"));
        assert!(
            want("tool_result_error") >= 1,
            "fixture must contain a tool error"
        );
    }
}
