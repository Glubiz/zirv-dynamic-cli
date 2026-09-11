//! ZCHK-RUNTIME-INVENTORY (issue #470, N01): `docs/design/
//! native-runtime-inventory.md` promises an implementation owner for every
//! command verb and every model-calling call site the native-runtime
//! roadmap (#469) has to migrate. A promise like that only stays true if
//! something checks it against the real binary on every run -- this check
//! parses both tables in that file and fails the moment either drifts from
//! the in-process clap model or the real source tree: a new command lands
//! with no owner, a stale command row nothing in the clap model still
//! answers to, an owner spelled outside the three allowed shapes, or an
//! entry-point row naming a file or symbol that does not actually exist.

use std::path::Path;

use regex::Regex;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-RUNTIME-INVENTORY";
const PROVES: &str = "every depth-1/depth-2 command verb and every model-calling call site in \
     src/ has a named implementation owner in docs/design/native-runtime-inventory.md, matching \
     the in-process clap model and the real source tree";
const FIX: &str = "add or correct the missing/stale row in docs/design/native-runtime-inventory.md \
     -- a new command verb needs an Owner of `shared`, `harness-backend`, or an `Nxx (#issue)` \
     roadmap step; a new model-calling call site needs its own Entry point row naming a real \
     Path and a Symbol that appears verbatim in that file";
const ORIGIN: &str = "issue #470 (N01): the native-runtime roadmap (#469) needs one committed, \
     enforced map of who owns each command and each model-calling call site before any step \
     starts moving them";

const INVENTORY_PATH: &str = "docs/design/native-runtime-inventory.md";
const COMMANDS_HEADING: &str = "## Commands";
const ENTRY_POINTS_HEADING: &str = "## Model-calling entry points";
const COMMANDS_HEADER_ROW: &str = "| Verb | Owner | Notes |";
const ENTRY_POINTS_HEADER_ROW: &str = "| Entry point | Path | Symbol | Owner | Notes |";

pub fn run(repo: &Path) -> BuiltinCheckResult {
    if !super::is_zirv_repo(repo) {
        return BuiltinCheckResult::not_applicable(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            super::not_the_zirv_repo(repo),
        );
    }

    let doc_path = repo.join(INVENTORY_PATH);
    let text = match std::fs::read_to_string(&doc_path) {
        Ok(text) => text,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!("cannot read {}: {err}", doc_path.display()),
            );
        }
    };

    let actual_verbs = match actual_command_verbs() {
        Ok(verbs) => verbs,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!("could not compute the clap command surface: {err}"),
            );
        }
    };

    let Some(command_rows) = extract_table(&text, COMMANDS_HEADING, COMMANDS_HEADER_ROW) else {
        return BuiltinCheckResult::inconclusive(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "could not find the `{COMMANDS_HEADING}` table in {}",
                doc_path.display()
            ),
        );
    };
    let Some(entry_rows) = extract_table(&text, ENTRY_POINTS_HEADING, ENTRY_POINTS_HEADER_ROW)
    else {
        return BuiltinCheckResult::inconclusive(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "could not find the `{ENTRY_POINTS_HEADING}` table in {}",
                doc_path.display()
            ),
        );
    };

    let mut problems = Vec::new();
    let mut documented_verbs = std::collections::BTreeSet::new();

    for row in &command_rows {
        let Some(verb) = row.first().and_then(|cell| backticked(cell)) else {
            problems.push(format!("command row has no backticked verb: {row:?}"));
            continue;
        };
        let owner = row.get(1).map(String::as_str).unwrap_or("").trim();
        if !valid_owner(owner) {
            problems.push(format!(
                "`{verb}`: owner `{owner}` is not `shared`, `harness-backend`, or `Nxx (#issue)`"
            ));
        }
        if !actual_verbs.contains(&verb) {
            problems.push(format!(
                "`{verb}` is documented but no longer in the clap model (stale row)"
            ));
        }
        documented_verbs.insert(verb);
    }

    for verb in actual_verbs.difference(&documented_verbs) {
        problems.push(format!(
            "`{verb}` is a real command but missing from `{COMMANDS_HEADING}`"
        ));
    }

    let mut entry_count = 0usize;
    for row in &entry_rows {
        let Some(path) = row.get(1).and_then(|cell| backticked(cell)) else {
            problems.push(format!("entry-point row has no backticked Path: {row:?}"));
            continue;
        };
        let Some(symbol) = row.get(2).and_then(|cell| backticked(cell)) else {
            problems.push(format!("entry-point row has no backticked Symbol: {row:?}"));
            continue;
        };
        let owner = row.get(3).map(String::as_str).unwrap_or("").trim();
        if !valid_owner(owner) {
            problems.push(format!(
                "`{path}` `{symbol}`: owner `{owner}` is not `shared`, `harness-backend`, or \
                 `Nxx (#issue)`"
            ));
        }
        if !is_repo_relative(&path) {
            problems.push(format!("entry-point path must be repo-relative: {path}"));
            entry_count += 1;
            continue;
        }
        let full_path = repo.join(&path);
        match std::fs::read_to_string(&full_path) {
            Ok(contents) => {
                if !contents.contains(&symbol) {
                    problems.push(format!(
                        "`{path}`: symbol `{symbol}` does not appear verbatim in this file"
                    ));
                }
            }
            Err(err) => {
                problems.push(format!("`{path}` does not exist under the repo: {err}"));
            }
        }
        entry_count += 1;
    }

    if problems.is_empty() {
        BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "{} command verbs and {entry_count} model-calling entry points all have a valid, \
                 matching owner",
                documented_verbs.len()
            ),
        )
    } else {
        BuiltinCheckResult::fail(ID, PROVES, FIX, ORIGIN, problems.join("; "))
    }
}

