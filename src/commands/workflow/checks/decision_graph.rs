//! ZCHK-DECISION-GRAPH: `Development/Decision Log.md`'s optional
//! `**Supersedes:**` links resolve to a real entry heading, and no cycle
//! exists among them. The Log's own guardrail is "supersede by deletion, not
//! by appending" -- most entries never use this field at all, which is a
//! vacuous pass, not a reason to skip the check.
//!
//! Convention (documented alongside this check in the Log itself): a line
//! shaped `**Supersedes:** <exact prior entry heading text, without the
//! leading "### YYYY-MM-DD -- ">` anywhere in an entry's body names the
//! entry it replaces. An entry may name more than one prior entry,
//! comma-separated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regex::Regex;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-DECISION-GRAPH";
const PROVES: &str = "every **Supersedes:** reference in Development/Decision Log.md resolves \
     to a real entry heading, and no cycle exists among them";
const FIX: &str = "fix the dangling **Supersedes:** reference to name an exact, existing entry \
     heading (the text after the date and dash, e.g. \"Usage headroom ranks a spawn, it never \
     refuses or delays one\"), or break the cycle by deleting one of the entries involved (the \
     Log's own guardrail: supersede by deletion, not by appending)";
const ORIGIN: &str = "vault hygiene -- Ruflo round-2 plugins/ruflo-adr precedent (supersedes-graph \
     DFS cycle and dangling-ref check), issue #276";

const HEADING_RE_SOURCE: &str = r"(?m)^### \d{4}-\d{2}-\d{2}(?:/\d{2})?\s*--\s*(.+)$";
const SUPERSEDES_RE_SOURCE: &str = r"(?mi)^\*\*Supersedes:\*\*\s*(.+)$";

pub fn run(repo: &Path) -> BuiltinCheckResult {
    let path = repo.join("docs/obsidian/Development/Decision Log.md");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!("cannot read {}: {err}", path.display()),
            );
        }
    };

    match analyze(&text) {
        Ok(count) => BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "{} entries, {} supersedes link(s), no dangling references or cycles",
                count.entries, count.links
            ),
        ),
        Err(reason) => BuiltinCheckResult::fail(ID, PROVES, FIX, ORIGIN, reason),
    }
}

struct AnalysisOk {
    entries: usize,
    links: usize,
}

/// Splits `text` into entries by heading, extracts each entry's own
/// `**Supersedes:**` targets (comma-separated, trimmed of trailing
/// punctuation/backticks), and checks both invariants. `Err` names every
/// problem found (dangling references, then any cycle), not just the first.
fn analyze(text: &str) -> Result<AnalysisOk, String> {
    let heading_re = Regex::new(HEADING_RE_SOURCE).unwrap();
    let supersedes_re = Regex::new(SUPERSEDES_RE_SOURCE).unwrap();

    let headings: Vec<(usize, String)> = heading_re
        .captures_iter(text)
        .map(|cap| (cap.get(0).unwrap().start(), cap[1].trim().to_string()))
        .collect();
    if headings.is_empty() {
        return Ok(AnalysisOk {
            entries: 0,
            links: 0,
        });
    }

    let known: BTreeSet<&str> = headings.iter().map(|(_, title)| title.as_str()).collect();

    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dangling: Vec<String> = Vec::new();
    let mut link_count = 0usize;

    for (index, (start, title)) in headings.iter().enumerate() {
        let end = headings
            .get(index + 1)
            .map(|(s, _)| *s)
            .unwrap_or(text.len());
        let body = &text[*start..end];
        for cap in supersedes_re.captures_iter(body) {
            for raw_target in cap[1].split(',') {
                let target = raw_target.trim().trim_matches(|c| c == '`' || c == '.');
                if target.is_empty() {
                    continue;
                }
                link_count += 1;
                if known.contains(target) {
                    edges
                        .entry(title.clone())
                        .or_default()
                        .push(target.to_string());
                } else {
                    dangling.push(format!("\"{title}\" -> \"{target}\" (no such entry)"));
                }
            }
        }
    }

    if !dangling.is_empty() {
        return Err(format!(
            "dangling supersedes reference(s): {}",
            dangling.join("; ")
        ));
    }

    if let Some(cycle) = find_cycle(&edges) {
        return Err(format!("supersedes cycle: {}", cycle.join(" -> ")));
    }

    Ok(AnalysisOk {
        entries: headings.len(),
        links: link_count,
    })
}

/// Plain DFS cycle detection over the supersedes graph (white/gray/black
/// coloring). Returns the first cycle found as a path of titles, `None` when
/// the graph is acyclic.
fn find_cycle(edges: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut color: BTreeMap<&str, Color> = BTreeMap::new();
    for node in edges.keys() {
        color.insert(node.as_str(), Color::White);
        for target in &edges[node.as_str()] {
            color.entry(target.as_str()).or_insert(Color::White);
        }
    }

    fn visit<'a>(
        node: &'a str,
        edges: &'a BTreeMap<String, Vec<String>>,
        color: &mut BTreeMap<&'a str, Color>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        color.insert(node, Color::Gray);
        stack.push(node);
        if let Some(targets) = edges.get(node) {
            for target in targets {
                match color.get(target.as_str()).copied().unwrap_or(Color::White) {
                    Color::White => {
                        if let Some(cycle) = visit(target.as_str(), edges, color, stack) {
                            return Some(cycle);
                        }
                    }
                    Color::Gray => {
                        let start = stack
                            .iter()
                            .position(|n| *n == target.as_str())
                            .unwrap_or(0);
                        let mut cycle: Vec<String> =
                            stack[start..].iter().map(|s| s.to_string()).collect();
                        cycle.push(target.clone());
                        return Some(cycle);
                    }
                    Color::Black => {}
                }
            }
        }
        stack.pop();
        color.insert(node, Color::Black);
        None
    }

    let nodes: Vec<&str> = color.keys().copied().collect();
    for node in nodes {
        if color.get(node).copied() == Some(Color::White) {
            let mut stack = Vec::new();
            if let Some(cycle) = visit(node, edges, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_log(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join("docs/obsidian/Development")).unwrap();
        std::fs::write(repo.join("docs/obsidian/Development/Decision Log.md"), body).unwrap();
    }

    #[test]
    fn no_supersedes_links_at_all_passes() {
        let repo = tempdir().unwrap();
        write_log(
            repo.path(),
            "### 2026-09-04 -- First entry\nbody\n\n### 2026-09-03 -- Second entry\nbody\n",
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_resolving_supersedes_link_passes() {
        let repo = tempdir().unwrap();
        write_log(
            repo.path(),
            "### 2026-09-04 -- New decision\n**Supersedes:** Old decision\nbody\n\n\
             ### 2026-09-03 -- Old decision\nbody\n",
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_dangling_supersedes_link_fails() {
        let repo = tempdir().unwrap();
        write_log(
            repo.path(),
            "### 2026-09-04 -- New decision\n**Supersedes:** Nonexistent decision\nbody\n",
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("Nonexistent decision"),
            "{result:?}"
        );
    }

    #[test]
    fn a_two_entry_cycle_fails() {
        let repo = tempdir().unwrap();
        write_log(
            repo.path(),
            "### 2026-09-04 -- Entry A\n**Supersedes:** Entry B\nbody\n\n\
             ### 2026-09-03 -- Entry B\n**Supersedes:** Entry A\nbody\n",
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("cycle"), "{result:?}");
    }

    #[test]
    fn missing_file_is_inconclusive() {
        let repo = tempdir().unwrap();
        let result = run(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    #[test]
    fn the_real_decision_log_has_no_dangling_or_cyclic_supersedes_links() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
