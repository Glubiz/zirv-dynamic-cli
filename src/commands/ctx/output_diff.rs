//! Issue #412: a bounded, lossless-shape summary for `git diff`/`show`/
//! `log -p`/`format-patch` (`super::output::CompactionScope::Diff`).
//!
//! `output::render_summary`'s head/tail is exactly wrong here: the omitted
//! middle of a diff is the part a model is about to edit against, so a
//! partial hunk would silently corrupt the edit rather than merely cost
//! tokens (the same reasoning that keeps `cat`/`sed`/`grep` fully
//! `Verbatim`). What makes a diff different from those readers is that its
//! grammar is known -- `diff --git`, `@@`, leading `+`/`-` -- so a per-file
//! `path +N -M` listing captures the shape of the change (which files, how
//! much) without ever showing a hunk, partial or otherwise. `[output]
//! max_summary_bytes` still bounds the rendered listing itself, the same cap
//! every other compaction shape respects.

use std::io::BufRead;
use std::path::Path;

use crate::commands::workflow::verification::scrub_output;

use super::output::retrieval_line;

/// How many changed files the listing names individually before falling back
/// to a single "... +N more files" line. Mirrors `output::MAX_FAILURE_
/// BLOCKS`'s role: a pathological changeset (a vendored dependency bump)
/// must not spend the whole summary budget on file names alone.
const MAX_LISTED_FILES: usize = 200;

/// Per-line display cap, the same reasoning as `output::MAX_LINE_BYTES`: one
/// absurd path must not dominate the listing.
const MAX_PATH_BYTES: usize = 200;

/// One changed file's shape: its path and how many `+`/`-` lines its hunks
/// carried. Never the hunk lines themselves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DiffFileStat {
    pub(crate) path: String,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

/// What the diff scan recovered: enough to render a per-file listing without
/// ever holding a hunk body.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DiffScan {
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: u64,
    pub(crate) files: Vec<DiffFileStat>,
    /// The stored file could not be read to the end -- see `output::
    /// DisplayScan::read_error`'s own doc comment for why this is never
    /// silently folded into a clean scan.
    pub(crate) read_error: bool,
}

/// The path named on a `diff --git a/<path> b/<path>` line. Renames aside,
/// both sides name the same file, so the `a/`-prefixed side is enough; a
/// `+++`/`---` line refines it when one is present (deletions, renames), and
/// wins over this fallback since it is the more authoritative of the two.
///
/// Not robust against a path that itself contains the literal substring
/// `" b/"` (git would quote such a path; unhandled here, the same scope limit
/// as every other simplification in this module -- the listing is a shape,
/// not a byte-exact parser).
fn path_from_diff_git_line(rest: &str) -> String {
    if let Some(after_a) = rest.strip_prefix("a/")
        && let Some(idx) = after_a.find(" b/")
    {
        return after_a[..idx].to_string();
    }
    rest.to_string()
}

/// The path named on a `+++ b/<path>` or `--- a/<path>` line, stripping the
/// `a/`/`b/` prefix `git diff` always adds. `/dev/null` (a new or deleted
/// file's missing side) is not a path at all.
fn path_from_side_line(rest: &str, prefix: &str) -> Option<String> {
    let rest = rest.trim();
    if rest.is_empty() || rest == "/dev/null" {
        return None;
    }
    Some(rest.strip_prefix(prefix).unwrap_or(rest).to_string())
}

