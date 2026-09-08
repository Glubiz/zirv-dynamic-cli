//! Issues #408/#409/#411: the shaping passes `output.rs`'s `render_summary`
//! and `summarize_stored` apply on top of the head/tail/diagnostic scan
//! ([`super::output::scan_for_display`]) before a summary is ever printed.
//!
//! Every pass here is subject to the same never-worse discipline as #410's
//! guard in `output.rs`: a pass that does not actually shrink what it is
//! shaping is skipped, and the caller renders the faithful, ungrouped form
//! instead. None of these functions do their own I/O except the two that are
//! explicitly file-based (`sniff_is_binary`, `try_json_summary`, both reading
//! a bounded prefix or a size-capped whole file) -- everything else is a pure
//! transformation over already-collected lines/blocks, kept separate from
//! `output.rs`'s own I/O shell for the same reason the rest of that module
//! is split that way.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

// ---------------------------------------------------------------------
// #408: group repeated diagnostics by signature
// ---------------------------------------------------------------------

/// An embedded location is exactly what makes two occurrences of the SAME
/// diagnostic look different, so grouping strips it before comparing.
static PANIC_TRAILING_LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+at\s+\S+:\d+(:\d+)?:?\s*$").expect("regex"));
/// `tsc`'s own `file.ts(10,5): ` prefix.
static LEADING_TS_LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\S+\(\d+,\d+\):\s*").expect("regex"));
/// An eslint stylish line's leading `10:5  ` position.
static LEADING_LINE_COL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+:\d+\s+").expect("regex"));
/// A generic `file:line:` or `file:line:col:` prefix.
static LEADING_FILE_LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\S+:\d+(:\d+)?:\s*").expect("regex"));

/// Whether `line` is a failing test's own NAME rather than a shared cause --
/// a cargo/nextest `test x ... FAILED`, a bare `FAILED path::test` (pytest),
/// or a pytest `---- name ----` capture banner. These carry an identity that
/// must never be merged with a DIFFERENT test's, even when every other word
/// matches, so they are signed by their own full text rather than a
/// location-stripped message.
fn test_identity(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("FAILED ") {
        return Some(rest.trim().to_string());
    }
    if let Some(name) = trimmed
        .strip_prefix("test ")
        .and_then(|rest| rest.split_once(" ... FAILED"))
        .map(|(name, _)| name)
    {
        return Some(name.trim().to_string());
    }
    if trimmed.len() > 8 && trimmed.starts_with("---- ") && trimmed.ends_with("----") {
        return Some(trimmed.to_string());
    }
    None
}

/// Strips whatever embedded location a recognised diagnostic family carries,
/// leaving the part that actually identifies WHICH diagnostic this is: a
/// rustc/clippy trigger line has none to strip (its location is the `-->`
/// continuation line, never part of the trigger at all); `tsc` and eslint
/// carry a leading `file(line,col):` / `line:col` prefix; a panic's trigger
/// line carries a trailing `at file:line:col:`.
fn strip_diagnostic_location(line: &str) -> String {
    let trimmed = line.trim();
    if let Some(m) = PANIC_TRAILING_LOCATION.find(trimmed) {
        return trimmed[..m.start()].trim_end().to_string();
    }
    if let Some(m) = LEADING_TS_LOCATION.find(trimmed) {
        return trimmed[m.end()..].to_string();
    }
    if let Some(m) = LEADING_LINE_COL.find(trimmed) {
        return trimmed[m.end()..].to_string();
    }
    if let Some(m) = LEADING_FILE_LOCATION.find(trimmed) {
        return trimmed[m.end()..].to_string();
    }
    trimmed.to_string()
}

/// The signature two occurrences of the same diagnostic share: a location-
/// independent form of its trigger line, or -- for a line naming a specific
/// failing test -- that name verbatim, so a distinct test is never folded
/// into another's count.
fn diagnostic_signature(block: &[String]) -> String {
    let Some(trigger) = block.first() else {
        return String::new();
    };
    match test_identity(trigger) {
        Some(identity) => format!("\u{0}{identity}"),
        None => strip_diagnostic_location(trigger),
    }
}

