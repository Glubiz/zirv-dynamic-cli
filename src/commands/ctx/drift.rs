//! Deterministic drift detection across instruction surfaces (issue #42):
//! duplicate, contradiction, and precedence/shadowing findings between
//! zirv's canonical `.zirv/context/` layer (issue #41, `context.rs`) and
//! every native CLAUDE.md/AGENTS.md surface `optimize::collect_surfaces`
//! already collects.
//!
//! Built entirely on `optimize.rs`'s existing normalization
//! (`optimize::normalize`/`optimize::statements`, via the shared
//! `optimize::all_instructions`/`group_by_normalized` helpers `lint_redundancy`
//! also uses) -- lexical/structural only, no model call, in keeping with
//! issue #42's "prefer deterministic lexical/structural analysis first".
//! Report-only, the same guarantee `optimize.rs` itself carries: `analyze`
//! takes already-collected `Surface` text and returns `Finding`s only, with
//! no filesystem write path anywhere in this module for a caller to
//! accidentally reach.
//!
//! **Informational vs. behavior-changing**, per issue #42's acceptance
//! criteria: every finding kind here is `Severity::Info` -- worth tidying,
//! changes nothing about what a session actually does -- except
//! `"contradiction"` (`Severity::Warning`), the one kind where two surfaces
//! disagree about the same rule and which one a session follows depends on
//! precedence that may not have been intended. Opposite-polarity wording
//! across *different* harnesses (fix round 1, review finding 12-1) is a
//! separate, deliberately `Info`-severity kind, `"differs-per-harness"`: a
//! Claude-specific and a Codex-specific file disagreeing is the intended
//! per-harness customization issue #41 exists to allow, not a bug -- only a
//! same-provider pair, or a pair where either side is the harness-neutral
//! canonical layer (`Provider::Zirv`), can actually contradict itself.
//!
//! **Scope note.** The near-duplicate/contradiction pass compares every pair
//! of cross-surface instructions using fixed Jaccard/negation-token
//! heuristics -- lexical overlap, not semantic understanding, and not
//! profiled against a deliberately adversarial input. Nominally `O(n^2)` in
//! the total bullet count, but each pair is pruned by a cheap token-count
//! ratio check (a necessary condition for the Jaccard threshold: Jaccard
//! similarity is always `<= min(|A|,|B|) / max(|A|,|B|)`, so the ratio check
//! can only reject pairs the full Jaccard computation would have rejected
//! too) before either side's token set is built or intersected, and total
//! emitted pair-findings are capped (`MAX_PAIR_FINDINGS`) with an explicit
//! truncation note rather than growing unbounded or truncating silently.
//! `optimize.rs` already bounds the input (`MAX_SURFACES`,
//! `cfg.optimize.max_surface_bytes`), which is the scope this module
//! inherits rather than adding its own surface-count cap.

use std::collections::BTreeSet;

use super::context;
use super::optimize::{self, Finding, Instruction, Layer, Severity, Surface};
use super::surface::Provider;

/// Above this Jaccard token-overlap ratio, two cross-surface instructions are
/// treated as the same rule in different words. A deliberately conservative
/// bound: a missed near-duplicate is cheaper than two unrelated rules
/// reported as one.
const NEAR_DUPLICATE_JACCARD: f64 = 0.7;

/// Fixed, deterministic negation/polarity markers, checked as whole tokens
/// against `Instruction::normalized` (never a substring match -- `normalize`
/// strips punctuation, so a contraction like "don't" already arrives as
/// separate tokens `don`/`t`). Two near-duplicate instructions whose
/// negation-token sets differ are read as disagreeing about the same rule
/// (a contradiction) rather than merely restating it (a near-duplicate).
/// Heuristic, not semantic understanding -- see the module doc's scope note.
const NEGATION_TOKENS: &[&str] = &[
    "never",
    "not",
    "no",
    "cannot",
    "disallow",
    "disallowed",
    "forbid",
    "forbidden",
    "prohibited",
    "disable",
    "disabled",
    "won",
    "don",
    "shouldn",
    "isn",
    "doesn",
];

