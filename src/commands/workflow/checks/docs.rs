//! Doc-drift checks that
//! compare a generated/counted fact against committed prose, never doc
//! against doc.
//!
//! - `ZCHK-DOC-EXIT-CODES` compares the supervised exit codes against the
//!   code list committed between anchor comments in README.md.
//! - `ZCHK-DOC-RESERVED` compares the reserved built-in names against the
//!   backtick-quoted list committed between anchor comments in README.md.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;

use super::BuiltinCheckResult;

pub const DOC_EXIT_CODES_ID: &str = "ZCHK-DOC-EXIT-CODES";
pub const DOC_RESERVED_ID: &str = "ZCHK-DOC-RESERVED";
const README_ORIGIN: &str =
    "external review 2026-09-07: README documented 2 of 7 exit codes and 17 of 19 reserved names";

pub fn run_doc_exit_codes(repo: &Path) -> BuiltinCheckResult {
    run_readme_list(
        repo,
        DOC_EXIT_CODES_ID,
        "the supervised exit codes match the code list committed between the zchk-doc-exit-codes anchor comments in README.md",
        "update the exit-code table between <!-- zchk-doc-exit-codes:start --> and <!-- zchk-doc-exit-codes:end --> in README.md to match exec.rs's EXIT_CODES",
        "zchk-doc-exit-codes",
        r"(?m)^\|\s*`(\d+)`\s*\|",
        crate::commands::ctx::exec::EXIT_CODES
            .iter()
            .map(|(code, _)| code.to_string())
            .collect(),
    )
}

pub fn run_doc_reserved(repo: &Path) -> BuiltinCheckResult {
    run_readme_list(
        repo,
        DOC_RESERVED_ID,
        "the reserved built-in names and aliases match the backtick-quoted list committed between the zchk-doc-reserved anchor comments in README.md",
        "update the backtick-quoted list between <!-- zchk-doc-reserved:start --> and <!-- zchk-doc-reserved:end --> in README.md to match utils::RESERVED_COMMANDS",
        "zchk-doc-reserved",
        r"`([a-z][a-z0-9-]*)`",
        crate::utils::RESERVED_COMMANDS
            .iter()
            .map(|name| name.to_string())
            .collect(),
    )
}

fn run_readme_list(
    repo: &Path,
    id: &'static str,
    proves: &'static str,
    fix: &'static str,
    anchor: &str,
    pattern: &str,
    actual: BTreeSet<String>,
) -> BuiltinCheckResult {
    if !super::is_zirv_repo(repo) {
        return BuiltinCheckResult::not_applicable(
            id,
            proves,
            fix,
            README_ORIGIN,
            super::not_the_zirv_repo(repo),
        );
    }
    let path = repo.join("README.md");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                id,
                proves,
                fix,
                README_ORIGIN,
                format!("cannot read {}: {err}", path.display()),
            );
        }
    };
    let start = format!("<!-- {anchor}:start -->");
    let end = format!("<!-- {anchor}:end -->");
    let Some(between) = text
        .split_once(&start)
        .and_then(|(_, tail)| tail.split_once(&end).map(|(between, _)| between))
    else {
        return BuiltinCheckResult::inconclusive(
            id,
            proves,
            fix,
            README_ORIGIN,
            format!("{start} / {end} not found in {}", path.display()),
        );
    };
    let documented: BTreeSet<String> = Regex::new(pattern)
        .unwrap()
        .captures_iter(between)
        .map(|cap| cap[1].to_string())
        .collect();
    let missing = actual.difference(&documented).cloned().collect::<Vec<_>>();
    let extra = documented.difference(&actual).cloned().collect::<Vec<_>>();
    if missing.is_empty() && extra.is_empty() {
        BuiltinCheckResult::pass(
            id,
            proves,
            fix,
            README_ORIGIN,
            format!("{} entries match exactly", actual.len()),
        )
    } else {
        BuiltinCheckResult::fail(
            id,
            proves,
            fix,
            README_ORIGIN,
            format!(
                "code has but the doc is missing: {}; doc lists but code no longer has: {}",
                missing.join(", "),
                extra.join(", ")
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn exit_code_documentation_passes_when_matching_and_names_missing_and_extra_codes() {
        let repo = tempdir().unwrap();
        super::super::write_manifest(repo.path(), "zirv");
        let rows = crate::commands::ctx::exec::EXIT_CODES
            .iter()
            .map(|(code, _)| format!("| `{code}` | meaning |\n"))
            .collect::<String>();
        let doc =
            format!("<!-- zchk-doc-exit-codes:start -->\n{rows}<!-- zchk-doc-exit-codes:end -->");
        std::fs::write(repo.path().join("README.md"), &doc).unwrap();
        assert_eq!(
            run_doc_exit_codes(repo.path()).outcome,
            super::super::BuiltinOutcome::Pass
        );
        std::fs::write(repo.path().join("README.md"), doc.replace("`81`", "`99`")).unwrap();
        let result = run_doc_exit_codes(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Fail);
        assert!(result.details.contains("missing: 81"), "{result:?}");
        assert!(result.details.contains("no longer has: 99"), "{result:?}");
    }

    #[test]
    fn reserved_name_documentation_passes_when_matching_and_names_missing_and_extra_names() {
        let repo = tempdir().unwrap();
        super::super::write_manifest(repo.path(), "zirv");
        let names = crate::utils::RESERVED_COMMANDS
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let doc =
            format!("<!-- zchk-doc-reserved:start -->\n{names}\n<!-- zchk-doc-reserved:end -->");
        std::fs::write(repo.path().join("README.md"), &doc).unwrap();
        assert_eq!(
            run_doc_reserved(repo.path()).outcome,
            super::super::BuiltinOutcome::Pass
        );
        std::fs::write(
            repo.path().join("README.md"),
            doc.replace("`update`", "`obsolete`"),
        )
        .unwrap();
        let result = run_doc_reserved(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Fail);
        assert!(result.details.contains("missing: update"), "{result:?}");
        assert!(
            result.details.contains("no longer has: obsolete"),
            "{result:?}"
        );
    }
}