/// Collapses `blocks` that share a signature into one representative block
/// carrying an `[x N]` count on its trigger line, sorted by count descending
/// with ties keeping first-seen order -- fifty repeats of the same clippy
/// lint spend one line instead of fifty, and every OTHER location stays
/// reachable through the ordinary `zirv ctx output show <id>` retrieval
/// line, never lost.
fn group_diagnostic_blocks(blocks: &[Vec<String>]) -> Vec<Vec<String>> {
    let mut order: Vec<String> = Vec::new();
    let mut members: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, block) in blocks.iter().enumerate() {
        let sig = diagnostic_signature(block);
        members.entry(sig.clone()).or_insert_with(|| {
            order.push(sig.clone());
            Vec::new()
        });
        members.get_mut(&sig).expect("just inserted").push(i);
    }
    order.sort_by(|a, b| members[b].len().cmp(&members[a].len()));
    order
        .into_iter()
        .map(|sig| {
            let idxs = &members[&sig];
            if idxs.len() == 1 {
                blocks[idxs[0]].clone()
            } else {
                let mut rep = blocks[idxs[0]].clone();
                rep[0] = format!("[x {}] {}", idxs.len(), rep[0]);
                rep
            }
        })
        .collect()
}

/// Bytes the caller's own block renderer would spend on `blocks`' lines
/// alone (a constant `"  " + line + "\n"` per line, matching
/// `output::push_blocks`) -- the section title and truncation footer are
/// identical whichever form is chosen, so they are left out of the
/// comparison on purpose.
fn blocks_render_len(blocks: &[Vec<String>]) -> usize {
    blocks
        .iter()
        .flat_map(|block| block.iter())
        .map(|line| line.len() + 3)
        .sum()
}

/// #408: groups repeated diagnostics by signature, but only when doing so
/// actually shrinks the rendered section -- a handful of already-distinct
/// blocks render exactly as [`super::output::scan_for_display`] collected
/// them, never reshuffled into a same-size "grouped" form for no benefit.
pub(crate) fn shaped_diagnostic_blocks(blocks: &[Vec<String>]) -> Vec<Vec<String>> {
    if blocks.len() < 2 {
        return blocks.to_vec();
    }
    let grouped = group_diagnostic_blocks(blocks);
    if grouped.len() < blocks.len() && blocks_render_len(&grouped) < blocks_render_len(blocks) {
        grouped
    } else {
        blocks.to_vec()
    }
}

// ---------------------------------------------------------------------
// #409a: normalized dedup of noisy repeated lines
// ---------------------------------------------------------------------

static TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
        .expect("regex")
});
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").expect("regex")
});
static HEX_RUN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{8,}\b").expect("regex"));
static LONG_INT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d{6,}\b").expect("regex"));

/// Replaces every match of `re` in `line` with `token`, EXCEPT an occurrence
/// directly touching a path separator -- issue #409's "paths untouched": a
/// version or line number embedded in a file path is part of what makes two
/// lines genuinely different, not noise to collapse away.
fn mask_matches(line: &str, re: &Regex, token: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    for m in re.find_iter(line) {
        let touches_path =
            line[..m.start()].ends_with(['/', '\\']) || line[m.end()..].starts_with(['/', '\\']);
        if touches_path {
            continue;
        }
        out.push_str(&line[last..m.start()]);
        out.push_str(token);
        last = m.end();
    }
    out.push_str(&line[last..]);
    out
}

/// The key two lines share when they differ only in a timestamp, a UUID, a
/// hex id (a docker layer digest, a commit SHA) or a long counter -- never
/// applied to a path, so two genuinely different file references still key
/// apart.
fn normalize_noise_line(line: &str) -> String {
    let masked = mask_matches(line, &TIMESTAMP_RE, "<ts>");
    let masked = mask_matches(&masked, &UUID_RE, "<uuid>");
    let masked = mask_matches(&masked, &HEX_RUN_RE, "<hex>");
    mask_matches(&masked, &LONG_INT_RE, "<n>")
}

