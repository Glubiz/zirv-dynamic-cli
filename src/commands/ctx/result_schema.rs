//! Issue #318: a structural contract a delegated worker's final report is
//! held to. `zirv ctx agent --result-schema`/`--result-kind` appends
//! [`render_contract_block`] to the worker's own prompt, then the headless
//! exit path extracts the worker's own final JSON candidate
//! ([`extract_json_candidate`]) and validates it ([`validate`]) before ever
//! telling the delegating session the run "succeeded". A failure carries its
//! own verbatim errors back to the worker for one bounded retry
//! ([`build_retry_message`]); a second failure is reported as
//! `contract_failed`, never a synthetic success.
//!
//! Pure module: no fs/clock/env/net. `built_in` reads its four shipped
//! shapes via `include_str!` at compile time, not from disk at runtime.

/// One field's shape inside a [`Schema`].
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub required: bool,
}

/// The value shapes a [`Field`] can declare. `ObjArray` nests a fresh field
/// list rather than a named sub-schema: a worker's report is a small, flat
/// contract, not a document format of its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Str,
    Bool,
    Int,
    Enum(Vec<String>),
    StrArray,
    ObjArray(Vec<Field>),
}

/// The structural contract itself: an ordered list of top-level fields. Order
/// is preserved from `from_json`/`built_in` and reused verbatim by
/// `render_contract_block` and `to_canonical_json`, so the contract a worker
/// reads and the one zirv validates against are demonstrably the same text.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    /// Parses the small hand-rolled shape `--result-schema` accepts:
    /// `{"fields":[{"name":...,"kind":...,"required":...,...}]}`. `kind` is
    /// one of `"str"`, `"bool"`, `"int"`, `"str_array"`, `"enum"` (needs a
    /// `"values"` array of strings), or `"obj_array"` (needs a nested
    /// `"fields"` array, recursively the same shape).
    pub fn from_json(text: &str) -> Result<Schema, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        Self::from_value(&value)
    }

    fn from_value(value: &serde_json::Value) -> Result<Schema, String> {
        let fields_value = value
            .get("fields")
            .ok_or_else(|| "missing top-level \"fields\" array".to_string())?;
        Ok(Schema {
            fields: parse_fields(fields_value)?,
        })
    }

    /// Deterministic JSON rendering of this schema -- the value exported
    /// verbatim into `ZIRV_CTX_RESULT_SCHEMA` for the worker's own env, and
    /// what the pane path (`mail::run_send_with`) re-parses via
    /// [`Schema::from_json`] to validate a self-report before it ever
    /// leaves the pane.
    pub fn to_canonical_json(&self) -> String {
        serde_json::to_string(&self.to_value()).unwrap_or_default()
    }

    fn to_value(&self) -> serde_json::Value {
        serde_json::Value::Object(
            [(
                "fields".to_string(),
                serde_json::Value::Array(self.fields.iter().map(Field::to_value).collect()),
            )]
            .into_iter()
            .collect(),
        )
    }
}

impl Field {
    fn to_value(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "name".to_string(),
            serde_json::Value::String(self.name.clone()),
        );
        obj.insert(
            "required".to_string(),
            serde_json::Value::Bool(self.required),
        );
        match &self.kind {
            Kind::Str => {
                obj.insert("kind".to_string(), serde_json::Value::String("str".into()));
            }
            Kind::Bool => {
                obj.insert("kind".to_string(), serde_json::Value::String("bool".into()));
            }
            Kind::Int => {
                obj.insert("kind".to_string(), serde_json::Value::String("int".into()));
            }
            Kind::StrArray => {
                obj.insert(
                    "kind".to_string(),
                    serde_json::Value::String("str_array".into()),
                );
            }
            Kind::Enum(values) => {
                obj.insert("kind".to_string(), serde_json::Value::String("enum".into()));
                obj.insert(
                    "values".to_string(),
                    serde_json::Value::Array(
                        values
                            .iter()
                            .map(|v| serde_json::Value::String(v.clone()))
                            .collect(),
                    ),
                );
            }
            Kind::ObjArray(nested) => {
                obj.insert(
                    "kind".to_string(),
                    serde_json::Value::String("obj_array".into()),
                );
                obj.insert(
                    "fields".to_string(),
                    serde_json::Value::Array(nested.iter().map(Field::to_value).collect()),
                );
            }
        }
        serde_json::Value::Object(obj)
    }
}

