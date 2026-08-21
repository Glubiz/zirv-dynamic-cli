//! Deterministic, context-aware memory retrieval (issue #35): a query- and
//! path-aware ranking on top of Task 4's core layer, spending the separate
//! hard budget issue #34 introduced (`cfg.memory.retrieval_max_bytes`/
//! `retrieval_max_entries`). The ranking function itself is pure -- no
//! clock, env, or filesystem reads -- so identical inputs always produce
//! identical output; every signal (query text, current path, changed
//! files, an entry's own tags/paths/importance/confidence/verification
//! age) arrives as plain data the caller gathers first, the same
//! discipline `rot.rs` holds for the same reason (CLAUDE.md).
//!
//! `zirv memory recall <query>` (`memory_cli.rs`) is the first live
//! consumer, wired in this task. It only ever fills in `query`: `cwd_path`
//! and `changed_paths` are inert on that path today (`RetrievalContext`
//! defaults them empty, so no path/module signal can fire), because
//! `recall` is a one-shot CLI call with no session context to gather them
//! from. A future task's session-startup wiring (the compiler, issue #44)
//! is what will actually populate those two fields, and can reuse
//! `candidates_for_repo`/`select` unchanged for the same "dormant read
//! primitive, wired in later" pattern `memory::list_scoped` followed
//! before `zirv memory` (issue #33) consumed it.

use std::path::Path;

use super::CtxResult;
use super::config::CtxConfig;
use super::memory::{Entry, MemoryScope};
use super::state::StateDir;

/// One memory entry as input to ranking, tagged with scope and with
/// verification staleness pre-computed in whole days (issue #35: this
/// module reads no clock; the caller computes this from `now`, mirroring
/// how `memory::render_for_prompt` computed age before issue #34 dropped
/// it from rendering).
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalCandidate {
    pub entry: Entry,
    /// True for a shared (repo-owned) entry; false for private. A shared
    /// entry's `importance`/`confidence`/tags are attacker-supplied
    /// repository content (see `memory::MemoryScope::Shared`), so `rank`
    /// uses this flag to enforce the same private-outranks-shared
    /// precedence `prompt::select_memory_within_cap` enforces for the core
    /// layer -- structurally, not by trusting the data.
    pub shared: bool,
    pub verified_age_days: u64,
}

/// Local, deterministic signals the caller gathers before ranking -- no
/// clock/env/filesystem reads happen inside this module. Empty fields
/// degrade safely: an empty `query` with no `cwd_path`/`changed_paths`
/// means no candidate can score above the relevance floor, so `select`
/// returns nothing rather than an arbitrary top-N of the whole bank
/// (issue #35's acceptance criterion).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetrievalContext {
    pub query: String,
    /// Current working directory / module path, repo-relative (e.g.
    /// "src/commands/ctx"). Empty when unknown.
    pub cwd_path: String,
    /// Changed file paths (e.g. from `git diff --name-only`),
    /// repo-relative.
    pub changed_paths: Vec<String>,
}

/// One ranked candidate plus why it scored the way it did -- the
/// selection diagnostics issue #35 asks for ("expose enough diagnostics to
/// understand why an entry was selected").
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked<'a> {
    pub candidate: &'a RetrievalCandidate,
    /// The relevance floor is tested against THIS field, never `score`
    /// (see `MIN_RELEVANCE_SCORE`'s doc comment): it is the key/keyword/
    /// path signal total before importance/confidence/staleness modifiers
    /// are applied, so a genuine match can never be pushed below the floor
    /// by those modifiers alone.
    pub base_score: i64,
    pub score: i64,
    pub reasons: Vec<String>,
}

/// A candidate needs at least this much BASE score (key/keyword/path
/// signals, before importance/confidence/staleness modifiers) to be
/// selectable at all -- the floor that makes an empty/weak query degrade
/// to "select nothing" rather than "select an arbitrary top N" (issue
/// #35's acceptance criterion). A single keyword/path/key-substring hit
/// clears it; nothing at all (bare importance/confidence/staleness
/// adjustments only) does not.
///
/// This floor is checked against the BASE score, never the final
/// (modifier-applied) score: staleness/importance/confidence only ever
/// refine the ORDERING of an already-relevant match, and must never be
/// able to erase it. Without this split, an exact-key match old enough
/// (or low-importance/low-confidence enough) could accumulate a large
/// enough negative modifier to drop below the floor and vanish from
/// `zirv memory recall` entirely, even though the match itself is real.
const MIN_RELEVANCE_SCORE: i64 = 1;

