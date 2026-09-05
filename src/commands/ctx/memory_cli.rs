//! Top-level `zirv memory` command family (issue #33): a management surface
//! for the memory bank (`super::memory`) that works without starting an AI
//! session -- `status`, `list`, `recall <query>`, `remember <key> <text>`,
//! `forget <key>`, `verify <key>`, each selecting the private (default) or
//! shared (`--shared`) scope. Intercepted directly against raw argv in
//! `main.rs`, the same way `ctx`/`chat`/`agent` are, rather than nested under
//! `zirv ctx` -- see `dispatch` below.
//!
//! This wraps the scope-generic store `super::memory` already exposes
//! (`list_scoped`/`upsert_scoped`/`forget_scoped`/`verify_scoped`,
//! `MemoryScope`, `duplicate_keys`) rather than duplicating any of its
//! logic. The private-scope arm of `remember` reuses `super::memory::
//! run_remember_with` directly -- the exact code path `zirv ctx remember`
//! itself calls -- so the two surfaces can never silently drift apart for
//! that scope; `zirv ctx remember`/`recall`/`forget` are otherwise untouched
//! by this module and keep working exactly as before. `recall` (issue #35)
//! routes through `super::retrieval`'s deterministic ranking engine instead
//! of the store's own `get_scoped` lookup -- see `run_recall_with`'s own
//! doc comment for the resulting behavior change.
//!
//! Gating: `list`/`recall` are reads and respect each scope's own gate
//! (`memory.enabled`/`memory.shared_enabled`) -- a disabled scope lists or
//! recalls nothing, via `MemoryScope::enabled`/`list_scoped`/`get_scoped`,
//! which already encode this. `status` is the one exception: it reports a
//! disabled scope's counts and bytes too (marked `disabled`), the same
//! "must never trap data" rule as `forget`/`verify`, since a byte count is
//! not the entry content the gate exists to withhold -- see
//! `write_scope_status`. `forget`/`verify` are maintenance verbs and stay
//! ungated, per the "disabling a scope must never trap data" rule `forget_scoped`/
//! `verify_scoped` already follow.

use std::io::{Read, Write};
use std::path::Path;

use clap::{Parser, Subcommand};
use serde::Serialize;

use super::CtxResult;
use super::adapters::{self, AGENT_ENV};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::handoff;
use super::memory::{self, Entry, MemoryScope};
use super::memory_optimize;
use super::retrieval::{self, Ranked, RetrievalContext};
use super::state::{StateDir, now_secs, repo_slug};

#[derive(Debug, Parser)]
#[command(
    name = "zirv memory",
    about = "Manage this repository's memory bank without starting an AI session.",
    disable_help_subcommand = true
)]
pub struct MemoryCli {
    #[command(subcommand)]
    pub verb: MemoryVerb,
}

#[derive(Debug, Subcommand)]
pub enum MemoryVerb {
    /// Bootstrap a small shared memory bank from durable repository surfaces.
    Init(InitArgs),
    /// Report scope availability, entry counts, stored bytes, and the
    /// configured injection budget -- never entry bodies. A disabled scope
    /// is marked `disabled` but still reports its counts and bytes: a byte
    /// count is not the content the gate exists to withhold.
    Status,
    /// List every entry in one scope (private by default).
    List(ListArgs),
    /// Rank this scope's entries by relevance to `query` (issue #35): key,
    /// keyword and tag matches, plus importance/confidence/staleness,
    /// budgeted by `memory.retrieval_max_bytes`/`retrieval_max_entries`.
    /// An empty or weak query (no matching signal) returns nothing rather
    /// than the whole bank.
    Recall(RecallArgs),
    /// Store a durable fact.
    Remember(RememberArgs),
    /// Remove one fact. Works even when the target scope is disabled --
    /// disabling a scope must never trap data behind it.
    Forget(ForgetArgs),
    /// Refresh a fact's `Verified` stamp, leaving its key, text and
    /// `Written` timestamp untouched. Works even when the target scope is
    /// disabled, same as `forget`.
    Verify(VerifyArgs),
    /// Analyze the shared memory bank for duplicates, near-duplicates,
    /// contradictions, stale/archived entries, obsolete paths, oversized or
    /// low-value entries, and core-regeneration opportunities (issue #38).
    /// REPORT-FIRST by default: prints findings and changes nothing.
    /// `--apply` (and not `--dry-run`, which always wins) additionally
    /// consolidates already-detected duplicate/near-duplicate groups: a
    /// merged body is proposed by a model, deterministically validated, and
    /// upserted onto the group's survivor entry -- every OTHER member of a
    /// group is left untouched, and a group containing a deliberate
    /// `Source: explicit` entry is never auto-applied. Never deletes or
    /// forgets anything, and never touches git.
    Optimize(OptimizeArgs),
    /// Reverses one journaled write by id (issue #295): restores the exact
    /// prior body for an overwrite, recreates a forgotten entry, or deletes
    /// an entry a create introduced. Replays through the normal write path,
    /// so a restored shared entry still runs the secret screen and a
    /// restored body over `max_entry_bytes` still truncates. Rolling back
    /// the same id twice is a no-op, not a double-inverse.
    Rollback(RollbackArgs),
    /// Moves an entry up a tier (issue #295): from the session tier (if a
    /// session id is present and the key lives there) or the private tier,
    /// into `--shared` or `--global`. Re-runs the destination scope's own
    /// caps and, for `--shared`, the secret screen -- a body that looks
    /// credential-shaped is refused, not silently promoted.
    Promote(PromoteArgs),
}

#[derive(Debug, clap::Args)]
pub struct RollbackArgs {
    /// The journal record id to reverse (`zirv ctx status` and this
    /// module's own journal reads print it).
    pub id: String,
}

#[derive(Debug, clap::Args)]
pub struct PromoteArgs {
    /// Key to promote.
    pub key: String,
    /// Promote into the shared, repository-owned bank.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
    /// Promote into the operator-owned global bank shared by every
    /// repository on this machine.
    #[arg(long, default_value_t = false, conflicts_with = "shared")]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Optional Markdown file or documentation/Obsidian directory to import.
    #[arg(long)]
    pub source: Option<std::path::PathBuf>,
    /// Propose entries without writing `.zirv/memory/`.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Add only missing keys when shared memory already exists.
    #[arg(long, default_value_t = false)]
    pub merge: bool,
    /// Maximum number of proposed entries.
    #[arg(long, default_value_t = 16)]
    pub max_entries: usize,
    /// Maximum total bytes across generated entry bodies.
    #[arg(long, default_value_t = 8192)]
    pub max_bytes: usize,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// List the shared, repository-owned bank (`<repo>/.zirv/memory/`,
    /// meant to be committed) instead of the private, machine-local one.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
    /// List the operator-owned global bank shared by every repository.
    #[arg(long, default_value_t = false, conflicts_with = "shared")]
    pub global: bool,
    /// Emit one JSON object per line instead of human-readable text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct RecallArgs {
    /// Text to search for.
    pub query: String,
    /// Search the shared bank instead of the private one.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
    /// Search the operator-owned global bank shared by every repository.
    #[arg(long, default_value_t = false, conflicts_with = "shared")]
    pub global: bool,
    /// Emit one JSON object per line instead of human-readable text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Issue #38: also consider `Archived` entries, which a normal recall
    /// excludes outright regardless of how strongly they match. Explicit
    /// recall is the one way an archived entry stays reachable at all.
    #[arg(long, default_value_t = false)]
    pub include_archived: bool,
}