/// The depth-1/depth-2 verb set of the real command surface, read straight
/// off the in-process clap model (never a spawned `zirv commands --json`) --
/// same depth rule `scripts/check-readme-features.sh` uses: for each path's
/// words after the leading "zirv ", the first word is a depth-1 verb and the
/// first two words (when a second exists) are a depth-2 verb.
fn actual_command_verbs() -> Result<std::collections::BTreeSet<String>, String> {
    let entries =
        crate::commands::command_schema::command_entries().map_err(|err| err.to_string())?;
    let mut verbs = std::collections::BTreeSet::new();
    for entry in entries {
        let words: Vec<&str> = entry.path.split_whitespace().skip(1).collect();
        if let Some(first) = words.first() {
            verbs.insert((*first).to_string());
        }
        if words.len() >= 2 {
            verbs.insert(format!("{} {}", words[0], words[1]));
        }
    }
    Ok(verbs)
}

/// `owner` is exactly `shared`, exactly `harness-backend`, or a roadmap step
/// token shaped `N` + two digits + ` (#` + digits + `)` (e.g. `N15 (#484)`).
fn valid_owner(owner: &str) -> bool {
    if owner == "shared" || owner == "harness-backend" {
        return true;
    }
    Regex::new(r"^N\d{2} \(#\d+\)$")
        .expect("static pattern")
        .is_match(owner)
}

/// Whether `path` (an entry-point row's `Path` cell, read from the
/// repo-owned, UNTRUSTED inventory doc) stays inside the repo once joined
/// onto it -- every component must be a plain name, never absolute, `..`,
/// a leading `.`, or a Windows prefix/root. Checked BEFORE the path is ever
/// joined or opened, so a hostile row is refused without reading anything
/// outside the repo, rather than turning `zirv verify --builtin` into an
/// arbitrary-file content oracle. This check is syntactic (path components
/// only) and does not resolve symlinks, so a symlink committed inside the
/// repository that points outside it is still followed; that is accepted
/// because the repository's own contents are already the trust boundary here.
fn is_repo_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// The text inside the first `` `...` `` span in `cell`, if any.
fn backticked(cell: &str) -> Option<String> {
    let start = cell.find('`')? + 1;
    let end = start + cell[start..].find('`')?;
    Some(cell[start..end].to_string())
}

