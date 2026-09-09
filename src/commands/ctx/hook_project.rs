//! Issue #418: one place that maps a non-claude agent's own native hook
//! payload onto the claude-shaped JSON `hook::run_pretool`/`hook::
//! run_posttool`/`safety::run_check_hook_mode_with_env` already parse, and
//! translates the claude-shaped `hookSpecificOutput` envelope those bodies
//! produce back into that agent's own documented response shape. Every
//! guard/compaction body itself stays untouched: this module is the whole of
//! what makes copilot, droid and gemini run through it, one small mapping
//! table per agent, no per-agent copy of any guard logic.
//!
//! Every field name and envelope shape here is DOCS-ONLY (researched
//! 2026-09-09 against each vendor's published reference docs; no live
//! binary) -- see `copilot.rs`'s, `droid.rs`'s and `gemini.rs`'s own
//! `native_hooks` doc comments for the citations.

use serde_json::{Value, json};

/// `(native tool name, claude-equivalent tool name)` pairs. An unrecognised
/// native tool name is left exactly as it appears in the payload: neither
/// [`super::hook::pretool_decision`]'s subagent guard nor the
/// [`super::hook::FILE_MODIFICATION_TOOLS`] write guard recognise it either,
/// so the call simply falls through their existing "allow, this is not a
/// tool I guard" path -- no special-casing required here.
const COPILOT_TOOL_MAP: &[(&str, &str)] =
    &[("bash", "Bash"), ("edit", "Edit"), ("create", "Write")];
const DROID_TOOL_MAP: &[(&str, &str)] = &[
    ("Execute", "Bash"),
    ("Edit", "Edit"),
    ("ApplyPatch", "Edit"),
    ("Create", "Write"),
];
const GEMINI_TOOL_MAP: &[(&str, &str)] = &[
    ("run_shell_command", "Bash"),
    ("replace", "Edit"),
    ("write_file", "Write"),
];

/// Field-name alternatives one agent's payload may carry a given fact under,
/// tried in order -- covers copilot's own two event-name-cased stdin shapes
/// (`toolName`/`tool_input` vs `tool_name`/`tool_input`) with one table.
struct FieldNames {
    session_id: &'static [&'static str],
    cwd: &'static [&'static str],
    tool_name: &'static [&'static str],
    tool_input: &'static [&'static str],
    command: &'static [&'static str],
    path: &'static [&'static str],
}

const COPILOT_FIELDS: FieldNames = FieldNames {
    session_id: &["sessionId", "session_id"],
    cwd: &["cwd"],
    tool_name: &["toolName", "tool_name"],
    tool_input: &["toolArgs", "tool_input"],
    command: &["command"],
    path: &["path", "file_path"],
};
const DROID_FIELDS: FieldNames = FieldNames {
    session_id: &["session_id"],
    cwd: &["cwd"],
    tool_name: &["tool_name"],
    tool_input: &["tool_input"],
    command: &["command"],
    path: &["file_path"],
};
const GEMINI_FIELDS: FieldNames = FieldNames {
    session_id: &["session_id"],
    cwd: &["cwd"],
    tool_name: &["tool_name"],
    tool_input: &["tool_input"],
    command: &["command"],
    path: &["file_path"],
};

fn get_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
}

fn map_tool_name<'a>(native: &'a str, table: &[(&'static str, &'static str)]) -> &'a str {
    table
        .iter()
        .find(|(from, _)| *from == native)
        .map(|(_, to)| *to)
        .unwrap_or(native)
}

/// Builds the claude-shaped `PreToolPayload`/`HookToolPayload` JSON both
/// `hook::run_pretool` and `safety::run_check_hook_mode_with_env` already
/// parse (both structs are `#[serde(default)]` throughout, so the fields
/// this never sets -- `agent_id`, `permission_mode`, `transcript_path`,
/// subagent-dispatch fields -- simply take their zero default, exactly as an
/// older/partial claude payload would). `None` on unrecognised JSON or an
/// unknown `agent` name; the caller's own fail-open contract (`Ok(0)`, no
/// output) applies from there.
pub(crate) fn project_pretool(agent: &str, raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let (fields, table): (&FieldNames, &[(&'static str, &'static str)]) = match agent {
        "copilot" => (&COPILOT_FIELDS, COPILOT_TOOL_MAP),
        "droid" => (&DROID_FIELDS, DROID_TOOL_MAP),
        "gemini" => (&GEMINI_FIELDS, GEMINI_TOOL_MAP),
        _ => return None,
    };
    let native_tool = get_str(&value, fields.tool_name)?;
    let tool_name = map_tool_name(native_tool, table);
    let empty = json!({});
    let tool_input = fields
        .tool_input
        .iter()
        .find_map(|key| value.get(key))
        .unwrap_or(&empty);
    let command = get_str(tool_input, fields.command).unwrap_or("");
    let path = get_str(tool_input, fields.path).unwrap_or("");
    let session_id = get_str(&value, fields.session_id).unwrap_or("");
    let cwd = get_str(&value, fields.cwd).unwrap_or("");
    let projected = json!({
        "tool_name": tool_name,
        "tool_input": {
            "command": command,
            "file_path": path,
        },
        "cwd": cwd,
        "session_id": session_id,
        "agent_id": "",
        "agent_type": "",
    });
    Some(projected.to_string())
}