#[derive(Debug, clap::Args)]
pub struct RememberArgs {
    /// The fact's key, e.g. "staging-db-creds".
    pub key: String,
    /// The fact's text.
    pub text: String,
    /// Store in the shared, repository-owned bank (committed with the repo)
    /// instead of the private, machine-local one. Unlike the private
    /// scope, which silently sanitizes any key into a safe file name, a
    /// shared key must be lowercase kebab-case and is REJECTED outright if
    /// it isn't.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
    /// Store in the operator-owned global bank shared by every repository.
    #[arg(long, default_value_t = false, conflicts_with = "shared")]
    pub global: bool,
    /// How important this fact is: `low`, `normal`, or `high`. Affects
    /// `zirv memory recall`'s ranking (`retrieval::score_one`); has no
    /// effect on `status`/`list`. Unset by default.
    #[arg(long)]
    pub importance: Option<String>,
    /// How confident this fact is: `low`, `normal`, or `high`. Affects
    /// `zirv memory recall`'s ranking the same way `--importance` does.
    #[arg(long)]
    pub confidence: Option<String>,
    /// A keyword `zirv memory recall` can also match this entry by.
    /// Repeatable: pass `--tag` more than once for more than one.
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Store into the shared bank even though its key, body, or a tag looks
    /// credential-shaped (`memory::sensitive_shared_match`). Has no effect
    /// without `--shared`, and no effect on the private bank, which the
    /// guard never inspects at all (issue #172's escape hatch, the same
    /// flag name and meaning `zirv ctx remember --repo --allow-sensitive`
    /// uses).
    #[arg(long, default_value_t = false)]
    pub allow_sensitive: bool,
    /// Issue #295: refuses the write (exit non-zero, nothing stored) unless
    /// the entry's CURRENT body hash matches this SHA-256 hex digest, or the
    /// literal value `absent`, which instead requires no entry exist yet for
    /// this key. See `zirv ctx remember --if-unchanged`'s own doc comment.
    #[arg(long)]
    pub if_unchanged: Option<String>,
}

/// The only values `--importance`/`--confidence` accept -- the same two
/// levels `retrieval::score_one` treats specially (`"high"`/`"low"`);
/// `"normal"` is the explicit no-op middle value. Rejects anything else
/// outright rather than silently storing a string ranking will just
/// ignore.
const LEVELS: [&str; 3] = ["low", "normal", "high"];

fn validate_level(flag: &str, value: &str) -> CtxResult<String> {
    if LEVELS.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(format!("{flag} must be one of {}; got '{value}'", LEVELS.join(", ")).into())
    }
}

fn validate_level_opt(flag: &str, value: &Option<String>) -> CtxResult<Option<String>> {
    value
        .as_deref()
        .map(|v| validate_level(flag, v))
        .transpose()
}

#[derive(Debug, clap::Args)]
pub struct ForgetArgs {
    /// Key to forget.
    pub key: String,
    /// Forget from the shared bank instead of the private one.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
    /// Forget from the operator-owned global bank shared by every repository.
    #[arg(long, default_value_t = false, conflicts_with = "shared")]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct VerifyArgs {
    /// Key to verify.
    pub key: String,
    /// Verify in the shared bank instead of the private one.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
    /// Verify in the operator-owned global bank shared by every repository.
    #[arg(long, default_value_t = false, conflicts_with = "shared")]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct OptimizeArgs {
    /// Apply consolidation for already-detected duplicate/near-duplicate
    /// groups that contain no deliberate `Source: explicit` entry. Without
    /// this flag (the default), `zirv memory optimize` only prints a
    /// report and changes nothing.
    #[arg(long, default_value_t = false)]
    pub apply: bool,
    /// Explicit report-only run: already the default, but always wins over
    /// `--apply` given on the same command line.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Skip the consolidation model call: report findings only, even under
    /// `--apply`.
    #[arg(long, default_value_t = false)]
    pub no_model: bool,
    /// Adapter name for the consolidation model call: claude or codex.
    /// Defaults to config, then claude.
    #[arg(long)]
    pub agent: Option<String>,
}
/// Thin, surface-local name for the centralized two-flag scope mapping.
fn scope_of(shared: bool, global: bool) -> MemoryScope {
    MemoryScope::from_flags(shared, global)
}

fn scope_label(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Private => "private",
        MemoryScope::Global => "global",
        MemoryScope::Shared => "shared",
        MemoryScope::Session => "session",
    }
}

/// Reports one scope's line: `enabled, N entries, B bytes` or `disabled, N
/// entries, B bytes` (body bytes only -- the same measure `optimize::
/// memory_bank_summary` uses, never header overhead). Counts and bytes are
/// shown even when the scope is disabled -- disabling a scope must never
/// hide what it holds, the same "must never trap data" rule `forget`/
/// `verify` already follow, so this reads through `list_scoped_unchecked`
/// rather than the gated `list_scoped`. An unreadable bank (I/O error) is
/// treated as empty rather than aborting the whole status report, the same
/// "unreadable means nothing" contract every other read path in this
/// module follows. Never prints a key or a body. For the shared scope
/// only, also warns about any canonical-key collision (`duplicate_keys`):
/// a hand-edited or merged directory can produce one, though
/// `upsert_shared` itself never creates one.
fn write_scope_status<W: Write>(
    w: &mut W,
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
) -> CtxResult<()> {
    let label = scope_label(scope);
    let status = if scope.enabled(cfg) {
        "enabled"
    } else {
        "disabled"
    };
    let entries = memory::list_scoped_unchecked(scope, repo, state, slug).unwrap_or_default();
    let bytes: usize = entries.iter().map(|(_, e)| e.body.len()).sum();
    writeln!(
        w,
        "{label} memory: {status}, {} entries, {bytes} bytes",
        entries.len()
    )?;
    if matches!(scope, MemoryScope::Shared) {
        let dups = memory::duplicate_keys(&entries);
        if !dups.is_empty() {
            writeln!(
                w,
                "  warning: {} canonical-key collision(s) from hand-edited or merged files: {}",
                dups.len(),
                dups.join(", ")
            )?;
        }
    }
    Ok(())
}

pub fn run_status_with<W: Write>(w: &mut W, repo: &Path, env: EnvLookup<'_>) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);

    writeln!(w, "memory bank status for {}", repo.display())?;
    write_scope_status(w, MemoryScope::Private, repo, &state, &slug, &cfg)?;
    write_scope_status(w, MemoryScope::Global, repo, &state, &slug, &cfg)?;
    write_scope_status(w, MemoryScope::Shared, repo, &state, &slug, &cfg)?;
    writeln!(
        w,
        "core injection budget: {} bytes (private + global + shared, merged private-first; issue #34)",
        cfg.memory.core_max_bytes
    )?;
    writeln!(
        w,
        "retrieval budget: {} bytes, {} entries max (bounds `zirv memory recall` today; \
         session-start injection lands with issue #44)",
        cfg.memory.retrieval_max_bytes, cfg.memory.retrieval_max_entries
    )?;
    Ok(0)
}

pub fn run_status<W: Write>(w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_status_with(w, &repo, &env)
}

pub fn run_init_with<W: Write>(args: &InitArgs, w: &mut W, repo: &Path) -> CtxResult<i32> {
    let report = crate::commands::setup::initialize_memory(
        repo,
        &crate::commands::setup::MemoryInitOptions {
            source: args.source.clone(),
            dry_run: args.dry_run,
            merge: args.merge,
            max_entries: args.max_entries,
            max_bytes: args.max_bytes,
        },
    )?;
    writeln!(
        w,
        "zirv memory init: {} proposed, {} written, {} existing skipped, {} body bytes{}",
        report.proposed,
        report.written,
        report.skipped_existing,
        report.body_bytes,
        if args.dry_run { " (dry run)" } else { "" }
    )?;
    Ok(0)
}

pub fn run_init<W: Write>(args: &InitArgs, w: &mut W) -> CtxResult<i32> {
    run_init_with(args, w, &std::env::current_dir()?)
}

