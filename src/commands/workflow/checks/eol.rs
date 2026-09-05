//! ZCHK-EOL-PINS: `.gitattributes` still carries the byte-exact dedupe pins
//! (issue #247) that make `zirv ctx compile`'s native-file skip comparison
//! safe across checkouts on any platform: `/CLAUDE.md -text`, `/AGENTS.md
//! -text`, and `/.zirv/context/*.md text eol=lf`. Losing any one of them
//! reintroduces the exact bug #247 fixed -- a Windows checkout with
//! `core.autocrlf=true` normalizing line endings would then make the
//! generated-file byte comparison always miss, injecting canonical content
//! twice on every launch. This check is also a witness marker in the sense
//! `common.md` uses the term: its presence in `.gitattributes` proves the
//! pins were never silently dropped by an unrelated edit.

use std::path::Path;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-EOL-PINS";
const PROVES: &str = ".gitattributes still anchors /CLAUDE.md -text, /AGENTS.md -text, and \
     /.zirv/context/*.md text eol=lf";
const FIX: &str = "restore the three anchored .gitattributes lines issue #247 added: \
     `/CLAUDE.md -text`, `/AGENTS.md -text`, `/.zirv/context/*.md text eol=lf` -- see that \
     file's own comment for why each is needed";
const ORIGIN: &str = "issue #247 -- an unpinned .gitattributes let core.autocrlf normalize the \
     render inputs/outputs differently per checkout, permanently breaking the byte-exact \
     dedupe skip comparison";

const REQUIRED_LINES: &[&str] = &[
    "/CLAUDE.md -text",
    "/AGENTS.md -text",
    "/.zirv/context/*.md text eol=lf",
];

pub fn run(repo: &Path) -> BuiltinCheckResult {
    let path = repo.join(".gitattributes");
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
    evaluate(&text)
}

fn evaluate(text: &str) -> BuiltinCheckResult {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let missing: Vec<&str> = REQUIRED_LINES
        .iter()
        .copied()
        .filter(|needle| !lines.contains(needle))
        .collect();

    if missing.is_empty() {
        BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!("all {} anchored pin(s) present", REQUIRED_LINES.len()),
        )
    } else {
        BuiltinCheckResult::fail(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!("missing pin(s): {}", missing.join(", ")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_is_inconclusive() {
        let repo = tempdir().unwrap();
        let result = run(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    #[test]
    fn all_pins_present_passes() {
        let result =
            evaluate("/CLAUDE.md -text\n/AGENTS.md -text\n/.zirv/context/*.md text eol=lf\n");
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_dropped_pin_fails() {
        let result = evaluate("/CLAUDE.md -text\n/AGENTS.md -text\n");
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("eol=lf"), "{result:?}");
    }

    #[test]
    fn the_real_gitattributes_has_every_pin() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