/// Reads `reader` as BYTE lines, never `BufRead::lines`, for the identical
/// reason `output::scan_for_display` does: one non-UTF-8 byte must not end
/// the scan early. Recognizes the unified-diff grammar structurally --
/// `diff --git` opens a file's own section, `@@` opens a hunk within it, and
/// only lines strictly inside a hunk are counted as `+`/`-` content -- so
/// diffstat/mbox furniture around a `git format-patch` section (a `-- `
/// signature line, a version footer) can never be mistaken for a removed
/// line: those appear between hunks, not inside one.
pub(crate) fn scan_diff(reader: impl BufRead) -> DiffScan {
    let mut scan = DiffScan::default();
    let mut current: Option<DiffFileStat> = None;
    let mut in_hunk = false;

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

        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                scan.files.push(file);
            }
            current = Some(DiffFileStat {
                path: path_from_diff_git_line(rest),
                added: 0,
                removed: 0,
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = current.as_mut() else {
            // Preamble before the first file: `git log`'s commit headers, or
            // `git format-patch`'s `From `/`Subject:`/diffstat lines.
            continue;
        };
        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(path) = path_from_side_line(rest, "b/") {
                file.path = path;
            }
            in_hunk = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if file.path.is_empty()
                && let Some(path) = path_from_side_line(rest, "a/")
            {
                file.path = path;
            }
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if line.is_empty() {
            // A genuinely empty line never occurs inside a hunk (every hunk
            // line carries a leading ' '/'+'/'-' marker, so a blank source
            // line still prints as a single space) -- only between sections,
            // so this is always a hunk boundary, never content.
            in_hunk = false;
            continue;
        }
        if line.trim_end() == "--" {
            // `git format-patch`'s mbox signature delimiter, exactly two
            // dashes with no other content. Never confused with a removed
            // diff line: a genuine hunk line for a source line that itself
            // starts with `--` (a Lua comment, say) carries the `-` MARKER
            // plus that content, so it is at least three dashes wide.
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            // `index`/mode/similarity/rename/"Binary files ... differ"
            // lines: shape furniture, never content.
            continue;
        }
        if line.starts_with('+') {
            file.added += 1;
        } else if line.starts_with('-') {
            file.removed += 1;
        }
    }
    if let Some(file) = current.take() {
        scan.files.push(file);
    }
    scan
}

/// [`scan_diff`] over a stored file, `read_error` set on an unreadable path
/// rather than a silent empty scan.
pub(crate) fn scan_diff_file(path: &Path) -> DiffScan {
    match std::fs::File::open(path) {
        Ok(file) => scan_diff(std::io::BufReader::new(file)),
        Err(_) => DiffScan {
            read_error: true,
            ..DiffScan::default()
        },
    }
}

/// Control characters scrubbed and capped, the same treatment `output::
/// display_line` gives every other line this codebase relays to a terminal
/// or a model.
fn display_path(raw: &str) -> String {
    let scrubbed = scrub_output(raw).replace(['\n', '\t'], " ");
    let trimmed = scrubbed.trim_end();
    if trimmed.len() <= MAX_PATH_BYTES {
        return trimmed.to_string();
    }
    let mut cut = MAX_PATH_BYTES;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} [...]", &trimmed[..cut])
}