fn token_set(normalized: &str) -> BTreeSet<&str> {
    normalized.split_whitespace().collect()
}

fn jaccard(a: &BTreeSet<&str>, b: &BTreeSet<&str>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

fn negation_tokens(normalized: &str) -> BTreeSet<&'static str> {
    let present = token_set(normalized);
    NEGATION_TOKENS
        .iter()
        .copied()
        .filter(|marker| present.contains(marker))
        .collect()
}

/// Hard cap on how many near-duplicate/contradiction findings one `analyze`
/// call will emit. A capped run says so explicitly (a
/// `"near-duplicate-scan-truncated"` finding naming the omitted count)
/// rather than growing unbounded or truncating silently.
const MAX_PAIR_FINDINGS: usize = 200;

fn layer_of(surfaces: &[Surface], instruction: &Instruction) -> Layer {
    surfaces[instruction.surface].layer
}

/// Whether an opposite-polarity pair is eligible to be reported as an actual
/// `"contradiction"` rather than downgraded to `"differs-per-harness"`: both
/// sides target the same harness, or either side is the harness-neutral
/// canonical layer (`Provider::Zirv`) -- a canonical rule disagreeing with a
/// harness-specific one is still a real conflict, since the canonical layer
/// is supposed to apply to every harness alike.
fn shares_provider_or_canonical(surfaces: &[Surface], a: &Instruction, b: &Instruction) -> bool {
    let (provider_a, provider_b) = (
        layer_of(surfaces, a).provider(),
        layer_of(surfaces, b).provider(),
    );
    provider_a == provider_b || provider_a == Provider::Zirv || provider_b == Provider::Zirv
}

/// Every finding this module can produce, over already-collected surfaces.
/// `surfaces` is normally `optimize::collect_surfaces`'s own output --
/// canonical and native surfaces mixed, exactly as issue #42 asks for.
// No production caller yet -- issue #42 (this task) is report-only findings
// production; Task 16 (`zirv context status`, issue #46) is the first real
// consumer, for duplicate/conflict counts. Same dormancy pattern as
// `optimize.rs`'s own `Layer::trust`/`Surface::context_surface`; this
// module's own tests exercise every path in the meantime.
#[allow(dead_code)]
pub fn analyze(surfaces: &[Surface]) -> Vec<Finding> {
    let mut findings = duplicate_findings(surfaces);
    findings.extend(near_duplicate_and_contradiction_findings(surfaces));
    findings
}

/// Exact-duplicate groups (same normalized text), classified by which layers
/// they span: a rule already stated in the canonical common layer and
/// restated elsewhere ("redundant with canonical"), a rule duplicated across
/// harness-specific-only surfaces with no canonical home yet (a
/// canonicalization candidate), or a plain exact duplicate elsewhere.
/// Additionally reports precedence/shadowing whenever a group's layers span
/// more than one `context::PrecedenceTier`.
fn duplicate_findings(surfaces: &[Surface]) -> Vec<Finding> {
    let all = optimize::all_instructions(surfaces);
    let (order, groups) = optimize::group_by_normalized(&all);

    let mut findings = Vec::new();
    for key in order {
        let Some(group) = groups.get(&key) else {
            continue;
        };
        if group.len() < 2 {
            continue;
        }

        let evidence: Vec<String> = group
            .iter()
            .copied()
            .map(|i| optimize::evidence_ref(&surfaces[i.surface], i.line))
            .collect();
        let text = &group[0].text;

        let has_common = group
            .iter()
            .copied()
            .any(|i| layer_of(surfaces, i) == Layer::ContextCommon);
        let distinct_surfaces: BTreeSet<usize> = group.iter().copied().map(|i| i.surface).collect();

        let (kind, title_prefix, detail) = if has_common {
            (
                "duplicate-redundant-with-canonical",
                "Already covered by the canonical common layer",
                "This rule is already stated in `.zirv/context/common.md`; the other copy is \
                 redundant and can be removed from its own file.",
            )
        } else if distinct_surfaces.len() > 1 {
            (
                "duplicate-canonicalizable",
                "Stated the same way in more than one file",
                "No canonical common layer states this yet, but it already appears in more than \
                 one file -- a candidate to move into `.zirv/context/common.md` instead of \
                 maintaining separate copies.",
            )
        } else {
            (
                "duplicate-exact",
                "Stated more than once",
                "The same instruction appears more than once in the same file.",
            )
        };

        findings.push(Finding {
            kind,
            severity: Severity::Info,
            title: format!("{title_prefix}: {text}"),
            evidence: evidence.clone(),
            detail: detail.to_string(),
            proposed_diff: None,
        });

        let tiered: Vec<(&Instruction, context::PrecedenceTier)> = group
            .iter()
            .copied()
            .filter_map(|i| context::precedence_tier(layer_of(surfaces, i)).map(|t| (i, t)))
            .collect();
        let tiers: BTreeSet<context::PrecedenceTier> = tiered.iter().map(|(_, t)| *t).collect();
        if tiers.len() > 1 {
            let (winner, winner_tier) = tiered
                .iter()
                .max_by_key(|(_, t)| *t)
                .expect("group is non-empty");
            findings.push(Finding {
                kind: "precedence-shadowing",
                severity: Severity::Info,
                title: format!("Stated at more than one precedence level: {text}"),
                evidence,
                detail: format!(
                    "{} ({}) takes precedence over the other copy of this rule.",
                    optimize::evidence_ref(&surfaces[winner.surface], winner.line),
                    winner_tier.label()
                ),
                proposed_diff: None,
            });
        }
    }

    findings
}