fn parse_fields(value: &serde_json::Value) -> Result<Vec<Field>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| "\"fields\" must be an array".to_string())?;
    arr.iter().map(parse_field).collect()
}

fn parse_field(value: &serde_json::Value) -> Result<Field, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "each field must be an object".to_string())?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "a field is missing its \"name\"".to_string())?
        .to_string();
    let kind_str = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("field \"{name}\" is missing its \"kind\""))?;
    let required = obj
        .get("required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let kind = match kind_str {
        "str" => Kind::Str,
        "bool" => Kind::Bool,
        "int" => Kind::Int,
        "str_array" => Kind::StrArray,
        "enum" => {
            let values = obj
                .get("values")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("field \"{name}\" of kind enum is missing \"values\""))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("field \"{name}\": enum values must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Kind::Enum(values)
        }
        "obj_array" => {
            let nested = obj.get("fields").ok_or_else(|| {
                format!("field \"{name}\" of kind obj_array is missing \"fields\"")
            })?;
            Kind::ObjArray(parse_fields(nested)?)
        }
        other => return Err(format!("field \"{name}\": unknown kind \"{other}\"")),
    };
    Ok(Field {
        name,
        kind,
        required,
    })
}

/// The OUTPUT CONTRACT block appended to a worker's prompt when
/// `--result-schema`/`--result-kind` was given. Starts with the fixed
/// header line every schema renders identically, lists every field (nested
/// `obj_array` fields indented under their parent), then the reply
/// instruction and one filled example -- so a worker sees both the rule and
/// a worked instance of it in one block.
pub fn render_contract_block(schema: &Schema) -> String {
    let mut lines = vec!["OUTPUT CONTRACT (machine-validated)".to_string()];
    render_fields(&schema.fields, 0, &mut lines);
    lines.push(String::new());
    lines.push(
        "Reply with your final message ending in one fenced json block matching this contract."
            .to_string(),
    );
    lines.push(String::new());
    lines.push("```json".to_string());
    lines.push(serde_json::to_string_pretty(&example_value(schema)).unwrap_or_default());
    lines.push("```".to_string());
    lines.join("\n")
}

fn render_fields(fields: &[Field], indent: usize, lines: &mut Vec<String>) {
    let pad = "  ".repeat(indent);
    for field in fields {
        let requirement = if field.required {
            "required"
        } else {
            "optional"
        };
        lines.push(format!(
            "{pad}- {}: {} ({requirement})",
            field.name,
            kind_description(&field.kind)
        ));
        if let Kind::ObjArray(nested) = &field.kind {
            render_fields(nested, indent + 1, lines);
        }
    }
}

fn kind_description(kind: &Kind) -> String {
    match kind {
        Kind::Str => "string".to_string(),
        Kind::Bool => "bool".to_string(),
        Kind::Int => "int".to_string(),
        Kind::StrArray => "array of strings".to_string(),
        Kind::Enum(values) => format!("enum [{}]", values.join(", ")),
        Kind::ObjArray(_) => "array of objects".to_string(),
    }
}

fn example_value(schema: &Schema) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for field in &schema.fields {
        obj.insert(field.name.clone(), example_field_value(field));
    }
    serde_json::Value::Object(obj)
}

fn example_field_value(field: &Field) -> serde_json::Value {
    match &field.kind {
        Kind::Str => serde_json::Value::String(format!("<{}>", field.name)),
        Kind::Bool => serde_json::Value::Bool(true),
        Kind::Int => serde_json::Value::Number(0.into()),
        Kind::Enum(values) => {
            serde_json::Value::String(values.first().cloned().unwrap_or_default())
        }
        Kind::StrArray => {
            serde_json::Value::Array(vec![serde_json::Value::String(format!("<{}>", field.name))])
        }
        Kind::ObjArray(nested) => {
            let mut obj = serde_json::Map::new();
            for nested_field in nested {
                obj.insert(nested_field.name.clone(), example_field_value(nested_field));
            }
            serde_json::Value::Array(vec![serde_json::Value::Object(obj)])
        }
    }
}

/// The LAST fenced ```json ... ``` (or untagged ``` fence whose content
/// starts with `{`) in `text`, tolerant of surrounding prose -- a worker's
/// final message is free text with the contract's json block at the end,
/// not a bare JSON document. Falls back to the last top-level balanced
/// `{...}` object in `text` when no fence at all matched, for a worker that
/// forgot the fence but still closed with a plain object.
pub fn extract_json_candidate(text: &str) -> Option<String> {
    last_fenced_json_block(text).or_else(|| last_balanced_object(text))
}

