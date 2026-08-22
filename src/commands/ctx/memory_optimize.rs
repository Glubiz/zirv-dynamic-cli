//! `zirv memory optimize` (issue #38): keeps the SHARED, repository-owned
//! memory bank compact, internally consistent, and useful as it evolves over
//! months and years, without ever silently discarding curated knowledge.
//!
//! Scoped to the SHARED bank only (`memory::MemoryScope::Shared`): the
//! acceptance criteria are all about a bank that is "reviewable with `git
//! diff`", which only holds for the scope meant to be committed with the
//! repository. The private, machine-local bank has no such reviewability and
//! is out of scope for this verb.
//!
//! Split the same way `memory.rs`'s own harvest/init halves are (see that
//! file's own module doc comment): everything that can be pure and
//! unit-tested with no model, filesystem, or clock involved IS pure --
//! `analyze` and every sub-detector it calls, plus `regenerate_core_proposal`
//! -- and only `apply_consolidation`'s one judgment call touches a model,
//! reusing `handoff::run_model` exactly the way harvest/init already do.
//!
//! **REPORT-FIRST, always (design decision 2):** `analyze`/`gather_candidates`
//! never write anything; `memory_cli::run_optimize_with` only ever calls
//! `apply_consolidation` when the operator passed `--apply` (and not
//! `--dry-run`, which always wins). **Never destructive (design decision 6):**
//! this module contains no delete/forget path at all -- consolidation only
//! ever upserts a survivor entry's body; every other member of a
//! duplicate/near-duplicate group is left untouched on disk, and a group
//! containing a deliberate `Source: explicit` entry is never auto-applied at
//! all. Lifecycle state (`retrieval::Lifecycle`) is never stored either: it is
//! derived fresh from each entry's own current fields on every read (see
//! `retrieval::classify_lifecycle`), so nothing here can ever leave a memory
//! entry's on-disk lifecycle "wrong" or in need of migration.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use super::CtxResult;
use super::adapters::AgentAdapter;
use super::config::CtxConfig;
use super::memory::{self, Entry, MemoryScope};
use super::retrieval::{self, Lifecycle};
use super::state::{StateDir, now_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Severity {
    Info,
    Warning,
    High,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::High => "high",
        }
    }
}

/// One optimize finding (design decision 3): never bare prose. `keys`/`paths`
/// are the evidence a caller can actually act on -- forget, verify, or
/// hand-edit a specific file -- rather than a description they have to go
/// hunting from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// `"duplicate"`, `"near-duplicate"`, `"contradiction"`, `"stale"`,
    /// `"archived"`, `"obsolete-path"`, `"oversized"`, `"low-value"`, or
    /// `"core-regen-opportunity"`.
    pub kind: &'static str,
    pub severity: Severity,
    /// Affected entry keys, evidence a caller can pass straight to `zirv
    /// memory forget`/`verify`/`recall --key`.
    pub keys: Vec<String>,
    /// Affected entries' own canonical files.
    pub paths: Vec<PathBuf>,
    pub detail: String,
}

/// One shared entry as input to analysis, with every impure signal
/// (verification age, lifecycle, dead-path check) precomputed by
/// `gather_candidates` -- the same "no clock/fs inside the pure engine"
/// discipline `retrieval::RetrievalCandidate` already follows, for the same
/// reason (design decision 4).
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizeCandidate {
    pub entry: Entry,
    /// The entry's own canonical file, for evidence.
    pub path: PathBuf,
    pub verified_age_days: u64,
    pub lifecycle: Lifecycle,
    /// The subset of `entry.paths` that do not exist under the repository
    /// root, precomputed by `gather_candidates` -- `analyze` never touches
    /// the filesystem itself.
    pub dead_paths: Vec<String>,
}

// --- Pure detectors -------------------------------------------------------

