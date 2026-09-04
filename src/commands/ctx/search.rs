//! `zirv ctx search`: zero-model cross-session recall over Claude/Codex
//! transcripts, handoffs, `.zirv/work/*` artifacts and mail (issue #315),
//! so a mid-task agent can check whether a prior session already worked
//! through a familiar failure without spending a model call to find out.
//!
//! [`rank`], [`lineage_survivors`], [`demoted_sessions`], [`build_window`]
//! and [`screen_text`] are pure functions of the index/text handed to them;
//! [`run`]/[`run_with`] are the thin I/O shell that resolves the state dir
//! and repo, walks the candidate directories, refreshes the persisted index
//! (`search_index::build_index`) and renders the result.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::config::{CtxConfig, env_from_process};
use super::search_index::{IndexedFile, SearchIndex, Source};
use super::state::{self, StateDir};
use super::{CtxResult, config::EnvLookup};

/// `--json`'s own schema version, the same convention `spend.rs`'s
/// `SPEND_SCHEMA_VERSION` uses.
pub const SEARCH_SCHEMA_VERSION: u32 = 1;

/// The fixed +-N message window around the top hit (DISCOVERY) or the named
/// message (SCROLL) -- not operator-configurable, matching the design's own
/// "adaptively hydrate only the top hit with a +-5-message window" origin
/// (Hermes' `session_search_tool.py`).
pub const DEFAULT_WINDOW_RADIUS: usize = 5;

