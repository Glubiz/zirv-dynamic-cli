//! Issue #416: strips PR/issue/release template boilerplate from `gh`/`glab`
//! `view` output before compaction sees it, so the summary keeps every real
//! paragraph of a PR/issue body without also carrying an HTML comment aimed
//! at the person filling out the template, an unchecked checklist section
//! that carries no other content, or the collapsed body of a `<details>`
//! block a submitter tucked debug output into.
//!
//! `gh pr view --json ...`/`glab ... --json` is untouched: that output is
//! JSON, not markdown, and already routes through `output_shape::
//! try_json_summary` (see `output.rs::summarize_stored`) -- this pass only
//! fires for the plain-text `view` rendering, never a `--json` invocation.
//! Reference behaviour: rtk's own `gh_cmd.rs::filter_markdown_body`.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::output::{bare_program, display_line, retrieval_line};

static HTML_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("regex"));
static DETAILS_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<details[^>]*>(.*?)</details>").expect("regex"));
static SUMMARY_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<summary[^>]*>.*?</summary>").expect("regex"));

/// Whether `command` is a `gh`/`glab` `pr`/`issue`/`release`/`mr` `view`
/// invocation WITHOUT `--json` -- the exact shape this module's stripping
/// applies to. `mr` is glab's merge-request subcommand (gh has no `mr`, so
/// accepting it there is harmless). A `--json` invocation's output is JSON,
/// not a template body, and already has its own summarizer.
pub(crate) fn is_gh_template_view(command: &str) -> bool {
    for segment in super::safety::normalize_segments(command) {
        let collapsed = super::safety::collapse_whitespace(&segment);
        let tokens: Vec<&str> = collapsed.split(' ').filter(|t| !t.is_empty()).collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        let program = bare_program(first);
        if program != "gh" && program != "glab" {
            continue;
        }
        let sub = tokens.get(1).map(|t| t.to_ascii_lowercase());
        let verb = tokens.get(2).map(|t| t.to_ascii_lowercase());
        let is_view = matches!(
            sub.as_deref(),
            Some("pr") | Some("issue") | Some("release") | Some("mr")
        ) && verb.as_deref() == Some("view");
        let has_json = tokens.iter().any(|t| {
            t.eq_ignore_ascii_case("--json") || t.to_ascii_lowercase().starts_with("--json=")
        });
        if is_view && !has_json {
            return true;
        }
    }
    false
}

fn is_heading(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

fn is_checklist_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed
        .strip_prefix('-')
        .or_else(|| trimmed.strip_prefix('*'))
    else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with("[ ]") || rest.starts_with("[x]") || rest.starts_with("[X]")
}

fn strip_html_comments(text: &str) -> String {
    HTML_COMMENT_RE.replace_all(text, "").into_owned()
}

/// Collapses every `<details>...</details>` block to just its `<summary>`
/// line (or nothing, when it carries none) -- the body is where a submitter
/// tucks debug dumps and internal notes nobody reading the compacted summary
/// needs.
fn strip_details_bodies(text: &str) -> String {
    DETAILS_BLOCK_RE
        .replace_all(text, |caps: &Captures<'_>| {
            let inner = &caps[1];
            SUMMARY_LINE_RE
                .find(inner)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default()
        })
        .into_owned()
}

/// Drops every heading section whose only non-empty content is a `- [ ]`/
/// `- [x]` checklist -- a template's own instructions, never a real answer a
/// submitter wrote. A section with ANY other non-empty line (prose, or a
/// checklist mixed with commentary) is kept in full, checklist lines
/// included: only a section that is ENTIRELY checklist boilerplate is
/// dropped.
fn strip_empty_checklist_sections(text: &str) -> String {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if is_heading(line) && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let mut out: Vec<&str> = Vec::new();
    for block in blocks {
        let has_heading = block.first().is_some_and(|l| is_heading(l));
        let body = if has_heading { &block[1..] } else { &block[..] };
        let non_empty: Vec<&str> = body
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let all_checklist = !non_empty.is_empty() && non_empty.iter().all(|l| is_checklist_line(l));
        if has_heading && all_checklist {
            continue;
        }
        out.extend(block);
    }
    collapse_blank_runs(&out)
}