fn last_fenced_json_block(text: &str) -> Option<String> {
    const FENCE: &str = "```";
    let mut positions = Vec::new();
    let mut search_from = 0usize;
    while let Some(pos) = text[search_from..].find(FENCE) {
        positions.push(search_from + pos);
        search_from += pos + FENCE.len();
    }
    let mut result = None;
    let mut i = 0;
    while i + 1 < positions.len() {
        let start = positions[i] + FENCE.len();
        let end = positions[i + 1];
        if start <= end {
            let segment = &text[start..end];
            let (tag, body) = match segment.find('\n') {
                Some(nl) => (segment[..nl].trim(), segment[nl + 1..].trim()),
                None => (segment.trim(), ""),
            };
            let tag_lower = tag.to_ascii_lowercase();
            if tag_lower == "json" || (tag.is_empty() && body.trim_start().starts_with('{')) {
                result = Some(body.to_string());
            }
        }
        i += 2;
    }
    result
}

fn last_balanced_object(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut last = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{'
            && let Some(end) = matching_brace(&chars, i)
        {
            last = Some(chars[i..=end].iter().collect::<String>());
            i = end + 1;
            continue;
        }
        i += 1;
    }
    last
}

fn matching_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (offset, &c) in chars.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn kind_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn type_error(path: &str, expected: &str, actual: &serde_json::Value) -> String {
    format!(
        "wrong type for \"{path}\" (expected {expected}, got {})",
        kind_of(actual)
    )
}

/// Every distinct, human-readable way `value` fails to satisfy `schema`:
/// not an object, a missing required field, a wrong type, an enum value
/// outside its declared set, or a wrongly-shaped array element (nested
/// `obj_array` errors prefixed `field[N].nested`). Empty is valid; unknown
/// extra fields on `value` are never an error.
pub fn validate(schema: &Schema, value: &serde_json::Value) -> Vec<String> {
    let mut errors = Vec::new();
    validate_fields(&schema.fields, value, "", &mut errors);
    errors
}

fn validate_fields(
    fields: &[Field],
    value: &serde_json::Value,
    prefix: &str,
    errors: &mut Vec<String>,
) {
    let Some(obj) = value.as_object() else {
        let label = if prefix.is_empty() {
            "value".to_string()
        } else {
            format!("\"{prefix}\"")
        };
        errors.push(format!("{label} is not an object"));
        return;
    };
    for field in fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}.{}", field.name)
        };
        match obj.get(&field.name) {
            None => {
                if field.required {
                    errors.push(format!("missing required field \"{path}\""));
                }
            }
            Some(found) => validate_field_value(field, found, &path, errors),
        }
    }
}

fn validate_field_value(
    field: &Field,
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    match &field.kind {
        Kind::Str => {
            if !value.is_string() {
                errors.push(type_error(path, "string", value));
            }
        }
        Kind::Bool => {
            if !value.is_boolean() {
                errors.push(type_error(path, "bool", value));
            }
        }
        Kind::Int => {
            if !value.is_i64() && !value.is_u64() {
                errors.push(type_error(path, "int", value));
            }
        }
        Kind::Enum(values) => match value.as_str() {
            Some(s) if values.iter().any(|v| v == s) => {}
            Some(s) => errors.push(format!(
                "enum value \"{s}\" for \"{path}\" not in [{}]",
                values.join(", ")
            )),
            None => errors.push(type_error(path, "enum string", value)),
        },
        Kind::StrArray => match value.as_array() {
            Some(arr) => {
                for (i, elem) in arr.iter().enumerate() {
                    if !elem.is_string() {
                        errors.push(format!(
                            "array element {i} of \"{path}\" wrong shape: expected string, got {}",
                            kind_of(elem)
                        ));
                    }
                }
            }
            None => errors.push(type_error(path, "array", value)),
        },
        Kind::ObjArray(nested) => match value.as_array() {
            Some(arr) => {
                for (i, elem) in arr.iter().enumerate() {
                    if elem.as_object().is_none() {
                        errors.push(format!(
                            "array element {i} of \"{path}\" wrong shape: expected object, got {}",
                            kind_of(elem)
                        ));
                        continue;
                    }
                    validate_fields(nested, elem, &format!("{path}[{i}]"), errors);
                }
            }
            None => errors.push(type_error(path, "array", value)),
        },
    }
}