/// A single instruction's precomputed comparison data, built once per
/// instruction rather than once per pair (fix round 1, review finding 12-3
/// -- the original pairwise loop rebuilt both `BTreeSet`s on every
/// comparison).
struct Profile<'a> {
    instruction: &'a Instruction,
    tokens: BTreeSet<&'a str>,
    negation: BTreeSet<&'static str>,
}

/// Cross-surface pairs whose normalized text is similar but not identical
/// (exact duplicates are `duplicate_findings`'s job). An opposite-polarity
/// pair is a `contradiction` when eligible (`shares_provider_or_canonical`),
/// otherwise a same-meaning-different-harness pair is expected divergence,
/// not a bug (`"differs-per-harness"`) -- see the module doc.
fn near_duplicate_and_contradiction_findings(surfaces: &[Surface]) -> Vec<Finding> {
    let all = optimize::all_instructions(surfaces);
    let profiles: Vec<Profile> = all
        .iter()
        .map(|instruction| Profile {
            instruction,
            tokens: token_set(&instruction.normalized),
            negation: negation_tokens(&instruction.normalized),
        })
        .collect();

    let mut findings = Vec::new();
    let mut omitted = 0usize;

    for i in 0..profiles.len() {
        for j in (i + 1)..profiles.len() {
            let (a, b) = (&profiles[i], &profiles[j]);
            if a.instruction.surface == b.instruction.surface
                || a.instruction.normalized == b.instruction.normalized
            {
                continue;
            }

            // Cheap necessary condition, checked before either token set is
            // intersected/unioned: Jaccard similarity is always
            // `<= min(|A|,|B|) / max(|A|,|B|)`, so a pair whose token-count
            // ratio already falls short of the threshold can never pass the
            // full Jaccard check either.
            let (len_a, len_b) = (a.tokens.len(), b.tokens.len());
            if len_a == 0 || len_b == 0 {
                continue;
            }
            let (shorter, longer) = if len_a <= len_b {
                (len_a, len_b)
            } else {
                (len_b, len_a)
            };
            if (shorter as f64) / (longer as f64) < NEAR_DUPLICATE_JACCARD {
                continue;
            }
            if jaccard(&a.tokens, &b.tokens) < NEAR_DUPLICATE_JACCARD {
                continue;
            }

            if findings.len() >= MAX_PAIR_FINDINGS {
                omitted += 1;
                continue;
            }

            let evidence = vec![
                optimize::evidence_ref(&surfaces[a.instruction.surface], a.instruction.line),
                optimize::evidence_ref(&surfaces[b.instruction.surface], b.instruction.line),
            ];
            let is_contradiction = a.negation != b.negation;
            let (a_text, b_text) = (&a.instruction.text, &b.instruction.text);

            findings.push(if !is_contradiction {
                Finding {
                    kind: "near-duplicate",
                    severity: Severity::Info,
                    title: format!("Near-duplicate wording: \"{a_text}\" vs \"{b_text}\""),
                    evidence,
                    detail: "These two instructions say close to the same thing in different \
                             wording; consider consolidating into one copy."
                        .to_string(),
                    proposed_diff: None,
                }
            } else if shares_provider_or_canonical(surfaces, a.instruction, b.instruction) {
                Finding {
                    kind: "contradiction",
                    severity: Severity::Warning,
                    title: format!("Possible contradiction: \"{a_text}\" vs \"{b_text}\""),
                    evidence,
                    detail: "These two instructions read as the same rule stated with opposite \
                             polarity. Which one a session actually follows depends on \
                             precedence, which may not be what was intended."
                        .to_string(),
                    proposed_diff: None,
                }
            } else {
                Finding {
                    kind: "differs-per-harness",
                    severity: Severity::Info,
                    title: format!("Differs per harness: \"{a_text}\" vs \"{b_text}\""),
                    evidence,
                    detail: "These two instructions read as the same rule stated with opposite \
                             polarity, but target different harnesses -- likely intended \
                             per-harness customization rather than a real conflict."
                        .to_string(),
                    proposed_diff: None,
                }
            });
        }
    }

    if omitted > 0 {
        findings.push(Finding {
            kind: "near-duplicate-scan-truncated",
            severity: Severity::Info,
            title: format!(
                "Near-duplicate/contradiction scan capped at {MAX_PAIR_FINDINGS} findings"
            ),
            evidence: Vec::new(),
            detail: format!(
                "{omitted} additional qualifying pair(s) were found but not reported. Narrow \
                 the surface set (fewer nested files, a smaller repo subtree) to see them."
            ),
            proposed_diff: None,
        });
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn surface(layer: Layer, path: &str, text: &str) -> Surface {
        Surface {
            layer,
            path: PathBuf::from(path),
            text: text.to_string(),
        }
    }

    #[test]
    fn a_rule_in_common_and_restated_natively_is_redundant_with_canonical() {
        let surfaces = vec![
            surface(
                Layer::ContextCommon,
                "/repo/.zirv/context/common.md",
                "- always run tests\n",
            ),
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- always run tests\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "duplicate-redundant-with-canonical")
            .unwrap_or_else(|| panic!("no redundant-with-canonical finding: {findings:#?}"));
        assert_eq!(finding.severity, Severity::Info);
        assert!(finding.evidence.iter().any(|e| e.contains("common.md")));
        assert!(finding.evidence.iter().any(|e| e.contains("CLAUDE.md")));
        assert!(finding.proposed_diff.is_none());
    }

    #[test]
    fn a_rule_duplicated_across_two_native_files_with_no_canonical_common_is_canonicalizable() {
        let surfaces = vec![
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- prefer rg over grep\n",
            ),
            surface(
                Layer::RepoAgentsMd,
                "/repo/AGENTS.md",
                "- prefer rg over grep\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "duplicate-canonicalizable")
            .unwrap_or_else(|| panic!("no canonicalizable finding: {findings:#?}"));
        assert!(finding.evidence.iter().any(|e| e.contains("CLAUDE.md")));
        assert!(finding.evidence.iter().any(|e| e.contains("AGENTS.md")));
        assert!(
            findings
                .iter()
                .all(|f| f.kind != "duplicate-redundant-with-canonical"),
            "no ContextCommon surface exists: {findings:#?}"
        );
    }

    #[test]
    fn a_rule_repeated_within_one_file_is_a_plain_exact_duplicate() {
        let surfaces = vec![surface(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- always run tests\n- do something else\n- always run tests\n",
        )];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "duplicate-exact")
            .unwrap_or_else(|| panic!("no plain duplicate finding: {findings:#?}"));
        assert_eq!(finding.evidence.len(), 2);
        assert!(
            findings
                .iter()
                .all(|f| f.kind != "duplicate-canonicalizable"),
            "a same-file repeat spans only one surface: {findings:#?}"
        );
    }

    /// The canonical layer (`CanonicalCommon` tier) and a native file
    /// (`Native` tier) disagree about which copy wins -- the native file
    /// does, since it composes last (`context::PrecedenceTier`'s own doc).
    #[test]
    fn a_duplicate_spanning_two_precedence_tiers_is_flagged_as_shadowing() {
        let surfaces = vec![
            surface(
                Layer::ContextCommon,
                "/repo/.zirv/context/common.md",
                "- always run tests\n",
            ),
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- always run tests\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "precedence-shadowing")
            .unwrap_or_else(|| panic!("no shadowing finding: {findings:#?}"));
        assert!(
            finding.detail.contains("CLAUDE.md") && finding.detail.contains("native"),
            "the native file should be named as the winner: {finding:?}"
        );
    }

    /// Two same-tier native duplicates (both `Native`) must not spuriously
    /// earn a shadowing finding -- there is only one precedence level here.
    #[test]
    fn a_duplicate_within_the_same_precedence_tier_is_not_shadowing() {
        let surfaces = vec![
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- prefer rg over grep\n",
            ),
            surface(
                Layer::RepoAgentsMd,
                "/repo/AGENTS.md",
                "- prefer rg over grep\n",
            ),
        ];

        let findings = analyze(&surfaces);
        assert!(
            findings.iter().all(|f| f.kind != "precedence-shadowing"),
            "both surfaces are Native tier: {findings:#?}"
        );
    }

    #[test]
    fn similar_wording_across_surfaces_is_a_near_duplicate() {
        let surfaces = vec![
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- always run tests before committing\n",
            ),
            surface(
                Layer::ContextClaude,
                "/repo/.zirv/context/claude.md",
                "- always run the tests before committing\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "near-duplicate")
            .unwrap_or_else(|| panic!("no near-duplicate finding: {findings:#?}"));
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(finding.evidence.len(), 2);
    }

    /// The one behavior-changing kind: opposite polarity over otherwise
    /// near-identical wording, within the same provider (two Claude
    /// surfaces) -- a real conflict, since both are read by the same
    /// harness.
    #[test]
    fn opposite_polarity_within_the_same_provider_is_a_contradiction() {
        let surfaces = vec![
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- always commit with a descriptive message explaining the change\n",
            ),
            surface(
                Layer::NestedClaudeMd,
                "/repo/crates/inner/CLAUDE.md",
                "- never commit with a descriptive message explaining the change\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "contradiction")
            .unwrap_or_else(|| panic!("no contradiction finding: {findings:#?}"));
        assert_eq!(finding.severity, Severity::Warning);
        assert!(
            findings
                .iter()
                .all(|f| f.kind != "near-duplicate" && f.kind != "differs-per-harness"),
            "opposite polarity within one provider must be a contradiction: {findings:#?}"
        );
    }

    /// Fix round 1, review finding 12-1: a Claude-specific file and a
    /// Codex-specific file disagreeing is the intended per-harness
    /// customization issue #41 exists to allow, not a bug -- this must
    /// downgrade to the informational `"differs-per-harness"` kind, never
    /// the behavior-changing `"contradiction"`.
    #[test]
    fn opposite_polarity_across_harnesses_differs_per_harness_not_a_contradiction() {
        let surfaces = vec![
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- always commit with a descriptive message explaining the change\n",
            ),
            surface(
                Layer::RepoAgentsMd,
                "/repo/AGENTS.md",
                "- never commit with a descriptive message explaining the change\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "differs-per-harness")
            .unwrap_or_else(|| panic!("no differs-per-harness finding: {findings:#?}"));
        assert_eq!(finding.severity, Severity::Info);
        assert!(
            findings.iter().all(|f| f.kind != "contradiction"),
            "cross-harness divergence must not count as a behavior-changing conflict: {findings:#?}"
        );
    }

    /// The canonical common layer (`Provider::Zirv`) disagreeing with a
    /// harness-specific file is still a real contradiction: the canonical
    /// layer is supposed to apply to every harness alike, so a
    /// Codex-specific override contradicting it is not "expected
    /// divergence" the way two harness-specific files diverging is.
    #[test]
    fn opposite_polarity_against_the_canonical_common_layer_is_still_a_contradiction() {
        let surfaces = vec![
            surface(
                Layer::ContextCommon,
                "/repo/.zirv/context/common.md",
                "- always commit with a descriptive message explaining the change\n",
            ),
            surface(
                Layer::RepoAgentsMd,
                "/repo/AGENTS.md",
                "- never commit with a descriptive message explaining the change\n",
            ),
        ];

        let findings = analyze(&surfaces);
        let finding = findings
            .iter()
            .find(|f| f.kind == "contradiction")
            .unwrap_or_else(|| panic!("no contradiction finding: {findings:#?}"));
        assert_eq!(finding.severity, Severity::Warning);
        assert!(findings.iter().all(|f| f.kind != "differs-per-harness"));
    }

    #[test]
    fn unrelated_instructions_produce_no_findings() {
        let surfaces = vec![
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- use rg not grep\n",
            ),
            surface(
                Layer::RepoAgentsMd,
                "/repo/AGENTS.md",
                "- keep functions under fifty lines\n",
            ),
        ];
        assert!(analyze(&surfaces).is_empty());
    }

    /// Settings surfaces carry no bullet-line instructions in this model
    /// (`Layer::is_settings`) -- a JSON/TOML value that happens to look like
    /// a bullet must never be quoted in a drift finding, the same guard
    /// `lint_redundancy` already relies on via the shared `all_instructions`.
    #[test]
    fn settings_surfaces_are_never_treated_as_instructions() {
        let surfaces = vec![surface(
            Layer::ProjectSettings,
            "/repo/.claude/settings.json",
            "{\"hooks\": {\"note\": \"- always run tests\"}}",
        )];
        assert!(analyze(&surfaces).is_empty());
    }

    /// Exceeding `MAX_PAIR_FINDINGS` must report the truncation finding and
    /// keep the total bounded rather than growing unboundedly or truncating
    /// silently. 22 surfaces each state a near-duplicate of the same rule
    /// with one unique word swapped in -- every cross-surface pair clears
    /// the 0.7 Jaccard threshold without ever being an exact duplicate, so
    /// all `C(22, 2) = 231` pairs qualify, comfortably over the 200 cap.
    #[test]
    fn exceeding_the_pair_finding_cap_reports_truncation_and_stays_bounded() {
        let surfaces: Vec<Surface> = (0..22)
            .map(|i| {
                surface(
                    Layer::NestedClaudeMd,
                    &format!("/repo/dir{i}/CLAUDE.md"),
                    &format!("- always keep functions short and readable unique{i}\n"),
                )
            })
            .collect();

        let findings = analyze(&surfaces);
        let truncation = findings
            .iter()
            .find(|f| f.kind == "near-duplicate-scan-truncated")
            .unwrap_or_else(|| panic!("no truncation finding: total={}", findings.len()));
        assert_eq!(truncation.severity, Severity::Info);
        assert!(truncation.evidence.is_empty());

        let near_duplicates = findings
            .iter()
            .filter(|f| f.kind == "near-duplicate")
            .count();
        assert_eq!(near_duplicates, MAX_PAIR_FINDINGS, "{findings:#?}");
        assert_eq!(
            findings.len(),
            MAX_PAIR_FINDINGS + 1,
            "the capped near-duplicate findings plus exactly one truncation finding: {findings:#?}"
        );
    }

    #[test]
    fn analyze_is_deterministic_for_the_same_input() {
        let surfaces = vec![
            surface(
                Layer::ContextCommon,
                "/repo/.zirv/context/common.md",
                "- always run tests\n",
            ),
            surface(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- always run tests\n",
            ),
        ];
        assert_eq!(analyze(&surfaces), analyze(&surfaces));
    }
}
