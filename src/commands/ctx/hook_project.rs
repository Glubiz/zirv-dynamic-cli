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

/// The first of `keys` that is present in `value`, whatever it is. `None`
/// only when NONE of `keys` is present at all -- used to tell "absent" apart
/// from "present but the wrong shape" for both a string field
/// ([`get_str_checked`]) and an object field (`tool_input`/`toolArgs`,
/// `toolResult`/`tool_result`).
fn find_present<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(key))
}

/// Looks up the first of `keys` present in `value`. `Ok(None)` when none of
/// `keys` is present at all. `Ok(Some(s))` when the first present key holds
/// a string. `Err(())` when the first present key holds something that is
/// NOT a string.
///
/// Review round (#418): the `Err` case matters. The original version of this
/// lookup folded "missing" and "wrong type" into the same `None` (via
/// `and_then(Value::as_str)`), so a malformed non-claude payload -- e.g.
/// `"cwd": 7` instead of a string -- silently read as "cwd absent" and
/// projected as the empty string, the same as a payload that never mentioned
/// `cwd` at all. A recognized field present with the wrong JSON type must
/// fail the whole projection instead (see [`optional_str`]/[`project_pretool`]'s
/// own callers), not be coerced into a default that looks identical to
/// "nothing was there."
fn get_str_checked<'a>(value: &'a Value, keys: &[&str]) -> Result<Option<&'a str>, ()> {
    match find_present(value, keys) {
        None => Ok(None),
        Some(found) => found.as_str().map(Some).ok_or(()),
    }
}

