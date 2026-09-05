//! ZCHK-UNIX-TESTS-DOC / ZCHK-DOC-VERBS: two doc-drift checks that both
//! compare a generated/counted fact against committed prose, never doc
//! against doc.
//!
//! - `ZCHK-UNIX-TESTS-DOC` counts `#[cfg(unix)]`-gated `#[test]` functions in
//!   `wrap.rs` and checks that `Development/Known Issues.md` either states
//!   the same count or points at this check instead (issue #276 allows
//!   either wording) -- the count itself drifts every time a PTY test is
//!   added or removed, so a hardcoded number in prose rots quietly.
//! - `ZCHK-DOC-VERBS` compares the real `ctx` verb names clap knows about
//!   (`CtxCli::command()`, the same `clap::CommandFactory` introspection
//!   `command_schema.rs` already uses) against the verb list committed
//!   between two anchor comments in `Modules/Built-in Commands.md`.

use std::collections::BTreeSet;
use std::path::Path;

use clap::CommandFactory;
use regex::Regex;

use super::BuiltinCheckResult;

pub const UNIX_TESTS_ID: &str = "ZCHK-UNIX-TESTS-DOC";
const UNIX_TESTS_PROVES: &str = "wrap.rs's #[cfg(unix)] test count matches what \
     Development/Known Issues.md states, or that doc points at this check instead of a number";
const UNIX_TESTS_FIX: &str = "either update the stated count in Development/Known Issues.md's \
     wrap.rs PTY-harness section to match, or replace the hardcoded number with a pointer to \
     ZCHK-UNIX-TESTS-DOC (recommended -- the count drifts every time a PTY test is added or \
     removed)";
const UNIX_TESTS_ORIGIN: &str = "Docker-run gotcha -- CLAUDE.md: anything touching wrap/announce/pace/argv must be verified \
     on Linux first, and the #[cfg(unix)] tests never even compile on Windows; issue #276";

/// Counts `#[cfg(unix)]`-gated `#[test]` functions in `wrap.rs` -- an
/// attribute-scanning state machine, not a bare `grep -c "#[cfg(unix)]"`
/// (44 total occurrences in the real file today, most of them gating a
/// helper fn/struct rather than a test): a small buffer of "attributes seen
/// since the last blank statement boundary" is accumulated line by line, and
/// counted only when it contains both `#[cfg(unix)]` and `#[test]` before an
/// `fn ` line closes it out.
fn count_cfg_unix_tests(source: &str) -> usize {
    let mut pending_cfg_unix = false;
    let mut pending_test = false;
    let mut count = 0usize;
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.starts_with("#[cfg(unix)]") {
            pending_cfg_unix = true;
            continue;
        }
        if line.starts_with("#[test]") {
            pending_test = true;
            continue;
        }
        if line.starts_with("#[") || line.starts_with("///") || line.starts_with("//") {
            // Any other attribute or doc/line comment does not reset the
            // pending flags -- `#[test]` and `#[cfg(unix)]` can appear in
            // either order, sometimes with `#[allow(...)]` between them.
            continue;
        }
        if line.starts_with("fn ") || line.contains(" fn ") {
            if pending_cfg_unix && pending_test {
                count += 1;
            }
            pending_cfg_unix = false;
            pending_test = false;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        // Any other real statement/item (a `struct`, `impl`, `use`, a bare
        // expression) closes out whatever attributes were pending without
        // counting them -- they were not gating a `fn` at all.
        pending_cfg_unix = false;
        pending_test = false;
    }
    count
}