fn normalized_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether `entry_path` and `other` name the same file/module or one is a
/// path-prefix of the other -- deliberately simple (string prefix, no glob
/// or path-component awareness) since exactness is not the point here: a
/// stored path like "src/commands/ctx" should relate to a changed file
/// "src/commands/ctx/memory.rs" and vice versa.
fn path_relates(entry_path: &str, other: &str) -> bool {
    !entry_path.is_empty()
        && !other.is_empty()
        && (entry_path.starts_with(other) || other.starts_with(entry_path))
}

/// Scores one candidate against `ctx`. Pure: no clock/env/fs reads, only
/// the data already on `candidate`/`ctx`. Every signal issue #35 lists is
/// additive/subtractive on one running score, with a short, human-readable
/// reason recorded for each one that actually fired -- the "selection
/// diagnostics" the issue asks for.
///
/// Key/keyword/path hits are the only signals that can establish base
/// relevance on their own; importance/confidence/staleness only ever
/// refine an ALREADY relevant match. Without that split, a bank where many
/// entries happen to be marked `importance: high` would let an empty or
/// unrelated query still clear `MIN_RELEVANCE_SCORE` on importance alone --
/// exactly the "arbitrary top-N of the whole bank" issue #35's degrade-
/// safely requirement rules out.
fn score_one(candidate: &RetrievalCandidate, ctx: &RetrievalContext) -> (i64, i64, Vec<String>) {
    let mut base_score = 0i64;
    let mut reasons = Vec::new();
    let entry = &candidate.entry;
    let query = ctx.query.trim().to_lowercase();

    if !query.is_empty() {
        let key_lower = entry.key.to_lowercase();
        if key_lower == query {
            base_score += 100;
            reasons.push("exact key match".to_string());
        } else if key_lower.contains(&query) {
            base_score += 50;
            reasons.push("partial key match".to_string());
        }

        let body_lower = entry.body.to_lowercase();
        let tag_words: Vec<String> = entry.tags.iter().map(|t| t.to_lowercase()).collect();
        let mut hits = 0usize;
        for word in normalized_words(&query) {
            if body_lower.contains(&word) {
                hits += 1;
            }
            if tag_words.iter().any(|t| t == &word) {
                hits += 1;
            }
        }
        if hits > 0 {
            base_score += hits as i64 * 10;
            let plural = if hits == 1 { "" } else { "es" };
            reasons.push(format!("{hits} keyword match{plural}"));
        }
    }

    let path_hit = entry.paths.iter().any(|p| {
        path_relates(p, &ctx.cwd_path) || ctx.changed_paths.iter().any(|c| path_relates(p, c))
    });
    if path_hit {
        base_score += 30;
        reasons.push("path/module match".to_string());
    }

    // Modifiers below only apply once a real signal above has already
    // established relevance -- see this function's own doc comment.
    if base_score <= 0 {
        return (base_score, base_score, reasons);
    }
    let mut score = base_score;

    match entry.importance.as_deref() {
        Some("high") => {
            score += 15;
            reasons.push("high importance".to_string());
        }
        Some("low") => score -= 5,
        _ => {}
    }
    match entry.confidence.as_deref() {
        Some("high") => {
            score += 10;
            reasons.push("high confidence".to_string());
        }
        Some("low") => score -= 5,
        _ => {}
    }

    // A gentle, gradual penalty -- one point per week stale -- so
    // staleness nudges the ranking without ever swamping a real match.
    let staleness_penalty = (candidate.verified_age_days / 7) as i64;
    if staleness_penalty > 0 {
        score -= staleness_penalty;
    }

    (base_score, score, reasons)
}