/// [`get_str_checked`]'s optional-field convenience: absence folds to
/// `default`, a found string is used as-is, and a found NON-string fails the
/// whole projection by returning `None` here for the caller's own `?` to
/// propagate -- see [`get_str_checked`]'s own doc comment for why this
/// matters.
fn optional_str<'a>(value: &'a Value, keys: &[&str], default: &'a str) -> Option<&'a str> {
    match get_str_checked(value, keys) {
        Ok(found) => Some(found.unwrap_or(default)),
        Err(()) => None,
    }
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
/// older/partial claude payload would). `None` on unrecognised JSON, an
/// unknown `agent` name, a missing `tool_name`, `tool_input`/`toolArgs`
/// present but not an object, or `cwd`/`session_id`/`command`/`file_path`
/// present but not a string (review round #418: a recognized field with the
/// wrong JSON type fails the whole projection rather than being silently
/// coerced into the empty string a genuinely absent field gets -- see
/// [`get_str_checked`]'s own doc comment). The caller's own fail-open
/// contract (`Ok(0)`, no output) applies from there.
pub(crate) fn project_pretool(agent: &str, raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let (fields, table): (&FieldNames, &[(&'static str, &'static str)]) = match agent {
        "copilot" => (&COPILOT_FIELDS, COPILOT_TOOL_MAP),
        "droid" => (&DROID_FIELDS, DROID_TOOL_MAP),
        "gemini" => (&GEMINI_FIELDS, GEMINI_TOOL_MAP),
        _ => return None,
    };
    // `tool_name` is required (there is nothing sensible to project without
    // one): `.ok()` folds the wrong-type `Err` into `None` and `.flatten()`
    // folds `Ok(None)` (genuinely absent) into the same `None`, so either
    // failure mode denies the whole projection via the `?` below.
    let native_tool = get_str_checked(&value, fields.tool_name).ok().flatten()?;
    let tool_name = map_tool_name(native_tool, table);
    let empty = json!({});
    let tool_input = match find_present(&value, fields.tool_input) {
        None => &empty,
        Some(v) if v.is_object() => v,
        Some(_) => return None,
    };
    let command = optional_str(tool_input, fields.command, "")?;
    let path = optional_str(tool_input, fields.path, "")?;
    let session_id = optional_str(&value, fields.session_id, "")?;
    let cwd = optional_str(&value, fields.cwd, "")?;
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
/// Only `"deny"` and `"ask"` `permissionDecision`s ever produce output for a
/// non-claude agent: an `"allow"` (with or without `additionalContext`) or
/// an `updatedInput` rewrite both collapse to plain allow (`None`, meaning
/// "print nothing") here, since none of the three agents' own documented
/// contracts have a verified non-blocking advisory or rewrite channel this
/// module can trust -- see this module's own doc comment.
///
/// **`"ask"` (review round #418) does not collapse to silence -- silence
/// means allow on all three agents, and a would-be prompt read as a silent
/// allow is the wrong failure direction for a confirmation gate.** Copilot
/// and droid both document `permissionDecision: "allow"|"deny"|"ask"`, so
/// `ask` passes straight through with its reason (copilot's own TOP-LEVEL
/// envelope; droid's claude-identical `hookSpecificOutput` one). Gemini's
/// own contract has no `ask` concept at all, so an `ask` there fails CLOSED
/// instead, translated to `{"decision":"deny","reason":"zirv: confirmation
/// required -- <reason>"}` rather than being silently dropped.
///
/// `None` also on a claude envelope this function cannot parse, an unknown
/// `agent`, an unrecognised decision, or no envelope at all: fail open,
/// matching every hook body's own contract.
pub(crate) fn translate_pretool_envelope(
    agent: &str,
    claude_envelope: Option<String>,
) -> Option<String> {
    let raw = claude_envelope?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let decision = value
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(Value::as_str)?;
    let reason = value
        .pointer("/hookSpecificOutput/permissionDecisionReason")
        .and_then(Value::as_str)
        .unwrap_or("");
    match (decision, agent) {
        // `allow` -- with or without `additionalContext`/`updatedInput` --
        // always collapses to silence: none of the three agents have a
        // verified non-blocking advisory/rewrite channel this module can
        // target instead.
        ("allow", _) => None,
        // Copilot's own deny/ask envelope is TOP-LEVEL, not nested under
        // `hookSpecificOutput` -- see this module's own doc comment.
        ("deny", "copilot") => Some(
            json!({"permissionDecision": "deny", "permissionDecisionReason": reason}).to_string(),
        ),
        ("ask", "copilot") => Some(
            json!({"permissionDecision": "ask", "permissionDecisionReason": reason}).to_string(),
        ),
        // Droid documents the exact same `hookSpecificOutput` shape claude
        // does for BOTH `deny` and `ask`, so the claude envelope is already
        // correct verbatim in either case.
        ("deny" | "ask", "droid") => Some(raw),
        ("deny", "gemini") => Some(json!({"decision": "deny", "reason": reason}).to_string()),
        // Gemini has no `ask` -- fail closed to a deny that still names the
        // reason a human would otherwise have been asked to confirm.
        ("ask", "gemini") => Some(
            json!({
                "decision": "deny",
                "reason": format!("zirv: confirmation required -- {reason}")
            })
            .to_string(),
        ),
        _ => None,
    }
}

/// Copilot-only: builds the claude-shaped `PostToolPayload` JSON `hook::
/// run_posttool` already parses, from copilot's own `postToolUse` stdin
/// (both event-name-cased variants). `BashToolOutput` deserializes with
/// `rename_all = "camelCase"`, so the projected `tool_response` object must
/// carry `isImage` (not `is_image`) for the round trip to actually work.
///
/// `None` on unrecognised JSON, `toolArgs`/`tool_input` or `toolResult`/
/// `tool_result` present but not an object, or `cwd`/`session_id`/`command`
/// present but not a string (review round #418, the same fail-open-on-
/// wrong-type contract [`project_pretool`] holds to -- see
/// [`get_str_checked`]'s own doc comment).
pub(crate) fn project_posttool_copilot(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let empty = json!({});
    let tool_input = match find_present(&value, &["toolArgs", "tool_input"]) {
        None => &empty,
        Some(v) if v.is_object() => v,
        Some(_) => return None,
    };
    let command = optional_str(tool_input, &["command"], "")?;
    let tool_result = match find_present(&value, &["toolResult", "tool_result"]) {
        None => &empty,
        Some(v) if v.is_object() => v,
        Some(_) => return None,
    };
    let text = optional_str(
        tool_result,
        &["textResultForLlm", "text_result_for_llm"],
        "",
    )?;
    let session_id = optional_str(&value, &["sessionId", "session_id"], "")?;
    let cwd = optional_str(&value, &["cwd"], "")?;
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
///
/// `native_raw` is copilot's own ORIGINAL `postToolUse` stdin (the same text
/// [`project_posttool_copilot`] was given) -- review round #418: this
/// function now preserves that payload's own `toolResult.resultType`/
/// `tool_result.result_type` in the translated `modifiedResult.resultType`
/// rather than always hardcoding `"success"`, since a `"failure"` result
/// compacted through this hook must not silently read as a success to
/// copilot's own harness. Defaults to `"success"` when `native_raw` does not
/// parse, carries no such field, or that field is not a string -- unlike
/// `project_posttool_copilot`'s own fields, a malformed `resultType` here is
/// cosmetic (it only affects how copilot LABELS an already-compacted
/// result), so it degrades to the same default an absent field gets rather
/// than failing the whole translation and losing the compaction outright.
pub(crate) fn translate_posttool_envelope(
    agent: &str,
    native_raw: &str,
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
    let result_type = serde_json::from_str::<Value>(native_raw)
        .ok()
        .and_then(|native| {
            find_present(&native, &["toolResult", "tool_result"])
                .and_then(|result| find_present(result, &["resultType", "result_type"]))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "success".to_string());
    Some(
        json!({"modifiedResult": {"resultType": result_type, "textResultForLlm": stdout}})
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

    /// Review round (#418): an `ask` decision must not collapse to silence
    /// (silence reads as allow) -- copilot and droid pass it through with
    /// its reason, gemini fails closed to a deny.
    #[test]
    fn translate_ask_envelope_per_agent() {
        let claude = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "ask", "permissionDecisionReason": "confirm this"}}).to_string();

        let copilot = translate_pretool_envelope("copilot", Some(claude.clone())).expect("some");
        let copilot_value: Value = serde_json::from_str(&copilot).unwrap();
        assert_eq!(copilot_value["permissionDecision"], "ask");
        assert_eq!(copilot_value["permissionDecisionReason"], "confirm this");

        let droid = translate_pretool_envelope("droid", Some(claude.clone())).expect("some");
        assert_eq!(
            droid, claude,
            "droid documents claude's exact envelope for ask too"
        );

        let gemini = translate_pretool_envelope("gemini", Some(claude)).expect("some");
        let gemini_value: Value = serde_json::from_str(&gemini).unwrap();
        assert_eq!(
            gemini_value["decision"], "deny",
            "gemini has no ask concept, so it fails closed"
        );
        assert!(
            gemini_value["reason"]
                .as_str()
                .unwrap()
                .contains("confirm this"),
            "the original reason must survive: {gemini_value}"
        );
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
            translate_posttool_envelope("copilot", raw, Some(claude_envelope)).expect("some");
        let translated_value: Value = serde_json::from_str(&translated).unwrap();
        assert_eq!(
            translated_value["modifiedResult"]["textResultForLlm"],
            "summary"
        );
        assert_eq!(translated_value["modifiedResult"]["resultType"], "success");
    }

    /// Review round (#418): a `"failure"` `resultType` on the ORIGINAL
    /// copilot payload must survive into the translated `modifiedResult`,
    /// not be overwritten with the hardcoded `"success"` default.
    #[test]
    fn translate_posttool_envelope_preserves_a_failure_result_type() {
        let raw = r#"{"sessionId":"s1","cwd":"/repo","toolArgs":{"command":"cargo test"},"toolResult":{"resultType":"failure","textResultForLlm":"a very very long failing result"}}"#;
        let claude_envelope = json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": {"stdout": "summary", "stderr": "", "interrupted": false, "isImage": false}}}).to_string();
        let translated =
            translate_posttool_envelope("copilot", raw, Some(claude_envelope)).expect("some");
        let translated_value: Value = serde_json::from_str(&translated).unwrap();
        assert_eq!(translated_value["modifiedResult"]["resultType"], "failure");
        assert_eq!(
            translated_value["modifiedResult"]["textResultForLlm"],
            "summary"
        );
    }

    #[test]
    fn translate_posttool_envelope_defaults_result_type_when_absent() {
        let raw = r#"{"sessionId":"s1","cwd":"/repo","toolArgs":{"command":"cargo test"}}"#;
        let claude_envelope =
            json!({"hookSpecificOutput": {"updatedToolOutput": {"stdout": "x"}}}).to_string();
        let translated =
            translate_posttool_envelope("copilot", raw, Some(claude_envelope)).expect("some");
        let translated_value: Value = serde_json::from_str(&translated).unwrap();
        assert_eq!(translated_value["modifiedResult"]["resultType"], "success");
    }

    #[test]
    fn translate_posttool_envelope_is_none_for_droid_and_gemini() {
        let raw = r#"{"sessionId":"s1","cwd":"/repo","toolArgs":{"command":"cargo test"}}"#;
        let claude_envelope =
            json!({"hookSpecificOutput": {"updatedToolOutput": {"stdout": "x"}}}).to_string();
        assert!(translate_posttool_envelope("droid", raw, Some(claude_envelope.clone())).is_none());
        assert!(translate_posttool_envelope("gemini", raw, Some(claude_envelope)).is_none());
    }

    /// Review round (#418): a recognized field present with the wrong JSON
    /// type must fail the whole projection, never be silently coerced into
    /// the empty-string default an absent field gets.
    #[test]
    fn project_pretool_fails_open_on_a_wrongly_typed_recognized_field() {
        let raw = r#"{"tool_name":"Execute","cwd":7,"tool_input":{"command":"sudo rm -rf /"}}"#;
        assert!(project_pretool("droid", raw).is_none());
    }

    #[test]
    fn project_pretool_fails_open_when_tool_input_is_not_an_object() {
        let raw = r#"{"tool_name":"Execute","cwd":"/repo","tool_input":"not-an-object"}"#;
        assert!(project_pretool("droid", raw).is_none());
    }

    #[test]
    fn project_posttool_copilot_fails_open_on_a_wrongly_typed_recognized_field() {
        let raw = r#"{"sessionId":"s1","cwd":7,"toolArgs":{"command":"cargo test"}}"#;
        assert!(project_posttool_copilot(raw).is_none());
    }
}