/// Translates the claude `hookSpecificOutput` envelope `hook::run_pretool`/
/// `safety::run_check_hook_mode_with_env` produced (`claude_envelope`, the
/// exact stdout text those bodies wrote, or `None` when they printed
/// nothing) into `agent`'s own documented response shape.
///
/// Only a `"deny"` `permissionDecision` ever produces output for a non-claude
/// agent: an `"allow"` (with or without `additionalContext`) or an
/// `updatedInput` rewrite both collapse to plain allow (`None`, meaning
/// "print nothing") here, since none of the three agents' own documented
/// contracts have a verified non-blocking advisory or rewrite channel this
/// module can trust -- see this module's own doc comment. `None` also on a
/// claude envelope this function cannot parse, an unknown `agent`, or no
/// envelope at all: fail open, matching every hook body's own contract.
pub(crate) fn translate_pretool_envelope(
    agent: &str,
    claude_envelope: Option<String>,
) -> Option<String> {
    let raw = claude_envelope?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let decision = value
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(Value::as_str)?;
    if decision != "deny" {
        return None;
    }
    let reason = value
        .pointer("/hookSpecificOutput/permissionDecisionReason")
        .and_then(Value::as_str)
        .unwrap_or("");
    match agent {
        // Copilot's own deny envelope is TOP-LEVEL, not nested under
        // `hookSpecificOutput` -- see this module's own doc comment.
        "copilot" => Some(
            json!({"permissionDecision": "deny", "permissionDecisionReason": reason}).to_string(),
        ),
        // Droid documents the exact same `hookSpecificOutput` shape claude
        // does, so the claude envelope is already correct verbatim.
        "droid" => Some(raw),
        "gemini" => Some(json!({"decision": "deny", "reason": reason}).to_string()),
        _ => None,
    }
}

/// Copilot-only: builds the claude-shaped `PostToolPayload` JSON `hook::
/// run_posttool` already parses, from copilot's own `postToolUse` stdin
/// (both event-name-cased variants). `BashToolOutput` deserializes with
/// `rename_all = "camelCase"`, so the projected `tool_response` object must
/// carry `isImage` (not `is_image`) for the round trip to actually work.
pub(crate) fn project_posttool_copilot(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let tool_input = value
        .get("toolArgs")
        .or_else(|| value.get("tool_input"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let command = tool_input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_result = value.get("toolResult").or_else(|| value.get("tool_result"));
    let text = tool_result
        .and_then(|result| {
            result
                .get("textResultForLlm")
                .or_else(|| result.get("text_result_for_llm"))
        })
        .and_then(Value::as_str)
        .unwrap_or("");
    let session_id = get_str(&value, &["sessionId", "session_id"]).unwrap_or("");
    let cwd = get_str(&value, &["cwd"]).unwrap_or("");
    let projected = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "tool_response": {
            "stdout": text,
            "stderr": "",
            "interrupted": false,
            "isImage": false,
        },
        "cwd": cwd,
        "session_id": session_id,
        "tool_use_id": "",
    });
    Some(projected.to_string())
}