/// Renders entries already selected from one scope. Never trusts an entry's
/// own header for its scope label (see `memory::ScopedEntry`, shared with
/// `zirv ctx recall`'s own JSON output rather than each surface keeping its
/// own identical copy -- issue #172 cross-review finding 6); a shared
/// entry's human-readable line additionally carries an explicit
/// untrusted-content note so a repo-committed `Source: explicit` can never
/// read as if it were operator-verified.
fn render_entries<W: Write>(
    w: &mut W,
    entries: &[Entry],
    scope: MemoryScope,
    json: bool,
) -> CtxResult<i32> {
    let now = now_secs();
    let label = scope_label(scope);
    for entry in entries {
        if json {
            let scoped = memory::ScopedEntry {
                entry,
                scope: label,
            };
            writeln!(w, "{}", serde_json::to_string(&scoped)?)?;
            continue;
        }
        let written_days = now.saturating_sub(entry.written) / 86_400;
        let verified_days = now.saturating_sub(entry.verified) / 86_400;
        let trust_note = match scope {
            MemoryScope::Shared => " -- shared: repository-owned content, not operator-verified",
            MemoryScope::Private | MemoryScope::Global | MemoryScope::Session => "",
        };
        writeln!(
            w,
            "{} [{label}{trust_note}] (written {written_days}d ago, verified {verified_days}d ago)\n{}\n",
            entry.key, entry.body
        )?;
    }
    Ok(0)
}

/// A ranked `Entry` plus its scope, score, and selection reasons -- the
/// JSON shape `zirv memory recall --json` emits (issue #35). `scope`
/// follows `memory::ScopedEntry`'s own rule: derived from which directory
/// was read, never from the entry's own header.
#[derive(Serialize)]
struct RankedEntry<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    scope: &'static str,
    score: i64,
    reasons: &'a [String],
}

/// Renders a ranking's selected entries. Reuses `memory::ScopedEntry`'s
/// scope-labeling and the shared-scope trust note from `render_entries`,
/// adding `score`/`reasons` -- the selection diagnostics issue #35 asks for
/// -- to both the JSON and human-readable forms.
fn render_ranked<W: Write>(
    w: &mut W,
    ranked: &[Ranked<'_>],
    scope: MemoryScope,
    json: bool,
) -> CtxResult<i32> {
    let now = now_secs();
    let label = scope_label(scope);
    for r in ranked {
        let entry = &r.candidate.entry;
        if json {
            let ranked_entry = RankedEntry {
                entry,
                scope: label,
                score: r.score,
                reasons: &r.reasons,
            };
            writeln!(w, "{}", serde_json::to_string(&ranked_entry)?)?;
            continue;
        }
        let written_days = now.saturating_sub(entry.written) / 86_400;
        let verified_days = now.saturating_sub(entry.verified) / 86_400;
        let trust_note = match scope {
            MemoryScope::Shared => " -- shared: repository-owned content, not operator-verified",
            MemoryScope::Private | MemoryScope::Global | MemoryScope::Session => "",
        };
        let reasons = if r.reasons.is_empty() {
            "no signal matched".to_string()
        } else {
            r.reasons.join(", ")
        };
        writeln!(
            w,
            "{} [{label}{trust_note}] (written {written_days}d ago, verified {verified_days}d ago) -- {reasons}\n{}\n",
            entry.key, entry.body
        )?;
    }
    Ok(0)
}

/// A disabled scope prints nothing on stdout (its own contract, unchanged
/// -- `list` is a read that respects the gate). Nit (fix round, memory
/// review): that used to be indistinguishable from a merely-empty scope,
/// and now reads inconsistently with `status`'s "(disabled)" transparency.
/// A one-line stderr note closes the gap without touching stdout, which a
/// caller may be parsing (`--json` or otherwise).
pub fn run_list_with<W: Write>(
    args: &ListArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let scope = scope_of(args.shared, args.global);
    if !scope.enabled(&cfg) {
        crate::output::warn(format!(
            "{} memory disabled ({}); listing nothing",
            scope_label(scope),
            scope.disabled_reason(&cfg)
        ));
    }
    let entries: Vec<Entry> = memory::list_scoped(scope, repo, &state, &slug, &cfg)?
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    render_entries(w, &entries, scope, args.json)
}

pub fn run_list<W: Write>(args: &ListArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_list_with(args, w, &repo, &env)
}

/// Issue #35: routes through the deterministic ranking engine
/// (`retrieval::rank`/`select`) instead of the old "exact key match, else
/// substring over key/body" pair. Deliberate behavior change: an exact key
/// match now ranks first rather than suppressing every other hit -- a
/// query that also matches other entries by keyword/tag/path can still
/// surface them, budgeted by `cfg.memory.retrieval_max_bytes`/
/// `retrieval_max_entries` and never exceeding either. An empty or weak
/// query (no signal clears the relevance floor) returns nothing, per
/// issue #35's "never inject the whole bank" requirement.
pub fn run_recall_with<W: Write>(
    args: &RecallArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let scope = scope_of(args.shared, args.global);

    let candidates = retrieval::candidates_for_scope(scope, repo, &state, &slug, &cfg, now_secs())?;
    let ctx = RetrievalContext {
        query: args.query.clone(),
        include_archived: args.include_archived,
        ..Default::default()
    };
    let selection = retrieval::select(
        &candidates,
        &ctx,
        cfg.memory.retrieval_max_bytes,
        cfg.memory.retrieval_max_entries,
    );
    render_ranked(w, &selection.selected, scope, args.json)
}

pub fn run_recall<W: Write>(args: &RecallArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_recall_with(args, w, &repo, &env)
}

pub fn run_remember_with<W: Write>(
    args: &RememberArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    stdin: &mut dyn Read,
) -> CtxResult<i32> {
    let importance = validate_level_opt("--importance", &args.importance)?;
    let confidence = validate_level_opt("--confidence", &args.confidence)?;

    let scope = scope_of(args.shared, args.global);
    if scope != MemoryScope::Shared {
        // The trusted arms are thin wrappers over the exact code path `zirv
        // ctx remember` calls (identity resolution, the `memory.enabled`
        // gate, the oversize/prune rules) -- reused rather than
        // reimplemented, so the two surfaces can never drift apart here.
        // `importance`/`confidence`/`tags` are `#[arg(skip)]` fields on
        // `memory::RememberArgs`: invisible to `zirv ctx remember`'s own
        // CLI parsing, settable only by building the struct directly here.
        let ctx_args = memory::RememberArgs {
            key: args.key.clone(),
            text: Some(args.text.clone()),
            text_file: None,
            verify: false,
            repo: false,
            global: scope == MemoryScope::Global,
            allow_sensitive: false,
            if_unchanged: args.if_unchanged.clone(),
            importance,
            confidence,
            tags: args.tags.clone(),
        };
        return memory::run_remember_with(&ctx_args, w, repo, env, stdin);
    }

    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let body = args.text.trim().to_string();
    if body.is_empty() {
        return Err("zirv memory remember: no text given".into());
    }
    // Review round 1, finding 2: same advisory lock `zirv ctx remember`'s
    // own `--if-unchanged` path takes, held from the check through the
    // write below (`_if_unchanged_lock` stays in scope until this function
    // returns), so two concurrent `zirv memory remember --shared
    // --if-unchanged` calls cannot both pass the check before either
    // writes.
    let _if_unchanged_lock = if args.if_unchanged.is_some() {
        match memory::lock_dir_for_if_unchanged(scope, repo, &state, &slug, None) {
            Some(dir) => Some(
                memory::lock_bank_dir(&dir).map_err(|e| format!("zirv memory remember: {e}"))?,
            ),
            None => None,
        }
    } else {
        None
    };
    if let Some(expected) = &args.if_unchanged {
        let existing = memory::get_scoped(scope, repo, &state, &slug, &cfg, &args.key)?;
        memory::check_if_unchanged(existing.as_ref(), expected)
            .map_err(|e| format!("zirv memory remember: {e}"))?;
    }
    let written_by = env(AGENT_ENV)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let now = now_secs();
    let entry = Entry {
        key: args.key.clone(),
        written_by,
        written: now,
        verified: now,
        source: "explicit".to_string(),
        body,
        importance,
        confidence,
        tags: args.tags.clone(),
        // Deliberately unwritable, unlike importance/confidence/tags above:
        // a path signal is inert until issue #44 wires it up (see
        // retrieval.rs's module doc), so no `--path` flag exists to set it
        // yet.
        paths: Vec::new(),
    };
    let path = if scope == MemoryScope::Shared && args.allow_sensitive {
        memory::upsert_shared_allow_sensitive(repo, &state, &slug, &cfg, &entry)?
    } else {
        memory::upsert_scoped(scope, repo, &state, &slug, &cfg, &entry)?
    };
    let label = scope_label(scope);
    writeln!(
        w,
        "zirv memory remember: stored '{}' in the {label} bank at {}",
        args.key,
        path.display()
    )?;
    Ok(0)
}

pub fn run_remember<W: Write>(args: &RememberArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_remember_with(args, w, &repo, &env, &mut std::io::stdin())
}

pub fn run_forget_with<W: Write>(
    args: &ForgetArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let scope = scope_of(args.shared, args.global);
    let label = scope_label(scope);
    if memory::forget_scoped(scope, repo, &state, &slug, &args.key)? {
        writeln!(
            w,
            "zirv memory forget: removed '{}' from the {label} bank",
            args.key
        )?;
    } else {
        writeln!(
            w,
            "zirv memory forget: no entry for '{}' in the {label} bank",
            args.key
        )?;
    }
    Ok(0)
}

pub fn run_forget<W: Write>(args: &ForgetArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_forget_with(args, w, &repo, &env)
}

pub fn run_verify_with<W: Write>(
    args: &VerifyArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let scope = scope_of(args.shared, args.global);
    let label = scope_label(scope);
    if memory::verify_scoped(scope, repo, &state, &slug, &args.key)? {
        writeln!(
            w,
            "zirv memory verify: verified '{}' in the {label} bank",
            args.key
        )?;
        Ok(0)
    } else {
        Err(format!(
            "zirv memory verify: no entry for '{}' in the {label} bank",
            args.key
        )
        .into())
    }
}

pub fn run_verify<W: Write>(args: &VerifyArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_verify_with(args, w, &repo, &env)
}

/// Issue #38. REPORT-FIRST: `analyze`/`gather_candidates` (both read-only)
/// run and print unconditionally; the model-driven consolidation pass only
/// runs when `args.apply && !args.dry_run` -- `--dry-run` always overrides
/// `--apply`, the same "safety wins" rule `zirv memory init --dry-run`
/// follows. Resolving an adapter is deferred
/// until a consolidation pass is actually about to run, so a plain report
/// (the common case) never fails just because no agent is configured or
/// available -- mirrors `optimize::run_with`'s own graceful degradation
/// when an adapter cannot be resolved.
pub fn run_optimize_with<W: Write>(
    args: &OptimizeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);

    let candidates = memory_optimize::gather_candidates(repo, &state, &slug, &cfg, now_secs())?;
    let findings = memory_optimize::analyze(&candidates, &cfg);
    let report = memory_optimize::render_report(&findings);
    write!(w, "{report}")?;

    if !args.apply || args.dry_run {
        writeln!(w, "-- report only, nothing written")?;
        return Ok(0);
    }

    let groups = memory_optimize::consolidation_groups(&candidates, &findings);
    if args.no_model || groups.is_empty() {
        writeln!(
            w,
            "-- --apply: no consolidation pass run ({})",
            if args.no_model {
                "--no-model"
            } else {
                "no duplicate/near-duplicate groups found"
            }
        )?;
        return Ok(0);
    }

    let adapter = match adapters::select(args.agent.as_deref().or(cfg.agent.as_deref()), &[], &cfg)
    {
        Ok(adapter) => adapter,
        Err(e) => {
            writeln!(
                w,
                "-- --apply: consolidation skipped, no adapter available ({e})"
            )?;
            return Ok(0);
        }
    };
    let model = handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    let timeout = std::time::Duration::from_secs(cfg.handoff.timeout_secs);
    let applied = memory_optimize::apply_consolidation(
        adapter.as_ref(),
        &model,
        timeout,
        &groups,
        &candidates,
        repo,
        &state,
        &slug,
        &cfg,
    );
    writeln!(
        w,
        "-- applied consolidation to {} survivor entr{}: {}",
        applied.len(),
        if applied.len() == 1 { "y" } else { "ies" },
        applied.join(", ")
    )?;
    Ok(0)
}

