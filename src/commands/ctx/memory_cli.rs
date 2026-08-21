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
use super::adapters::AGENT_ENV;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::memory::{self, Entry, MemoryScope};
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
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// List the shared, repository-owned bank (`<repo>/.zirv/memory/`,
    /// meant to be committed) instead of the private, machine-local one.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
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
    /// Emit one JSON object per line instead of human-readable text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
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
}

#[derive(Debug, clap::Args)]
pub struct VerifyArgs {
    /// Key to verify.
    pub key: String,
    /// Verify in the shared bank instead of the private one.
    #[arg(long, default_value_t = false)]
    pub shared: bool,
}

fn scope_of(shared: bool) -> MemoryScope {
    if shared {
        MemoryScope::Shared
    } else {
        MemoryScope::Private
    }
}

fn scope_label(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Private => "private",
        MemoryScope::Shared => "shared",
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
    write_scope_status(w, MemoryScope::Shared, repo, &state, &slug, &cfg)?;
    writeln!(
        w,
        "core injection budget: {} bytes (private + shared, merged private-first; issue #34)",
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

/// An `Entry` plus the scope it was read from, for JSON output. `scope` is
/// derived from which directory the CLI actually read, never from the
/// entry's own header fields -- a shared entry's `Source`/`Written-By` are
/// attacker-supplied repository content (see `MemoryScope::Shared`'s own doc
/// comment) and must never be the thing that tells a reader which bank an
/// entry came from.
#[derive(Serialize)]
struct ScopedEntry<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    scope: &'static str,
}

/// Renders entries already selected from one scope. Never trusts an entry's
/// own header for its scope label (see `ScopedEntry`); a shared entry's
/// human-readable line additionally carries an explicit untrusted-content
/// note so a repo-committed `Source: explicit` can never read as if it were
/// operator-verified.
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
            let scoped = ScopedEntry {
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
            MemoryScope::Private => "",
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
/// follows `ScopedEntry`'s own rule: derived from which directory was
/// read, never from the entry's own header.
#[derive(Serialize)]
struct RankedEntry<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    scope: &'static str,
    score: i64,
    reasons: &'a [String],
}

/// Renders a ranking's selected entries. Reuses `ScopedEntry`'s scope-
/// labeling and the shared-scope trust note from `render_entries`, adding
/// `score`/`reasons` -- the selection diagnostics issue #35 asks for -- to
/// both the JSON and human-readable forms.
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
            MemoryScope::Private => "",
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

pub fn run_list_with<W: Write>(
    args: &ListArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let scope = scope_of(args.shared);
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
    let scope = scope_of(args.shared);

    let candidates = retrieval::candidates_for_scope(scope, repo, &state, &slug, &cfg, now_secs())?;
    let ctx = RetrievalContext {
        query: args.query.clone(),
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

    if !args.shared {
        // The private arm is a thin wrapper over the exact code path `zirv
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
        paths: Vec::new(),
    };
    let path = memory::upsert_scoped(MemoryScope::Shared, repo, &state, &slug, &cfg, &entry)?;
    writeln!(
        w,
        "zirv memory remember: stored '{}' in the shared bank at {}",
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
    let scope = scope_of(args.shared);
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
    let scope = scope_of(args.shared);
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
        MemoryVerb::Status => run_status(&mut out),
        MemoryVerb::List(a) => run_list(a, &mut out),
        MemoryVerb::Recall(a) => run_recall(a, &mut out),
        MemoryVerb::Remember(a) => run_remember(a, &mut out),
        MemoryVerb::Forget(a) => run_forget(a, &mut out),
        MemoryVerb::Verify(a) => run_verify(a, &mut out),
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

    fn env_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parses_every_verb() {
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
    }

    #[test]
    fn help_exits_zero_on_every_verb_and_bare_memory() {
        for argv in [
            vec!["memory", "--help"],
            vec!["memory", "status", "--help"],
            vec!["memory", "list", "--help"],
            vec!["memory", "recall", "--help"],
            vec!["memory", "remember", "--help"],
            vec!["memory", "forget", "--help"],
            vec!["memory", "verify", "-h"],
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
            },
            &mut Vec::new(),
            repo.path(),
            &|k| enabled_env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember private");
        run_remember_with(
            &RememberArgs {
                key: "shared-fact".to_string(),
                text: "shared body".to_string(),
                shared: true,
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
            importance: None,
            confidence: None,
            tags: Vec::new(),
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
            importance: None,
            confidence: None,
            tags: Vec::new(),
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                json: true,
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
                    importance: None,
                    confidence: None,
                    tags: Vec::new(),
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
                json: true,
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
                    importance: None,
                    confidence: None,
                    tags: Vec::new(),
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
                json: true,
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                json: true,
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                importance: Some("high".to_string()),
                confidence: Some("high".to_string()),
                tags: vec!["release".to_string(), "deploy".to_string()],
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
                importance: Some("low".to_string()),
                confidence: None,
                tags: Vec::new(),
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
                json: false,
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
                importance: Some("urgent".to_string()),
                confidence: None,
                tags: Vec::new(),
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
    fn forget_and_verify_work_in_both_scopes_even_when_disabled() {
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
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                text: "text".to_string(),
                shared: true,
                importance: None,
                confidence: None,
                tags: Vec::new(),
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
                key: "shr".to_string(),
                shared: true,
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
                key: "shr".to_string(),
                shared: true,
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
            },
            &mut Vec::new(),
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect_err("verifying an absent key is an error");
        assert!(err.to_string().contains("no entry"), "got {err}");
    }
}
