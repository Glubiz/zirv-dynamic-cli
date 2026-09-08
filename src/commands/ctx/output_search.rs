//! Issue #414: opt-in shape-aware compaction for search and listing output,
//! gated by `[output] compact_search` (default `false`, operator-only --
//! see `config::OutputConfig`'s own doc comment). Off, `rg`/`grep`/`find`/
//! `fd`/`ls`/`dir`/`tree` stay exactly `output::VERBATIM_PROGRAMS`'s existing
//! behaviour: never compacted at any size, because a model reads that output
//! verbatim before searching or listing again. On, those seven get
//! `output::CompactionScope::Shape` instead of `Verbatim`: a bounded, grouped
//! rendering built from each program's own known grammar (a match's
//! `path:line:text`, an `ls -l` row's fixed date anchor, a path list's own
//! directory structure), never a head/tail guess through text whose whole
//! point is that a model is about to search or list it again.
//!
//! Mirrors `output_diff.rs`'s own shape: a pure `scan_*`/`render_*` pair per
//! shape, with `output.rs`'s `summarize_stored` owning the `OutputRecord`/
//! sidecar/prune plumbing, the same as the `Diff` scope.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::output::{bare_program, display_line, retrieval_line};

/// The `VERBATIM_PROGRAMS` members `[output] compact_search` may pull into
/// `Shape` instead -- a small, named subset, never every reader: a raw file
/// dump (`cat`/`sed`) has no shape to group by, only a search/listing tool's
/// own structured grammar does.
pub(crate) const SEARCH_SHAPE_PROGRAMS: &[&str] =
    &["rg", "grep", "find", "fd", "ls", "dir", "tree"];

/// Lines shown per group before an overflow note takes over -- the same
/// per-signature bound `output_shape::MAX_BLOCK_LINES` gives one diagnostic
/// block, applied here per path/directory instead.
const PER_GROUP_CAP: usize = 8;
/// Total shown lines across every group, so a pathological single group (one
/// file with thousands of matches) cannot spend the whole summary budget on
/// its own before a later, smaller group is even reached.
const TOTAL_LINE_CAP: usize = 60;
/// How many `ls -l` rows are named individually before a single "more
/// entries" line takes over -- mirrors `output_diff::MAX_LISTED_FILES`'s
/// role for a pathological directory.
const MAX_LISTED_ENTRIES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeKind {
    /// `rg`/`grep`: `path:line:text` matches (and `path-line-text` context
    /// lines), grouped by path.
    Search,
    /// `ls -l`/`ls -la` (or `dir` with an equivalent long-format flag): one
    /// row per entry, parsed on its date anchor.
    Listing,
    /// `find`/`fd`/`tree`, and `ls`/`dir` with no long-format flag: a bare
    /// path/name list, grouped by top-level directory.
    Tree,
}

/// Which of the three shapes `command` calls for. Only meaningful once
/// `command` has already classified as `CompactionScope::Shape` --
/// `output::classify_compaction` is the single source of truth for THAT
/// decision; this only picks a rendering among the three once it has been
/// made.
pub(crate) fn detect_shape(command: &str) -> Option<ShapeKind> {
    for segment in super::safety::normalize_segments(command) {
        let collapsed = super::safety::collapse_whitespace(&segment);
        let tokens: Vec<&str> = collapsed.split(' ').filter(|t| !t.is_empty()).collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        match bare_program(first).as_str() {
            "rg" | "grep" => return Some(ShapeKind::Search),
            "ls" | "dir" => {
                return Some(if has_long_flag(&tokens) {
                    ShapeKind::Listing
                } else {
                    ShapeKind::Tree
                });
            }
            "find" | "fd" | "tree" => return Some(ShapeKind::Tree),
            _ => continue,
        }
    }
    None
}

/// `-l`/`-la`/`-al`/`-lh`/... (any short flag cluster containing `l`) or
/// `--long`.
fn has_long_flag(tokens: &[&str]) -> bool {
    tokens
        .iter()
        .any(|t| *t == "--long" || (t.starts_with('-') && !t.starts_with("--") && t.contains('l')))
}

fn header_line(command: &str, exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!(
            "zirv compacted output: exit {code} -- {}\n",
            display_line(command)
        ),
        None => format!("zirv compacted output: {}\n", display_line(command)),
    }
}

// ---------------------------------------------------------------------
// Search shape: rg/grep
// ---------------------------------------------------------------------

