//! Issue #276: a built-in self-check registry -- named invariants with
//! `proves:`/`fix:`/`origin:` labels, run by `zirv verify --builtin` (and by
//! plain `zirv verify` alongside `.zirv/verify.toml`/discovered checks). Each
//! module owns one or two checks and cites the incident it exists to prevent
//! in its own `ORIGIN` constant; this file is the registry plus the shared
//! result shape every check module returns.
//!
//! Deliberately capped at the table below (issue #276's own "discipline"
//! section): a new check must cite an origin and ship with a fixture that
//! fails without the fix, not be added freely.

pub mod argv;
pub mod docs;
pub mod eol;
pub mod forbidden;
pub mod hooks;
pub mod inventory;
pub mod version_bump;

use std::path::Path;

use serde::Serialize;

/// One built-in check's three-valued verdict. Mirrors `GateOutcome`'s
/// Pass/Fail/Inconclusive shape (issue #268's degraded-gate ban: an
/// `Inconclusive` check must block a gate exactly as hard as a `Failed` one)
/// rather than reusing that type directly -- `GateOutcome`'s own
/// `InconclusiveReason` enum is scoped to test-runner-output classification
/// (`ToolMissing`/`RunnerCrashed`/`NoTestsSelected`/...), and none of its
/// variants describe what makes a builtin check here inconclusive ("no git
/// available", "no base branch", "the doc's anchor comments are missing").
///
/// `NotApplicable` is the fourth verdict and the only non-blocking one
/// besides `Pass`: most checks here guard zirv's OWN source and README
/// invariants, which are a statement about this repository and no other. See
/// [`is_zirv_repo`], which decides that once, for every such check.
/// `Inconclusive` stays reserved for an input this repository is supposed to
/// have but that could not be found, read or parsed -- that really is a
/// degraded gate, and issue #268's ban still applies to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuiltinOutcome {
    Pass,
    Fail,
    Inconclusive,
    NotApplicable,
}

impl BuiltinOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Inconclusive => "inconclusive",
            Self::NotApplicable => "not-applicable",
        }
    }

    /// Whether this verdict lets `zirv verify` exit 0. A check that never
    /// applied here blocks nothing.
    pub fn is_passing(self) -> bool {
        matches!(self, Self::Pass | Self::NotApplicable)
    }
}

/// One check's report line: a stable `id`, its verdict, the `proves:`/
/// `fix:`/`origin:` labels issue #276 asks for (always present, not only on
/// failure, so `--json` carries the full story either way), and a free-text
/// `details` naming what was actually found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuiltinCheckResult {
    pub id: &'static str,
    pub outcome: BuiltinOutcome,
    pub proves: &'static str,
    pub fix: &'static str,
    pub origin: &'static str,
    pub details: String,
}

impl BuiltinCheckResult {
    pub fn pass(
        id: &'static str,
        proves: &'static str,
        fix: &'static str,
        origin: &'static str,
        details: impl Into<String>,
    ) -> Self {
        Self {
            id,
            outcome: BuiltinOutcome::Pass,
            proves,
            fix,
            origin,
            details: details.into(),
        }
    }

    pub fn fail(
        id: &'static str,
        proves: &'static str,
        fix: &'static str,
        origin: &'static str,
        details: impl Into<String>,
    ) -> Self {
        Self {
            id,
            outcome: BuiltinOutcome::Fail,
            proves,
            fix,
            origin,
            details: details.into(),
        }
    }

    pub fn not_applicable(
        id: &'static str,
        proves: &'static str,
        fix: &'static str,
        origin: &'static str,
        details: impl Into<String>,
    ) -> Self {
        Self {
            id,
            outcome: BuiltinOutcome::NotApplicable,
            proves,
            fix,
            origin,
            details: details.into(),
        }
    }

    pub fn inconclusive(
        id: &'static str,
        proves: &'static str,
        fix: &'static str,
        origin: &'static str,
        details: impl Into<String>,
    ) -> Self {
        Self {
            id,
            outcome: BuiltinOutcome::Inconclusive,
            proves,
            fix,
            origin,
            details: details.into(),
        }
    }
}