pub fn run_optimize<W: Write>(args: &OptimizeArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_optimize_with(args, w, &repo, &env)
}

pub fn run_rollback_with<W: Write>(
    args: &RollbackArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let written_by = env(AGENT_ENV)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    if memory::rollback(repo, &state, &slug, &cfg, &args.id, &written_by)? {
        writeln!(w, "zirv memory rollback: reversed record '{}'", args.id)?;
    } else {
        writeln!(
            w,
            "zirv memory rollback: record '{}' was already rolled back; nothing changed",
            args.id
        )?;
    }
    Ok(0)
}

pub fn run_rollback<W: Write>(args: &RollbackArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_rollback_with(args, w, &repo, &env)
}

pub fn run_promote_with<W: Write>(
    args: &PromoteArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let session_id = env(super::adapters::SESSION_ENV).filter(|v| !v.trim().is_empty());
    let target = if args.global {
        MemoryScope::Global
    } else if args.shared {
        MemoryScope::Shared
    } else {
        return Err("zirv memory promote: pass --shared or --global".into());
    };
    let path = memory::promote(
        repo,
        &state,
        &slug,
        session_id.as_deref(),
        &cfg,
        &args.key,
        target,
    )?;
    writeln!(
        w,
        "zirv memory promote: promoted '{}' to the {} bank at {}",
        args.key,
        scope_label(target),
        path.display()
    )?;
    Ok(0)
}

pub fn run_promote<W: Write>(args: &PromoteArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_promote_with(args, w, &repo, &env)
}