pub fn run_unix_tests_doc(repo: &Path) -> BuiltinCheckResult {
    let wrap_path = repo.join("src/commands/ctx/wrap.rs");
    let wrap_source = match std::fs::read_to_string(&wrap_path) {
        Ok(source) => source,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                UNIX_TESTS_ID,
                UNIX_TESTS_PROVES,
                UNIX_TESTS_FIX,
                UNIX_TESTS_ORIGIN,
                format!("cannot read {}: {err}", wrap_path.display()),
            );
        }
    };
    let actual = count_cfg_unix_tests(&wrap_source);

    let known_issues_path = repo.join("docs/obsidian/Development/Known Issues.md");
    let known_issues = match std::fs::read_to_string(&known_issues_path) {
        Ok(text) => text,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                UNIX_TESTS_ID,
                UNIX_TESTS_PROVES,
                UNIX_TESTS_FIX,
                UNIX_TESTS_ORIGIN,
                format!("cannot read {}: {err}", known_issues_path.display()),
            );
        }
    };

    if known_issues.contains(UNIX_TESTS_ID) {
        return BuiltinCheckResult::pass(
            UNIX_TESTS_ID,
            UNIX_TESTS_PROVES,
            UNIX_TESTS_FIX,
            UNIX_TESTS_ORIGIN,
            format!(
                "{actual} #[cfg(unix)] test(s) in wrap.rs today; Known Issues.md points at this \
                 check instead of stating a number"
            ),
        );
    }

    // "N as of YYYY-MM-DD" is the wording this doc already uses -- match the
    // leading integer right before "as of" or "#[cfg(unix)]" wherever it
    // appears near the section this check cares about, rather than requiring
    // one exact sentence shape.
    let stated_re = Regex::new(r"(\d+)\s*(?:\([^)]*\)\s*)?as of").unwrap();
    let Some(captures) = stated_re.captures(&known_issues) else {
        return BuiltinCheckResult::inconclusive(
            UNIX_TESTS_ID,
            UNIX_TESTS_PROVES,
            UNIX_TESTS_FIX,
            UNIX_TESTS_ORIGIN,
            format!(
                "Known Issues.md states no parseable count and does not mention {UNIX_TESTS_ID} \
                 -- actual count is {actual}"
            ),
        );
    };
    let stated: usize = captures[1].parse().unwrap_or(usize::MAX);

    if stated == actual {
        BuiltinCheckResult::pass(
            UNIX_TESTS_ID,
            UNIX_TESTS_PROVES,
            UNIX_TESTS_FIX,
            UNIX_TESTS_ORIGIN,
            format!("Known Issues.md states {stated}, matching the actual count"),
        )
    } else {
        BuiltinCheckResult::fail(
            UNIX_TESTS_ID,
            UNIX_TESTS_PROVES,
            UNIX_TESTS_FIX,
            UNIX_TESTS_ORIGIN,
            format!(
                "Known Issues.md states {stated} #[cfg(unix)] test(s), but wrap.rs actually has \
                 {actual}"
            ),
        )
    }
}

pub const DOC_VERBS_ID: &str = "ZCHK-DOC-VERBS";
const DOC_VERBS_PROVES: &str = "the ctx verb names clap actually parses match the verb list \
     committed between the zchk-doc-verbs anchor comments in Modules/Built-in Commands.md";
const DOC_VERBS_FIX: &str = "update the backtick-quoted, comma-separated, alphabetically sorted \
     verb list between <!-- zchk-doc-verbs:start --> and <!-- zchk-doc-verbs:end --> in \
     Modules/Built-in Commands.md to match `zirv ctx --help`'s real subcommand list";
const DOC_VERBS_ORIGIN: &str = "doc drift -- docs surfaces restate verb lists that drift from the clap definitions \
     (issue #276, generalizing the completeness pattern command_schema.rs's own \
     every_discovered_leaf_is_classified_exactly_once test already uses)";

const ANCHOR_START: &str = "<!-- zchk-doc-verbs:start -->";
const ANCHOR_END: &str = "<!-- zchk-doc-verbs:end -->";