/// Collapses internal whitespace and case for an exact-duplicate comparison.
/// Deliberately coarser than a byte-exact match: two bodies that differ only
/// in incidental spacing or capitalization are still the same fact restated,
/// which is exactly what a `"duplicate"` finding exists to catch.
fn normalize_body(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Same tokenization spirit as `retrieval::normalized_words` (that function
/// is private to its own module, so this is a local, set-shaped copy --
/// the same "duplicated locally to keep this file's edits isolated" idiom
/// `memory.rs`'s own `strip_bullet` already uses).
fn normalized_word_set(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Jaccard similarity over two word sets: `0.0` when either is empty (never
/// spuriously "similar" to an empty body), `1.0` for identical sets.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Word-overlap floor for a `"near-duplicate"` finding: high enough that two
/// entries about merely related, but genuinely distinct, subjects do not
/// trigger a consolidation suggestion.
const NEAR_DUPLICATE_THRESHOLD: f64 = 0.6;

/// Word-overlap floor for even considering two entries as possibly
/// contradicting each other -- lower than the near-duplicate threshold on
/// purpose: two entries can share a subject without restating each other.
const CONTRADICTION_SUBJECT_THRESHOLD: f64 = 0.3;

/// Fraction of `cfg.memory.max_entry_bytes` at or beyond which a body is
/// flagged `"oversized"` -- a warning ahead of the hard truncation
/// `memory::remember`/`upsert_shared` would otherwise apply silently.
const OVERSIZED_RATIO: f64 = 0.8;

/// A body shorter than this is flagged `"low-value"` on size alone,
/// regardless of `memory::is_temporary_or_generic`.
const LOW_VALUE_MIN_BYTES: usize = 20;

/// Deterministic, structural contradiction markers: a pair of entries whose
/// subjects overlap (see `CONTRADICTION_SUBJECT_THRESHOLD`) but whose bodies
/// each contain one word from opposite sides of the same pair are flagged as
/// a likely contradiction (design decision 8: lexical/structural, no model).
/// A blunt instrument by design, the same posture `memory::REJECT_PATTERNS`
/// takes: a false positive costs one findable, human-reviewable line in the
/// report; a false negative just means this pass does not catch every real
/// contradiction, which a model-driven pass never fully closes either.
const POLARITY_PAIRS: &[(&str, &str)] = &[
    ("always", "never"),
    ("must", "optional"),
    ("required", "optional"),
    ("enabled", "disabled"),
    ("deprecated", "current"),
    ("true", "false"),
    ("supported", "unsupported"),
    ("allowed", "forbidden"),
];

fn contains_word(lower_body: &str, word: &str) -> bool {
    format!(" {lower_body} ").contains(&format!(" {word} "))
}

/// Entries whose normalized bodies are byte-for-byte identical (mod
/// whitespace/case) -- the strongest, least ambiguous duplicate signal.
/// Grouped through a `BTreeMap` keyed on the normalized text itself, not a
/// `HashMap`, so iteration order (and therefore the order findings come out
/// in) never depends on hash-seed randomization (design decision 4).
fn find_duplicates(candidates: &[OptimizeCandidate]) -> Vec<Finding> {
    let mut groups: BTreeMap<String, Vec<&OptimizeCandidate>> = BTreeMap::new();
    for c in candidates {
        let norm = normalize_body(&c.entry.body);
        if norm.is_empty() {
            continue;
        }
        groups.entry(norm).or_default().push(c);
    }
    let mut out = Vec::new();
    for group in groups.into_values() {
        if group.len() < 2 {
            continue;
        }
        let mut keys: Vec<String> = group.iter().map(|c| c.entry.key.clone()).collect();
        keys.sort();
        let mut paths: Vec<PathBuf> = group.iter().map(|c| c.path.clone()).collect();
        paths.sort();
        out.push(Finding {
            kind: "duplicate",
            severity: Severity::Warning,
            detail: format!(
                "{} entries share identical body text (mod whitespace/case)",
                keys.len()
            ),
            keys,
            paths,
        });
    }
    out
}

/// Entries whose bodies are similar but not identical, above
/// `NEAR_DUPLICATE_THRESHOLD` -- candidates for consolidation. Pairwise
/// (`candidates` is capped by `cfg.memory.max_entries`, so this stays cheap
/// in practice); iterates `candidates` in its own already-deterministic
/// order (from `list_scoped`'s sorted directory scan), so output order is
/// deterministic without needing a second sort.
fn find_near_duplicates(candidates: &[OptimizeCandidate]) -> Vec<Finding> {
    let mut out = Vec::new();
    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            let a = &candidates[i];
            let b = &candidates[j];
            let na = normalize_body(&a.entry.body);
            let nb = normalize_body(&b.entry.body);
            if na.is_empty() || nb.is_empty() || na == nb {
                continue; // exact duplicates are `find_duplicates`'s job.
            }
            let sim = jaccard(
                &normalized_word_set(&a.entry.body),
                &normalized_word_set(&b.entry.body),
            );
            if sim < NEAR_DUPLICATE_THRESHOLD {
                continue;
            }
            let mut keys = vec![a.entry.key.clone(), b.entry.key.clone()];
            keys.sort();
            let mut paths = vec![a.path.clone(), b.path.clone()];
            paths.sort();
            out.push(Finding {
                kind: "near-duplicate",
                severity: Severity::Info,
                detail: format!(
                    "'{}' and '{}' overlap {:.0}% by shared words -- candidates for consolidation",
                    a.entry.key,
                    b.entry.key,
                    sim * 100.0
                ),
                keys,
                paths,
            });
        }
    }
    out
}

/// Entries whose subjects overlap but whose bodies disagree using one of
/// `POLARITY_PAIRS`. See that constant's own doc comment for the detection
/// rule and its deliberate limits.
fn find_contradictions(candidates: &[OptimizeCandidate]) -> Vec<Finding> {
    let mut out = Vec::new();
    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            let a = &candidates[i];
            let b = &candidates[j];
            let sim = jaccard(
                &normalized_word_set(&a.entry.body),
                &normalized_word_set(&b.entry.body),
            );
            if sim < CONTRADICTION_SUBJECT_THRESHOLD {
                continue;
            }
            let body_a = a.entry.body.to_lowercase();
            let body_b = b.entry.body.to_lowercase();
            for (pos, neg) in POLARITY_PAIRS {
                let a_has_pos = contains_word(&body_a, pos);
                let a_has_neg = contains_word(&body_a, neg);
                let b_has_pos = contains_word(&body_b, pos);
                let b_has_neg = contains_word(&body_b, neg);
                if (a_has_pos && b_has_neg) || (a_has_neg && b_has_pos) {
                    let mut keys = vec![a.entry.key.clone(), b.entry.key.clone()];
                    keys.sort();
                    let mut paths = vec![a.path.clone(), b.path.clone()];
                    paths.sort();
                    out.push(Finding {
                        kind: "contradiction",
                        severity: Severity::High,
                        detail: format!(
                            "'{}' and '{}' share subject overlap ({:.0}%) but disagree on '{pos}' vs '{neg}'",
                            a.entry.key,
                            b.entry.key,
                            sim * 100.0
                        ),
                        keys,
                        paths,
                    });
                    break; // one contradiction finding per pair is enough.
                }
            }
        }
    }
    out
}