/// Never more than one consecutive blank line in the output -- stripping a
/// whole section (or a comment) otherwise leaves a visible gap where it used
/// to be.
fn collapse_blank_runs(lines: &[&str]) -> String {
    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The pre-pass itself (issue #416): HTML comments gone, `<details>` bodies
/// collapsed to their `<summary>` line, and any heading section that is
/// nothing but a template checklist dropped whole. Every other non-empty
/// paragraph survives verbatim -- this never reorders or rewrites a line it
/// keeps.
pub(crate) fn filter_markdown_body(text: &str) -> String {
    let text = strip_html_comments(text);
    let text = strip_details_bodies(&text);
    strip_empty_checklist_sections(&text)
}

/// Renders the filtered body as the summary, bounded by `max_bytes`. Unlike
/// `output::render_summary`'s head/tail, the WHOLE filtered document is kept
/// when it fits: the point of this pass is that the boilerplate is already
/// gone, so truncating what remains would cut real content a diagnostic scan
/// has no way to tell apart from noise. `None` -- the same fail-open
/// discipline as every other scope-specific renderer -- when even the header
/// does not fit, when the filtered body itself is still too large to show
/// whole (the caller's ordinary scan applies instead), or when the result
/// does not beat the raw byte count.
pub(crate) fn render_markdown_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    filtered: &str,
    total_lines: usize,
    raw_bytes: usize,
    max_bytes: usize,
) -> Option<String> {
    let retrieval = retrieval_line(id);
    let budget = max_bytes.checked_sub(retrieval.len() + 1)?;

    let header = match exit_code {
        Some(code) => format!(
            "zirv compacted output: exit {code} -- {}\n",
            display_line(command)
        ),
        None => format!("zirv compacted output: {}\n", display_line(command)),
    };
    let counts = format!("captured {total_lines} lines, {raw_bytes} bytes\n");
    let note = "stripped template boilerplate (comments, empty checklist sections, collapsed \
                <details> blocks)\n";
    let prefix_len = header.len() + counts.len() + note.len();
    if prefix_len > budget {
        return None;
    }
    let body_budget = budget - prefix_len;
    if filtered.len() > body_budget {
        // The trimmed body is still too big to show whole -- the whole point
        // of this pass is showing every real paragraph, so a partial cut
        // here would silently drop content a diagnostic scan cannot tell
        // apart from noise. Fail open: the caller's ordinary scan applies.
        return None;
    }

    let mut out = header;
    out.push_str(&counts);
    out.push_str(note);
    out.push_str(filtered);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&retrieval);
    out.push('\n');
    // Issue #410's never-worse guard: a summary that grew is not compression.
    if out.len() >= raw_bytes {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_template_fixture() -> String {
        let mut debug_dump = String::new();
        for i in 0..300 {
            debug_dump.push_str(&format!("internal debug line {i} nobody needs to read\n"));
        }
        format!(
            "<!-- Thanks for contributing! Please fill out this template. -->\n\
             \n\
             ## Description\n\
             \n\
             Fixes the frobnicator to handle the edge case where the widget is null.\n\
             \n\
             ## Type of change\n\
             \n\
             - [ ] Bug fix\n\
             - [ ] New feature\n\
             - [ ] Breaking change\n\
             \n\
             ## Testing\n\
             \n\
             - [ ] Unit tests added\n\
             - [ ] Manual testing performed\n\
             \n\
             <!-- Delete this section if not applicable -->\n\
             \n\
             ## Additional context\n\
             \n\
             <details>\n\
             <summary>Internal debug notes</summary>\n\
             \n\
             {debug_dump}\
             </details>\n\
             \n\
             ## Screenshots\n\
             \n\
             N/A\n"
        )
    }

    #[test]
    fn filter_markdown_body_strips_boilerplate_and_keeps_every_paragraph_verbatim() {
        let raw = pr_template_fixture();
        let filtered = filter_markdown_body(&raw);

        assert!(!filtered.contains("Thanks for contributing"), "{filtered}");
        assert!(
            !filtered.contains("Delete this section if not applicable"),
            "{filtered}"
        );
        assert!(!filtered.contains("<!--"), "{filtered}");
        assert!(!filtered.contains("-->"), "{filtered}");

        // Both checklist-only sections (heading + checklist, nothing else)
        // are dropped whole, heading included.
        assert!(!filtered.contains("Type of change"), "{filtered}");
        assert!(!filtered.contains("Bug fix"), "{filtered}");
        assert!(!filtered.contains("Testing"), "{filtered}");
        assert!(!filtered.contains("Unit tests added"), "{filtered}");

        // The <details> body is gone, but its <summary> line survives.
        assert!(
            !filtered.contains("internal debug line"),
            "the debug dump body must be dropped: {filtered}"
        );
        assert!(
            filtered.contains("<summary>Internal debug notes</summary>"),
            "{filtered}"
        );

        // Every other non-empty paragraph stays verbatim.
        assert!(filtered.contains("## Description"), "{filtered}");
        assert!(
            filtered.contains(
                "Fixes the frobnicator to handle the edge case where the widget is null."
            ),
            "{filtered}"
        );
        assert!(filtered.contains("## Screenshots"), "{filtered}");
        assert!(filtered.contains("N/A"), "{filtered}");

        assert!(filtered.len() < raw.len(), "the filtered body must shrink");
    }

    #[test]
    fn a_section_with_prose_alongside_a_checklist_keeps_the_checklist() {
        let raw = "## Type of change\n\nSomething extra was written here too.\n\n- [ ] Bug fix\n- [x] New feature\n";
        let filtered = filter_markdown_body(raw);
        assert!(filtered.contains("Something extra was written here too."));
        assert!(
            filtered.contains("- [ ] Bug fix") && filtered.contains("- [x] New feature"),
            "a section with other content must keep its checklist too: {filtered}"
        );
    }

    #[test]
    fn is_gh_template_view_recognises_pr_issue_and_release_view() {
        for command in [
            "gh pr view 123",
            "gh issue view 42",
            "gh release view v1.0.0",
            "glab pr view 123",
            "glab issue view 7",
            "glab mr view 42",
        ] {
            assert!(is_gh_template_view(command), "{command}");
        }
    }

    #[test]
    fn is_gh_template_view_rejects_json_and_other_subcommands() {
        for command in [
            "gh pr view 123 --json title,body",
            "gh pr view --json=title",
            "gh pr list",
            "gh pr diff 123",
            "gh api /repos/x/y/issues",
            "glab pr list",
        ] {
            assert!(!is_gh_template_view(command), "{command}");
        }
    }

    #[test]
    fn render_markdown_summary_fails_open_when_the_filtered_body_does_not_fit() {
        let filtered = "x".repeat(5000);
        assert_eq!(
            render_markdown_summary("id1", "gh pr view 1", Some(0), &filtered, 10, 20000, 4096),
            None,
            "a filtered body too large to show whole must fail open"
        );
    }

    #[test]
    fn render_markdown_summary_never_grows_past_the_raw_size() {
        let filtered = "short body\n";
        assert_eq!(
            render_markdown_summary("id1", "gh pr view 1", Some(0), filtered, 1, 5, 4096),
            None,
            "a tiny raw capture must not gain a summary"
        );
    }
}