static MATCH_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?):(\d+):(.*)$").expect("regex"));
static CONTEXT_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)-(\d+)-(.*)$").expect("regex"));

#[derive(Debug, Default)]
pub(crate) struct SearchScan {
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: u64,
    order: Vec<String>,
    groups: HashMap<String, Vec<String>>,
    match_count: usize,
    pub(crate) read_error: bool,
}

/// Reads `reader` as `path:line:text` matches (ripgrep/grep's own shape) or
/// `path-line-text` context lines, grouped by path in first-seen order. Any
/// other line (a `--` group separator, a "binary file ... matches" notice)
/// is simply not counted -- this pass groups matches, it does not have to
/// account for every byte the way `output::scan_for_display` does.
pub(crate) fn scan_search(reader: impl BufRead) -> SearchScan {
    let mut scan = SearchScan::default();
    for chunk in reader.split(b'\n') {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(_) => {
                scan.read_error = true;
                break;
            }
        };
        let line = String::from_utf8_lossy(&bytes);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        scan.total_lines += 1;
        scan.total_bytes = scan.total_bytes.saturating_add(bytes.len() as u64 + 1);

        let Some(caps) = MATCH_LINE_RE
            .captures(line)
            .or_else(|| CONTEXT_LINE_RE.captures(line))
        else {
            continue;
        };
        let path = caps[1].to_string();
        let rendered = format!("{}:{}", &caps[2], display_line(&caps[3]));
        if !scan.groups.contains_key(&path) {
            scan.order.push(path.clone());
            scan.groups.insert(path.clone(), Vec::new());
        }
        scan.groups
            .get_mut(&path)
            .expect("just inserted")
            .push(rendered);
        scan.match_count += 1;
    }
    scan
}

pub(crate) fn scan_search_file(path: &Path) -> SearchScan {
    match std::fs::File::open(path) {
        Ok(file) => scan_search(std::io::BufReader::new(file)),
        Err(_) => SearchScan {
            read_error: true,
            ..SearchScan::default()
        },
    }
}

/// Renders the grouped, capped match listing, or `None` -- the same
/// fail-open discipline as every other scope-specific renderer -- when even
/// the mandatory header does not fit, or when the result does not beat the
/// raw byte count (issue #410's never-worse guard): a handful of matches
/// across many files can render LARGER once grouped (a `path:` header line
/// per file) than the raw, ungrouped text it replaces.
pub(crate) fn render_search_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    scan: &SearchScan,
    max_bytes: usize,
) -> Option<String> {
    let retrieval = retrieval_line(id);
    let budget = max_bytes.checked_sub(retrieval.len() + 1)?;

    let mut body = header_line(command, exit_code);
    body.push_str(&format!(
        "captured {} lines, {} bytes\n",
        scan.total_lines, scan.total_bytes
    ));
    if scan.read_error {
        body.push_str("note: the stored output ended in a read error; it may be truncated\n");
    }
    body.push_str(&format!(
        "{} matches in {} files\n",
        scan.match_count,
        scan.order.len()
    ));
    if body.len() > budget {
        return None;
    }

    let mut shown_files = 0usize;
    let mut shown_lines = 0usize;
    for path in &scan.order {
        let lines = &scan.groups[path];
        let cap = PER_GROUP_CAP.min(lines.len());
        if shown_lines + cap > TOTAL_LINE_CAP {
            break;
        }
        let mut group = format!("{}:\n", display_line(path));
        for line in &lines[..cap] {
            group.push_str("  ");
            group.push_str(line);
            group.push('\n');
        }
        if lines.len() > cap {
            group.push_str(&format!(
                "  ... +{} more in {}\n",
                lines.len() - cap,
                display_line(path)
            ));
        }
        if body.len() + group.len() > budget {
            break;
        }
        body.push_str(&group);
        shown_files += 1;
        shown_lines += cap;
    }
    let remaining_files = scan.order.len() - shown_files;
    if remaining_files > 0 {
        let more = format!("... +{remaining_files} more files\n");
        if body.len() + more.len() <= budget {
            body.push_str(&more);
        }
    }

    body.push_str(&retrieval);
    body.push('\n');
    if body.len() >= scan.total_bytes as usize {
        return None;
    }
    Some(body)
}

// ---------------------------------------------------------------------
// Listing shape: `ls -l`/`ls -la` (and `dir` with an equivalent flag)
// ---------------------------------------------------------------------