/// `Stale`/`Archived` findings, purely a readout of `candidate.lifecycle`
/// (already computed by `gather_candidates` via `retrieval::
/// classify_lifecycle`) -- this function makes no lifecycle decision of its
/// own.
fn find_lifecycle_findings(candidates: &[OptimizeCandidate]) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in candidates {
        match c.lifecycle {
            Lifecycle::Stale => out.push(Finding {
                kind: "stale",
                severity: Severity::Info,
                keys: vec![c.entry.key.clone()],
                paths: vec![c.path.clone()],
                detail: format!(
                    "not verified in {} days; down-ranked from normal retrieval, still \
                     reachable with `zirv memory recall`",
                    c.verified_age_days
                ),
            }),
            Lifecycle::Archived => out.push(Finding {
                kind: "archived",
                severity: Severity::Info,
                keys: vec![c.entry.key.clone()],
                paths: vec![c.path.clone()],
                detail: format!(
                    "not verified in {} days and low-value; excluded from normal retrieval, \
                     still reachable with `zirv memory recall --include-archived`",
                    c.verified_age_days
                ),
            }),
            Lifecycle::Active => {}
        }
    }
    out
}

/// Entries whose `paths` name a repository path that no longer exists
/// (`dead_paths`, precomputed by `gather_candidates`).
fn find_obsolete_paths(candidates: &[OptimizeCandidate]) -> Vec<Finding> {
    candidates
        .iter()
        .filter(|c| !c.dead_paths.is_empty())
        .map(|c| Finding {
            kind: "obsolete-path",
            severity: Severity::Warning,
            keys: vec![c.entry.key.clone()],
            paths: vec![c.path.clone()],
            detail: format!(
                "references path(s) that no longer exist: {}",
                c.dead_paths.join(", ")
            ),
        })
        .collect()
}

/// Entries whose body is close to (or over) `cfg.memory.max_entry_bytes` --
/// a warning ahead of the silent truncation the store would otherwise apply.
fn find_oversized(candidates: &[OptimizeCandidate], cfg: &CtxConfig) -> Vec<Finding> {
    let threshold = ((cfg.memory.max_entry_bytes as f64) * OVERSIZED_RATIO) as usize;
    candidates
        .iter()
        .filter(|c| c.entry.body.len() >= threshold.max(1))
        .map(|c| Finding {
            kind: "oversized",
            severity: Severity::Info,
            keys: vec![c.entry.key.clone()],
            paths: vec![c.path.clone()],
            detail: format!(
                "body is {} bytes, close to the {}-byte per-entry cap",
                c.entry.body.len(),
                cfg.memory.max_entry_bytes
            ),
        })
        .collect()
}

/// Entries that read as trivial (too short to be a useful standalone fact)
/// or as task narration/session state (`memory::is_temporary_or_generic`,
/// reused rather than duplicated).
fn find_low_value(candidates: &[OptimizeCandidate]) -> Vec<Finding> {
    candidates
        .iter()
        .filter(|c| {
            c.entry.body.trim().len() < LOW_VALUE_MIN_BYTES
                || memory::is_temporary_or_generic(&c.entry.body)
        })
        .map(|c| Finding {
            kind: "low-value",
            severity: Severity::Info,
            keys: vec![c.entry.key.clone()],
            paths: vec![c.path.clone()],
            detail: "reads as trivial or task-narration content rather than a durable project \
                      fact"
                .to_string(),
        })
        .collect()
}