#[derive(Debug, Clone, clap::Args)]
pub struct SearchArgs {
    /// Search query text. Not required when `--session`/`--around` scroll to
    /// a specific point in an already-indexed session instead of ranking.
    pub query: Option<String>,
    /// SCROLL mode: show the window around this message ordinal in the named
    /// session instead of ranking a query. Requires `--session`.
    #[arg(long, requires = "session")]
    pub around: Option<usize>,
    /// SCROLL mode: the session id (transcript file stem) to scroll within.
    /// Requires `--around`.
    #[arg(long, requires = "around")]
    pub session: Option<String>,
    /// Widen Claude transcript scope to every project, not just the current
    /// repository's own. Handoffs, work artifacts and mail stay scoped to
    /// the current repo either way -- they are stored per-repo already.
    #[arg(long, default_value_t = false)]
    pub all_repos: bool,
    /// Machine-readable output, schema-versioned (`"schema": 1`).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

// ---------------------------------------------------------------------
// Candidate discovery (I/O: directory walks)
// ---------------------------------------------------------------------

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    if is_symlink(dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_symlink(&path) {
            continue;
        }
        if path.is_dir() {
            walk_jsonl(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn md_files_in(dir: &Path) -> Vec<PathBuf> {
    if is_symlink(dir) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            !is_symlink(p) && p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md")
        })
        .collect()
}

/// Claude Code's own per-project transcript directory (`permissions::
/// claude_project_dir_name`, the identical encoding `--repo`/cwd audit
/// scoping already uses) by default; every project under `window::
/// projects_root()` with `--all-repos`.
fn claude_candidates(repo: &Path, all_repos: bool) -> Vec<(PathBuf, Source)> {
    let Ok(root) = super::window::projects_root() else {
        return Vec::new();
    };
    let mut files = Vec::new();
    if all_repos {
        walk_jsonl(&root, &mut files);
    } else {
        walk_jsonl(
            &root.join(super::permissions::claude_project_dir_name(repo)),
            &mut files,
        );
    }
    files.into_iter().map(|p| (p, Source::Claude)).collect()
}

/// Codex has no per-project rollout directory to scope by (`permissions::
/// transcripts_root`'s own doc comment on `AuditAgent::Codex`), so this is
/// always machine-wide regardless of `--all-repos`, mirroring the audit's
/// own codex behaviour.
fn codex_candidates() -> Vec<(PathBuf, Source)> {
    let Ok(home) = crate::utils::home_dir() else {
        return Vec::new();
    };
    let mut files = Vec::new();
    walk_jsonl(&home.join(".codex").join("sessions"), &mut files);
    files.into_iter().map(|p| (p, Source::Codex)).collect()
}

/// `<state>/handoffs/<repo_slug>/*.md` -- zirv's own `state::repo_slug`, not
/// Claude Code's project encoding (`handoff::store`'s own layout).
fn handoff_candidates(state: &StateDir, repo: &Path) -> Vec<(PathBuf, Source)> {
    md_files_in(&state.handoffs().join(state::repo_slug(repo)))
        .into_iter()
        .map(|p| (p, Source::Handoff))
        .collect()
}

/// `<state>/mail/<repo_slug>/{unread,read}/*.md` (`mail.rs`'s own layout;
/// there is no separate "archive" -- see this issue's own verify note).
fn mail_candidates(state: &StateDir, repo: &Path) -> Vec<(PathBuf, Source)> {
    let base = state.mail().join(state::repo_slug(repo));
    let mut out = Vec::new();
    for sub in ["unread", "read"] {
        out.extend(
            md_files_in(&base.join(sub))
                .into_iter()
                .map(|p| (p, Source::Mail)),
        );
    }
    out
}

/// `<repo>/.zirv/work/<id>/*.md` and `<repo>/.zirv/work/<id>/review/*.md`.
fn work_candidates(repo: &Path) -> Vec<(PathBuf, Source)> {
    let root = repo.join(".zirv").join("work");
    if is_symlink(&root) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || is_symlink(&dir) {
            continue;
        }
        out.extend(md_files_in(&dir).into_iter().map(|p| (p, Source::Work)));
        out.extend(
            md_files_in(&dir.join("review"))
                .into_iter()
                .map(|p| (p, Source::Work)),
        );
    }
    out
}

fn all_candidates(state: &StateDir, repo: &Path, all_repos: bool) -> Vec<(PathBuf, Source)> {
    let mut out = claude_candidates(repo, all_repos);
    out.extend(codex_candidates());
    out.extend(handoff_candidates(state, repo));
    out.extend(mail_candidates(state, repo));
    out.extend(work_candidates(repo));
    out
}

// ---------------------------------------------------------------------
// Demotion: loop/exec-launched sessions rank below interactive ones
// ---------------------------------------------------------------------

/// Every session id the durable decision log (`<state>/logs/decisions.
/// jsonl`, `log::LOG_FILE`) ever recorded under `verb` `"loop"` or `"exec"`
/// -- the field distinguishing a stateless-loop/headless-exec launch from an
/// interactive `wrap`/`chat`/`dash` one, mirroring Hermes' own
/// `_DEMOTED_SESSION_SOURCES = ("cron",)`. Read from the decision log rather
/// than `sessions::Record` because a `Record` is deleted the moment its
/// session ends (`SessionGuard::drop`) -- by the time a search runs, almost
/// every session it would classify is already gone from that registry, but
/// the append-only decision log keeps every verb it ever logged. Pure over
/// the log's own text; a corrupt or missing log degrades to "nothing
/// demoted", never a hard error.
pub fn demoted_sessions(decisions_text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in decisions_text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let verb = v.get("verb").and_then(|x| x.as_str());
        if matches!(verb, Some("loop") | Some("exec"))
            && let Some(session) = v.get("session").and_then(|x| x.as_str())
        {
            out.insert(session.to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------
// Lineage dedupe: collapse a restart chain to its newest file
// ---------------------------------------------------------------------

/// Indices into `files` that survive lineage dedupe: every file with no
/// detected `lineage_root` (standalone), plus for each `lineage_root` that
/// IS shared by two or more files, only the one with the greatest `mtime`
/// (`"collapses ... to its newest hit"`, issue #315's own acceptance
/// criterion). Pure over `IndexedFile`'s own `mtime`/`lineage_root` fields --
/// no message content is inspected here.
fn lineage_survivors(files: &[IndexedFile]) -> Vec<usize> {
    let mut newest_by_root: HashMap<&str, usize> = HashMap::new();
    let mut standalone = Vec::new();
    for (i, f) in files.iter().enumerate() {
        match f.lineage_root.as_deref() {
            None => standalone.push(i),
            Some(root) => {
                newest_by_root
                    .entry(root)
                    .and_modify(|cur| {
                        if files[*cur].mtime < f.mtime {
                            *cur = i;
                        }
                    })
                    .or_insert(i);
            }
        }
    }
    let mut out: Vec<usize> = newest_by_root.into_values().collect();
    out.extend(standalone);
    out
}

// ---------------------------------------------------------------------
// Ranking: case-folded BM25 with recency decay (pure)
// ---------------------------------------------------------------------

/// Half-life, in days, of a message's recency-decay multiplier -- tuned so a
/// two-week-old session still ranks (0.5x its raw score) but a same-day one
/// dominates ties. Not a config knob: this is a ranking-quality constant,
/// not a trust boundary, and every other numeric ranking constant here
/// (`K1`/`B`) is fixed the same way.
const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;
/// Floor on the recency multiplier: an old but strongly relevant hit must
/// still surface, never decayed away to (near) zero.
const MIN_RECENCY_FACTOR: f64 = 0.1;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 2)
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoredHit {
    pub path: String,
    pub source: &'static str,
    pub session_id: Option<String>,
    pub ordinal: usize,
    pub role: String,
    /// Already screened (`screen_text`) -- see [`rank`]'s own doc comment.
    /// Never the raw indexed text: a hit reaches a caller (JSON or plain
    /// text) only after any credential-shaped or high-entropy line has been
    /// redacted.
    pub text: String,
    pub at: Option<u64>,
    pub score: f64,
    pub demoted: bool,
}

/// Ranks every message across `files` against `query`: case-folded BM25 term
/// scoring, a recency-decay multiplier from each message's own `at` (or its
/// file's `mtime` when the message has none), lineage dedupe applied first
/// (older files in a detected restart chain never contribute candidates),
/// and demoted (`loop`/`exec`-launched) sessions ranked below an
/// equally-scored interactive one. A message with no query-term overlap at
/// all is never returned. Every returned hit's own `text` is screened
/// (`screen_text`) before it is stored, so a caller can serialize a
/// `ScoredHit` (JSON `top_hit`, in particular) without re-screening it
/// itself -- the same guarantee `build_window` already gives the hydrated
/// window. Pure: `now` is the only clock input, passed in by the caller
/// rather than read here.
pub fn rank(
    files: &[IndexedFile],
    query: &str,
    demoted: &HashSet<String>,
    now: u64,
) -> Vec<ScoredHit> {
    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return Vec::new();
    }
    let survivors = lineage_survivors(files);

    struct Doc {
        file_i: usize,
        msg_i: usize,
        terms: HashMap<String, u32>,
        len: usize,
    }
    let mut docs = Vec::new();
    for &fi in &survivors {
        for (mi, m) in files[fi].messages.iter().enumerate() {
            let toks = tokenize(&m.text);
            if toks.is_empty() {
                continue;
            }
            let mut terms = HashMap::new();
            for t in &toks {
                *terms.entry(t.clone()).or_insert(0u32) += 1;
            }
            docs.push(Doc {
                file_i: fi,
                msg_i: mi,
                terms,
                len: toks.len(),
            });
        }
    }
    if docs.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as f64;
    let avgdl = docs.iter().map(|d| d.len as f64).sum::<f64>() / n;
    let mut df: HashMap<&str, f64> = HashMap::new();
    for term in &query_terms {
        let count = docs.iter().filter(|d| d.terms.contains_key(term)).count() as f64;
        df.insert(term.as_str(), count);
    }

    let mut hits = Vec::new();
    for doc in &docs {
        let mut score = 0.0;
        for term in &query_terms {
            let tf = *doc.terms.get(term).unwrap_or(&0) as f64;
            if tf == 0.0 {
                continue;
            }
            let dfreq = *df.get(term.as_str()).unwrap_or(&0.0);
            let idf = ((n - dfreq + 0.5) / (dfreq + 0.5) + 1.0).ln();
            let denom = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * (doc.len as f64 / avgdl));
            score += idf * (tf * (BM25_K1 + 1.0)) / denom;
        }
        if score <= 0.0 {
            continue;
        }
        let f = &files[doc.file_i];
        let m = &f.messages[doc.msg_i];
        let at = m.at.unwrap_or(f.mtime);
        let age_days = now.saturating_sub(at) as f64 / 86_400.0;
        let decay = 0.5f64
            .powf(age_days / RECENCY_HALF_LIFE_DAYS)
            .max(MIN_RECENCY_FACTOR);
        let is_demoted = f
            .session_id
            .as_deref()
            .map(|s| demoted.contains(s))
            .unwrap_or(false);
        hits.push(ScoredHit {
            path: f.path.clone(),
            source: f.source.label(),
            session_id: f.session_id.clone(),
            ordinal: m.ordinal,
            role: m.role.clone(),
            text: screen_text(&m.text),
            at: m.at,
            score: score * decay,
            demoted: is_demoted,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.demoted.cmp(&b.demoted))
            .then(b.at.unwrap_or(0).cmp(&a.at.unwrap_or(0)))
            .then(a.path.cmp(&b.path))
    });
    hits
}

// ---------------------------------------------------------------------
// Secret screening + window rendering (pure)
// ---------------------------------------------------------------------

/// Screens `text` line by line with `screen::screen`'s own credential-shape
/// and high-entropy-run detectors (plus its prompt-injection markers, which
/// are just as unsafe to echo back verbatim into a session): a flagged line
/// is replaced outright by a `[redacted -- <summary>]` marker rather than
/// printed raw, unlike `screen::screen` itself, which only flags and never
/// mutates. Whole-line redaction, not a sub-string cut: neither detector
/// exposes the matched span, so the safe unit to drop is the line it fired
/// on.
pub fn screen_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let report = super::screen::screen(line);
            if report.is_clean() {
                line.to_string()
            } else {
                format!("[redacted -- {}]", report.summary())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowLine {
    pub ordinal: usize,
    pub role: String,
    pub text: String,
}

/// The +-`radius` message window around `center` in `file`, screened. Never
/// panics on an out-of-range `center`: it simply clamps to the file's own
/// message count.
pub fn build_window(file: &IndexedFile, center: usize, radius: usize) -> Vec<WindowLine> {
    if file.messages.is_empty() {
        return Vec::new();
    }
    let last = file.messages.len() - 1;
    let center = center.min(last);
    let lo = center.saturating_sub(radius);
    let hi = (center + radius).min(last);
    file.messages[lo..=hi]
        .iter()
        .enumerate()
        .map(|(offset, m)| WindowLine {
            ordinal: lo + offset,
            role: m.role.clone(),
            text: screen_text(&m.text),
        })
        .collect()
}

/// Renders `lines` (already screened) into one block, hard-capped at
/// `max_bytes` total (`[search] max_output_bytes`) so a mid-task agent can
/// call this without flooding its own context -- a window that would
/// overflow is truncated with a trailing marker rather than silently
/// dropping lines off the end.
pub fn render_window_text(lines: &[WindowLine], center: usize, max_bytes: usize) -> String {
    let mut out = String::new();
    for line in lines {
        let marker = if line.ordinal == center { "*" } else { " " };
        out.push_str(&format!(
            "{marker} [{} #{}] {}\n",
            line.role, line.ordinal, line.text
        ));
    }
    if out.len() > max_bytes {
        let mut cut = max_bytes.saturating_sub(20).max(1);
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n... [truncated]");
    }
    out
}

// ---------------------------------------------------------------------
// I/O shell
// ---------------------------------------------------------------------

pub fn run<W: Write>(args: &SearchArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let repo = std::env::current_dir()?;
    run_with(args, w, &repo, &env, state::now_secs())
}

pub fn run_with<W: Write>(
    args: &SearchArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now: u64,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let repo_slug = state::repo_slug(repo);

    let candidates = all_candidates(&state, repo, args.all_repos);
    let existing = SearchIndex::load(&state, &repo_slug);
    let index = super::search_index::build_index(existing, &candidates);
    let _ = index.save(&state, &repo_slug);

    let decisions_text =
        std::fs::read_to_string(state.logs().join(super::log::LOG_FILE)).unwrap_or_default();
    let demoted = demoted_sessions(&decisions_text);

    if let (Some(session), Some(around)) = (&args.session, args.around) {
        return scroll(
            &index,
            session,
            around,
            cfg.search.max_output_bytes,
            args.json,
            w,
        );
    }

    let Some(query) = args.query.as_deref().filter(|q| !q.trim().is_empty()) else {
        writeln!(
            w,
            "zirv ctx search: a query is required unless --session and --around are given"
        )?;
        return Ok(2);
    };

    let hits = rank(&index.files, query, &demoted, now);
    let Some(top) = hits.first() else {
        if args.json {
            writeln!(
                w,
                "{}",
                serde_json::json!({"schema": SEARCH_SCHEMA_VERSION, "query": query, "hits": 0})
            )?;
        } else {
            writeln!(w, "zirv ctx search: no matches for \"{query}\"")?;
        }
        return Ok(0);
    };
    let file = index
        .files
        .iter()
        .find(|f| f.path == top.path)
        .expect("top hit's own path must be in the freshly built index");
    let window = build_window(file, top.ordinal, DEFAULT_WINDOW_RADIUS);

    if args.json {
        let payload = serde_json::json!({
            "schema": SEARCH_SCHEMA_VERSION,
            "query": query,
            "top_hit": top,
            "window": window,
        });
        writeln!(w, "{}", serde_json::to_string(&payload)?)?;
    } else {
        let header = format!(
            "zirv ctx search \"{query}\" -> {} {} (message #{}, score {:.2}{})\n",
            top.source,
            top.session_id.as_deref().unwrap_or(&top.path),
            top.ordinal,
            top.score,
            if top.demoted {
                ", loop/exec session"
            } else {
                ""
            }
        );
        let body = render_window_text(
            &window,
            top.ordinal,
            cfg.search.max_output_bytes.saturating_sub(header.len()),
        );
        writeln!(w, "{header}{body}")?;
    }
    Ok(0)
}

fn scroll<W: Write>(
    index: &SearchIndex,
    session: &str,
    around: usize,
    max_output_bytes: usize,
    json: bool,
    w: &mut W,
) -> CtxResult<i32> {
    let Some(file) = index
        .files
        .iter()
        .find(|f| f.session_id.as_deref() == Some(session) || f.path.contains(session))
    else {
        writeln!(
            w,
            "zirv ctx search: no indexed file matching session \"{session}\""
        )?;
        return Ok(1);
    };
    let window = build_window(file, around, DEFAULT_WINDOW_RADIUS);
    if json {
        let payload = serde_json::json!({
            "schema": SEARCH_SCHEMA_VERSION,
            "session": session,
            "around": around,
            "source": file.source.label(),
            "path": file.path,
            "window": window,
        });
        writeln!(w, "{}", serde_json::to_string(&payload)?)?;
    } else {
        let header = format!(
            "zirv ctx search --session {session} --around {around} -> {}\n",
            file.path
        );
        let body = render_window_text(
            &window,
            around,
            max_output_bytes.saturating_sub(header.len()),
        );
        writeln!(w, "{header}{body}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::search_index::IndexedMessage;

    fn file(
        path: &str,
        mtime: u64,
        lineage: Option<&str>,
        messages: Vec<IndexedMessage>,
    ) -> IndexedFile {
        IndexedFile {
            path: path.to_string(),
            mtime,
            size: 1,
            lineage_root: lineage.map(str::to_string),
            source: Source::Claude,
            session_id: Some(path.to_string()),
            messages,
        }
    }

    fn msg(ordinal: usize, role: &str, text: &str, at: Option<u64>) -> IndexedMessage {
        IndexedMessage {
            ordinal,
            role: role.to_string(),
            text: text.to_string(),
            at,
        }
    }

    // -- demoted_sessions --------------------------------------------------

    #[test]
    fn demoted_sessions_collects_loop_and_exec_verbs_only() {
        let log = "{\"session\":\"s1\",\"verb\":\"loop\"}\n{\"session\":\"s2\",\"verb\":\"exec\"}\n{\"session\":\"s3\",\"verb\":\"wrap\"}\n";
        let demoted = demoted_sessions(log);
        assert!(demoted.contains("s1"));
        assert!(demoted.contains("s2"));
        assert!(!demoted.contains("s3"));
    }

    #[test]
    fn demoted_sessions_is_empty_for_a_missing_or_corrupt_log() {
        assert!(demoted_sessions("").is_empty());
        assert!(demoted_sessions("not json\nalso not json\n").is_empty());
    }

    // -- lineage_survivors ---------------------------------------------------

    #[test]
    fn lineage_survivors_keeps_only_the_newest_of_a_shared_root() {
        let files = vec![
            file("old", 100, Some("ship webhook"), vec![]),
            file("new", 200, Some("ship webhook"), vec![]),
            file("standalone", 50, None, vec![]),
        ];
        let mut survivors = lineage_survivors(&files);
        survivors.sort();
        assert_eq!(survivors, vec![1, 2]);
    }

    // -- rank ----------------------------------------------------------------

    #[test]
    fn rank_finds_the_matching_message() {
        let files = vec![file(
            "a",
            1000,
            None,
            vec![
                msg(0, "user", "please fix the webhook timeout bug", Some(1000)),
                msg(
                    1,
                    "assistant",
                    "unrelated text about something else",
                    Some(1001),
                ),
            ],
        )];
        let hits = rank(&files, "webhook timeout", &HashSet::new(), 1000);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ordinal, 0);
    }

    #[test]
    fn rank_returns_nothing_for_an_empty_query() {
        let files = vec![file(
            "a",
            1000,
            None,
            vec![msg(0, "user", "hello there", Some(1000))],
        )];
        assert!(rank(&files, "", &HashSet::new(), 1000).is_empty());
    }

    #[test]
    fn rank_excludes_messages_with_no_term_overlap() {
        let files = vec![file(
            "a",
            1000,
            None,
            vec![msg(0, "user", "completely unrelated content", Some(1000))],
        )];
        assert!(rank(&files, "webhook timeout", &HashSet::new(), 1000).is_empty());
    }

    #[test]
    fn rank_never_surfaces_a_message_from_an_older_lineage_file() {
        let files = vec![
            file(
                "old",
                100,
                Some("ship webhook"),
                vec![msg(
                    0,
                    "user",
                    "webhook timeout bug still happening",
                    Some(100),
                )],
            ),
            file(
                "new",
                200,
                Some("ship webhook"),
                vec![msg(
                    0,
                    "user",
                    "unrelated content in the newer session",
                    Some(200),
                )],
            ),
        ];
        let hits = rank(&files, "webhook timeout", &HashSet::new(), 200);
        assert!(
            hits.is_empty(),
            "the older lineage file must never contribute a hit"
        );
    }

    #[test]
    fn rank_breaks_a_relevance_tie_in_favour_of_the_interactive_session() {
        // Two files with byte-identical message text (so the raw BM25 score
        // and recency decay are IDENTICAL) -- only the demotion flag differs.
        let files = vec![
            file(
                "loop-session",
                1000,
                None,
                vec![msg(
                    0,
                    "assistant",
                    "fixed the webhook timeout bug",
                    Some(1000),
                )],
            ),
            file(
                "interactive-session",
                1000,
                None,
                vec![msg(
                    0,
                    "assistant",
                    "fixed the webhook timeout bug",
                    Some(1000),
                )],
            ),
        ];
        let mut demoted = HashSet::new();
        demoted.insert("loop-session".to_string());
        let hits = rank(&files, "webhook timeout", &demoted, 1000);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "interactive-session");
        assert_eq!(hits[1].path, "loop-session");
    }

    #[test]
    fn rank_prefers_a_more_relevant_demoted_hit_over_a_less_relevant_interactive_one() {
        // Demotion is a tie-break, not an absolute floor: a strongly
        // relevant loop-session hit still outranks a barely relevant
        // interactive one.
        let files = vec![
            file(
                "loop-session",
                1000,
                None,
                vec![msg(
                    0,
                    "assistant",
                    "webhook timeout webhook timeout webhook timeout",
                    Some(1000),
                )],
            ),
            file(
                "interactive-session",
                1000,
                None,
                vec![msg(
                    0,
                    "assistant",
                    "webhook mentioned once in passing",
                    Some(1000),
                )],
            ),
        ];
        let mut demoted = HashSet::new();
        demoted.insert("loop-session".to_string());
        let hits = rank(&files, "webhook timeout", &demoted, 1000);
        assert_eq!(hits[0].path, "loop-session");
    }

    // -- screening / redaction ----------------------------------------------

    #[test]
    fn screen_text_redacts_a_credential_shaped_line_without_printing_it_raw() {
        let text = "here is a key: ghp_1234567890abcdefghijklmnopqrstuvwx for the build";
        let out = screen_text(text);
        assert!(!out.contains("ghp_1234567890abcdefghijklmnopqrstuvwx"));
        assert!(out.starts_with("[redacted"));
    }

    #[test]
    fn screen_text_leaves_ordinary_lines_untouched() {
        let text = "the build failed because tests timed out";
        assert_eq!(screen_text(text), text);
    }

    #[test]
    fn screen_text_redacts_only_the_flagged_line_of_a_multiline_block() {
        let text = "ordinary line one\nghp_1234567890abcdefghijklmnopqrstuvwx\nordinary line two";
        let out = screen_text(text);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "ordinary line one");
        assert!(lines[1].starts_with("[redacted"));
        assert_eq!(lines[2], "ordinary line two");
    }

    // -- window building / capping -------------------------------------------

    #[test]
    fn build_window_centers_on_the_requested_ordinal() {
        let f = file(
            "a",
            1,
            None,
            (0..20)
                .map(|i| msg(i, "user", &format!("msg {i}"), None))
                .collect(),
        );
        let window = build_window(&f, 10, 5);
        assert_eq!(window.first().unwrap().ordinal, 5);
        assert_eq!(window.last().unwrap().ordinal, 15);
    }

    #[test]
    fn build_window_clamps_at_the_edges() {
        let f = file(
            "a",
            1,
            None,
            (0..3)
                .map(|i| msg(i, "user", &format!("msg {i}"), None))
                .collect(),
        );
        let window = build_window(&f, 0, 5);
        assert_eq!(window.len(), 3);
    }

    #[test]
    fn render_window_text_caps_total_output_bytes() {
        let lines: Vec<WindowLine> = (0..50)
            .map(|i| WindowLine {
                ordinal: i,
                role: "user".to_string(),
                text: "x".repeat(200),
            })
            .collect();
        let rendered = render_window_text(&lines, 0, 2048);
        assert!(rendered.len() <= 2048 + 32, "got {} bytes", rendered.len());
        assert!(rendered.contains("truncated"));
    }

    // -- run_with acceptance --------------------------------------------------

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn run_with_finds_a_hit_in_a_handoff_and_screens_a_secret_line() {
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = home.path().join("state");
        let repo_slug = state::repo_slug(repo.path());
        let handoff_dir = state_dir.join("handoffs").join(&repo_slug);
        std::fs::create_dir_all(&handoff_dir).expect("mkdir");
        std::fs::write(
            handoff_dir.join("0000000001-abc.md"),
            "## Task\nfix the webhook timeout bug\n\n## Next step\ndeploy it\nghp_1234567890abcdefghijklmnopqrstuvwx\n\n",
        )
        .expect("write handoff");

        let env_vars = env_map(&[("ZIRV_CTX_STATE_DIR", state_dir.to_str().unwrap())]);
        let env: EnvLookup<'_> = &|k| env_vars.get(k).cloned();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let args = SearchArgs {
            query: Some("webhook timeout".to_string()),
            around: None,
            session: None,
            all_repos: false,
            json: true,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, repo.path(), env, 1_000_000).expect("run_with");
        assert_eq!(code, 0);
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("fix the webhook timeout bug"));
        assert!(!rendered.contains("ghp_1234567890abcdefghijklmnopqrstuvwx"));
    }

    #[test]
    fn run_with_requires_a_query_outside_scroll_mode() {
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = home.path().join("state");
        let env_vars = env_map(&[("ZIRV_CTX_STATE_DIR", state_dir.to_str().unwrap())]);
        let env: EnvLookup<'_> = &|k| env_vars.get(k).cloned();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let args = SearchArgs {
            query: None,
            around: None,
            session: None,
            all_repos: false,
            json: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, repo.path(), env, 1_000_000).expect("run_with");
        assert_eq!(code, 2);
    }
}