static LS_TOTAL_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^total\s+\d+$").expect("regex"));
/// Anchored on the date, not the owner/group columns: those vary in width
/// (a numeric uid, a long group name, an unresolvable owner) in a way the
/// month/day/time-or-year triple never does, so the non-greedy owner/group
/// capture just consumes whatever sits between the link count and the size.
static LS_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^\S+\s+\d+\s+.+?\s+
        (?P<size>\d+)\s+
        (?P<date>(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+
        (?:\d{1,2}:\d{2}|\d{4}))\s+
        (?P<name>.+)$
        ",
    )
    .expect("regex")
});

#[derive(Debug, Default)]
pub(crate) struct ListingScan {
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: u64,
    rows: Vec<(String, u64, String)>,
    parse_failed: bool,
    pub(crate) read_error: bool,
}

/// Parses each row on its date anchor. `total N` (the block-count line every
/// `ls -l` emits first) and blank lines are skipped, never counted as a
/// parse failure; anything else that does not match sets `parse_failed`,
/// which `render_listing_summary` treats as "this is not `ls -l` shaped
/// after all" -- fail open to the untouched original rather than a
/// half-parsed listing.
pub(crate) fn scan_listing(reader: impl BufRead) -> ListingScan {
    let mut scan = ListingScan::default();
    for chunk in reader.split(b'\n') {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(_) => {
                scan.read_error = true;
                break;
            }
        };
        let line = String::from_utf8_lossy(&bytes);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        scan.total_lines += 1;
        scan.total_bytes = scan.total_bytes.saturating_add(bytes.len() as u64 + 1);
        let trimmed = line.trim();
        if trimmed.is_empty() || LS_TOTAL_LINE_RE.is_match(trimmed) {
            continue;
        }
        match LS_ROW_RE.captures(line) {
            Some(caps) => {
                let size: u64 = caps["size"].parse().unwrap_or(0);
                scan.rows
                    .push((caps["name"].to_string(), size, caps["date"].to_string()));
            }
            None => scan.parse_failed = true,
        }
    }
    scan
}

pub(crate) fn scan_listing_file(path: &Path) -> ListingScan {
    match std::fs::File::open(path) {
        Ok(file) => scan_listing(std::io::BufReader::new(file)),
        Err(_) => ListingScan {
            read_error: true,
            ..ListingScan::default()
        },
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Renders `name  size(human)  date` per row, or `None` on ANY parse failure
/// (see [`scan_listing`]) or when the mandatory header/never-worse guard is
/// not met -- a listing this pass cannot fully account for is shown
/// untouched, never partially reshaped.
pub(crate) fn render_listing_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    scan: &ListingScan,
    max_bytes: usize,
) -> Option<String> {
    if scan.parse_failed || scan.rows.is_empty() {
        return None;
    }
    let retrieval = retrieval_line(id);
    let budget = max_bytes.checked_sub(retrieval.len() + 1)?;

    let mut body = header_line(command, exit_code);
    body.push_str(&format!(
        "captured {} lines, {} bytes\n",
        scan.total_lines, scan.total_bytes
    ));
    if scan.read_error {
        body.push_str("note: the stored output ended in a read error; it may be truncated\n");
    }
    body.push_str(&format!("{} entries\n", scan.rows.len()));
    if body.len() > budget {
        return None;
    }

    let capped = &scan.rows[..scan.rows.len().min(MAX_LISTED_ENTRIES)];
    let mut shown = 0usize;
    for (name, size, date) in capped {
        let row = format!("  {}  {}  {date}\n", display_line(name), human_size(*size));
        if body.len() + row.len() > budget {
            break;
        }
        body.push_str(&row);
        shown += 1;
    }
    let remaining = scan.rows.len() - shown;
    if remaining > 0 {
        let more = format!("  ... +{remaining} more entries\n");
        if body.len() + more.len() <= budget {
            body.push_str(&more);
        }
    }

    body.push_str(&retrieval);
    body.push('\n');
    if body.len() >= scan.total_bytes as usize {
        return None;
    }
    Some(body)
}

// ---------------------------------------------------------------------
// Directory-group shape: `find`/`fd`/`tree`, and `ls`/`dir` with no
// long-format flag
// ---------------------------------------------------------------------

/// The first path segment before a `/` or `\`, or the whole (trimmed) line
/// when it carries none -- `find`/`fd` print real relative paths, so this
/// groups by top-level directory as intended. `tree`'s own indented ASCII-art
/// output carries no path at all per line, so every one of its lines groups
/// under its own (indentation-stripped) name instead: a degraded but still
/// bounded grouping, never a crash or a silent mis-group.
fn top_level_key(line: &str) -> String {
    let stripped = line.trim_start_matches([
        ' ', '|', '\u{2502}', '\u{251c}', '\u{2514}', '\u{2500}', '-',
    ]);
    let normalized = stripped.trim_start_matches("./").replace('\\', "/");
    match normalized.split_once('/') {
        Some((first, _)) if !first.is_empty() => first.to_string(),
        _ => normalized,
    }
}

#[derive(Debug, Default)]
pub(crate) struct TreeScan {
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: u64,
    order: Vec<String>,
    groups: HashMap<String, Vec<String>>,
    total_paths: usize,
    pub(crate) read_error: bool,
}

pub(crate) fn scan_tree(reader: impl BufRead) -> TreeScan {
    let mut scan = TreeScan::default();
    for chunk in reader.split(b'\n') {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(_) => {
                scan.read_error = true;
                break;
            }
        };
        let line = String::from_utf8_lossy(&bytes);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        scan.total_lines += 1;
        scan.total_bytes = scan.total_bytes.saturating_add(bytes.len() as u64 + 1);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = top_level_key(trimmed);
        if key.is_empty() {
            continue;
        }
        if !scan.groups.contains_key(&key) {
            scan.order.push(key.clone());
            scan.groups.insert(key.clone(), Vec::new());
        }
        scan.groups
            .get_mut(&key)
            .expect("just inserted")
            .push(display_line(trimmed));
        scan.total_paths += 1;
    }
    scan
}