/// The rendered byte cost of one entry counted the way `retrieval::select`
/// and `prompt::render_memory_entry` already count it: `"key\nbody"`.
fn rendered_len(entry: &Entry) -> usize {
    entry.key.len() + 1 + entry.body.len()
}

/// Deterministic core-worthiness ordering: importance descending, then
/// confidence descending, then most-recently-verified first, then key
/// ascending as the final tiebreak -- mirrors `prompt::ranked_by_recency`'s
/// intent (freshest, most important knowledge first) without depending on
/// its private implementation.
fn core_priority_key(c: &OptimizeCandidate) -> (i32, i32, u64, String) {
    let importance_rank = match c.entry.importance.as_deref() {
        Some("high") => 0,
        Some("low") => 2,
        _ => 1,
    };
    let confidence_rank = match c.entry.confidence.as_deref() {
        Some("high") => 0,
        Some("low") => 2,
        _ => 1,
    };
    (
        importance_rank,
        confidence_rank,
        c.verified_age_days,
        c.entry.key.clone(),
    )
}

/// PURE: greedily keeps candidates in core-priority order within
/// `cap_bytes`, skipping (never truncating) an entry that would not fit --
/// design decision 7's "core-memory regeneration must stay within its
/// existing hard budget" holds by construction here, not by luck: the
/// running total is checked before every push and never allowed to exceed
/// `cap_bytes`.
pub fn regenerate_core_proposal(candidates: &[OptimizeCandidate], cap_bytes: usize) -> Vec<String> {
    let mut ordered: Vec<&OptimizeCandidate> = candidates.iter().collect();
    ordered.sort_by_key(|c| core_priority_key(c));

    let mut kept = Vec::new();
    let mut used = 0usize;
    for c in ordered {
        let cost = rendered_len(&c.entry) + if kept.is_empty() { 0 } else { 2 };
        if used + cost > cap_bytes {
            continue;
        }
        used += cost;
        kept.push(c.entry.key.clone());
    }
    kept
}

/// A `"core-regen-opportunity"` finding, only when the candidates' total
/// rendered size already exceeds `cfg.memory.core_max_bytes` -- an
/// informational report of what a regenerated core would keep
/// (`regenerate_core_proposal`), never applied automatically by this module
/// (see this file's own module doc comment).
fn core_regen_finding(candidates: &[OptimizeCandidate], cfg: &CtxConfig) -> Option<Finding> {
    if candidates.is_empty() {
        return None;
    }
    let total: usize = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| rendered_len(&c.entry) + if i == 0 { 0 } else { 2 })
        .sum();
    if total <= cfg.memory.core_max_bytes {
        return None;
    }
    let proposal = regenerate_core_proposal(candidates, cfg.memory.core_max_bytes);
    let mut keys: Vec<String> = candidates.iter().map(|c| c.entry.key.clone()).collect();
    keys.sort();
    let mut paths: Vec<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
    paths.sort();
    Some(Finding {
        kind: "core-regen-opportunity",
        severity: Severity::Warning,
        detail: format!(
            "the shared bank's core-eligible content is {total} bytes, over the {}-byte core \
             budget; a regenerated core keeping {} of {} entries would fit: {}",
            cfg.memory.core_max_bytes,
            proposal.len(),
            candidates.len(),
            proposal.join(", "),
        ),
        keys,
        paths,
    })
}

/// THE pure detection engine (design decision 4): no clock, no filesystem,
/// no HashMap iteration order anywhere in its output -- every sub-detector
/// above either iterates `candidates` in its own already-deterministic
/// input order or groups through a `BTreeMap`. Calling this twice on the
/// same `candidates`/`cfg` always yields the identical `Vec<Finding>`, in
/// the identical order.
pub fn analyze(candidates: &[OptimizeCandidate], cfg: &CtxConfig) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(find_duplicates(candidates));
    findings.extend(find_near_duplicates(candidates));
    findings.extend(find_contradictions(candidates));
    findings.extend(find_lifecycle_findings(candidates));
    findings.extend(find_obsolete_paths(candidates));
    findings.extend(find_oversized(candidates, cfg));
    findings.extend(find_low_value(candidates));
    findings.extend(core_regen_finding(candidates, cfg));
    findings
}

/// Renders `findings` as a plain-text report. Never spends a body verbatim:
/// only keys, paths, and each finding's own short `detail` -- the report is
/// meant to be skimmed and acted on, not a second copy of the bank.
pub fn render_report(findings: &[Finding]) -> String {
    let mut out = String::from("# zirv memory optimize report\n\n");
    if findings.is_empty() {
        out.push_str("no findings -- the shared memory bank looks consolidated and current.\n");
        return out;
    }
    for f in findings {
        out.push_str(&format!("## [{}] {}\n", f.severity.as_str(), f.kind));
        out.push_str(&format!("keys: {}\n", f.keys.join(", ")));
        if !f.paths.is_empty() {
            let rendered: Vec<String> = f.paths.iter().map(|p| p.display().to_string()).collect();
            out.push_str(&format!("files: {}\n", rendered.join(", ")));
        }
        out.push_str(&format!("{}\n\n", f.detail));
    }
    out
}