/// Collapses lines whose normalized form (see [`normalize_noise_line`])
/// occurs more than twice into one `[x N] <first line>` entry at the
/// position of its first occurrence, dropping the later occurrences
/// entirely; a form seen once or twice is left exactly as it was -- two
/// coincidentally-similar lines are not a run of noise.
fn dedupe_noise_lines(lines: &[String]) -> Vec<String> {
    let keys: Vec<String> = lines.iter().map(|l| normalize_noise_line(l)).collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    for (i, key) in keys.iter().enumerate() {
        *counts.entry(key.as_str()).or_insert(0) += 1;
        first_seen.entry(key.as_str()).or_insert(i);
    }
    let mut out = Vec::with_capacity(lines.len());
    for (i, (line, key)) in lines.iter().zip(keys.iter()).enumerate() {
        let count = counts[key.as_str()];
        if count > 2 {
            if first_seen[key.as_str()] == i {
                out.push(format!("[x {count}] {line}"));
            }
        } else {
            out.push(line.clone());
        }
    }
    out
}

fn lines_render_len(lines: &[String]) -> usize {
    lines.iter().map(|l| l.len() + 3).sum()
}

/// #409a: collapses near-identical noisy lines, but only when the result is
/// actually smaller -- never-worse applies per pass, same as
/// [`shaped_diagnostic_blocks`].
pub(crate) fn shaped_noise_lines(lines: &[String]) -> Vec<String> {
    if lines.len() < 3 {
        return lines.to_vec();
    }
    let deduped = dedupe_noise_lines(lines);
    if deduped.len() < lines.len() && lines_render_len(&deduped) < lines_render_len(lines) {
        deduped
    } else {
        lines.to_vec()
    }
}

// ---------------------------------------------------------------------
// #411: binary safety
// ---------------------------------------------------------------------

/// How much of a stored capture is sniffed for binary content: bounded, like
/// every other sniff in this module, rather than reading an arbitrarily
/// large file whole just to decide whether to summarize it at all.
const BINARY_SNIFF_BYTES: usize = 64 * 1024;

/// Whether `bytes` looks binary rather than text -- a NUL byte (never
/// legitimate in a text stream) or more than 1% Unicode replacement
/// characters after a lossy decode (a strong sign the bytes are not UTF-8
/// text at all). Either one means "produce no summary", never a garbled
/// line-based one built from mis-decoded bytes.
pub(crate) fn is_binary_content(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    if bytes.is_empty() {
        return false;
    }
    let text = String::from_utf8_lossy(bytes);
    let mut total = 0usize;
    let mut replaced = 0usize;
    for ch in text.chars() {
        total += 1;
        if ch == '\u{FFFD}' {
            replaced += 1;
        }
    }
    total > 0 && (replaced as f64 / total as f64) > 0.01
}

/// Binary safety for a STORED file: only the first [`BINARY_SNIFF_BYTES`]
/// are read. A PNG/JPEG/zip signature carries a NUL byte within its first
/// handful of bytes, so this bounded prefix is enough to catch the shapes
/// issue #411 actually cares about without loading an arbitrarily large
/// capture whole just to sniff it.
pub(crate) fn sniff_is_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; BINARY_SNIFF_BYTES];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    is_binary_content(&buf[..n])
}

// ---------------------------------------------------------------------
// #411: JSON-aware structural summary
// ---------------------------------------------------------------------

const JSON_MAX_DEPTH: usize = 5;
const JSON_MAX_ARRAY_ITEMS: usize = 5;
const JSON_MAX_OBJECT_KEYS: usize = 8;
const JSON_MAX_STRING_CHARS: usize = 120;
/// A document larger than this is never even read for JSON detection: the
/// point of this pass is a bounded structural summary of something like a
/// `gh api` response, not loading an arbitrarily large capture into memory
/// whole.
const JSON_SNIFF_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The compact type name a subtree collapses to once [`JSON_MAX_DEPTH`] is
/// reached, or once the types-only schema mode ([`render_json_schema`])
/// reaches a scalar -- always a single self-contained token, so it can never
/// leave a brace or bracket unbalanced.
fn type_tag(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Array(items) => format!("[...{} items]", items.len()),
        Value::Object(map) => format!("{{...{} keys}}", map.len()),
    }
}

fn render_json_string(s: &str) -> String {
    if s.chars().count() <= JSON_MAX_STRING_CHARS {
        return format!("{s:?}");
    }
    let truncated: String = s.chars().take(JSON_MAX_STRING_CHARS).collect();
    format!("{:?}", format!("{truncated}..."))
}