pub(crate) fn scan_tree_file(path: &Path) -> TreeScan {
    match std::fs::File::open(path) {
        Ok(file) => scan_tree(std::io::BufReader::new(file)),
        Err(_) => TreeScan {
            read_error: true,
            ..TreeScan::default()
        },
    }
}

pub(crate) fn render_tree_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    scan: &TreeScan,
    max_bytes: usize,
) -> Option<String> {
    let retrieval = retrieval_line(id);
    let budget = max_bytes.checked_sub(retrieval.len() + 1)?;

    let mut body = header_line(command, exit_code);
    body.push_str(&format!(
        "captured {} lines, {} bytes\n",
        scan.total_lines, scan.total_bytes
    ));
    if scan.read_error {
        body.push_str("note: the stored output ended in a read error; it may be truncated\n");
    }
    body.push_str(&format!(
        "{} paths in {} top-level dirs\n",
        scan.total_paths,
        scan.order.len()
    ));
    if body.len() > budget {
        return None;
    }

    let mut shown_dirs = 0usize;
    let mut shown_lines = 0usize;
    for key in &scan.order {
        let lines = &scan.groups[key];
        let cap = PER_GROUP_CAP.min(lines.len());
        if shown_lines + cap > TOTAL_LINE_CAP {
            break;
        }
        let mut group = format!("{key}/:\n");
        for line in &lines[..cap] {
            group.push_str("  ");
            group.push_str(line);
            group.push('\n');
        }
        if lines.len() > cap {
            group.push_str(&format!("  ... +{} more in {key}\n", lines.len() - cap));
        }
        if body.len() + group.len() > budget {
            break;
        }
        body.push_str(&group);
        shown_dirs += 1;
        shown_lines += cap;
    }
    let remaining = scan.order.len() - shown_dirs;
    if remaining > 0 {
        let more = format!("... +{remaining} more dirs\n");
        if body.len() + more.len() <= budget {
            body.push_str(&more);
        }
    }

    body.push_str(&retrieval);
    body.push('\n');
    if body.len() >= scan.total_bytes as usize {
        return None;
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_shape_routes_each_program_correctly() {
        assert_eq!(detect_shape("rg TODO src"), Some(ShapeKind::Search));
        assert_eq!(detect_shape("grep -r TODO src"), Some(ShapeKind::Search));
        assert_eq!(detect_shape("ls -la /tmp"), Some(ShapeKind::Listing));
        assert_eq!(detect_shape("ls /tmp"), Some(ShapeKind::Tree));
        assert_eq!(detect_shape("find . -name '*.rs'"), Some(ShapeKind::Tree));
        assert_eq!(detect_shape("fd .rs"), Some(ShapeKind::Tree));
        assert_eq!(detect_shape("tree src"), Some(ShapeKind::Tree));
        assert_eq!(detect_shape("cargo build"), None);
    }

    fn rg_output(files: usize, matches_per_file: usize) -> String {
        let mut text = String::new();
        for f in 0..files {
            for m in 0..matches_per_file {
                text.push_str(&format!(
                    "src/file{f}.rs:{}:    let todo_{m} = 1; // TODO fix this\n",
                    m + 1
                ));
            }
        }
        text
    }

    #[test]
    fn scan_search_groups_matches_by_path() {
        let text = rg_output(3, 5);
        let scan = scan_search(text.as_bytes());
        assert_eq!(scan.order.len(), 3, "{:?}", scan.order);
        assert_eq!(scan.match_count, 15);
        assert_eq!(scan.groups[&scan.order[0]].len(), 5);
    }

    #[test]
    fn a_large_rg_result_renders_grouped_and_under_budget() {
        let text = rg_output(20, 20);
        assert!(text.len() > 4096, "{}", text.len());
        let scan = scan_search(text.as_bytes());
        let summary = render_search_summary("id1", "rg TODO src", Some(0), &scan, 4096)
            .expect("a grouped summary");
        assert!(summary.len() <= 4096, "{} bytes", summary.len());
        assert!(summary.contains("matches in"), "{summary}");
        assert!(summary.len() < text.len(), "{summary}");
    }

    /// Issue #414 acceptance: a small, already-scattered result (30 matches
    /// across 30 distinct single-line files) does not shrink once grouped --
    /// each group's own header line costs as much as the match line it
    /// groups -- so the never-worse guard must refuse the summary.
    #[test]
    fn a_thirty_match_result_stays_verbatim_because_grouping_is_not_smaller() {
        let text = rg_output(30, 1);
        let scan = scan_search(text.as_bytes());
        assert_eq!(scan.order.len(), 30);
        let summary = render_search_summary("id1", "rg TODO src", Some(0), &scan, 4096);
        assert!(
            summary.is_none(),
            "grouping 30 single-line matches must not beat the raw text: {summary:?}"
        );
    }

    fn ls_l_output() -> String {
        let mut text = String::from("total 24\n");
        for i in 0..10 {
            text.push_str(&format!(
                "-rw-r--r--  1 user  staff  {} Jan  {} 12:34 file{i}.rs\n",
                1000 + i,
                i + 1
            ));
        }
        text
    }

    #[test]
    fn ls_l_rows_parse_and_render_name_size_date() {
        let text = ls_l_output();
        let scan = scan_listing(text.as_bytes());
        assert!(!scan.parse_failed, "{scan:?}");
        assert_eq!(scan.rows.len(), 10);
        let summary =
            render_listing_summary("id1", "ls -l", Some(0), &scan, 4096).expect("a summary");
        assert!(summary.contains("file0.rs"), "{summary}");
        assert!(summary.contains("Jan"), "{summary}");
    }

    #[test]
    fn ls_l_parse_failure_falls_back_verbatim() {
        let text = "this is not an ls -l row at all\njust plain text\n";
        let scan = scan_listing(text.as_bytes());
        assert!(scan.parse_failed);
        let summary = render_listing_summary("id1", "ls -l", Some(0), &scan, 4096);
        assert!(
            summary.is_none(),
            "an unparseable ls -l body must fail open to verbatim: {summary:?}"
        );
    }

    #[test]
    fn human_size_formats_common_magnitudes() {
        assert_eq!(human_size(500), "500B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    #[test]
    fn scan_tree_groups_paths_by_top_level_directory() {
        let text = "src/main.rs\nsrc/lib.rs\ntests/it.rs\nREADME.md\n";
        let scan = scan_tree(text.as_bytes());
        assert_eq!(scan.total_paths, 4);
        assert!(scan.order.contains(&"src".to_string()), "{:?}", scan.order);
        assert!(
            scan.order.contains(&"tests".to_string()),
            "{:?}",
            scan.order
        );
        assert_eq!(scan.groups["src"].len(), 2);
    }

    #[test]
    fn a_large_find_result_renders_grouped_and_under_budget() {
        let mut text = String::new();
        for d in 0..25 {
            for f in 0..25 {
                text.push_str(&format!("dir{d}/file{f}.rs\n"));
            }
        }
        assert!(text.len() > 4096, "{}", text.len());
        let scan = scan_tree(text.as_bytes());
        let summary = render_tree_summary("id1", "find . -name *.rs", Some(0), &scan, 4096)
            .expect("a summary");
        assert!(summary.len() <= 4096, "{} bytes", summary.len());
        assert!(summary.contains("top-level dirs"), "{summary}");
        assert!(summary.len() < text.len(), "{summary}");
    }
}