// --- Impure gathering -------------------------------------------------

/// Gathers this repository's SHARED bank as optimize candidates (impure:
/// reads the store, checks `entry.paths` against the filesystem, reads the
/// clock via `now`) -- the read/clock/fs boundary this module draws so
/// `analyze` above stays pure, the same discipline `retrieval::
/// candidates_for_scope` already follows.
pub fn gather_candidates(
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    now: u64,
) -> CtxResult<Vec<OptimizeCandidate>> {
    let entries = memory::list_scoped(MemoryScope::Shared, repo, state, slug, cfg)?;
    Ok(entries
        .into_iter()
        .map(|(path, entry)| {
            let verified_age_days = now.saturating_sub(entry.verified) / 86_400;
            let lifecycle = retrieval::classify_lifecycle(&entry, verified_age_days);
            let dead_paths: Vec<String> = entry
                .paths
                .iter()
                .filter(|p| !repo.join(p).exists())
                .cloned()
                .collect();
            OptimizeCandidate {
                entry,
                path,
                verified_age_days,
                lifecycle,
                dead_paths,
            }
        })
        .collect())
}

// --- Consolidation: the one thing --apply may write ------------------

/// One duplicate/near-duplicate group, with a survivor already picked
/// deterministically -- the detection half (design decision 8: the GROUP
/// and its survivor are decided without any model). `has_explicit_member`
/// gates whether `apply_consolidation` may ever touch this group at all.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationGroup {
    pub survivor_key: String,
    pub member_keys: Vec<String>,
    pub has_explicit_member: bool,
}

/// Deterministically picks the group's survivor: highest importance, then
/// most recently verified (lowest `verified_age_days`), then smallest key as
/// the final tiebreak -- the entry an operator would most likely already
/// trust, so consolidation keeps ITS identity and merges the rest into it.
fn survivor_of<'a>(members: &[&'a OptimizeCandidate]) -> &'a OptimizeCandidate {
    members
        .iter()
        .copied()
        .min_by_key(|c| {
            let importance_rank = match c.entry.importance.as_deref() {
                Some("high") => 0,
                Some("low") => 2,
                _ => 1,
            };
            (importance_rank, c.verified_age_days, c.entry.key.clone())
        })
        .expect("members is never empty: called only from groups with >= 2 members")
}

/// Builds one `ConsolidationGroup` per `"duplicate"`/`"near-duplicate"`
/// finding in `findings` -- reuses `analyze`'s own detection rather than
/// running it a second time, so the groups offered for consolidation are
/// always exactly the ones the report itself named.
pub fn consolidation_groups(
    candidates: &[OptimizeCandidate],
    findings: &[Finding],
) -> Vec<ConsolidationGroup> {
    let by_key: HashMap<&str, &OptimizeCandidate> = candidates
        .iter()
        .map(|c| (c.entry.key.as_str(), c))
        .collect();
    let mut groups = Vec::new();
    for f in findings {
        if f.kind != "duplicate" && f.kind != "near-duplicate" {
            continue;
        }
        let members: Vec<&OptimizeCandidate> = f
            .keys
            .iter()
            .filter_map(|k| by_key.get(k.as_str()).copied())
            .collect();
        if members.len() < 2 {
            continue;
        }
        let survivor = survivor_of(&members);
        let mut member_keys: Vec<String> = members.iter().map(|c| c.entry.key.clone()).collect();
        member_keys.sort();
        let has_explicit_member = members.iter().any(|c| c.entry.source == "explicit");
        groups.push(ConsolidationGroup {
            survivor_key: survivor.entry.key.clone(),
            member_keys,
            has_explicit_member,
        });
    }
    groups
}

pub const CONSOLIDATE_PROMPT_VERSION: &str = "v1";

/// The one model call this module may make (design decision 8): asks for a
/// single merged body for an already-detected group, keyed on the
/// already-chosen survivor's key -- the model never gets to choose the key
/// or the group, only propose the merged text.
fn consolidation_prompt(survivor: &Entry, others: &[&Entry]) -> String {
    let mut listing = format!("- {} (KEEP THIS KEY): {}\n", survivor.key, survivor.body);
    for o in others {
        listing.push_str(&format!("- {}: {}\n", o.key, o.body));
    }
    format!(
        "You are consolidating near-duplicate MEMORY entries ({CONSOLIDATE_PROMPT_VERSION}) in a \
long-lived shared project memory bank. The entries below overlap or restate the same fact. Merge \
them into ONE consolidated fact that keeps everything true and useful from all of them, written as \
one or two plain sentences.\n\n\
Answer with EXACTLY one line, `key: body`, using the exact key \"{survivor_key}\" -- do not invent a \
different key and do not answer in markdown, headings, or prose. If the entries do not actually \
overlap enough to merge safely, answer with nothing at all.\n\n\
{listing}",
        survivor_key = survivor.key,
    )
}