/// The single entry point both the headless retry path (`agent.rs`) and the
/// pane self-report path (`mail.rs`'s `run_send_with`) use to hold a
/// worker's free text to a [`Schema`]: extract the last JSON candidate,
/// parse it, then validate it. `Err` carries every failure reason in one
/// list, exactly what [`build_retry_message`] renders back to the worker.
pub fn evaluate(schema: &Schema, text: &str) -> Result<serde_json::Value, Vec<String>> {
    let Some(candidate) = extract_json_candidate(text) else {
        return Err(vec![
            "no JSON object found in the worker's final message".to_string(),
        ]);
    };
    let value: serde_json::Value = match serde_json::from_str(&candidate) {
        Ok(value) => value,
        Err(e) => return Err(vec![format!("invalid JSON: {e}")]),
    };
    let errors = validate(schema, &value);
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(errors)
    }
}

/// The one-turn retry prompt sent back to a worker whose report failed
/// [`validate`]: every error verbatim, as a bounded, bulleted list -- never
/// paraphrased, since the worker needs the exact same words zirv used to
/// reject it.
pub fn build_retry_message(errors: &[String]) -> String {
    let mut lines = vec!["Your report did not satisfy the OUTPUT CONTRACT:".to_string()];
    for error in errors {
        lines.push(format!("- {error}"));
    }
    lines.push("Reply again with only the corrected fenced json block.".to_string());
    lines.join("\n")
}

/// `--result-kind`'s closed vocabulary. Kept as an array (not a `HashSet`)
/// because its whole 4-element contents are also what a bad `--result-kind`
/// error message lists, in this fixed order.
pub const BUILT_IN_KINDS: [&str; 4] = ["review", "implement", "research", "test"];

fn raw_built_in(kind: &str) -> Option<&'static str> {
    match kind {
        "review" => Some(include_str!("schemas/review.json")),
        "implement" => Some(include_str!("schemas/implement.json")),
        "research" => Some(include_str!("schemas/research.json")),
        "test" => Some(include_str!("schemas/test.json")),
        _ => None,
    }
}

/// One of [`BUILT_IN_KINDS`]'s shapes, or `None` for anything else --
/// callers (`agent::resolve_result_schema`) turn that into the operator-
/// facing "not one of the built-in kinds" error, listing `BUILT_IN_KINDS`
/// itself so the message never drifts from the actual set.
pub fn built_in(kind: &str) -> Option<Schema> {
    let raw = raw_built_in(kind)?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    Schema::from_value(&value).ok()
}