/// Translates claude's `PostToolUse` `updatedToolOutput` envelope into
/// copilot's own `modifiedResult` shape. `None` on no envelope, an
/// unparseable one, or any agent other than `"copilot"` -- droid and gemini
/// get no `posttool` hook installed at all (`Capabilities::post_tool_hook`),
/// and `run_posttool_for_agent` never calls this for them.
pub(crate) fn translate_posttool_envelope(
    agent: &str,
    claude_envelope: Option<String>,
) -> Option<String> {
    if agent != "copilot" {
        return None;
    }
    let raw = claude_envelope?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let stdout = value
        .pointer("/hookSpecificOutput/updatedToolOutput/stdout")
        .and_then(Value::as_str)?;
    Some(
        json!({"modifiedResult": {"resultType": "success", "textResultForLlm": stdout}})
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_copilot_camel_case_edit_payload() {
        let raw = r#"{"sessionId":"s1","cwd":"/repo","toolName":"edit","toolArgs":{"path":"/repo/a.rs"}}"#;
        let projected = project_pretool("copilot", raw).expect("projects");
        let value: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(value["tool_name"], "Edit");
        assert_eq!(value["tool_input"]["file_path"], "/repo/a.rs");
        assert_eq!(value["session_id"], "s1");
        assert_eq!(value["cwd"], "/repo");
    }

    #[test]
    fn projects_copilot_pascal_snake_case_edit_payload_identically() {
        let raw = r#"{"session_id":"s1","cwd":"/repo","tool_name":"edit","tool_input":{"file_path":"/repo/a.rs"}}"#;
        let camel = project_pretool(
            "copilot",
            r#"{"sessionId":"s1","cwd":"/repo","toolName":"edit","toolArgs":{"path":"/repo/a.rs"}}"#,
        )
        .expect("projects");
        let pascal = project_pretool("copilot", raw).expect("projects");
        assert_eq!(camel, pascal);
    }

    #[test]
    fn projects_droid_execute_to_bash() {
        let raw = r#"{"session_id":"s1","cwd":"/repo","tool_name":"Execute","tool_input":{"command":"rm -rf /"}}"#;
        let projected = project_pretool("droid", raw).expect("projects");
        let value: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(value["tool_name"], "Bash");
        assert_eq!(value["tool_input"]["command"], "rm -rf /");
    }

    #[test]
    fn projects_gemini_run_shell_command_to_bash() {
        let raw = r#"{"session_id":"s1","cwd":"/repo","tool_name":"run_shell_command","tool_input":{"command":"ls"}}"#;
        let projected = project_pretool("gemini", raw).expect("projects");
        let value: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(value["tool_name"], "Bash");
    }

    #[test]
    fn unknown_tool_name_passes_through_unchanged() {
        let raw =
            r#"{"session_id":"s1","cwd":"/repo","tool_name":"some_future_tool","tool_input":{}}"#;
        let projected = project_pretool("droid", raw).expect("projects");
        let value: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(value["tool_name"], "some_future_tool");
    }

    #[test]
    fn translate_deny_envelope_per_agent() {
        let claude = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "nope"}}).to_string();

        let copilot = translate_pretool_envelope("copilot", Some(claude.clone())).expect("some");
        let copilot_value: Value = serde_json::from_str(&copilot).unwrap();
        assert_eq!(copilot_value["permissionDecision"], "deny");
        assert_eq!(copilot_value["permissionDecisionReason"], "nope");

        let droid = translate_pretool_envelope("droid", Some(claude.clone())).expect("some");
        assert_eq!(droid, claude);

        let gemini = translate_pretool_envelope("gemini", Some(claude)).expect("some");
        let gemini_value: Value = serde_json::from_str(&gemini).unwrap();
        assert_eq!(gemini_value["decision"], "deny");
        assert_eq!(gemini_value["reason"], "nope");
    }

    #[test]
    fn translate_allow_and_no_envelope_both_produce_nothing() {
        let allow = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "additionalContext": "note"}}).to_string();
        assert!(translate_pretool_envelope("copilot", Some(allow)).is_none());
        assert!(translate_pretool_envelope("copilot", None).is_none());
    }

    #[test]
    fn projects_and_translates_copilot_posttool_roundtrip() {
        let raw = r#"{"sessionId":"s1","cwd":"/repo","toolArgs":{"command":"cargo test"},"toolResult":{"resultType":"success","textResultForLlm":"a very very long result"}}"#;
        let projected = project_posttool_copilot(raw).expect("projects");
        let value: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(value["tool_name"], "Bash");
        assert_eq!(value["tool_response"]["stdout"], "a very very long result");
        assert_eq!(value["tool_response"]["isImage"], false);

        let claude_envelope = json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": {"stdout": "summary", "stderr": "", "interrupted": false, "isImage": false}}}).to_string();
        let translated =
            translate_posttool_envelope("copilot", Some(claude_envelope)).expect("some");
        let translated_value: Value = serde_json::from_str(&translated).unwrap();
        assert_eq!(
            translated_value["modifiedResult"]["textResultForLlm"],
            "summary"
        );
        assert_eq!(translated_value["modifiedResult"]["resultType"], "success");
    }

    #[test]
    fn translate_posttool_envelope_is_none_for_droid_and_gemini() {
        let claude_envelope =
            json!({"hookSpecificOutput": {"updatedToolOutput": {"stdout": "x"}}}).to_string();
        assert!(translate_posttool_envelope("droid", Some(claude_envelope.clone())).is_none());
        assert!(translate_posttool_envelope("gemini", Some(claude_envelope)).is_none());
    }
}