/// Parses the model's answer with `memory::parse_harvest` (reused unchanged,
/// same strict `key: body` shape every other distiller call in this
/// codebase already uses) and applies the mandatory post-validation: the
/// answered key must be exactly `survivor_key` (a model proposing a
/// different key is rejected outright, never silently renamed or
/// redirected), the body is truncated to `cfg.memory.max_entry_bytes` (the
/// same cap every other entry gets), and dropped if truncation leaves
/// nothing but whitespace.
fn parse_and_validate_consolidation(
    answer: &str,
    survivor_key: &str,
    cfg: &CtxConfig,
) -> Option<String> {
    let (_, body) = memory::parse_harvest(answer)
        .into_iter()
        .find(|(key, _)| key == survivor_key)?;
    let body = crate::utils::truncate_bytes(body, Some(cfg.memory.max_entry_bytes));
    if body.trim().is_empty() {
        return None;
    }
    Some(body)
}

/// The most consolidation groups one `--apply` run will act on, regardless
/// of how many `analyze` reports -- a conservative per-run cap on spawned
/// model calls, the same spirit as `memory::harvest_max_entries` (a
/// different budget for a different call site).
const MAX_CONSOLIDATION_GROUPS_PER_RUN: usize = 5;

/// Applies already-detected consolidation groups: for each one WITHOUT an
/// explicit-source member, asks the model for a merged body, validates it
/// deterministically, and upserts the survivor with it. Every OTHER member
/// of the group is left completely untouched on disk -- this never deletes
/// or forgets anything (design decision 6); the operator reviews the
/// group's own finding and, if they agree the losers are now redundant,
/// removes them by hand with `zirv memory forget`.
///
/// Best-effort per group, same shape as `memory::write_durable`'s own
/// per-key loop: a model failure, a rejected proposal, or a write refused
/// partway through skips that one group and moves on to the next, so one
/// bad group in a batch never aborts the rest. Re-reads the survivor with a
/// fresh `get_scoped` right before writing (not the batch read
/// `gather_candidates` already did) so a concurrent edit -- including one
/// that promoted the entry to `Source: explicit` in the meantime -- is
/// never silently overwritten by a stale in-memory copy; this mirrors
/// `write_durable`'s own "fresh read right before each write" rule and its
/// reasoning.
#[allow(clippy::too_many_arguments)]
pub fn apply_consolidation(
    adapter: &dyn AgentAdapter,
    model: &str,
    timeout: Duration,
    groups: &[ConsolidationGroup],
    candidates: &[OptimizeCandidate],
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
) -> Vec<String> {
    let by_key: HashMap<&str, &OptimizeCandidate> = candidates
        .iter()
        .map(|c| (c.entry.key.as_str(), c))
        .collect();
    let mut applied = Vec::new();

    for group in groups.iter().take(MAX_CONSOLIDATION_GROUPS_PER_RUN) {
        if group.has_explicit_member {
            continue;
        }
        let Some(survivor) = by_key.get(group.survivor_key.as_str()) else {
            continue;
        };
        let others: Vec<&Entry> = group
            .member_keys
            .iter()
            .filter(|k| **k != group.survivor_key)
            .filter_map(|k| by_key.get(k.as_str()).map(|c| &c.entry))
            .collect();
        if others.is_empty() {
            continue;
        }

        let prompt = consolidation_prompt(&survivor.entry, &others);
        let Ok(answer) = super::handoff::run_model(adapter, model, &prompt, timeout) else {
            continue;
        };
        let Some(merged_body) = parse_and_validate_consolidation(&answer, &group.survivor_key, cfg)
        else {
            continue;
        };

        let Ok(Some(current)) = memory::get_scoped(
            MemoryScope::Shared,
            repo,
            state,
            slug,
            cfg,
            &group.survivor_key,
        ) else {
            continue;
        };
        if current.source == "explicit" {
            continue;
        }

        let mut updated = current;
        updated.body = merged_body;
        updated.verified = now_secs();
        updated.source = "optimize".to_string();
        if memory::upsert_scoped(MemoryScope::Shared, repo, state, slug, cfg, &updated).is_ok() {
            applied.push(group.survivor_key.clone());
        }
    }

    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(key: &str, body: &str) -> OptimizeCandidate {
        OptimizeCandidate {
            entry: Entry {
                key: key.to_string(),
                written_by: "claude".to_string(),
                written: 1_700_000_000,
                verified: 1_700_000_000,
                source: "harvest".to_string(),
                body: body.to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            path: PathBuf::from(format!("/repo/.zirv/memory/{key}.md")),
            verified_age_days: 0,
            lifecycle: Lifecycle::Active,
            dead_paths: Vec::new(),
        }
    }

    // --- Duplicate / near-duplicate detection -----------------------

    #[test]
    fn exact_duplicates_are_flagged_with_key_and_path_evidence() {
        let candidates = vec![
            candidate("db-driver", "The project uses postgres for the database."),
            candidate("db-driver-2", "the project uses postgres for the database."),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        let dup = findings
            .iter()
            .find(|f| f.kind == "duplicate")
            .expect("a duplicate finding");
        assert_eq!(dup.keys, vec!["db-driver", "db-driver-2"]);
        assert_eq!(dup.paths.len(), 2);
    }

    #[test]
    fn near_duplicates_above_the_threshold_are_flagged() {
        let candidates = vec![
            candidate(
                "deploy-a",
                "deploy the service through the release pipeline using the staging config",
            ),
            candidate(
                "deploy-b",
                "deploy the service via the release pipeline with the staging config",
            ),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        let near = findings
            .iter()
            .find(|f| f.kind == "near-duplicate")
            .expect("a near-duplicate finding");
        assert_eq!(near.keys, vec!["deploy-a", "deploy-b"]);
    }

    #[test]
    fn unrelated_entries_produce_no_duplicate_or_near_duplicate_finding() {
        let candidates = vec![
            candidate("db-driver", "the project uses postgres for the database"),
            candidate("release-cmd", "run cargo build --release before tagging"),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        assert!(
            !findings
                .iter()
                .any(|f| f.kind == "duplicate" || f.kind == "near-duplicate"),
            "got {findings:?}"
        );
    }

    // --- Contradiction detection --------------------------------------

    #[test]
    fn a_lexical_contradiction_is_flagged_with_evidence() {
        let candidates = vec![
            candidate(
                "db-driver",
                "the project always uses postgres for the database driver",
            ),
            candidate(
                "db-driver-note",
                "the project never uses postgres for the database driver, it uses sqlite",
            ),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        let contradiction = findings
            .iter()
            .find(|f| f.kind == "contradiction")
            .expect("a contradiction finding");
        assert_eq!(contradiction.keys, vec!["db-driver", "db-driver-note"]);
        assert_eq!(contradiction.severity, Severity::High);
    }

    #[test]
    fn agreeing_entries_about_the_same_subject_are_not_flagged_as_contradictions() {
        let candidates = vec![
            candidate(
                "build-cmd",
                "always run cargo build before tagging a release",
            ),
            candidate(
                "release-cmd",
                "always run cargo test before tagging a release",
            ),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        assert!(
            !findings.iter().any(|f| f.kind == "contradiction"),
            "got {findings:?}"
        );
    }

    // --- Determinism (design decision 4) -------------------------------

    #[test]
    fn analyze_is_pure_and_deterministic_across_repeated_runs() {
        let candidates = vec![
            candidate(
                "db-driver",
                "the project always uses postgres for the database",
            ),
            candidate(
                "db-driver-note",
                "the project never uses postgres for the database, it uses sqlite",
            ),
            candidate("build-cmd", "cargo build --release"),
            candidate("build-cmd-2", "cargo build --release"),
        ];
        let cfg = CtxConfig::default();
        let first = analyze(&candidates, &cfg);
        let second = analyze(&candidates, &cfg);
        assert_eq!(first, second);
    }

    // --- Lifecycle findings tie into `analyze` --------------------------

    #[test]
    fn an_old_but_high_importance_verified_entry_produces_no_stale_or_archived_finding() {
        let mut c = candidate("architecture-invariant", "the rot engine is pure");
        c.entry.importance = Some("high".to_string());
        c.verified_age_days = 10_000;
        // Mirrors `retrieval::classify_lifecycle`'s own contract exactly:
        // importance high overrides age unconditionally.
        c.lifecycle = retrieval::classify_lifecycle(&c.entry, c.verified_age_days);
        assert_eq!(c.lifecycle, Lifecycle::Active);

        let findings = analyze(&[c], &CtxConfig::default());
        assert!(
            !findings
                .iter()
                .any(|f| f.kind == "stale" || f.kind == "archived"),
            "an old but important, verified entry must never be flagged stale/archived by age \
             alone: {findings:?}"
        );
    }

    #[test]
    fn a_low_value_unverified_entry_is_flagged_stale_or_archived() {
        let mut c = candidate("old-note", "some minor detail nobody re-verified");
        c.entry.importance = Some("low".to_string());
        c.verified_age_days = retrieval::ARCHIVE_AFTER_DAYS;
        c.lifecycle = retrieval::classify_lifecycle(&c.entry, c.verified_age_days);

        let findings = analyze(&[c], &CtxConfig::default());
        assert!(
            findings.iter().any(|f| f.kind == "archived"),
            "got {findings:?}"
        );
    }

    // --- Obsolete paths, oversized, low-value ---------------------------

    #[test]
    fn an_entry_with_a_dead_path_is_flagged_with_the_dead_path_named() {
        let mut c = candidate("router-note", "the router lives in src/router.rs");
        c.dead_paths = vec!["src/router.rs".to_string()];
        let findings = analyze(&[c], &CtxConfig::default());
        let f = findings
            .iter()
            .find(|f| f.kind == "obsolete-path")
            .expect("an obsolete-path finding");
        assert!(f.detail.contains("src/router.rs"), "got {f:?}");
    }

    #[test]
    fn an_oversized_body_is_flagged() {
        let cfg = CtxConfig::default();
        let big_body = "x".repeat(cfg.memory.max_entry_bytes);
        let c = candidate("big-fact", &big_body);
        let findings = analyze(&[c], &cfg);
        assert!(
            findings.iter().any(|f| f.kind == "oversized"),
            "got {findings:?}"
        );
    }

    #[test]
    fn a_task_narration_body_is_flagged_low_value() {
        let c = candidate("session-note", "todo: still need to finish the migration");
        let findings = analyze(&[c], &CtxConfig::default());
        assert!(
            findings.iter().any(|f| f.kind == "low-value"),
            "got {findings:?}"
        );
    }

    // --- Core regeneration ----------------------------------------------

    #[test]
    fn regenerate_core_proposal_never_exceeds_the_cap() {
        let candidates: Vec<OptimizeCandidate> = (0..20)
            .map(|i| candidate(&format!("fact-{i}"), &"word ".repeat(50)))
            .collect();
        for cap in [0usize, 1, 50, 200, 1_000, 100_000] {
            let proposal = regenerate_core_proposal(&candidates, cap);
            let by_key: HashMap<&str, &OptimizeCandidate> = candidates
                .iter()
                .map(|c| (c.entry.key.as_str(), c))
                .collect();
            let total: usize = proposal
                .iter()
                .enumerate()
                .map(|(i, k)| rendered_len(&by_key[k.as_str()].entry) + if i == 0 { 0 } else { 2 })
                .sum();
            assert!(
                total <= cap,
                "cap={cap}: regenerated core is {total} bytes, over budget: {proposal:?}"
            );
        }
    }

    #[test]
    fn regenerate_core_proposal_keeps_everything_when_it_all_fits() {
        let candidates = vec![candidate("a", "short"), candidate("b", "also short")];
        let proposal = regenerate_core_proposal(&candidates, 10_000);
        assert_eq!(proposal.len(), 2);
    }

    #[test]
    fn a_core_regen_finding_only_appears_when_the_cap_is_exceeded() {
        let cfg = CtxConfig::default();
        let small = vec![candidate("a", "tiny")];
        assert!(
            !analyze(&small, &cfg)
                .iter()
                .any(|f| f.kind == "core-regen-opportunity")
        );

        let big: Vec<OptimizeCandidate> = (0..50)
            .map(|i| candidate(&format!("fact-{i}"), &"word ".repeat(100)))
            .collect();
        assert!(
            analyze(&big, &cfg)
                .iter()
                .any(|f| f.kind == "core-regen-opportunity")
        );
    }

    // --- Consolidation groups: detection is model-free ------------------

    #[test]
    fn consolidation_groups_are_built_from_duplicate_and_near_duplicate_findings() {
        let candidates = vec![
            candidate("db-a", "the project uses postgres for the database"),
            candidate("db-b", "the project uses postgres for the database"),
            candidate("unrelated", "cargo build --release"),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        let groups = consolidation_groups(&candidates, &findings);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].member_keys, vec!["db-a", "db-b"]);
        assert!(!groups[0].has_explicit_member);
    }

    #[test]
    fn a_group_with_an_explicit_member_is_flagged_as_such() {
        let mut a = candidate("db-a", "the project uses postgres for the database");
        a.entry.source = "explicit".to_string();
        let b = candidate("db-b", "the project uses postgres for the database");
        let candidates = vec![a, b];
        let findings = analyze(&candidates, &CtxConfig::default());
        let groups = consolidation_groups(&candidates, &findings);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].has_explicit_member);
    }

    // --- Report rendering -------------------------------------------------

    #[test]
    fn an_empty_finding_set_renders_a_clean_report() {
        let report = render_report(&[]);
        assert!(report.contains("no findings"));
    }

    #[test]
    fn the_report_never_includes_a_finding_bare_prose_only() {
        let candidates = vec![
            candidate("db-a", "the project uses postgres for the database"),
            candidate("db-b", "the project uses postgres for the database"),
        ];
        let findings = analyze(&candidates, &CtxConfig::default());
        let report = render_report(&findings);
        assert!(report.contains("db-a"), "got {report}");
        assert!(report.contains("db-b"), "got {report}");
    }
}