#[cfg(test)]
fn built_in_example(kind: &str) -> Option<serde_json::Value> {
    let raw = raw_built_in(kind)?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value.get("example").cloned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn str_field(name: &str, required: bool) -> Field {
        Field {
            name: name.to_string(),
            kind: Kind::Str,
            required,
        }
    }

    #[test]
    fn from_json_parses_every_kind_including_nested_obj_array() {
        let raw = r#"{"fields":[
            {"name":"status","kind":"enum","values":["done","blocked"],"required":true},
            {"name":"ok","kind":"bool","required":false},
            {"name":"count","kind":"int","required":false},
            {"name":"tags","kind":"str_array","required":false},
            {"name":"items","kind":"obj_array","required":false,"fields":[
                {"name":"name","kind":"str","required":true}
            ]}
        ]}"#;
        let schema = Schema::from_json(raw).expect("parses");
        assert_eq!(schema.fields.len(), 5);
        assert_eq!(
            schema.fields[4].kind,
            Kind::ObjArray(vec![str_field("name", true)])
        );
    }

    #[test]
    fn from_json_rejects_invalid_top_level_json() {
        let err = Schema::from_json("not json").expect_err("must fail");
        assert!(err.contains("invalid JSON"), "got {err}");
    }

    #[test]
    fn from_json_requires_a_fields_array() {
        let err = Schema::from_json("{}").expect_err("must fail");
        assert!(err.contains("\"fields\""), "got {err}");
    }

    #[test]
    fn from_json_rejects_an_unknown_kind() {
        let err = Schema::from_json(r#"{"fields":[{"name":"x","kind":"weird"}]}"#)
            .expect_err("must fail");
        assert!(err.contains("unknown kind"), "got {err}");
    }

    #[test]
    fn canonical_json_round_trips_through_from_json() {
        let schema = Schema {
            fields: vec![
                Field {
                    name: "status".to_string(),
                    kind: Kind::Enum(vec!["done".to_string(), "blocked".to_string()]),
                    required: true,
                },
                Field {
                    name: "findings".to_string(),
                    kind: Kind::ObjArray(vec![str_field("file", true)]),
                    required: false,
                },
            ],
        };
        let canonical = schema.to_canonical_json();
        let reparsed = Schema::from_json(&canonical).expect("round trips");
        assert_eq!(schema, reparsed);
    }

    #[test]
    fn render_contract_block_names_every_field_and_ends_with_a_json_example() {
        let schema = Schema {
            fields: vec![
                Field {
                    name: "status".to_string(),
                    kind: Kind::Enum(vec!["done".to_string(), "blocked".to_string()]),
                    required: true,
                },
                Field {
                    name: "findings".to_string(),
                    kind: Kind::ObjArray(vec![str_field("file", true)]),
                    required: false,
                },
            ],
        };
        let block = render_contract_block(&schema);
        assert!(block.starts_with("OUTPUT CONTRACT (machine-validated)"));
        assert!(block.contains("status: enum [done, blocked] (required)"));
        assert!(block.contains("findings: array of objects (optional)"));
        assert!(block.contains("file: string (required)"));
        assert!(block.contains("fenced json block"));
        assert!(block.contains("```json"));
        let example_start = block.find("```json").expect("fence") + "```json".len();
        let example_end = block.rfind("```").expect("closing fence");
        let example: serde_json::Value =
            serde_json::from_str(block[example_start..example_end].trim()).expect("valid json");
        assert!(validate(&schema, &example).is_empty(), "{example:?}");
    }

    #[test]
    fn extract_json_candidate_reads_a_clean_fenced_block() {
        let text = "here you go\n```json\n{\"status\": \"done\"}\n```\n";
        assert_eq!(
            extract_json_candidate(text),
            Some("{\"status\": \"done\"}".to_string())
        );
    }

    #[test]
    fn extract_json_candidate_ignores_prose_around_the_fence() {
        let text =
            "All done. Summary follows.\n\n```json\n{\"status\": \"blocked\"}\n```\n\nThanks!";
        assert_eq!(
            extract_json_candidate(text),
            Some("{\"status\": \"blocked\"}".to_string())
        );
    }

    #[test]
    fn extract_json_candidate_takes_the_last_of_several_fenced_blocks() {
        let text = "```json\n{\"status\": \"partial\"}\n```\nOn reflection:\n```json\n{\"status\": \"done\"}\n```";
        assert_eq!(
            extract_json_candidate(text),
            Some("{\"status\": \"done\"}".to_string())
        );
    }

    #[test]
    fn extract_json_candidate_accepts_an_untagged_fence_starting_with_a_brace() {
        let text = "```\n{\"status\": \"done\"}\n```";
        assert_eq!(
            extract_json_candidate(text),
            Some("{\"status\": \"done\"}".to_string())
        );
    }

    #[test]
    fn extract_json_candidate_falls_back_to_a_balanced_object_with_no_fence_at_all() {
        let text = "Done. {\"status\": \"done\"} -- see above.";
        assert_eq!(
            extract_json_candidate(text),
            Some("{\"status\": \"done\"}".to_string())
        );
    }

    #[test]
    fn extract_json_candidate_is_none_for_text_with_no_json_shape() {
        assert_eq!(extract_json_candidate("just prose, nothing else"), None);
    }

    fn sample_schema() -> Schema {
        Schema {
            fields: vec![
                Field {
                    name: "status".to_string(),
                    kind: Kind::Enum(vec!["done".to_string(), "blocked".to_string()]),
                    required: true,
                },
                Field {
                    name: "count".to_string(),
                    kind: Kind::Int,
                    required: false,
                },
                Field {
                    name: "tags".to_string(),
                    kind: Kind::StrArray,
                    required: false,
                },
                Field {
                    name: "findings".to_string(),
                    kind: Kind::ObjArray(vec![
                        str_field("file", true),
                        Field {
                            name: "line".to_string(),
                            kind: Kind::Int,
                            required: true,
                        },
                    ]),
                    required: false,
                },
            ],
        }
    }

    #[test]
    fn validate_accepts_a_fully_populated_valid_value() {
        let value = serde_json::json!({
            "status": "done",
            "count": 3,
            "tags": ["a", "b"],
            "findings": [{"file": "x.rs", "line": 1}],
        });
        assert_eq!(validate(&sample_schema(), &value), Vec::<String>::new());
    }

    #[test]
    fn validate_accepts_a_value_missing_every_optional_field() {
        let value = serde_json::json!({"status": "done"});
        assert_eq!(validate(&sample_schema(), &value), Vec::<String>::new());
    }

    #[test]
    fn validate_reports_not_an_object() {
        let value = serde_json::json!("just a string");
        let errors = validate(&sample_schema(), &value);
        assert_eq!(errors, vec!["value is not an object".to_string()]);
    }

    #[test]
    fn validate_reports_a_missing_required_field() {
        let value = serde_json::json!({"count": 1});
        let errors = validate(&sample_schema(), &value);
        assert_eq!(
            errors,
            vec!["missing required field \"status\"".to_string()]
        );
    }

    #[test]
    fn validate_reports_a_wrong_type() {
        let value = serde_json::json!({"status": "done", "count": "not a number"});
        let errors = validate(&sample_schema(), &value);
        assert_eq!(
            errors,
            vec!["wrong type for \"count\" (expected int, got string)".to_string()]
        );
    }

    #[test]
    fn validate_reports_an_enum_value_outside_its_set() {
        let value = serde_json::json!({"status": "bogus"});
        let errors = validate(&sample_schema(), &value);
        assert_eq!(
            errors,
            vec!["enum value \"bogus\" for \"status\" not in [done, blocked]".to_string()]
        );
    }

    #[test]
    fn validate_reports_a_wrongly_shaped_array_element() {
        let value = serde_json::json!({"status": "done", "tags": ["ok", 5]});
        let errors = validate(&sample_schema(), &value);
        assert_eq!(
            errors,
            vec![
                "array element 1 of \"tags\" wrong shape: expected string, got number".to_string()
            ]
        );
    }

    #[test]
    fn validate_reports_nested_obj_array_errors_prefixed_with_the_index() {
        let value = serde_json::json!({
            "status": "done",
            "findings": [{"line": 1}],
        });
        let errors = validate(&sample_schema(), &value);
        assert_eq!(
            errors,
            vec!["missing required field \"findings[0].file\"".to_string()]
        );
    }

    #[test]
    fn validate_collects_every_distinct_error_at_once() {
        let value = serde_json::json!({"count": "bad"});
        let errors = validate(&sample_schema(), &value);
        assert_eq!(errors.len(), 2, "{errors:?}");
    }

    #[test]
    fn evaluate_returns_the_parsed_value_when_valid() {
        let text = "```json\n{\"status\": \"done\"}\n```";
        let value = evaluate(&sample_schema(), text).expect("valid");
        assert_eq!(value, serde_json::json!({"status": "done"}));
    }

    #[test]
    fn evaluate_returns_errors_for_invalid_json_syntax() {
        let text = "```json\n{\"status\": \n```";
        let errors = evaluate(&sample_schema(), text).expect_err("must fail");
        assert!(errors[0].contains("invalid JSON"), "{errors:?}");
    }

    #[test]
    fn evaluate_returns_errors_when_no_candidate_is_found() {
        let errors = evaluate(&sample_schema(), "no json here at all").expect_err("must fail");
        assert_eq!(
            errors,
            vec!["no JSON object found in the worker's final message".to_string()]
        );
    }

    #[test]
    fn build_retry_message_carries_every_error_verbatim() {
        let errors = vec![
            "missing required field \"status\"".to_string(),
            "wrong type for \"count\" (expected int, got string)".to_string(),
        ];
        let message = build_retry_message(&errors);
        assert!(message.starts_with("Your report did not satisfy the OUTPUT CONTRACT:"));
        for error in &errors {
            assert!(
                message.contains(&format!("- {error}")),
                "missing {error} in {message}"
            );
        }
        assert!(message.ends_with("Reply again with only the corrected fenced json block."));
    }

    #[test]
    fn built_in_kinds_all_resolve_to_a_schema() {
        for kind in BUILT_IN_KINDS {
            assert!(built_in(kind).is_some(), "{kind} should resolve");
        }
        assert!(built_in("bogus").is_none());
    }

    #[test]
    fn every_built_in_kinds_example_validates_against_its_own_schema() {
        let mut checked: BTreeMap<&str, usize> = BTreeMap::new();
        for kind in BUILT_IN_KINDS {
            let schema = built_in(kind).unwrap_or_else(|| panic!("{kind} must resolve"));
            let example = built_in_example(kind).unwrap_or_else(|| panic!("{kind} has no example"));
            let errors = validate(&schema, &example);
            assert!(errors.is_empty(), "{kind}: {errors:?}");
            checked.insert(kind, schema.fields.len());
        }
        assert_eq!(checked.len(), BUILT_IN_KINDS.len());
    }
}