/// The `details` text every check uses when one of the zirv-repository files
/// it reads is simply not there.
pub fn absent_input(path: &Path) -> String {
    format!(
        "{} is absent -- this check reads zirv's own source/README files",
        path.display()
    )
}

/// Whether `repo` IS the zirv checkout, read from its own `[package] name`.
///
/// Review round 1 (R9): every zirv-specific check keyed `NotApplicable` on
/// its input FILE being absent, which answers the wrong question in both
/// directions -- an ordinary repository that happens to own a
/// `.gitattributes` was judged against zirv's invariants and FAILED, while
/// deleting one of those files inside the zirv checkout made the check that
/// guards it silently pass. Applicability is a fact about the repository, so
/// it is decided here, once, before any input is read; an absent input
/// inside the zirv repo stays `Inconclusive`.
pub fn is_zirv_repo(repo: &Path) -> bool {
    std::fs::read_to_string(repo.join("Cargo.toml"))
        .ok()
        .and_then(|manifest| version_bump::parse_package_field(&manifest, "name"))
        .as_deref()
        == Some("zirv")
}

/// The `details` text a zirv-specific check reports when `repo` is some other
/// repository entirely -- the one `NotApplicable` reason that is a statement
/// about the repository rather than about the invariant.
pub fn not_the_zirv_repo(repo: &Path) -> String {
    format!(
        "{} is not the zirv repository -- this check guards zirv's own source/README invariants",
        repo.display()
    )
}

/// Every builtin check id, in the fixed run order -- also the completeness
/// list `every_id_in_all_ids_is_actually_produced_by_run_all` guards.
pub const ALL_IDS: &[&str] = &[
    version_bump::ID,
    argv::CODEX_ID,
    argv::CLAUDE_ID,
    forbidden::ID,
    docs::DOC_EXIT_CODES_ID,
    docs::DOC_RESERVED_ID,
    hooks::ID,
    eol::ID,
    inventory::ID,
];

/// Runs every registered builtin check against `repo`, skipping any id in
/// `exclude` (`workflow.builtin_checks_exclude`, REPO_FORBIDDEN) -- so an
/// excluded check is simply absent from the report rather than reported
/// `Skipped`, mirroring how `only`/`--check` narrows `verification::run_mode`
/// today. Order matches `ALL_IDS`.
pub fn run_all(repo: &Path, exclude: &[String]) -> Vec<BuiltinCheckResult> {
    let mut checks = vec![
        version_bump::run(repo),
        argv::run_codex_exec(repo),
        argv::run_claude_headless(repo),
        forbidden::run(repo),
        docs::run_doc_exit_codes(repo),
        docs::run_doc_reserved(repo),
        hooks::run(repo),
        eol::run(repo),
        inventory::run(repo),
    ];
    checks.retain(|check| !exclude.iter().any(|excluded| excluded == check.id));
    checks
}

