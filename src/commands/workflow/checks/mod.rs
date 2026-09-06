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
pub mod decision_graph;
pub mod docs;
pub mod eol;
pub mod forbidden;
pub mod hooks;
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
/// besides `Pass`: most checks here read zirv's OWN files, and their absence
/// in some other repository is a statement about the repository, not about
/// the invariant. `Inconclusive` stays reserved for an input that EXISTS but
/// could not be read or parsed -- that really is a degraded gate, and issue
/// #268's ban still applies to it.
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
/// it reads is simply not there -- one wording, so `not-applicable` always
/// names the missing input and says why that is not a verdict about the
/// repository being verified.
pub fn absent_input(path: &Path) -> String {
    format!(
        "{} is absent -- this check reads zirv's own source/vault files, so it does not apply to \
         this repository",
        path.display()
    )
}

/// Every builtin check id, in the fixed run order -- also the completeness
/// list `every_id_in_all_ids_is_actually_produced_by_run_all` guards.
pub const ALL_IDS: &[&str] = &[
    version_bump::ID,
    argv::CODEX_ID,
    argv::CLAUDE_ID,
    forbidden::ID,
    docs::UNIX_TESTS_ID,
    docs::DOC_VERBS_ID,
    decision_graph::ID,
    hooks::ID,
    eol::ID,
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
        docs::run_unix_tests_doc(repo),
        docs::run_doc_verbs(repo),
        decision_graph::run(repo),
        hooks::run(repo),
        eol::run(repo),
    ];
    checks.retain(|check| !exclude.iter().any(|excluded| excluded == check.id));
    checks
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
    /// config.rs`, `docs/obsidian/...`, `.gitattributes`). In any other
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
                docs::UNIX_TESTS_ID,
                docs::DOC_VERBS_ID,
                decision_graph::ID,
                eol::ID,
            ],
            "the repo-independent checks (argv, hooks) must still report a real verdict"
        );
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