/// `args[0]` is the literal "memory" as it appeared in argv (discarded below,
/// same as `ctx::dispatch`'s own `args[0]`: clap gets a synthetic program
/// name instead, so the case the user actually typed never matters here).
pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv memory".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match MemoryCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return match err.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };

    let mut out = std::io::stdout();
    let result = match &cli.verb {
        MemoryVerb::Init(a) => run_init(a, &mut out),
        MemoryVerb::Status => run_status(&mut out),
        MemoryVerb::List(a) => run_list(a, &mut out),
        MemoryVerb::Recall(a) => run_recall(a, &mut out),
        MemoryVerb::Remember(a) => run_remember(a, &mut out),
        MemoryVerb::Forget(a) => run_forget(a, &mut out),
        MemoryVerb::Verify(a) => run_verify(a, &mut out),
        MemoryVerb::Optimize(a) => run_optimize(a, &mut out),
        MemoryVerb::Rollback(a) => run_rollback(a, &mut out),
        MemoryVerb::Promote(a) => run_promote(a, &mut out),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            crate::output::error(e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state;
    use crate::commands::ctx::testenv::HomeGuard;
    #[cfg(unix)]
    use crate::commands::ctx::testenv::VarGuard;

    fn env_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parses_every_verb() {
        let cli = MemoryCli::try_parse_from(["zirv memory", "init", "--dry-run"]).expect("init");
        assert!(matches!(cli.verb, MemoryVerb::Init(_)));

        let cli = MemoryCli::try_parse_from(["zirv memory", "status"]).expect("status");
        assert!(matches!(cli.verb, MemoryVerb::Status));

        let cli =
            MemoryCli::try_parse_from(["zirv memory", "list", "--shared", "--json"]).expect("list");
        match cli.verb {
            MemoryVerb::List(a) => {
                assert!(a.shared);
                assert!(a.json);
            }
            other => panic!("expected List, got {other:?}"),
        }

        let cli = MemoryCli::try_parse_from(["zirv memory", "recall", "staging"]).expect("recall");
        match cli.verb {
            MemoryVerb::Recall(a) => {
                assert_eq!(a.query, "staging");
                assert!(!a.shared);
            }
            other => panic!("expected Recall, got {other:?}"),
        }

        let cli = MemoryCli::try_parse_from(["zirv memory", "remember", "k", "some text"])
            .expect("remember");
        match cli.verb {
            MemoryVerb::Remember(a) => {
                assert_eq!(a.key, "k");
                assert_eq!(a.text, "some text");
                assert!(!a.shared);
            }
            other => panic!("expected Remember, got {other:?}"),
        }

        let cli =
            MemoryCli::try_parse_from(["zirv memory", "forget", "k", "--shared"]).expect("forget");
        match cli.verb {
            MemoryVerb::Forget(a) => {
                assert_eq!(a.key, "k");
                assert!(a.shared);
            }
            other => panic!("expected Forget, got {other:?}"),
        }

        let cli = MemoryCli::try_parse_from(["zirv memory", "verify", "k"]).expect("verify");
        match cli.verb {
            MemoryVerb::Verify(a) => {
                assert_eq!(a.key, "k");
                assert!(!a.shared);
            }
            other => panic!("expected Verify, got {other:?}"),
        }
        let cli = MemoryCli::try_parse_from([
            "zirv memory",
            "optimize",
            "--apply",
            "--no-model",
            "--agent",
            "claude",
        ])
        .expect("optimize");
        match cli.verb {
            MemoryVerb::Optimize(a) => {
                assert!(a.apply);
                assert!(!a.dry_run);
                assert!(a.no_model);
                assert_eq!(a.agent, Some("claude".to_string()));
            }
            other => panic!("expected Optimize, got {other:?}"),
        }
    }

    #[test]
    fn global_and_shared_flags_conflict_on_every_verb() {
        for argv in [
            vec!["zirv memory", "list", "--shared", "--global"],
            vec!["zirv memory", "recall", "query", "--shared", "--global"],
            vec![
                "zirv memory",
                "remember",
                "key",
                "body",
                "--shared",
                "--global",
            ],
            vec!["zirv memory", "forget", "key", "--shared", "--global"],
            vec!["zirv memory", "verify", "key", "--shared", "--global"],
        ] {
            assert!(
                MemoryCli::try_parse_from(&argv).is_err(),
                "both scope flags must conflict: {argv:?}"
            );
        }
    }

    #[test]
    fn status_reports_the_global_bank_between_private_and_shared() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        memory::remember(
            &state,
            memory::GLOBAL_SLUG,
            &Entry {
                key: "global-fact".to_string(),
                written_by: "test".to_string(),
                written: 1,
                verified: 1,
                source: "explicit".to_string(),
                body: "global body".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &CtxConfig::default(),
        )
        .expect("remember global");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut out = Vec::new();

        run_status_with(&mut out, repo.path(), &|key| env.get(key).cloned()).expect("status");

        let text = String::from_utf8(out).expect("utf8");
        let private = text.find("private memory:").expect("private status");
        let global = text.find("global memory:").expect("global status");
        let shared = text.find("shared memory:").expect("shared status");
        assert!(private < global && global < shared, "{text}");
        assert!(text.contains("global memory: enabled, 1 entries"), "{text}");
        assert!(
            text.contains("private + global + shared, merged private-first"),
            "{text}"
        );
    }

    #[test]
    fn help_exits_zero_on_every_verb_and_bare_memory() {
        for argv in [
            vec!["memory", "--help"],
            vec!["memory", "init", "--help"],
            vec!["memory", "status", "--help"],
            vec!["memory", "list", "--help"],
            vec!["memory", "recall", "--help"],
            vec!["memory", "remember", "--help"],
            vec!["memory", "forget", "--help"],
            vec!["memory", "verify", "-h"],
            vec!["memory", "optimize", "--help"],
        ] {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert_eq!(dispatch(&args), 0, "--help must exit 0: {argv:?}");
        }
    }

    #[test]
    fn an_unknown_verb_exits_two() {
        let args: Vec<String> = ["memory", "nope"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        assert_eq!(dispatch(&args), 2);
    }

    #[test]
    fn a_bare_memory_invocation_exits_two() {
        let args: Vec<String> = ["memory"].iter().map(|a| (*a).to_string()).collect();
        assert_eq!(dispatch(&args), 2);
    }

    #[test]
    fn status_reports_each_scope_as_disabled_without_touching_disk() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY", "false"),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
        ]);

        let mut out = Vec::new();
        let code =
            run_status_with(&mut out, repo.path(), &|k| env.get(k).cloned()).expect("status");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("private memory: disabled"), "got {text}");
        assert!(text.contains("global memory: disabled"), "got {text}");
        assert!(text.contains("shared memory: disabled"), "got {text}");
    }

    /// Fix round (memory review): disabling a scope must never hide what it
    /// holds, the same "must never trap data" rule `forget`/`verify`
    /// already follow -- a byte count is not the content the gate exists
    /// to withhold.
    #[test]
    fn status_reports_a_disabled_scopes_real_counts_and_bytes_not_zero() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let enabled_env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        run_remember_with(
            &RememberArgs {
                key: "private-fact".to_string(),
                text: "private body".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| enabled_env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember private");
        run_remember_with(
            &RememberArgs {
                key: "global-fact".to_string(),
                text: "global body".to_string(),
                shared: false,
                global: true,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| enabled_env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember global");
        run_remember_with(
            &RememberArgs {
                key: "shared-fact".to_string(),
                text: "shared body".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| enabled_env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember shared");

        let disabled_env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY", "false"),
        ]);
        let mut out = Vec::new();
        run_status_with(&mut out, repo.path(), &|k| disabled_env.get(k).cloned()).expect("status");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("private memory: disabled, 1 entries"),
            "a disabled scope must still report what it holds: {text}"
        );
        assert!(
            text.contains("global memory: disabled, 1 entries"),
            "got {text}"
        );
        assert!(
            text.contains("shared memory: disabled, 1 entries"),
            "got {text}"
        );
    }

    /// Fix round (memory review): a scope directory this process cannot
    /// read must never abort the whole status report -- treated as empty,
    /// the same "unreadable means nothing" contract every other read path
    /// in this module follows.
    #[cfg(unix)]
    #[test]
    fn status_treats_an_unreadable_private_bank_as_empty_rather_than_erroring() {
        use std::os::unix::fs::PermissionsExt;

        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);

        let state = StateDir::from_root(state_dir);
        let slug = repo_slug(repo.path());
        let bank_dir = state.memory().join(&slug);
        std::fs::create_dir_all(&bank_dir).expect("mkdir");
        std::fs::set_permissions(&bank_dir, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let mut out = Vec::new();
        let result = run_status_with(&mut out, repo.path(), &|k| env.get(k).cloned());

        // Restore permissions so the tempdir can be cleaned up.
        std::fs::set_permissions(&bank_dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");

        let code = result.expect("an unreadable bank must not abort the status report");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("private memory: enabled, 0 entries"),
            "an unreadable bank must be treated as empty, not abort the report: {text}"
        );
    }

    #[test]
    fn status_counts_entries_and_bytes_per_scope_without_printing_bodies() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);

        let remember_args = RememberArgs {
            key: "private-fact".to_string(),
            text: "this body must never appear verbatim in status".to_string(),
            shared: false,
            global: false,
            importance: None,
            confidence: None,
            tags: Vec::new(),
            allow_sensitive: false,
            if_unchanged: None,
        };
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_remember_with(
            &remember_args,
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember private");

        let shared_args = RememberArgs {
            key: "shared-fact".to_string(),
            text: "shared body text".to_string(),
            shared: true,
            global: false,
            importance: None,
            confidence: None,
            tags: Vec::new(),
            allow_sensitive: false,
            if_unchanged: None,
        };
        run_remember_with(
            &shared_args,
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember shared");

        let mut out = Vec::new();
        run_status_with(&mut out, repo.path(), &|k| env.get(k).cloned()).expect("status");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("private memory: enabled, 1 entries"),
            "got {text}"
        );
        assert!(
            text.contains("shared memory: enabled, 1 entries"),
            "got {text}"
        );
        assert!(
            !text.contains("this body must never appear verbatim"),
            "status must never dump a body: {text}"
        );
    }

    /// Issue #34: `zirv memory status` reports the core budget (which now
    /// covers both scopes, private-first) and the retrieval budget (#35),
    /// not just the old single "injection budget" line.
    #[test]
    fn status_reports_the_core_and_retrieval_budgets() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY_CORE_MAX_BYTES", "1024"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES", "4096"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES", "3"),
        ]);

        let mut out = Vec::new();
        run_status_with(&mut out, repo.path(), &|k| env.get(k).cloned()).expect("status");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("core injection budget: 1024 bytes"),
            "got {text}"
        );
        assert!(
            text.contains("retrieval budget: 4096 bytes, 3 entries max"),
            "got {text}"
        );
        assert!(
            text.contains("bounds `zirv memory recall` today"),
            "the status line must not overstate retrieval as already wired into session-start \
             injection (that lands with issue #44): {text}"
        );
    }

    #[test]
    fn status_warns_about_a_pre_existing_shared_key_collision() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let make = |key: &str, at: &str| {
            format!(
                "## Memory\n- Key: {key}\n- Written-by: human\n- Written: 1\n- Verified: 1\n- Source: explicit\n\nbody\n"
            )
            .replace("body", at)
        };
        std::fs::write(dir.join("shared-fact.md"), make("shared-fact", "one")).expect("write");
        std::fs::write(dir.join("hand-notes.md"), make("shared-fact", "two")).expect("write");

        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut out = Vec::new();
        run_status_with(&mut out, repo.path(), &|k| env.get(k).cloned()).expect("status");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("collision"), "got {text}");
        assert!(text.contains("shared-fact"), "got {text}");
    }

    #[test]
    fn list_defaults_to_private_and_shared_needs_the_flag() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);

        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_remember_with(
            &RememberArgs {
                key: "priv".to_string(),
                text: "private body".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember private");
        run_remember_with(
            &RememberArgs {
                key: "shr".to_string(),
                text: "shared body".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember shared");

        let mut out = Vec::new();
        run_list_with(
            &ListArgs {
                shared: false,
                global: false,
                json: true,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("list private");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"key\":\"priv\""), "got {text}");
        assert!(!text.contains("\"key\":\"shr\""), "got {text}");
        assert!(text.contains("\"scope\":\"private\""), "got {text}");

        let mut out = Vec::new();
        run_list_with(
            &ListArgs {
                shared: true,
                global: false,
                json: true,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("list shared");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"key\":\"shr\""), "got {text}");
        assert!(!text.contains("\"key\":\"priv\""), "got {text}");
        assert!(text.contains("\"scope\":\"shared\""), "got {text}");
    }

    #[test]
    fn list_reports_a_disabled_scope_as_empty_rather_than_erroring() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("shared-fact.md"),
            "## Memory\n- Key: shared-fact\n- Written-by: human\n- Written: 1\n- Verified: 1\n- Source: explicit\n\nbody\n",
        )
        .expect("write");

        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
        ]);
        let mut out = Vec::new();
        let code = run_list_with(
            &ListArgs {
                shared: true,
                global: false,
                json: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("list");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "a disabled scope lists as empty: {out:?}");
    }

    #[test]
    fn shared_list_output_carries_an_untrusted_content_note() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("shared-fact.md"),
            "## Memory\n- Key: shared-fact\n- Written-by: attacker\n- Written: 1\n- Verified: 1\n- Source: explicit\n\nbody\n",
        )
        .expect("write");

        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut out = Vec::new();
        run_list_with(
            &ListArgs {
                shared: true,
                global: false,
                json: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("list");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("not operator-verified"),
            "a shared entry's rendering must not read as operator-attested: {text}"
        );
    }

    /// Issue #35 deliberately changed this test's asserted behavior: recall
    /// now ranks a whole list rather than returning a single dominant
    /// match, so an exact key match ranking FIRST (not ALONE) is the new
    /// contract -- a query that also has real signal elsewhere (here, a
    /// keyword hit in another entry's body) can still surface it, just
    /// ranked below the exact match.
    #[test]
    fn recall_ranks_an_exact_key_match_ahead_of_a_substring_hit_elsewhere() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        run_remember_with(
            &RememberArgs {
                key: "db".to_string(),
                text: "the exact-key entry".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember db");
        run_remember_with(
            &RememberArgs {
                key: "staging-db-creds".to_string(),
                text: "mentions db in its key only".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember staging-db-creds");

        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: "db".to_string(),
                shared: false,
                global: false,
                json: true,
                include_archived: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall");
        let text = String::from_utf8(out).expect("utf8");
        let db_at = text.find("\"key\":\"db\"").expect("exact match present");
        let substring_at = text
            .find("\"key\":\"staging-db-creds\"")
            .expect("the other entry still has real signal (a keyword hit) and is not suppressed");
        assert!(
            db_at < substring_at,
            "the exact key match must rank first: {text}"
        );
    }

    /// Issue #35's acceptance criterion at the CLI seam: an empty query
    /// (no signal to rank by) must return nothing, not the whole bank.
    #[test]
    fn recall_with_an_empty_query_returns_nothing() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        for key in ["build-cmd", "staging-db-creds", "deploy-notes"] {
            run_remember_with(
                &RememberArgs {
                    key: key.to_string(),
                    text: format!("body for {key}"),
                    shared: false,
                    global: false,
                    importance: None,
                    confidence: None,
                    tags: Vec::new(),
                    allow_sensitive: false,
                    if_unchanged: None,
                },
                &mut Vec::new(),
                repo.path(),
                &|k| env.get(k).cloned(),
                &mut stdin,
            )
            .expect("remember");
        }

        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: String::new(),
                shared: false,
                global: false,
                json: true,
                include_archived: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall");
        assert!(
            out.is_empty(),
            "an empty query must never inject the whole bank: {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Issue #35: `zirv memory recall` respects `retrieval_max_entries`,
    /// not just the pure engine's own unit tests.
    #[test]
    fn recall_respects_the_configured_retrieval_entry_cap() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES", "2"),
        ]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        for i in 0..5 {
            run_remember_with(
                &RememberArgs {
                    key: format!("release-note-{i}"),
                    text: "mentions the release process".to_string(),
                    shared: false,
                    global: false,
                    importance: None,
                    confidence: None,
                    tags: Vec::new(),
                    allow_sensitive: false,
                    if_unchanged: None,
                },
                &mut Vec::new(),
                repo.path(),
                &|k| env.get(k).cloned(),
                &mut stdin,
            )
            .expect("remember");
        }

        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: "release".to_string(),
                shared: false,
                global: false,
                json: true,
                include_archived: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(
            text.lines().count(),
            2,
            "the configured entry cap must be respected: {text}"
        );
    }

    #[test]
    fn recall_falls_back_to_a_substring_match_over_key_or_body() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        run_remember_with(
            &RememberArgs {
                key: "staging-db-creds".to_string(),
                text: "the staging DB creds live in 1Password".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember");

        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: "1password".to_string(),
                shared: false,
                global: false,
                json: true,
                include_archived: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall");
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("\"key\":\"staging-db-creds\"")
        );
    }

    #[test]
    fn remember_shared_writes_the_canonical_file_and_refuses_when_disabled() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        let mut out = Vec::new();
        run_remember_with(
            &RememberArgs {
                key: "build-cmd".to_string(),
                text: "cargo build --release".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember shared");
        let path = repo.path().join(".zirv/memory/build-cmd.md");
        assert!(path.is_file(), "expected {}", path.display());
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("cargo build --release")
        );

        let disabled_env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
        ]);
        let err = run_remember_with(
            &RememberArgs {
                key: "other".to_string(),
                text: "text".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("shared_enabled = false must refuse the write");
        assert!(err.to_string().contains("shared_enabled"), "got {err}");
    }

    #[test]
    fn remember_shared_refuses_a_credential_shaped_key_unless_allow_sensitive_is_set() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        let err = run_remember_with(
            &RememberArgs {
                key: "staging-db-creds".to_string(),
                text: "see the ops runbook for details.".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("a credential-shaped key must be refused without --allow-sensitive");
        assert!(err.to_string().contains("credential"), "got {err}");
        assert!(
            !repo
                .path()
                .join(".zirv/memory/staging-db-creds.md")
                .exists(),
            "nothing must be written when the guard refuses"
        );

        let mut out = Vec::new();
        run_remember_with(
            &RememberArgs {
                key: "staging-db-creds".to_string(),
                text: "see the ops runbook for details.".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: true,
                if_unchanged: None,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("--allow-sensitive must permit the write");
        assert!(
            repo.path()
                .join(".zirv/memory/staging-db-creds.md")
                .is_file()
        );
    }

    #[test]
    fn remember_private_still_respects_the_memory_enabled_gate() {
        // Proves the private arm's reuse of `memory::run_remember_with`
        // actually carries the gate check over -- not just that it compiles.
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY", "false"),
        ]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_remember_with(
            &RememberArgs {
                key: "k".to_string(),
                text: "t".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("memory.enabled = false must refuse the private write");
        assert!(err.to_string().contains("disabled"), "got {err}");
    }

    /// The entry format already supports these fields (`memory::Entry`);
    /// `zirv memory remember` is the only surface that can set them, and
    /// the point of doing so is that `zirv memory recall`'s ranking
    /// actually uses them (`retrieval::score_one`).
    #[test]
    fn remember_flags_land_in_the_stored_entry_and_affect_recall_ordering() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        run_remember_with(
            &RememberArgs {
                key: "release-notes-a".to_string(),
                text: "how the release process works".to_string(),
                shared: false,
                global: false,
                importance: Some("high".to_string()),
                confidence: Some("high".to_string()),
                tags: vec!["release".to_string(), "deploy".to_string()],
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember with flags");
        run_remember_with(
            &RememberArgs {
                key: "release-notes-b".to_string(),
                text: "also about the release process".to_string(),
                shared: false,
                global: false,
                importance: Some("low".to_string()),
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember without flags");

        // The flags land in the stored entry.
        let entries = memory::list(
            &StateDir::resolve(&|k| env.get(k).cloned()).expect("state"),
            &repo_slug(repo.path()),
        )
        .expect("list");
        let a = entries
            .iter()
            .find(|(_, e)| e.key == "release-notes-a")
            .expect("entry a")
            .1
            .clone();
        assert_eq!(a.importance, Some("high".to_string()));
        assert_eq!(a.confidence, Some("high".to_string()));
        assert_eq!(a.tags, vec!["release".to_string(), "deploy".to_string()]);

        // And they affect `zirv memory recall`'s ranking: both entries
        // match the query equally on keywords, so the higher-importance,
        // higher-confidence one (with a matching tag too) must rank first.
        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: "release process".to_string(),
                shared: false,
                global: false,
                json: false,
                include_archived: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall");
        let text = String::from_utf8(out).expect("utf8");
        let a_at = text.find("release-notes-a").expect("entry a present");
        let b_at = text.find("release-notes-b").expect("entry b present");
        assert!(
            a_at < b_at,
            "the higher importance/confidence/tag-matched entry must rank first: {text}"
        );
    }

    #[test]
    fn remember_rejects_an_invalid_importance_value() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        let err = run_remember_with(
            &RememberArgs {
                key: "k".to_string(),
                text: "t".to_string(),
                shared: false,
                global: false,
                importance: Some("urgent".to_string()),
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("an unrecognized --importance value must be rejected");
        assert!(err.to_string().contains("--importance"), "got {err}");
        assert!(
            !repo.path().join("state").join("memory").exists()
                || memory::list(
                    &StateDir::resolve(&|k| env.get(k).cloned()).expect("state"),
                    &repo_slug(repo.path())
                )
                .expect("list")
                .is_empty(),
            "a rejected value must write nothing"
        );
    }

    #[test]
    fn forget_and_verify_work_in_all_three_scopes_even_when_disabled() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        run_remember_with(
            &RememberArgs {
                key: "priv".to_string(),
                text: "text".to_string(),
                shared: false,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember private");
        run_remember_with(
            &RememberArgs {
                key: "glob".to_string(),
                text: "text".to_string(),
                shared: false,
                global: true,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember global");
        run_remember_with(
            &RememberArgs {
                key: "shr".to_string(),
                text: "text".to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember shared");

        let disabled_env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY", "false"),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
        ]);

        let mut out = Vec::new();
        let code = run_verify_with(
            &VerifyArgs {
                key: "priv".to_string(),
                shared: false,
                global: false,
            },
            &mut out,
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
        )
        .expect("verify private while disabled");
        assert_eq!(code, 0);

        let mut out = Vec::new();
        let code = run_verify_with(
            &VerifyArgs {
                key: "glob".to_string(),
                shared: false,
                global: true,
            },
            &mut out,
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
        )
        .expect("verify global while disabled");
        assert_eq!(code, 0);

        let mut out = Vec::new();
        let code = run_verify_with(
            &VerifyArgs {
                key: "shr".to_string(),
                shared: true,
                global: false,
            },
            &mut out,
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
        )
        .expect("verify shared while disabled");
        assert_eq!(code, 0);

        let mut out = Vec::new();
        let code = run_forget_with(
            &ForgetArgs {
                key: "priv".to_string(),
                shared: false,
                global: false,
            },
            &mut out,
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
        )
        .expect("forget private while disabled");
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).expect("utf8").contains("removed"));

        let mut out = Vec::new();
        let code = run_forget_with(
            &ForgetArgs {
                key: "glob".to_string(),
                shared: false,
                global: true,
            },
            &mut out,
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
        )
        .expect("forget global while disabled");
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).expect("utf8").contains("removed"));

        let mut out = Vec::new();
        let code = run_forget_with(
            &ForgetArgs {
                key: "shr".to_string(),
                shared: true,
                global: false,
            },
            &mut out,
            repo.path(),
            &|k| disabled_env.get(k).cloned(),
        )
        .expect("forget shared while disabled");
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).expect("utf8").contains("removed"));
    }

    #[test]
    fn verify_reports_an_error_and_nonzero_when_the_key_is_absent() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);

        let err = run_verify_with(
            &VerifyArgs {
                key: "no-such-key".to_string(),
                shared: false,
                global: false,
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect_err("verifying an absent key is an error");
        assert!(err.to_string().contains("no entry"), "got {err}");
    }

    // Issue #38: `zirv memory optimize`.

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Every readable file under `root`, sorted, for a before/after
    /// unchanged-tree assertion -- mirrors `optimize.rs`'s own private
    /// `tree_snapshot` test helper (design decision 2's own precedent).
    fn tree_snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(text) = std::fs::read_to_string(&path) {
                    found.push((path, text));
                }
            }
        }
        found.sort();
        found
    }

    fn remember_shared(
        repo: &std::path::Path,
        env: &std::collections::HashMap<String, String>,
        key: &str,
        text: &str,
    ) {
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_remember_with(
            &RememberArgs {
                key: key.to_string(),
                text: text.to_string(),
                shared: true,
                global: false,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                allow_sensitive: false,
                if_unchanged: None,
            },
            &mut Vec::new(),
            repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember shared");
    }

    #[cfg(unix)]
    fn remember_harvested_shared(
        repo: &std::path::Path,
        env: &std::collections::HashMap<String, String>,
        key: &str,
        text: &str,
    ) {
        let cfg = CtxConfig::load(repo, &|name| env.get(name).cloned()).expect("config");
        let state = StateDir::resolve(&|name| env.get(name).cloned()).expect("state");
        let slug = repo_slug(repo);
        let timestamp = now_secs();
        memory::upsert_scoped(
            MemoryScope::Shared,
            repo,
            &state,
            &slug,
            &cfg,
            &Entry {
                key: key.to_string(),
                written_by: "claude".to_string(),
                written: timestamp,
                verified: timestamp,
                source: "handoff".to_string(),
                body: text.to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
        )
        .expect("remember harvested shared");
    }

    #[test]
    fn optimize_report_only_run_never_modifies_the_shared_bank() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);

        remember_shared(
            repo.path(),
            &env,
            "db-a",
            "the project uses postgres for the database",
        );
        remember_shared(
            repo.path(),
            &env,
            "db-b",
            "the project uses postgres for the database",
        );

        let before = tree_snapshot(repo.path());
        let mut out = Vec::new();
        let code = run_optimize_with(
            &OptimizeArgs {
                apply: false,
                dry_run: false,
                no_model: true,
                agent: None,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("optimize report");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("duplicate"), "got {text}");
        assert_eq!(
            before,
            tree_snapshot(repo.path()),
            "a report-only run must change nothing on disk"
        );
    }

    #[test]
    fn dry_run_overrides_apply_and_still_changes_nothing() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            (
                "ZIRV_CTX_AGENT_BIN",
                fixture("fake-model.sh").to_str().expect("utf8"),
            ),
        ]);
        remember_shared(
            repo.path(),
            &env,
            "db-a",
            "the project uses postgres for the database",
        );
        remember_shared(
            repo.path(),
            &env,
            "db-b",
            "the project uses postgres for the database",
        );

        let before = tree_snapshot(repo.path());
        run_optimize_with(
            &OptimizeArgs {
                apply: true,
                dry_run: true,
                no_model: false,
                agent: Some("claude".to_string()),
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("optimize dry-run");
        assert_eq!(
            before,
            tree_snapshot(repo.path()),
            "--dry-run must override --apply and change nothing"
        );
    }

    #[test]
    fn apply_never_touches_a_group_with_an_explicit_member() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        // A nonexistent binary: if consolidation ever tried to spawn a
        // model for this group, the whole call would fail loudly rather
        // than silently -- this proves the group was skipped before any
        // model was ever touched, not just that its output happened to be
        // rejected.
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_AGENT_BIN", "/nonexistent/model-binary"),
        ]);

        // `remember` (private-scope helper reused by the shared arm) sets
        // `Source: explicit` for every shared write through this CLI.
        remember_shared(
            repo.path(),
            &env,
            "db-a",
            "the project uses postgres for the database",
        );
        remember_shared(
            repo.path(),
            &env,
            "db-b",
            "the project uses postgres for the database",
        );

        let before = tree_snapshot(repo.path());
        let mut out = Vec::new();
        run_optimize_with(
            &OptimizeArgs {
                apply: true,
                dry_run: false,
                no_model: false,
                agent: Some("claude".to_string()),
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("optimize apply");
        assert_eq!(
            before,
            tree_snapshot(repo.path()),
            "a group with an explicit member must never be auto-consolidated"
        );
    }

    /// Model-driven: exercises the real consolidation write path end to end
    /// against `tests/fixtures/fake-model.sh`'s `consolidate` mode. This
    /// cannot execute on Windows (`fake-model.sh` needs a POSIX shell,
    /// `os error 193` here) -- it is the CI-only counterpart to the
    /// model-free tests above, the same split `memory.rs`'s own harvest/init
    /// model tests already accept.
    #[cfg(unix)]
    #[test]
    fn apply_consolidates_a_duplicate_group_as_an_ordinary_working_tree_change() {
        let _model_mode = VarGuard::set(&[("FAKE_MODEL_MODE", Some("consolidate"))]);
        let repo = crate::commands::ctx::testenv::repo();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("init")
            .arg("--quiet")
            .status();
        if !matches!(init, Ok(status) if status.success()) {
            eprintln!(
                "skipping apply_consolidates_a_duplicate_group_as_an_ordinary_working_tree_change: \
                 no usable git binary"
            );
            return;
        }

        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            (
                "ZIRV_CTX_AGENT_BIN",
                fixture("fake-model.sh").to_str().expect("utf8"),
            ),
        ]);
        remember_harvested_shared(
            repo.path(),
            &env,
            "db-a",
            "the project uses postgres for the database",
        );
        remember_harvested_shared(
            repo.path(),
            &env,
            "db-b",
            "the project uses postgres for the database",
        );

        let mut out = Vec::new();
        run_optimize_with(
            &OptimizeArgs {
                apply: true,
                dry_run: false,
                no_model: false,
                agent: Some("claude".to_string()),
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("optimize apply");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("applied consolidation to 1"), "got {text}");

        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["log", "--oneline"])
            .output()
            .expect("git log");
        assert!(
            !log.status.success() || log.stdout.is_empty(),
            "consolidation must never create a commit: {}",
            String::from_utf8_lossy(&log.stdout)
        );
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&status.stdout).contains(".zirv/memory/"),
            "the consolidated survivor must land as an ordinary, unstaged working-tree change"
        );

        // The losing entry is untouched, still present -- consolidation
        // never deletes.
        let mut recall_out = Vec::new();
        run_list_with(
            &ListArgs {
                shared: true,
                global: false,
                json: true,
            },
            &mut recall_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("list shared");
        let recall_text = String::from_utf8(recall_out).expect("utf8");
        assert!(
            recall_text.contains("\"key\":\"db-a\""),
            "got {recall_text}"
        );
        assert!(
            recall_text.contains("\"key\":\"db-b\""),
            "got {recall_text}"
        );
    }

    #[test]
    fn recall_include_archived_surfaces_an_otherwise_excluded_entry() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);

        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Old enough and low-value enough to classify as `Archived`
        // (`retrieval::ARCHIVE_AFTER_DAYS` past `Verified`, `Importance:
        // low`, `Source` not `explicit`).
        let verified = now_secs().saturating_sub((retrieval::ARCHIVE_AFTER_DAYS + 10) * 86_400);
        std::fs::write(
            dir.join("old-note.md"),
            format!(
                "## Memory\n- Key: old-note\n- Written-by: claude\n- Written: {verified}\n- \
                 Verified: {verified}\n- Source: harvest\n- Importance: low\n\nmentions the release \
                 process\n"
            ),
        )
        .expect("write");

        // An exact key match (not just a keyword hit): a candidate this old
        // also takes retrieval's own large gradual staleness penalty (one
        // point per week, ~53 points at this age), so the query needs a
        // strong enough base signal to still clear `MIN_RELEVANCE_SCORE`
        // once explicitly included -- the same way a real, deliberate
        // `--include-archived` recall would need to.
        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: "old-note".to_string(),
                shared: true,
                global: false,
                json: true,
                include_archived: false,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall");
        assert!(
            out.is_empty(),
            "an archived entry must not surface in a normal recall: {}",
            String::from_utf8_lossy(&out)
        );

        let mut out = Vec::new();
        run_recall_with(
            &RecallArgs {
                query: "old-note".to_string(),
                shared: true,
                global: false,
                json: true,
                include_archived: true,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("recall --include-archived");
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("\"key\":\"old-note\""),
            "--include-archived must surface it"
        );
    }
}