pub fn run_doc_verbs(repo: &Path) -> BuiltinCheckResult {
    let cmd = crate::commands::ctx::CtxCli::command();
    let mut clap_verbs: BTreeSet<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    // `disable_help_subcommand` still leaves clap's own `help` visible as a
    // real subcommand entry on some versions; `zirv ctx` never has a
    // separate `help` verb of its own (it is a flag, `--help`), and it is
    // deliberately excluded from the docs table below because it is not one
    // of the verbs `CtxVerb` declares.
    clap_verbs.remove("help");

    let doc_path = repo.join("docs/obsidian/Modules/Built-in Commands.md");
    let doc_text = match std::fs::read_to_string(&doc_path) {
        Ok(text) => text,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                DOC_VERBS_ID,
                DOC_VERBS_PROVES,
                DOC_VERBS_FIX,
                DOC_VERBS_ORIGIN,
                format!("cannot read {}: {err}", doc_path.display()),
            );
        }
    };

    let Some(start) = doc_text.find(ANCHOR_START) else {
        return BuiltinCheckResult::inconclusive(
            DOC_VERBS_ID,
            DOC_VERBS_PROVES,
            DOC_VERBS_FIX,
            DOC_VERBS_ORIGIN,
            format!("{ANCHOR_START} not found in {}", doc_path.display()),
        );
    };
    let Some(end) = doc_text[start..].find(ANCHOR_END) else {
        return BuiltinCheckResult::inconclusive(
            DOC_VERBS_ID,
            DOC_VERBS_PROVES,
            DOC_VERBS_FIX,
            DOC_VERBS_ORIGIN,
            format!("{ANCHOR_END} not found in {}", doc_path.display()),
        );
    };
    let between = &doc_text[start + ANCHOR_START.len()..start + end];

    let verb_re = Regex::new(r"`([a-z][a-z0-9-]*)`").unwrap();
    let doc_verbs: BTreeSet<String> = verb_re
        .captures_iter(between)
        .map(|cap| cap[1].to_string())
        .collect();

    let missing_from_doc: Vec<&String> = clap_verbs.difference(&doc_verbs).collect();
    let stale_in_doc: Vec<&String> = doc_verbs.difference(&clap_verbs).collect();

    if missing_from_doc.is_empty() && stale_in_doc.is_empty() {
        BuiltinCheckResult::pass(
            DOC_VERBS_ID,
            DOC_VERBS_PROVES,
            DOC_VERBS_FIX,
            DOC_VERBS_ORIGIN,
            format!("{} verbs match exactly", clap_verbs.len()),
        )
    } else {
        let mut details = Vec::new();
        if !missing_from_doc.is_empty() {
            details.push(format!(
                "clap has but the doc is missing: {}",
                missing_from_doc
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !stale_in_doc.is_empty() {
            details.push(format!(
                "doc lists but clap no longer has: {}",
                stale_in_doc
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        BuiltinCheckResult::fail(
            DOC_VERBS_ID,
            DOC_VERBS_PROVES,
            DOC_VERBS_FIX,
            DOC_VERBS_ORIGIN,
            details.join("; "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn count_cfg_unix_tests_counts_only_test_fns_not_helper_fns_or_structs() {
        let source = r#"
#[cfg(unix)]
pub(crate) fn fixture(name: &str) -> PathBuf {
    todo!()
}

#[cfg(unix)]
pub(crate) struct Harness;

#[cfg(unix)]
#[test]
fn a_real_unix_test() {
    assert!(true);
}

#[test]
fn a_non_unix_test() {
    assert!(true);
}

#[test]
#[cfg(unix)]
fn cfg_after_test_still_counts() {
    assert!(true);
}
"#;
        assert_eq!(count_cfg_unix_tests(source), 2);
    }

    #[test]
    fn missing_wrap_rs_is_inconclusive() {
        let repo = tempdir().unwrap();
        let result = run_unix_tests_doc(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    fn write_wrap_rs(repo: &Path, unix_test_count: usize) {
        std::fs::create_dir_all(repo.join("src/commands/ctx")).unwrap();
        let mut body = String::new();
        for i in 0..unix_test_count {
            body.push_str(&format!(
                "#[cfg(unix)]\n#[test]\nfn unix_test_{i}() {{ assert!(true); }}\n\n"
            ));
        }
        std::fs::write(repo.join("src/commands/ctx/wrap.rs"), body).unwrap();
    }

    fn write_known_issues(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join("docs/obsidian/Development")).unwrap();
        std::fs::write(repo.join("docs/obsidian/Development/Known Issues.md"), body).unwrap();
    }

    #[test]
    fn a_matching_stated_count_passes() {
        let repo = tempdir().unwrap();
        write_wrap_rs(repo.path(), 3);
        write_known_issues(
            repo.path(),
            "Every #[cfg(unix)] test (3 as of 2026-09-05) hangs.",
        );
        let result = run_unix_tests_doc(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_stale_stated_count_fails() {
        let repo = tempdir().unwrap();
        write_wrap_rs(repo.path(), 5);
        write_known_issues(
            repo.path(),
            "Every #[cfg(unix)] test (3 as of 2026-09-05) hangs.",
        );
        let result = run_unix_tests_doc(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
    }

    #[test]
    fn a_see_check_pointer_passes_regardless_of_count() {
        let repo = tempdir().unwrap();
        write_wrap_rs(repo.path(), 5);
        write_known_issues(
            repo.path(),
            "The #[cfg(unix)] test count is enforced by ZCHK-UNIX-TESTS-DOC, not stated here.",
        );
        let result = run_unix_tests_doc(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    fn write_doc_verbs(repo: &Path, between: &str) {
        std::fs::create_dir_all(repo.join("docs/obsidian/Modules")).unwrap();
        std::fs::write(
            repo.join("docs/obsidian/Modules/Built-in Commands.md"),
            format!("prose\n{ANCHOR_START}\n{between}\n{ANCHOR_END}\nmore prose\n"),
        )
        .unwrap();
    }

    #[test]
    fn missing_anchors_are_inconclusive() {
        let repo = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("docs/obsidian/Modules")).unwrap();
        std::fs::write(
            repo.path()
                .join("docs/obsidian/Modules/Built-in Commands.md"),
            "no anchors here",
        )
        .unwrap();
        let result = run_doc_verbs(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    #[test]
    fn a_verb_missing_from_the_doc_fails() {
        let repo = tempdir().unwrap();
        // Deliberately drop `score` (a real ctx verb) from the doc list.
        let real_verbs_minus_one: Vec<String> = crate::commands::ctx::CtxCli::command()
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .filter(|name| name != "help" && name != "score")
            .collect();
        let between = real_verbs_minus_one
            .iter()
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(", ");
        write_doc_verbs(repo.path(), &between);
        let result = run_doc_verbs(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("score"), "{result:?}");
    }

    #[test]
    fn the_real_doc_matches_the_real_clap_tree() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run_doc_verbs(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
