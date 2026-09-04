//! Issue #355: the installed binary is the authority for its own syntax.
//! `zirv --skill`/`zirv skill` prints this bundled operator orientation,
//! release-matched to the running binary's version; `--json` gives the same
//! content machine-readable, with its byte and (very rough) token cost.
//! `zirv commands --json` (`commands::command_schema`) is this skill's
//! companion: the generated command schema this text keeps pointing an
//! agent at instead of restating command syntax itself.

use serde::Serialize;

/// The bundled operator skill template, substituted by [`render`]. Kept
/// small deliberately -- a floor an agent can hold in full, not a manual --
/// see `skill_stays_under_the_committed_ceiling`.
pub const SKILL: &str = include_str!("../assets/zirv-skill.md");

/// Substitutes the crate version into the bundled skill template.
pub fn render(version: &str) -> String {
    SKILL.replace("{version}", version)
}

/// One entry per `## ` heading in [`SKILL`], in document order -- the same
/// text [`to_json`]'s `sections` field reports, so the two can never drift
/// apart into two independently typed lists of the same headings.
fn sections() -> Vec<&'static str> {
    SKILL
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillJson {
    pub version: String,
    pub bytes: usize,
    pub approx_tokens: usize,
    pub sections: Vec<String>,
}

/// The rendered skill's machine-readable form: not the rendered text itself
/// (an agent that wants that runs `zirv --skill` without `--json`), but its
/// version, size, and section headings, so a caller can decide whether to
/// fetch it at all before spending the bytes.
pub fn to_json(version: &str) -> SkillJson {
    let rendered = render(version);
    let bytes = rendered.len();
    SkillJson {
        version: version.to_string(),
        bytes,
        approx_tokens: crate::commands::ctx::compile::estimate_tokens(bytes),
        sections: sections().into_iter().map(str::to_string).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A floor, not a manual -- see `DEFAULT_PROMPT`'s own `< 3500` ceiling
    /// test in `prompt.rs`, which this mirrors.
    #[test]
    fn skill_stays_under_the_committed_ceiling() {
        assert!(
            SKILL.len() <= 4096,
            "the bundled skill is {} bytes, over the 4096 byte ceiling",
            SKILL.len()
        );
    }

    #[test]
    fn render_substitutes_the_version_and_leaves_no_placeholder_behind() {
        let rendered = render("9.9.9");
        assert!(rendered.contains("9.9.9"));
        assert!(!rendered.contains("{version}"));
    }

    #[test]
    fn render_is_release_matched_to_the_running_binary() {
        let version = env!("CARGO_PKG_VERSION");
        let rendered = render(version);
        assert!(
            rendered.contains(version),
            "the rendered skill must name this binary's own version"
        );
    }

    #[test]
    fn to_json_reports_version_bytes_tokens_and_sections() {
        let json = to_json("1.2.3");
        assert_eq!(json.version, "1.2.3");
        assert_eq!(json.bytes, render("1.2.3").len());
        assert_eq!(
            json.approx_tokens,
            crate::commands::ctx::compile::estimate_tokens(json.bytes)
        );
        assert!(!json.sections.is_empty());
    }

    /// The activation guard is the single most safety-relevant instruction
    /// in this skill: an agent must stop rather than operate a session it
    /// cannot verify is registered.
    #[test]
    fn the_skill_teaches_the_activation_guard() {
        assert!(SKILL.contains("ZIRV_CTX_SESSION"));
        assert!(SKILL.to_lowercase().contains("stop"));
    }

    /// The whole point of this skill (issue #355): point at the generated
    /// surfaces instead of restating command syntax that can drift.
    #[test]
    fn the_skill_points_at_the_generated_command_surface() {
        assert!(SKILL.contains("zirv commands --json"));
        assert!(SKILL.contains("zirv --skill") || SKILL.contains("`zirv skill`"));
    }

    /// Opaque ids: the skill must teach reading returned ids, never
    /// predicting them.
    #[test]
    fn the_skill_teaches_opaque_ids_are_read_not_predicted() {
        let lower = SKILL.to_lowercase();
        assert!(lower.contains("opaque"));
        assert!(lower.contains("never predict") || lower.contains("never guess"));
    }

    /// Acceptance (issue #355): help text, the generated command schema, and
    /// this skill must stay compatible. Every `` `zirv ...` `` code span in
    /// the skill (a bare placeholder like `` `zirv <cmd> --help` `` aside) is
    /// resolved against `command_schema::command_entries`'s real leaf paths
    /// -- either an exact match or a valid namespace prefix (`` `zirv
    /// workflow` `` names a whole subtree, not one leaf) -- so a renamed or
    /// removed command is caught here instead of silently teaching a model
    /// to run something that no longer exists.
    #[test]
    fn every_command_the_skill_names_exists_in_the_generated_schema() {
        let entries = crate::commands::command_schema::command_entries()
            .expect("the command schema must classify cleanly");
        let known_paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();

        let mut checked = 0;
        let spans = SKILL.split('`');
        for (index, span) in spans.enumerate() {
            // `split('`')` alternates outside (even) / inside (odd) a pair
            // of backticks, since the skill's own backticks are all matched
            // pairs (never an unescaped lone backtick).
            if index % 2 == 0 || !span.starts_with("zirv ") || span.contains('<') {
                continue;
            }
            let path: String = span
                .split_whitespace()
                .take_while(|token| !token.starts_with('-'))
                .collect::<Vec<_>>()
                .join(" ");
            checked += 1;
            assert!(
                known_paths.iter().any(|known| {
                    *known == path
                        || known
                            .strip_prefix(&path)
                            .is_some_and(|rest| rest.starts_with(' '))
                }),
                "the skill names '{span}' (resolved to '{path}'), which does not exist in \
                 the generated command schema"
            );
        }
        assert!(
            checked >= 5,
            "expected the skill to name several real commands in backticks; only checked {checked}"
        );
    }

    /// The bundled skill is deliberately harness-neutral -- one template for
    /// every adapter, mirroring `prompt::HARNESS_PROMPT`'s own "no
    /// vendor-specific vocabulary" discipline. `tests/fixtures/skill/
    /// {claude,codex}.txt` pin that: both are byte-identical golden copies
    /// of the shipped template, so a future edit that quietly introduces
    /// per-harness command drift (a claude-only or codex-only verb) fails
    /// here instead of shipping silently.
    #[test]
    fn the_skill_carries_no_harness_specific_command_drift() {
        let claude = include_str!("../tests/fixtures/skill/claude.txt");
        let codex = include_str!("../tests/fixtures/skill/codex.txt");
        assert_eq!(
            claude, codex,
            "the skill must name the same verbs for every harness"
        );
        assert_eq!(
            claude, SKILL,
            "the fixtures must mirror the shipped skill template exactly"
        );
    }
}
