//! Issue #408: shaping passes `output.rs`'s `render_summary` applies on top
//! of the head/tail/diagnostic scan ([`super::output::scan_for_display`])
//! before a summary is ever printed.
//!
//! Every pass here is subject to the same never-worse discipline as #410's
//! guard in `output.rs`: a pass that does not actually shrink what it is
//! shaping is skipped, and the caller renders the faithful, ungrouped form
//! instead. Everything here is a pure transformation over already-collected
//! blocks, kept separate from `output.rs`'s own I/O shell for the same
//! reason the rest of that module is split that way.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

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
}