/// PURE deterministic ranking (issue #35): identical `candidates`/`ctx` ->
/// identical output, always. Private-outranks-shared precedence mirrors
/// `prompt::select_memory_within_cap` (issue #34's controller ruling):
/// each candidate is scored independently, then every private candidate is
/// placed ahead of every shared one regardless of score -- a shared
/// entry's own signals can only compete with OTHER shared entries, never
/// displace a private one. Within each group, ordering is score
/// descending, then key ascending as a final deterministic tiebreak.
pub fn rank<'a>(candidates: &'a [RetrievalCandidate], ctx: &RetrievalContext) -> Vec<Ranked<'a>> {
    let scored: Vec<Ranked> = candidates
        .iter()
        .map(|candidate| {
            let (base_score, score, reasons) = score_one(candidate, ctx);
            Ranked {
                candidate,
                base_score,
                score,
                reasons,
            }
        })
        .collect();

    let (mut private, mut shared): (Vec<Ranked>, Vec<Ranked>) =
        scored.into_iter().partition(|r| !r.candidate.shared);
    let by_score_then_key = |a: &Ranked, b: &Ranked| {
        b.score
            .cmp(&a.score)
            .then(a.candidate.entry.key.cmp(&b.candidate.entry.key))
    };
    private.sort_by(by_score_then_key);
    shared.sort_by(by_score_then_key);
    private.extend(shared);
    private
}

/// The outcome of a budgeted retrieval: what was selected, and how many
/// candidates were left out for each of two independent reasons (issue
/// #35: never inject the whole bank).
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalSelection<'a> {
    pub selected: Vec<Ranked<'a>>,
    /// Scored but below `MIN_RELEVANCE_SCORE` -- the empty/weak-query
    /// degrade-safely path.
    pub below_relevance: usize,
    /// Relevant enough, but the byte or entry-count budget ran out first.
    pub over_budget: usize,
}

/// Ranks, then greedily fills `max_bytes`/`max_entries` in rank order
/// (private-first, the same structural precedence `rank` already
/// establishes) -- a candidate whose BASE score is below
/// `MIN_RELEVANCE_SCORE` is never selected regardless of budget headroom,
/// which is what makes an empty/weak query select nothing rather than an
/// arbitrary top-N of the whole bank. Checking the base score (not the
/// final, modifier-adjusted `score`) is deliberate: a genuine match must
/// never be erased by staleness/importance/confidence penalties alone. An
/// oversized entry is skipped rather than starving smaller ones behind
/// it, the same rule `prompt::select_memory_within_cap` uses.
pub fn select<'a>(
    candidates: &'a [RetrievalCandidate],
    ctx: &RetrievalContext,
    max_bytes: usize,
    max_entries: usize,
) -> RetrievalSelection<'a> {
    let ranked = rank(candidates, ctx);
    let mut selected: Vec<Ranked> = Vec::new();
    let mut used_bytes = 0usize;
    let mut below_relevance = 0usize;
    let mut over_budget = 0usize;

    for entry in ranked {
        if entry.base_score < MIN_RELEVANCE_SCORE {
            below_relevance += 1;
            continue;
        }
        if selected.len() >= max_entries {
            over_budget += 1;
            continue;
        }
        let rendered = format!(
            "{}\n{}",
            entry.candidate.entry.key, entry.candidate.entry.body
        )
        .len();
        let separator = if selected.is_empty() { 0 } else { 2 };
        if used_bytes + separator + rendered > max_bytes {
            over_budget += 1;
            continue;
        }
        used_bytes += separator + rendered;
        selected.push(entry);
    }

    RetrievalSelection {
        selected,
        below_relevance,
        over_budget,
    }
}

/// Gathers ONE scope's bank as retrieval candidates (impure: reads the
/// store, takes `now` for staleness -- the read/clock boundary lives here,
/// same discipline `memory::render_for_prompt` follows). Reuses `memory::
/// list_scoped` verbatim -- already scope-generic and self-gated on
/// `scope.enabled(cfg)` -- rather than duplicating any store-reading
/// logic.
pub fn candidates_for_scope(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    now: u64,
) -> CtxResult<Vec<RetrievalCandidate>> {
    let entries = super::memory::list_scoped(scope, repo, state, slug, cfg)?;
    Ok(entries
        .into_iter()
        .map(|(_, entry)| {
            let verified_age_days = now.saturating_sub(entry.verified) / 86_400;
            RetrievalCandidate {
                shared: scope == MemoryScope::Shared,
                verified_age_days,
                entry,
            }
        })
        .collect())
}