/// The full structural summary (issue #411): every array past
/// [`JSON_MAX_ARRAY_ITEMS`] items and every object past
/// [`JSON_MAX_OBJECT_KEYS`] keys is capped with an explicit `+N` count, long
/// strings are cut with an ellipsis, and depth past [`JSON_MAX_DEPTH`]
/// collapses a whole subtree to its type -- so the result is always BALANCED
/// text, never a mid-token cut: every brace/bracket this function writes is
/// closed by the very call that wrote it, unlike slicing pre-rendered text
/// at a byte budget.
fn render_json(value: &Value, depth: usize) -> String {
    if depth >= JSON_MAX_DEPTH {
        return type_tag(value);
    }
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => render_json_string(s),
        Value::Array(items) => {
            let shown: Vec<String> = items
                .iter()
                .take(JSON_MAX_ARRAY_ITEMS)
                .map(|v| render_json(v, depth + 1))
                .collect();
            if items.len() > JSON_MAX_ARRAY_ITEMS {
                format!(
                    "[{}, ... +{} more]",
                    shown.join(", "),
                    items.len() - JSON_MAX_ARRAY_ITEMS
                )
            } else {
                format!("[{}]", shown.join(", "))
            }
        }
        Value::Object(map) => {
            if map.len() > JSON_MAX_OBJECT_KEYS {
                let keys: Vec<String> = map
                    .keys()
                    .take(JSON_MAX_OBJECT_KEYS)
                    .map(|k| format!("{k:?}"))
                    .collect();
                format!(
                    "{{{}, ... +{} keys}}",
                    keys.join(", "),
                    map.len() - JSON_MAX_OBJECT_KEYS
                )
            } else {
                let shown: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("{k:?}: {}", render_json(v, depth + 1)))
                    .collect();
                format!("{{{}}}", shown.join(", "))
            }
        }
    }
}

/// The types-only fallback for when even [`render_json`]'s structural
/// summary does not fit the budget: every scalar becomes its type name, an
/// array becomes one representative element's schema plus its length, and
/// an object keeps only its (capped) key names -- small enough for even a
/// wide, deeply-populated document, at the cost of the actual values.
fn render_json_schema(value: &Value, depth: usize) -> String {
    if depth >= JSON_MAX_DEPTH {
        return type_tag(value);
    }
    match value {
        Value::Array(items) => match items.first() {
            Some(first) => format!(
                "[{} x{}]",
                render_json_schema(first, depth + 1),
                items.len()
            ),
            None => "[]".to_string(),
        },
        Value::Object(map) => {
            let keys: Vec<String> = map
                .keys()
                .take(JSON_MAX_OBJECT_KEYS)
                .map(|k| format!("{k:?}"))
                .collect();
            if map.is_empty() {
                "{}".to_string()
            } else if map.len() > JSON_MAX_OBJECT_KEYS {
                format!(
                    "{{{}, ... +{} keys}}",
                    keys.join(", "),
                    map.len() - JSON_MAX_OBJECT_KEYS
                )
            } else {
                format!("{{{}}}", keys.join(", "))
            }
        }
        other => type_tag(other),
    }
}

/// Whether `bytes`, trimmed of ASCII whitespace, is bracket-shaped JSON --
/// starts with `{`/`[` and ends with the matching close. Cheap and
/// allocation-free: no `serde_json` parse happens unless this returns
/// `true`, so the common non-JSON path never pays for one.
fn looks_like_json(bytes: &[u8]) -> bool {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace());
    let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace());
    match (start, end) {
        (Some(s), Some(e)) if s <= e => {
            matches!((bytes[s], bytes[e]), (b'{', b'}') | (b'[', b']'))
        }
        _ => false,
    }
}

/// #411: the JSON-aware summary, tried before the ordinary line-based scan
/// whenever the stored output is bracket-shaped JSON. `None` on anything
/// that is not this shape at all (the ordinary path handles it), on a parse
/// failure (a false-positive bracket match: still not this pass's problem
/// -- the caller falls back to the line scan), on a document too large to
/// sniff, or when the result -- even in schema mode -- does not fit
/// `max_bytes` or does not beat the raw byte count (the same never-worse
/// discipline as `output::render_summary`'s own guard).
pub(crate) fn try_json_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    path: &Path,
    total_lines: usize,
    max_bytes: usize,
) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > JSON_SNIFF_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if !looks_like_json(&bytes) {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    render_json_summary(
        id,
        command,
        exit_code,
        bytes.len(),
        total_lines,
        &value,
        max_bytes,
    )
}