/// Finds the markdown table whose header row is exactly `header_row` inside
/// the section starting at `heading` (up to the next `## ` heading, or end
/// of file), and returns its data rows -- everything after the `|---|...`
/// separator, each split into trimmed cells, up to the first line that does
/// not start with `|`.
fn extract_table(text: &str, heading: &str, header_row: &str) -> Option<Vec<Vec<String>>> {
    let start = text.find(heading)? + heading.len();
    let section = &text[start..];
    let end = section.find("\n## ").unwrap_or(section.len());
    let section = &section[..end];

    let mut lines = section.lines();
    for line in lines.by_ref() {
        if line.trim() == header_row {
            break;
        }
    }
    // The separator row (`|---|---|...`) is required and skipped unread.
    lines.next()?;

    let mut rows = Vec::new();
    for line in lines {
        let line = line.trim();
        if !line.starts_with('|') {
            break;
        }
        let cells: Vec<String> = line
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect();
        rows.push(cells);
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a doc whose Commands table is the REAL, current clap command
    /// surface (never hand-copied, so this test cannot go stale the way a
    /// pinned verb list would) and whose entry-points table names two files
    /// this test writes into the fixture repo itself.
    fn passing_doc(repo: &Path) -> String {
        let entries = crate::commands::command_schema::command_entries().expect("clap model");
        let mut verbs = std::collections::BTreeSet::new();
        for entry in &entries {
            let words: Vec<&str> = entry.path.split_whitespace().skip(1).collect();
            if let Some(first) = words.first() {
                verbs.insert((*first).to_string());
            }
            if words.len() >= 2 {
                verbs.insert(format!("{} {}", words[0], words[1]));
            }
        }
        let mut commands = format!("{COMMANDS_HEADING}\n\n{COMMANDS_HEADER_ROW}\n|---|---|---|\n");
        for verb in &verbs {
            commands.push_str(&format!("| `{verb}` | shared |  |\n"));
        }

        std::fs::create_dir_all(repo.join("src/commands/ctx")).expect("mkdir");
        std::fs::write(
            repo.join("src/commands/ctx/chat.rs"),
            "fn build_launch() {}\n",
        )
        .expect("write fixture source");

        let entry_points = format!(
            "{ENTRY_POINTS_HEADING}\n\n{ENTRY_POINTS_HEADER_ROW}\n|---|---|---|---|---|\n\
             | Interactive orchestrator launch | `src/commands/ctx/chat.rs` | `build_launch` | \
             N11 (#480) |  |\n"
        );

        format!("{commands}\n{entry_points}")
    }

    fn write_doc(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join("docs/design")).expect("mkdir");
        std::fs::write(repo.join(INVENTORY_PATH), body).expect("write doc");
    }

    #[test]
    fn a_complete_matching_inventory_passes() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "zirv");
        let doc = passing_doc(repo.path());
        write_doc(repo.path(), &doc);

        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_missing_verb_fails() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "zirv");
        let doc = passing_doc(repo.path());
        // Drop one real verb's row entirely.
        let doc: String = doc
            .lines()
            .filter(|line| !line.starts_with("| `ctx wrap`"))
            .collect::<Vec<_>>()
            .join("\n");
        write_doc(repo.path(), &doc);

        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("ctx wrap"), "{result:?}");
        assert!(result.details.contains("missing from"), "{result:?}");
    }

    #[test]
    fn a_bad_owner_fails() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "zirv");
        let doc = passing_doc(repo.path())
            .replace("| `ctx wrap` | shared |  |", "| `ctx wrap` | somebody |  |");
        write_doc(repo.path(), &doc);

        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("somebody"), "{result:?}");
    }

    #[test]
    fn a_stale_entry_point_symbol_fails() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "zirv");
        let doc =
            passing_doc(repo.path()).replace("`build_launch`", "`this_symbol_does_not_exist`");
        write_doc(repo.path(), &doc);

        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("this_symbol_does_not_exist"),
            "{result:?}"
        );
        assert!(
            result.details.contains("does not appear verbatim"),
            "{result:?}"
        );
    }

    /// The doc's `Path` cell is repo-owned, UNTRUSTED text: an absolute path
    /// must be refused before anything is read, even when the absolute
    /// target genuinely contains the symbol text (proving the guard runs
    /// first, not that the symbol happens to be missing).
    #[test]
    fn an_absolute_entry_point_path_fails_without_reading_the_target() {
        let outer = tempfile::tempdir().expect("tempdir");
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        super::super::write_manifest(&repo, "zirv");

        let canary = outer.path().join("abs-evil.rs");
        std::fs::write(&canary, "fn build_launch() {}\n").expect("write canary");
        let absolute = canary.to_string_lossy().to_string();

        let doc =
            passing_doc(&repo).replace("`src/commands/ctx/chat.rs`", &format!("`{absolute}`"));
        write_doc(&repo, &doc);

        let result = run(&repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("must be repo-relative"),
            "{result:?}"
        );
    }

    /// Same guard, the `../` escape shape: a target one directory above the
    /// repo genuinely contains the symbol text, so a pass here would prove
    /// the guard was bypassed rather than that the file was merely missing.
    #[test]
    fn a_dot_dot_entry_point_path_fails_without_reading_the_target() {
        let outer = tempfile::tempdir().expect("tempdir");
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        super::super::write_manifest(&repo, "zirv");

        std::fs::write(outer.path().join("evil.rs"), "fn build_launch() {}\n")
            .expect("write canary");

        let doc = passing_doc(&repo).replace("`src/commands/ctx/chat.rs`", "`../evil.rs`");
        write_doc(&repo, &doc);

        let result = run(&repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("must be repo-relative"),
            "{result:?}"
        );
    }

    #[test]
    fn a_stale_command_row_fails() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "zirv");
        let doc = passing_doc(repo.path()).replace(
            &format!("{COMMANDS_HEADER_ROW}\n|---|---|---|\n"),
            &format!("{COMMANDS_HEADER_ROW}\n|---|---|---|\n| `not-a-real-verb` | shared |  |\n"),
        );
        write_doc(repo.path(), &doc);

        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("not-a-real-verb"), "{result:?}");
        assert!(result.details.contains("stale row"), "{result:?}");
    }

    #[test]
    fn a_non_zirv_repo_is_not_applicable() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "some-other-crate");
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::NotApplicable,
            "{result:?}"
        );
    }

    #[test]
    fn a_missing_doc_inside_the_zirv_repo_is_inconclusive() {
        let repo = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(repo.path(), "zirv");
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Inconclusive,
            "{result:?}"
        );
    }

    #[test]
    fn backticked_reads_the_first_span_only() {
        assert_eq!(backticked("`ctx wrap`"), Some("ctx wrap".to_string()));
        assert_eq!(
            backticked("`src/foo.rs` `bar`"),
            Some("src/foo.rs".to_string())
        );
        assert_eq!(backticked("no backticks here"), None);
    }

    #[test]
    fn valid_owner_accepts_exactly_the_three_shapes() {
        assert!(valid_owner("shared"));
        assert!(valid_owner("harness-backend"));
        assert!(valid_owner("N15 (#484)"));
        assert!(!valid_owner("N5 (#484)"));
        assert!(!valid_owner("N15 #484"));
        assert!(!valid_owner("check only"));
        assert!(!valid_owner(""));
    }

    /// The real inventory this repository ships must itself pass the check
    /// it defines -- the same "the check must PASS against the real repo"
    /// requirement issue #470's own verification step names.
    #[test]
    fn the_real_repo_inventory_passes() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