/// Gathers BOTH scopes as retrieval candidates, for session-startup
/// retrieval -- mirrors `memory::render_for_prompt`'s own private+shared
/// merge, tagging provenance the same way. Each scope's own gate degrades
/// to empty rather than erroring (`list_scoped`'s existing contract), so a
/// disabled scope simply contributes nothing.
///
/// Dormant: not yet wired into any live launch seam. Wiring session
/// startup itself into a composed prompt is a later task's job (the
/// context compiler, issue #44) -- this function is the ready-to-use
/// primitive it should call, the same "dormant read primitive, wired in
/// later" pattern `memory::list_scoped` followed before issue #33 gave it
/// a consumer.
#[allow(dead_code)]
pub fn candidates_for_repo(
    state: &StateDir,
    repo: &Path,
    slug: &str,
    cfg: &CtxConfig,
    now: u64,
) -> Vec<RetrievalCandidate> {
    let mut candidates =
        candidates_for_scope(MemoryScope::Private, repo, state, slug, cfg, now).unwrap_or_default();
    candidates.extend(
        candidates_for_scope(MemoryScope::Shared, repo, state, slug, cfg, now).unwrap_or_default(),
    );
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(key: &str, body: &str, shared: bool) -> RetrievalCandidate {
        RetrievalCandidate {
            entry: Entry {
                key: key.to_string(),
                written_by: "claude".to_string(),
                written: 1_700_000_000,
                verified: 1_700_000_000,
                source: "explicit".to_string(),
                body: body.to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            shared,
            verified_age_days: 0,
        }
    }

    fn ctx(query: &str) -> RetrievalContext {
        RetrievalContext {
            query: query.to_string(),
            ..Default::default()
        }
    }

    // Determinism.

    #[test]
    fn ranking_the_same_inputs_twice_produces_the_same_order() {
        let candidates = vec![
            candidate("build-cmd", "cargo build --release", false),
            candidate(
                "staging-db-creds",
                "the staging DB creds live in 1Password",
                false,
            ),
            candidate("deploy-notes", "deploy via the release pipeline", false),
        ];
        let query = ctx("release");
        let first: Vec<String> = rank(&candidates, &query)
            .iter()
            .map(|r| r.candidate.entry.key.clone())
            .collect();
        let second: Vec<String> = rank(&candidates, &query)
            .iter()
            .map(|r| r.candidate.entry.key.clone())
            .collect();
        assert_eq!(
            first, second,
            "identical inputs must rank identically every time"
        );
    }

    // Signals: exact/partial key match.

    #[test]
    fn an_exact_key_match_ranks_ahead_of_a_mere_substring_hit() {
        let candidates = vec![
            candidate("staging-db-creds", "mentions db in its key only", false),
            candidate("db", "the exact-key entry", false),
        ];
        let ranked = rank(&candidates, &ctx("db"));
        assert_eq!(
            ranked[0].candidate.entry.key, "db",
            "exact match ranks first"
        );
        assert!(
            ranked[0].score > ranked[1].score,
            "exact match must outscore a mere substring hit: {ranked:?}"
        );
    }

    // Signals: keyword matches in body/tags.

    #[test]
    fn a_body_keyword_match_scores_above_the_relevance_floor() {
        let candidates = vec![candidate(
            "staging-db-creds",
            "the staging DB creds live in 1Password",
            false,
        )];
        let ranked = rank(&candidates, &ctx("1password"));
        assert!(
            ranked[0].score >= MIN_RELEVANCE_SCORE,
            "a body keyword hit must be selectable: {ranked:?}"
        );
        assert!(
            ranked[0].reasons.iter().any(|r| r.contains("keyword")),
            "the reason must name the keyword match: {:?}",
            ranked[0].reasons
        );
    }

    #[test]
    fn a_tag_keyword_match_also_scores_above_the_relevance_floor() {
        let mut c = candidate("deploy-notes", "see the runbook", false);
        c.entry.tags = vec!["release".to_string(), "deploy".to_string()];
        let candidates = [c];
        let ranked = rank(&candidates, &ctx("release"));
        assert!(ranked[0].score >= MIN_RELEVANCE_SCORE, "{ranked:?}");
    }

    // Signals: path/module relevance.

    #[test]
    fn a_matching_stored_path_outranks_an_unrelated_entry_for_the_same_weak_query() {
        let mut relevant = candidate("ctx-gotcha", "a gotcha about this module", false);
        relevant.entry.paths = vec!["src/commands/ctx".to_string()];
        let unrelated = candidate("unrelated-fact", "true about the repo generally", false);

        let candidates = vec![unrelated, relevant];
        let mut query = ctx("");
        query.cwd_path = "src/commands/ctx/memory.rs".to_string();
        let ranked = rank(&candidates, &query);
        assert_eq!(
            ranked[0].candidate.entry.key, "ctx-gotcha",
            "the path-relevant entry must rank first: {ranked:?}"
        );
    }

    #[test]
    fn a_changed_file_path_also_counts_as_a_path_match() {
        let mut c = candidate("ctx-gotcha", "a gotcha about this module", false);
        c.entry.paths = vec!["src/commands/ctx/prompt.rs".to_string()];
        let mut query = ctx("");
        query.changed_paths = vec!["src/commands/ctx/prompt.rs".to_string()];
        let candidates = [c];
        let ranked = rank(&candidates, &query);
        assert!(ranked[0].score >= MIN_RELEVANCE_SCORE, "{ranked:?}");
    }

    // Signals: importance/confidence/staleness.

    #[test]
    fn high_importance_and_confidence_add_to_the_score_low_ones_subtract() {
        // A real base signal (the query matches the key) establishes
        // relevance first -- importance/confidence only ever refine an
        // already-relevant match, never establish relevance on their own
        // (see `score_one`'s own doc comment on why).
        let mut high = candidate("release-notes", "body", false);
        high.entry.importance = Some("high".to_string());
        high.entry.confidence = Some("high".to_string());
        let mut low = candidate("release-notes", "body", false);
        low.entry.importance = Some("low".to_string());
        low.entry.confidence = Some("low".to_string());

        let (_, high_score, _) = score_one(&high, &ctx("release-notes"));
        let (_, low_score, _) = score_one(&low, &ctx("release-notes"));
        assert!(
            high_score > low_score,
            "high importance/confidence must score above low: {high_score} vs {low_score}"
        );
    }

    /// Bare importance/confidence with no real signal at all must never
    /// clear the relevance floor -- otherwise a bank full of "importance:
    /// high" entries would defeat an empty/weak query's degrade-safely
    /// guarantee.
    #[test]
    fn bare_importance_alone_cannot_clear_the_relevance_floor() {
        let mut c = candidate("a", "body", false);
        c.entry.importance = Some("high".to_string());
        c.entry.confidence = Some("high".to_string());
        let (base_score, score, reasons) = score_one(&c, &ctx(""));
        assert!(
            base_score < MIN_RELEVANCE_SCORE,
            "importance/confidence alone must not establish relevance: {base_score}, {reasons:?}"
        );
        assert_eq!(
            base_score, score,
            "with no real signal, base and final score must be equal (zero)"
        );
    }

    #[test]
    fn a_stale_verification_lowers_the_score_relative_to_a_fresh_one() {
        // Same reasoning as above: staleness only refines an already-
        // relevant match, so a real base signal is needed first.
        let mut fresh = candidate("release-notes", "body", false);
        fresh.verified_age_days = 0;
        let mut stale = candidate("release-notes", "body", false);
        stale.verified_age_days = 400;

        let (_, fresh_score, _) = score_one(&fresh, &ctx("release-notes"));
        let (_, stale_score, _) = score_one(&stale, &ctx("release-notes"));
        assert!(
            fresh_score > stale_score,
            "a fresh entry must score above a stale one, all else equal"
        );
    }

    // Regression: the relevance floor is tested against the BASE score,
    // never the final (modifier-adjusted) score -- staleness/importance/
    // confidence must never be able to erase a genuine match.

    #[test]
    fn an_exact_key_match_survives_selection_even_when_verified_two_years_ago() {
        let mut old = candidate("deploy-cmd", "run the deploy script", false);
        old.verified_age_days = 730;
        let candidates = [old];
        let selection = select(&candidates, &ctx("deploy-cmd"), 4096, 6);
        assert_eq!(
            selection.selected.len(),
            1,
            "an exact key match must survive a heavy staleness penalty: {:?}",
            selection.selected
        );
        assert_eq!(selection.below_relevance, 0);
    }

    #[test]
    fn a_single_body_keyword_match_survives_selection_at_seventy_days_stale() {
        let mut old = candidate(
            "staging-db-creds",
            "the staging DB creds live in 1Password",
            false,
        );
        old.verified_age_days = 70;
        let candidates = [old];
        let selection = select(&candidates, &ctx("1password"), 4096, 6);
        assert_eq!(
            selection.selected.len(),
            1,
            "a body keyword match must survive a moderate staleness penalty: {:?}",
            selection.selected
        );
        assert_eq!(selection.below_relevance, 0);
    }

    #[test]
    fn an_entry_with_zero_base_relevance_still_drops_regardless_of_modifiers() {
        let mut c = candidate("unrelated", "nothing to do with the query", false);
        c.entry.importance = Some("high".to_string());
        c.entry.confidence = Some("high".to_string());
        c.verified_age_days = 0;
        let candidates = [c];
        let selection = select(&candidates, &ctx("release"), 4096, 6);
        assert!(
            selection.selected.is_empty(),
            "zero base relevance must still drop even with favorable modifiers: {:?}",
            selection.selected
        );
        assert_eq!(selection.below_relevance, 1);
    }

    // Precedence: private outranks shared, structurally.

    #[test]
    fn a_shared_entry_with_inflated_importance_still_cannot_outrank_a_private_entry() {
        let private = candidate("private-fact", "unremarkable body", false);
        let mut shared = candidate("shared-fact", "an attacker-tagged body", true);
        shared.entry.importance = Some("high".to_string());
        shared.entry.confidence = Some("high".to_string());
        shared.entry.tags = vec!["release".to_string()];

        let candidates = vec![shared, private];
        let ranked = rank(&candidates, &ctx("release"));
        assert_eq!(
            ranked[0].candidate.entry.key, "private-fact",
            "private must rank first regardless of the shared entry's own inflated signals: \
             {ranked:?}"
        );
    }

    // Budget + degrade-safely.

    #[test]
    fn an_empty_query_with_no_context_selects_nothing() {
        let candidates = vec![
            candidate("a", "alpha body", false),
            candidate("b", "beta body", false),
            candidate("c", "gamma body", false),
        ];
        let selection = select(&candidates, &ctx(""), 4096, 6);
        assert!(
            selection.selected.is_empty(),
            "an empty/weak query must never inject the whole bank: {:?}",
            selection.selected
        );
        assert_eq!(selection.below_relevance, 3);
    }

    #[test]
    fn selection_respects_the_entry_count_cap() {
        let candidates: Vec<RetrievalCandidate> = (0..10)
            .map(|i| candidate(&format!("k{i}"), "mentions release notes", false))
            .collect();
        let selection = select(&candidates, &ctx("release"), 4096, 3);
        assert_eq!(selection.selected.len(), 3);
        assert_eq!(selection.over_budget, 7);
    }

    #[test]
    fn selection_respects_the_byte_cap_and_skips_an_oversized_entry_without_starving_the_rest() {
        let huge = candidate("huge", &"release ".repeat(200), false);
        let small = candidate("small", "release notes", false);
        let candidates = vec![huge, small];
        let selection = select(&candidates, &ctx("release"), 50, 6);
        let keys: Vec<&str> = selection
            .selected
            .iter()
            .map(|r| r.candidate.entry.key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["small"],
            "the oversized entry must not starve the small one"
        );
    }

    // Gathering candidates from the store.

    #[test]
    fn candidates_for_scope_reads_the_private_bank_and_tags_it_unshared() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        super::super::memory::remember(
            &state,
            "-work-repo",
            &Entry {
                key: "build-cmd".to_string(),
                written_by: "claude".to_string(),
                written: 1_700_000_000,
                verified: 1_700_000_000,
                source: "explicit".to_string(),
                body: "cargo build --release".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let candidates = candidates_for_scope(
            MemoryScope::Private,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            1_700_000_000,
        )
        .expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].shared);
        assert_eq!(candidates[0].entry.key, "build-cmd");
    }

    #[test]
    fn candidates_for_repo_merges_both_scopes() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        super::super::memory::remember(
            &state,
            "-work-repo",
            &Entry {
                key: "private-fact".to_string(),
                written_by: "claude".to_string(),
                written: 1_700_000_000,
                verified: 1_700_000_000,
                source: "explicit".to_string(),
                body: "private body".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember private");

        let shared_dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&shared_dir).expect("mkdir");
        std::fs::write(
            shared_dir.join("shared-fact.md"),
            Entry {
                key: "shared-fact".to_string(),
                written_by: "claude".to_string(),
                written: 1_700_000_000,
                verified: 1_700_000_000,
                source: "explicit".to_string(),
                body: "shared body".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            }
            .to_markdown(),
        )
        .expect("write shared");

        let candidates =
            candidates_for_repo(&state, repo.path(), "-work-repo", &cfg, 1_700_000_000);
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .any(|c| c.entry.key == "private-fact" && !c.shared)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.entry.key == "shared-fact" && c.shared)
        );
    }
}