/// Test fixtures declare which repository they stand for the same way
/// [`is_zirv_repo`] reads it, so a check module's own fixture is explicit
/// about whether zirv's invariants apply to it at all.
#[cfg(test)]
fn write_manifest(repo: &Path, name: &str) {
    std::fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
    )
    .expect("write Cargo.toml");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL_IDS` and `run_all`'s own unfiltered output must name exactly the
    /// same set: a check added to one and not the other is either invisible
    /// to `--json`'s completeness or silently never run.
    #[test]
    fn all_ids_matches_what_run_all_actually_produces() {
        let repo = tempfile::tempdir().unwrap();
        let produced: Vec<&str> = run_all(repo.path(), &[])
            .iter()
            .map(|check| check.id)
            .collect();
        assert_eq!(produced, ALL_IDS);
    }

    /// Most of these checks read zirv's OWN files (`src/commands/ctx/
    /// config.rs`, `README.md`, `.gitattributes`). In any other
    /// repository those inputs are simply absent, which says nothing about
    /// that repository -- reporting it as `Inconclusive` (or, for the version
    /// bump, `Fail`) made `zirv verify` unable to exit 0 anywhere outside the
    /// zirv checkout, and `workflow.builtin_checks_exclude` is REPO_FORBIDDEN
    /// so a repo cannot opt out either.
    #[test]
    fn checks_whose_inputs_are_absent_are_not_applicable_not_inconclusive() {
        let repo = tempfile::tempdir().unwrap();
        let produced = run_all(repo.path(), &[]);
        let unresolved: Vec<(&str, &str, &str)> = produced
            .iter()
            .filter(|check| {
                matches!(
                    check.outcome,
                    BuiltinOutcome::Fail | BuiltinOutcome::Inconclusive
                )
            })
            .map(|check| (check.id, check.outcome.as_str(), check.details.as_str()))
            .collect();
        assert!(
            unresolved.is_empty(),
            "a check whose required input is simply absent must be not-applicable: {unresolved:?}"
        );
        let not_applicable: Vec<&str> = produced
            .iter()
            .filter(|check| check.outcome == BuiltinOutcome::NotApplicable)
            .map(|check| check.id)
            .collect();
        assert_eq!(
            not_applicable,
            vec![
                version_bump::ID,
                forbidden::ID,
                docs::DOC_EXIT_CODES_ID,
                docs::DOC_RESERVED_ID,
                eol::ID,
                inventory::ID,
            ],
            "the repo-independent checks (argv, hooks) must still report a real verdict"
        );
    }

    use super::write_manifest as manifest;

    /// Review round 1 (R9): keying `NotApplicable` on the input FILE being
    /// absent answers the wrong question. An ordinary repository that happens
    /// to own a `.gitattributes` was judged against zirv's own invariants and
    /// FAILED -- the very "cannot exit 0 anywhere outside the zirv checkout"
    /// symptom the verdict was added to fix, just moved to the repositories
    /// that do have such a file.
    #[test]
    fn a_non_zirv_repo_that_owns_a_lookalike_input_is_still_not_applicable() {
        let repo = tempfile::tempdir().unwrap();
        manifest(repo.path(), "some-other-crate");
        std::fs::write(repo.path().join(".gitattributes"), "* text=auto\n").unwrap();

        let produced = run_all(repo.path(), &[]);
        let blocking: Vec<(&str, &str, &str)> = produced
            .iter()
            .filter(|check| !check.outcome.is_passing())
            .map(|check| (check.id, check.outcome.as_str(), check.details.as_str()))
            .collect();
        assert!(
            blocking.is_empty(),
            "zirv's own invariants must not be judged against another repository: {blocking:?}"
        );
    }

    /// The other direction of the same rule: inside the zirv checkout an
    /// absent input is a real problem (someone deleted `.gitattributes`), so
    /// it must never be the non-blocking `NotApplicable` verdict.
    #[test]
    fn an_absent_input_inside_the_zirv_repo_is_not_a_pass() {
        let repo = tempfile::tempdir().unwrap();
        manifest(repo.path(), "zirv");

        let produced = run_all(repo.path(), &[]);
        for id in [
            forbidden::ID,
            docs::DOC_EXIT_CODES_ID,
            docs::DOC_RESERVED_ID,
            eol::ID,
            inventory::ID,
        ] {
            let check = produced
                .iter()
                .find(|check| check.id == id)
                .expect("every id is produced");
            assert_eq!(
                check.outcome,
                BuiltinOutcome::Inconclusive,
                "a missing input inside zirv's own checkout is not a pass: {check:?}"
            );
        }
    }

    #[test]
    fn excluded_ids_are_absent_from_the_report() {
        let repo = tempfile::tempdir().unwrap();
        let excluded = vec![eol::ID.to_string()];
        let produced = run_all(repo.path(), &excluded);
        assert!(!produced.iter().any(|check| check.id == eol::ID));
        assert_eq!(produced.len(), ALL_IDS.len() - 1);
    }
}