fn render_json_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    raw_len: usize,
    total_lines: usize,
    value: &Value,
    max_bytes: usize,
) -> Option<String> {
    let retrieval = super::output::retrieval_line(id);
    let header = match exit_code {
        Some(code) => format!(
            "zirv compacted output: exit {code} -- {}\n",
            super::output::display_line(command)
        ),
        None => format!(
            "zirv compacted output: {}\n",
            super::output::display_line(command)
        ),
    };
    let counts = format!("captured {total_lines} lines, {raw_len} bytes (json)\n");
    const JSON_LABEL: &str = "json summary:\n  ";
    let prefix_len = header.len() + counts.len() + JSON_LABEL.len();
    let suffix_len = 1 + retrieval.len() + 1; // trailing "\n" + retrieval + "\n"
    let budget = max_bytes.checked_sub(prefix_len + suffix_len)?;

    let mut rendered = render_json(value, 0);
    if rendered.len() > budget {
        rendered = render_json_schema(value, 0);
    }
    if rendered.len() > budget {
        return None;
    }

    let mut body = header;
    body.push_str(&counts);
    body.push_str(JSON_LABEL);
    body.push_str(&rendered);
    body.push('\n');
    body.push_str(&retrieval);
    body.push('\n');
    if body.len() >= raw_len {
        // #410's never-worse guard applies here exactly as it does to the
        // line-based summary: a small JSON document is left alone.
        return None;
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    // -- #408: diagnostic grouping --------------------------------------

    #[test]
    fn forty_identical_clippy_warnings_group_into_one_counted_block() {
        let blocks: Vec<Vec<String>> = (0..40)
            .map(|i| {
                block(&[
                    "warning: unused variable: `x`",
                    &format!("  --> src/file{i}.rs:{i}:5"),
                ])
            })
            .collect();
        let shaped = shaped_diagnostic_blocks(&blocks);
        assert_eq!(shaped.len(), 1, "{shaped:?}");
        assert!(
            shaped[0][0].starts_with("[x 40] warning:"),
            "{:?}",
            shaped[0]
        );
    }

    #[test]
    fn three_distinct_errors_are_never_merged() {
        let blocks = vec![
            block(&["error[E0308]: mismatched types", "  --> a.rs:1:1"]),
            block(&["error[E0502]: cannot borrow", "  --> b.rs:2:2"]),
            block(&["error: linking failed", "  --> c.rs:3:3"]),
        ];
        let shaped = shaped_diagnostic_blocks(&blocks);
        assert_eq!(shaped.len(), 3, "{shaped:?}");
        for original in &blocks {
            assert!(shaped.contains(original), "{shaped:?}");
        }
    }

    #[test]
    fn a_distinct_failing_test_name_is_never_merged_with_another() {
        let blocks = vec![
            block(&["FAILED tests::alpha - AssertionError"]),
            block(&["FAILED tests::beta - AssertionError"]),
            block(&["FAILED tests::alpha - AssertionError"]),
        ];
        let shaped = shaped_diagnostic_blocks(&blocks);
        // alpha repeats twice (one counted group), beta stands alone: two
        // groups, never merged into one just because the message matches.
        assert_eq!(shaped.len(), 2, "{shaped:?}");
        assert!(
            shaped
                .iter()
                .any(|b| b[0].contains("[x 2]") && b[0].contains("tests::alpha")),
            "{shaped:?}"
        );
        assert!(
            shaped
                .iter()
                .any(|b| b[0] == "FAILED tests::beta - AssertionError"),
            "{shaped:?}"
        );
    }

    #[test]
    fn grouping_is_skipped_when_every_block_is_already_distinct() {
        let blocks = vec![
            block(&["warning: a"]),
            block(&["warning: b"]),
            block(&["warning: c"]),
        ];
        let shaped = shaped_diagnostic_blocks(&blocks);
        assert_eq!(shaped, blocks, "no merge happened, so nothing may change");
    }

    // -- #409a: normalized noise dedup -----------------------------------

    #[test]
    fn docker_style_layer_lines_collapse_to_one_counted_line() {
        let mut lines: Vec<String> = Vec::new();
        lines.push("Using default tag: latest".to_string());
        for i in 0..30 {
            lines.push(format!(
                "{:012x}: Pull complete",
                0xabc000000000u64 + i as u64
            ));
        }
        lines.push("Status: Downloaded newer image for alpine:latest".to_string());
        let shaped = shaped_noise_lines(&lines);
        assert!(
            shaped.len() < lines.len(),
            "30 layer lines must collapse: {shaped:?}"
        );
        assert!(shaped.iter().any(|l| l.starts_with("[x 30]")), "{shaped:?}");
    }

    #[test]
    fn a_form_seen_twice_is_left_alone() {
        let lines: Vec<String> = vec![
            "Compiling foo v0.1.0".to_string(),
            "Compiling foo v0.1.0".to_string(),
            "Compiling bar v0.2.0".to_string(),
        ];
        let shaped = shaped_noise_lines(&lines);
        assert_eq!(shaped, lines, "two occurrences is not a run of noise");
    }

    #[test]
    fn dedup_is_skipped_when_it_is_not_smaller() {
        let lines: Vec<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let shaped = shaped_noise_lines(&lines);
        assert_eq!(shaped, lines);
    }

    #[test]
    fn numbers_embedded_in_a_path_are_left_alone() {
        let a = normalize_noise_line("/build/v1.2024681/output.log");
        let b = normalize_noise_line("/build/v1.2024682/output.log");
        assert_ne!(
            a, b,
            "a number touching a path separator must not be masked away"
        );
    }

    // -- #411: binary safety ---------------------------------------------

    #[test]
    fn nul_bytes_are_detected_as_binary() {
        assert!(is_binary_content(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
    }

    #[test]
    fn mostly_replacement_chars_are_detected_as_binary() {
        let bytes: Vec<u8> = vec![0xff, 0xfe, 0xff, 0xfe, b'a'];
        assert!(is_binary_content(&bytes));
    }

    #[test]
    fn ordinary_text_is_not_binary() {
        assert!(!is_binary_content(b"cargo test\nrunning 3 tests\n"));
    }

    // -- #411: JSON structural summary ------------------------------------

    #[test]
    fn a_large_array_renders_as_a_bounded_structural_summary() {
        let items: Vec<Value> = (0..500)
            .map(|i| {
                serde_json::json!({
                    "id": i,
                    "name": format!("item-{i}"),
                    "description": "x".repeat(300),
                })
            })
            .collect();
        let value = Value::Array(items);
        let rendered = render_json(&value, 0);
        assert!(
            rendered.len() < 2000,
            "a bounded summary must not scale with the input: {} bytes",
            rendered.len()
        );
        assert!(rendered.contains("+495 more"), "{rendered}");
        assert!(is_bracket_balanced(&rendered), "{rendered}");
    }

    #[test]
    fn a_small_object_renders_untouched() {
        let value = serde_json::json!({"ok": true, "count": 3});
        let rendered = render_json(&value, 0);
        assert!(rendered.contains("\"ok\": true"), "{rendered}");
        assert!(rendered.contains("\"count\": 3"), "{rendered}");
    }

    #[test]
    fn every_rendered_json_summary_is_bracket_balanced() {
        for value in [
            serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
            serde_json::json!({"a": {"b": {"c": {"d": {"e": {"f": "too deep"}}}}}}),
            serde_json::json!({"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8,"k9":9}),
        ] {
            let full = render_json(&value, 0);
            let schema = render_json_schema(&value, 0);
            assert!(is_bracket_balanced(&full), "{full}");
            assert!(is_bracket_balanced(&schema), "{schema}");
        }
    }

    fn is_bracket_balanced(text: &str) -> bool {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for ch in text.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return false;
            }
        }
        depth == 0 && !in_string
    }

    #[test]
    fn non_json_text_is_never_parsed_as_json() {
        assert!(!looks_like_json(b"cargo test\nrunning 3 tests\n"));
        assert!(!looks_like_json(b"plain text without brackets"));
    }

    #[test]
    fn looks_like_json_recognises_both_bracket_shapes() {
        assert!(looks_like_json(b"  {\"a\": 1}  "));
        assert!(looks_like_json(b"[1, 2, 3]"));
        assert!(!looks_like_json(b"{unbalanced"));
    }
}