/// Renders the bounded per-file listing, or `None` when even the mandatory
/// header does not fit inside `max_bytes` alongside the retrieval line --
/// the same fail-open discipline as `output::render_summary`: a summary that
/// silently dropped its own totals is not compression, it is corruption.
///
/// Never emits a `@@` hunk header or a hunk body line: [`scan_diff`] never
/// even retains one, so there is nothing here that could leak one.
pub(crate) fn render_diff_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    scan: &DiffScan,
    max_bytes: usize,
) -> Option<String> {
    let retrieval = retrieval_line(id);
    let budget = max_bytes.checked_sub(retrieval.len() + 1)?;

    let mut body = String::new();
    body.push_str(&match exit_code {
        Some(code) => format!(
            "zirv compacted diff: exit {code} -- {}\n",
            display_path(command)
        ),
        None => format!("zirv compacted diff: {}\n", display_path(command)),
    });
    body.push_str(&format!(
        "captured {} lines, {} bytes\n",
        scan.total_lines, scan.total_bytes
    ));
    if scan.read_error {
        body.push_str("note: the stored diff ended in a read error; it may be truncated\n");
    }
    body.push_str(
        "diff exceeded [output] diff_max_bytes -- showing a per-file listing, never a partial \
         hunk\n",
    );

    let total_added: usize = scan.files.iter().map(|f| f.added).sum();
    let total_removed: usize = scan.files.iter().map(|f| f.removed).sum();
    body.push_str(&format!(
        "totals: {} files changed, +{total_added} -{total_removed}\n",
        scan.files.len()
    ));

    // Everything above is mandatory. If it does not fit, there is no honest
    // summary to emit at all.
    if body.len() > budget {
        return None;
    }

    // Up to `MAX_LISTED_FILES` file lines, capped again by the byte budget.
    // Built with room reserved for the trailing "more files" line whenever
    // one turns out to be needed -- a naive greedy fill can spend the very
    // last bytes of budget on a file line and leave no room to say how many
    // more there were, which would silently under-report the change.
    let capped: Vec<&DiffFileStat> = scan.files.iter().take(MAX_LISTED_FILES).collect();
    let file_line = |file: &DiffFileStat| -> String {
        format!(
            "  {} +{} -{}\n",
            display_path(&file.path),
            file.added,
            file.removed
        )
    };
    let all_capped_lines: String = capped.iter().map(|file| file_line(file)).collect();
    if scan.files.len() <= capped.len() && body.len() + all_capped_lines.len() <= budget {
        // Every changed file fits: no truncation, no "more files" line.
        body.push_str(&all_capped_lines);
    } else {
        let mut shown = capped.len();
        loop {
            let omitted = scan.files.len() - shown;
            let more = format!("  ... +{omitted} more files\n");
            let listing: String = capped[..shown].iter().map(|file| file_line(file)).collect();
            if body.len() + listing.len() + more.len() <= budget {
                body.push_str(&listing);
                body.push_str(&more);
                break;
            }
            if shown == 0 {
                // Not even the "more" line fits; the totals line above
                // already carries the file count, so the summary stays
                // honest without it rather than overrun `max_bytes`.
                break;
            }
            shown -= 1;
        }
    }

    body.push_str(&retrieval);
    body.push('\n');
    debug_assert!(body.len() <= max_bytes);
    // Review finding F2: same never-worse guard as `output::render_summary`
    // (#410) and `output_shape::render_json_summary` -- a repo-lowered
    // `diff_max_bytes` can make the per-file listing itself larger than the
    // raw diff it replaces (e.g. a single small hunk vs. a multi-line
    // listing plus totals plus the retrieval line). A summary that grew is
    // not compression.
    if body.len() >= scan.total_bytes as usize {
        return None;
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal two-file diff: the per-file counts and the totals must be
    /// exact, and the listing must never carry a hunk header or body line.
    #[test]
    fn scan_diff_counts_added_and_removed_lines_per_file() {
        let text = "diff --git a/src/a.rs b/src/a.rs\n\
                     index 111..222 100644\n\
                     --- a/src/a.rs\n\
                     +++ b/src/a.rs\n\
                     @@ -1,3 +1,3 @@\n\
                     -let x = 1;\n\
                     +let x = 2;\n\
                      let y = 3;\n\
                     diff --git a/src/b.rs b/src/b.rs\n\
                     index 333..444 100644\n\
                     --- a/src/b.rs\n\
                     +++ b/src/b.rs\n\
                     @@ -5,2 +5,4 @@\n\
                     +fn extra() {}\n\
                     +fn another() {}\n\
                      fn kept() {}\n";
        let scan = scan_diff(text.as_bytes());
        assert_eq!(scan.files.len(), 2, "{:?}", scan.files);
        assert_eq!(scan.files[0].path, "src/a.rs");
        assert_eq!(scan.files[0].added, 1);
        assert_eq!(scan.files[0].removed, 1);
        assert_eq!(scan.files[1].path, "src/b.rs");
        assert_eq!(scan.files[1].added, 2);
        assert_eq!(scan.files[1].removed, 0);
    }

    /// A deleted file has no `+++ b/...` line (`/dev/null` instead), so the
    /// path must come from `--- a/...` -- or, failing that, the `diff --git`
    /// line's own fallback.
    #[test]
    fn scan_diff_finds_the_path_of_a_deleted_file() {
        let text = "diff --git a/src/gone.rs b/src/gone.rs\n\
                     deleted file mode 100644\n\
                     index 111..000\n\
                     --- a/src/gone.rs\n\
                     +++ /dev/null\n\
                     @@ -1,2 +0,0 @@\n\
                     -fn gone() {}\n\
                     -fn also_gone() {}\n";
        let scan = scan_diff(text.as_bytes());
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].path, "src/gone.rs");
        assert_eq!(scan.files[0].added, 0);
        assert_eq!(scan.files[0].removed, 2);
    }

    /// Binary files carry no `+++`/`---` pair at all -- the path must still
    /// come from the `diff --git` line, and no lines are miscounted.
    #[test]
    fn scan_diff_handles_a_binary_file_with_no_hunk() {
        let text = "diff --git a/assets/logo.png b/assets/logo.png\n\
                     index aaa..bbb 100644\n\
                     Binary files a/assets/logo.png and b/assets/logo.png differ\n";
        let scan = scan_diff(text.as_bytes());
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].path, "assets/logo.png");
        assert_eq!(scan.files[0].added, 0);
        assert_eq!(scan.files[0].removed, 0);
    }

    /// Review-shaped hazard: a `git format-patch` file carries mbox furniture
    /// (`From `, `Subject:`, a diffstat, a `-- ` signature, a version
    /// footer) around its actual diff. None of that may be mistaken for a
    /// removed line, since only lines structurally INSIDE a hunk are counted.
    #[test]
    fn scan_diff_ignores_format_patch_furniture_around_the_diff() {
        let text = "From abc123 Mon Sep 17 00:00:00 2001\n\
                     From: A U Thor <a@example.com>\n\
                     Subject: [PATCH] do the thing\n\
                     \n\
                     src/a.rs | 2 +-\n\
                     1 file changed, 1 insertion(+), 1 deletion(-)\n\
                     \n\
                     diff --git a/src/a.rs b/src/a.rs\n\
                     index 111..222 100644\n\
                     --- a/src/a.rs\n\
                     +++ b/src/a.rs\n\
                     @@ -1 +1 @@\n\
                     -let x = 1;\n\
                     +let x = 2;\n\
                     -- \n\
                     2.43.0\n";
        let scan = scan_diff(text.as_bytes());
        assert_eq!(scan.files.len(), 1, "{:?}", scan.files);
        assert_eq!(scan.files[0].added, 1);
        assert_eq!(
            scan.files[0].removed, 1,
            "the trailing `-- ` signature must never be counted as a removed line"
        );
    }

    /// A diff whose only changes are line endings (CRLF vs LF) must not be
    /// reported as empty: every changed line still starts with `+`/`-`
    /// regardless of what its trailing `\r` does, so the count is exact.
    #[test]
    fn scan_diff_is_not_empty_for_a_crlf_vs_lf_only_change() {
        let mut text = String::from(
            "diff --git a/file.txt b/file.txt\n\
             index 111..222 100644\n\
             --- a/file.txt\n\
             +++ b/file.txt\n\
             @@ -1,2 +1,2 @@\n",
        );
        // Review finding F2's never-worse guard means `render_diff_summary`
        // must beat the raw byte count -- padding with unchanged context
        // lines (never counted as added/removed) keeps the diff itself tiny
        // while giving the rendered listing (header, totals, retrieval line)
        // room to still come out smaller than the raw capture.
        for i in 0..200 {
            text.push_str(&format!(" context filler line {i}\n"));
        }
        text.push_str(
            "-line one\n\
             -line two\n\
             +line one\r\n\
             +line two\r\n",
        );
        let scan = scan_diff(text.as_bytes());
        assert_eq!(scan.files.len(), 1);
        assert_eq!(
            scan.files[0].removed, 2,
            "a CRLF-only change must still count every removed line"
        );
        assert_eq!(
            scan.files[0].added, 2,
            "a CRLF-only change must still count every added line"
        );

        let summary = render_diff_summary("id1", "git diff", None, &scan, 4096)
            .expect("a summary for a non-empty diff");
        assert!(
            summary.contains("+2 -2"),
            "a CRLF-only diff must not render as empty: {summary}"
        );
    }

    /// A 20 KB diff comfortably below any realistic `diff_max_bytes` --
    /// `hook::run_posttool` itself decides whether to compact at all (see
    /// its own tests); this only pins that a small, ordinary diff renders a
    /// correct, budget-respecting listing when asked to.
    #[test]
    fn a_small_ordinary_diff_renders_a_correct_listing() {
        let mut text = String::new();
        for i in 0..20 {
            text.push_str(&format!("diff --git a/src/file{i}.rs b/src/file{i}.rs\n"));
            text.push_str("index 111..222 100644\n");
            text.push_str(&format!("--- a/src/file{i}.rs\n"));
            text.push_str(&format!("+++ b/src/file{i}.rs\n"));
            text.push_str("@@ -1,3 +1,3 @@\n");
            text.push_str(" context line\n");
            text.push_str("-let x = 1;\n");
            text.push_str("+let x = 2;\n");
        }
        assert!(text.len() < 20 * 1024, "fixture must be a small diff");
        let scan = scan_diff(text.as_bytes());
        let summary = render_diff_summary("id1", "git diff main...HEAD", Some(0), &scan, 4096)
            .expect("a summary");
        assert!(!summary.contains("@@"), "{summary}");
        assert!(
            summary.contains("totals: 20 files changed, +20 -20"),
            "{summary}"
        );
        assert!(summary.len() <= 4096, "{} bytes", summary.len());
    }

    /// Acceptance criterion: a 1 MB diff renders as a per-file listing under
    /// `max_summary_bytes`, capped at 200 named files plus a "more files"
    /// line, and never contains a `@@` hunk header.
    #[test]
    fn a_one_megabyte_diff_renders_under_the_summary_budget() {
        let mut text = String::new();
        let mut file_index = 0usize;
        while text.len() < 1024 * 1024 {
            text.push_str(&format!(
                "diff --git a/src/generated/file{file_index}.rs b/src/generated/file{file_index}.rs\n"
            ));
            text.push_str("index 111..222 100644\n");
            text.push_str(&format!("--- a/src/generated/file{file_index}.rs\n"));
            text.push_str(&format!("+++ b/src/generated/file{file_index}.rs\n"));
            text.push_str("@@ -1,50 +1,50 @@\n");
            for line in 0..50 {
                text.push_str(&format!(
                    "-old line {line} of a fairly long generated body\n"
                ));
                text.push_str(&format!(
                    "+new line {line} of a fairly long generated body\n"
                ));
            }
            file_index += 1;
        }
        assert!(text.len() >= 1024 * 1024, "fixture must reach 1 MB");
        assert!(
            file_index > MAX_LISTED_FILES,
            "the fixture must exceed the per-file listing cap"
        );

        let scan = scan_diff(text.as_bytes());
        assert_eq!(scan.files.len(), file_index);

        let summary = render_diff_summary("bigid", "git diff main...HEAD", Some(0), &scan, 4096)
            .expect("a summary for a 1 MB diff");
        assert!(
            summary.len() <= 4096,
            "the summary must respect max_summary_bytes: {} bytes",
            summary.len()
        );
        assert!(!summary.contains("@@"), "{summary}");
        assert!(
            summary.contains("more files"),
            "the listing must name how many files it could not list: {summary}"
        );
        assert!(
            summary.contains("full output: zirv ctx output show"),
            "{summary}"
        );
    }

    /// No fixture in this module ever yields a `@@` in its rendered summary
    /// -- pinned once, across every scan built above, rather than repeated
    /// per test.
    #[test]
    fn no_summary_in_this_module_ever_contains_a_hunk_header() {
        // Review finding F2's never-worse guard means each fixture needs
        // enough raw bytes (padded with content that never changes
        // `added`/`removed`: unchanged hunk context lines here, ordinary
        // furniture lines for the binary fixture) for the rendered listing
        // to still come out smaller than what it replaces.
        let mut two_files = String::from("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n");
        for i in 0..200 {
            two_files.push_str(&format!(" context filler line {i}\n"));
        }
        two_files.push_str("-x\n+y\n");

        let mut binary = String::from("diff --git a/x.png b/x.png\n");
        for i in 0..200 {
            binary.push_str(&format!("furniture filler line {i}\n"));
        }
        binary.push_str("Binary files a/x.png and b/x.png differ\n");

        let fixtures: Vec<(&str, DiffScan)> = vec![
            ("two files", scan_diff(two_files.as_bytes())),
            ("binary", scan_diff(binary.as_bytes())),
        ];
        for (name, scan) in fixtures {
            let summary = render_diff_summary("id", "git diff", None, &scan, 4096)
                .unwrap_or_else(|| panic!("{name}: expected a summary"));
            assert!(!summary.contains("@@"), "{name}: {summary}");
        }
    }

    /// The retrieval line survives even when the mandatory header alone
    /// leaves no room for a single file line -- the same fail-open contract
    /// as `output::render_summary`.
    #[test]
    fn render_diff_summary_fails_open_when_the_header_does_not_fit() {
        let scan = DiffScan {
            total_lines: 5,
            total_bytes: 500,
            files: vec![DiffFileStat {
                path: "src/a.rs".to_string(),
                added: 1,
                removed: 1,
            }],
            read_error: false,
        };
        assert_eq!(
            render_diff_summary("id1", "git diff", Some(0), &scan, 40),
            None,
            "a budget too small for the mandatory header must yield no summary"
        );
    }
}
