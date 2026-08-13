//! Cross-session memory bank: agent sessions leave durable notes ("we always
//! run migrations before tests", "the staging DB creds live in 1Password")
//! that outlive any single transcript. Mirrors `mail.rs`'s storage idioms
//! (zero-padded seconds prefix, tolerant markdown parsing, atomic
//! `create_new` claims) but one entry per *key* rather than per message: a
//! `remember` on an existing key replaces the old file instead of appending
//! another one, and entries are pruned to a cap on the embedded `Written`
//! timestamp rather than filesystem mtime, since `verify` rewrites a file in
//! place without disturbing when it was first written.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::CtxResult;
use super::adapters::AGENT_ENV;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::state::{StateDir, now_secs, repo_slug};

/// One remembered fact: a short markdown body plus who wrote it, when, and
/// when it was last confirmed still true.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub key: String,
    /// Agent name that wrote this entry, or `"unknown"`.
    pub written_by: String,
    /// Unix seconds the entry was first written.
    pub written: u64,
    /// Unix seconds the entry was last confirmed still true. Equal to
    /// `written` for a freshly remembered entry; `verify` refreshes this
    /// alone, leaving `written` and `body` untouched.
    pub verified: u64,
    /// `"explicit"` (a session or user asked to remember this) or
    /// `"handoff"` (harvested from a distilled handoff). Free-form on parse:
    /// an unrecognized value is kept as-is rather than rejected, the same
    /// tolerance `mail::parse_markdown` gives unknown header values.
    pub source: String,
    pub body: String,
}

impl Entry {
    /// Renders the `## Memory` header block (Key, Written-by, Written,
    /// Verified, Source as list items) followed by the free markdown body.
    pub fn to_markdown(&self) -> String {
        format!(
            "## Memory\n- Key: {}\n- Written-by: {}\n- Written: {}\n- Verified: {}\n- Source: {}\n\n{}\n",
            self.key, self.written_by, self.written, self.verified, self.source, self.body
        )
    }
}

/// Same bullet styles `mail::strip_bullet` accepts. Duplicated locally
/// (rather than made `pub(crate)` elsewhere) to keep this file's edits
/// isolated from files other tasks are actively working in.
fn strip_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        return Some(trimmed[digits.len() + 2..].trim().to_string());
    }
    None
}

/// Parses a `## Memory` header block and body with the same tolerance as
/// `mail::parse_markdown`: unknown headers and unknown sections are skipped
/// rather than treated as an error.
pub fn parse_markdown(md: &str) -> Entry {
    let mut entry = Entry {
        key: String::new(),
        written_by: "unknown".to_string(),
        written: 0,
        verified: 0,
        source: "explicit".to_string(),
        body: String::new(),
    };
    let mut in_entry = false;
    let mut in_header = false;
    let mut body_lines: Vec<&str> = Vec::new();

    for line in md.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            in_entry = rest.trim().eq_ignore_ascii_case("Memory");
            in_header = in_entry;
            continue;
        }
        if !in_entry {
            continue;
        }
        if in_header {
            if trimmed.is_empty() {
                continue;
            }
            if let Some(bullet) = strip_bullet(line)
                && let Some((key, value)) = bullet.split_once(':')
            {
                match key.trim().to_ascii_lowercase().as_str() {
                    "key" => entry.key = value.trim().to_string(),
                    "written-by" => entry.written_by = value.trim().to_string(),
                    "written" => entry.written = value.trim().parse().unwrap_or(0),
                    "verified" => entry.verified = value.trim().parse().unwrap_or(0),
                    "source" => entry.source = value.trim().to_string(),
                    // Unknown header inside the block: skipped, not an error.
                    _ => {}
                }
                continue;
            }
            // First non-bullet, non-blank line ends the header block.
            in_header = false;
        }
        body_lines.push(line);
    }

    entry.body = body_lines.join("\n").trim().to_string();
    entry
}

/// Same atomic-claim idiom as `mail::claim_and_write`: opens with
/// `create_new` so the open itself is the collision check, retrying the next
/// `_NNN` suffix on `AlreadyExists` rather than racing a separate
/// `.exists()` probe against a concurrent writer.
fn claim_and_write(dir: &Path, base: &str, contents: &str) -> std::io::Result<PathBuf> {
    let mut n = 0u32;
    loop {
        let candidate = if n == 0 {
            dir.join(format!("{base}.md"))
        } else {
            dir.join(format!("{base}_{n:03}.md"))
        };

        let mut opts = std::fs::OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        match opts.open(&candidate) {
            Ok(mut file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                file.write_all(contents.as_bytes())?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Replaces every character outside `[A-Za-z0-9-]` with `-` and lowercases,
/// the same rule `state::repo_slug` uses, capped short so filenames stay
/// reasonable.
fn slug_key(key: &str) -> String {
    let raw: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let trimmed = raw.trim_matches('-');
    if trimmed.is_empty() {
        "entry".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Lists every entry stored for `slug`, oldest-written-first by filename
/// order (the zero-padded seconds prefix each file name carries). Files that
/// cannot be read are skipped rather than failing the whole listing. Reads
/// only `state.memory().join(slug)` -- no repository path is ever consulted,
/// so nothing checked into a repo can seed, alter, or hide what this
/// returns.
pub fn list(state: &StateDir, slug: &str) -> CtxResult<Vec<(PathBuf, Entry)>> {
    let dir = state.memory().join(slug);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        out.push((path, parse_markdown(&text)));
    }
    Ok(out)
}

/// Removes every file whose parsed `Written` timestamp is not among the
/// `keep` newest, oldest first. Unlike `state::prune_to_newest` (filesystem
/// mtime), this keys off the embedded `Written` field: `verify` rewrites a
/// file's bytes without changing when it was first written, and pruning
/// must not treat a freshly-verified old entry as if it were new.
fn prune_to_cap(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<(u64, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            Some((parse_markdown(&text).written, path))
        })
        .collect();
    if items.len() <= keep {
        return;
    }
    items.sort_by_key(|(written, _)| *written);
    let excess = items.len() - keep;
    for (_, path) in items.iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Writes `entry` under `<state>/memory/<repo_slug>/`, replacing any
/// existing entry sharing its key, truncating an oversized body (never
/// failing the store), and pruning the bank down to `cfg.memory.max_entries`
/// oldest-`Written`-first.
pub fn remember(
    state: &StateDir,
    slug: &str,
    entry: &Entry,
    cfg: &CtxConfig,
) -> CtxResult<PathBuf> {
    let dir = state.memory().join(slug);
    super::state::create_private_dir_all(&dir)?;

    for (path, existing) in list(state, slug)? {
        if existing.key == entry.key {
            let _ = std::fs::remove_file(&path);
        }
    }

    let mut entry = entry.clone();
    let cap = cfg.memory.max_entry_bytes;
    if entry.body.len() > cap {
        const MARKER: &str = "\n[truncated]";
        let keep = cap.saturating_sub(MARKER.len());
        let mut truncated = crate::utils::truncate_bytes(entry.body.clone(), Some(keep));
        truncated.push_str(MARKER);
        entry.body = truncated;
    }

    let base = format!("{:010}-{}", entry.written, slug_key(&entry.key));
    let path = claim_and_write(&dir, &base, &entry.to_markdown())?;

    prune_to_cap(&dir, cfg.memory.max_entries);
    Ok(path)
}

/// The single entry for `key`, if any.
// Consumed by the memory prompt layer and harvest (next waves); recall's own
// key filter goes through `list` so its output ordering matches the bank.
#[allow(dead_code)]
pub fn get(state: &StateDir, slug: &str, key: &str) -> CtxResult<Option<Entry>> {
    Ok(list(state, slug)?
        .into_iter()
        .find(|(_, entry)| entry.key == key)
        .map(|(_, entry)| entry))
}

/// Removes the entry for `key`, if one exists. Returns whether anything was
/// removed.
pub fn forget(state: &StateDir, slug: &str, key: &str) -> CtxResult<bool> {
    let mut removed = false;
    for (path, entry) in list(state, slug)? {
        if entry.key == key {
            std::fs::remove_file(&path)?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Empties the whole bank for `slug`.
pub fn forget_all(state: &StateDir, slug: &str) -> CtxResult<()> {
    let dir = state.memory().join(slug);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Refreshes only the `Verified` stamp on the entry for `key`, leaving
/// `Written`, `written_by`, `source` and `body` untouched. Returns whether an
/// entry was found.
pub fn verify(state: &StateDir, slug: &str, key: &str) -> CtxResult<bool> {
    for (path, mut entry) in list(state, slug)? {
        if entry.key == key {
            entry.verified = now_secs();
            super::state::write_private(&path, &entry.to_markdown())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Loads this repo's memory bank as already-rendered prompt lines
/// (`prompt::MemoryLine`), gated on `cfg.memory.enabled` -- an empty vec
/// when disabled, the same "disabled means nothing delivered" contract
/// every mail delivery seam already follows for `cfg.mail.enabled`. `now` is
/// a plain `u64` (`state::now_secs()`, read by the caller) rather than read
/// in here: `prompt.rs` stays clock-free like `rot.rs`, so the one clock
/// read for "how old is this entry" happens at this call, the last point
/// before the rendered text crosses into that module.
///
/// The wording ("written Nd ago, verified Nd ago") matches `run_recall_
/// with`'s own human-readable branch, so "how old" reads the same everywhere
/// it appears.
pub fn render_for_prompt(
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    now: u64,
) -> Vec<super::prompt::MemoryLine> {
    if !cfg.memory.enabled {
        return Vec::new();
    }
    list(state, slug)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, entry)| {
            let written_days = now.saturating_sub(entry.written) / 86_400;
            let verified_days = now.saturating_sub(entry.verified) / 86_400;
            super::prompt::MemoryLine {
                key: entry.key,
                age: format!("written {written_days}d ago, verified {verified_days}d ago"),
                body: entry.body,
            }
        })
        .collect()
}

#[derive(Debug, clap::Args)]
pub struct RememberArgs {
    /// The fact's key, e.g. "staging-db-creds".
    #[arg(long)]
    pub key: String,
    /// Entry text. When omitted (and `--verify` is not given alone), read
    /// from `--text-file`, else from stdin.
    #[arg(long)]
    pub text: Option<String>,
    /// Path to a file holding the entry text.
    #[arg(long)]
    pub text_file: Option<PathBuf>,
    /// With no text given, just refresh the existing entry's `Verified`
    /// stamp rather than requiring new text.
    #[arg(long, default_value_t = false)]
    pub verify: bool,
}

#[derive(Debug, clap::Args)]
pub struct RecallArgs {
    /// Only show the entry for this key.
    #[arg(long)]
    pub key: Option<String>,
    /// Only show entries whose `Verified` stamp is older than this many
    /// days.
    #[arg(long)]
    pub stale: Option<u64>,
    /// Emit one JSON object per line instead of human-readable text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ForgetArgs {
    /// Key to forget. Omit when passing `--all`.
    pub key: Option<String>,
    /// Remove every entry in this repository's memory bank.
    #[arg(long, default_value_t = false)]
    pub all: bool,
}

/// `env(key)`, treating a missing or blank value as `"unknown"` -- the same
/// convention `mail::identity_or_unknown` uses.
fn identity_or_unknown(env: EnvLookup<'_>, key: &str) -> String {
    env(key)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

enum RememberIntent {
    Store(String),
    VerifyOnly,
}

/// `--text`, else `--text-file`, else (if `--verify` is set and no text
/// source was given) a verify-only intent, else stdin -- trimmed either way.
fn resolve_remember(args: &RememberArgs, stdin: &mut dyn Read) -> CtxResult<RememberIntent> {
    if let Some(text) = &args.text {
        return Ok(RememberIntent::Store(text.trim().to_string()));
    }
    if let Some(path) = &args.text_file {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(RememberIntent::Store(text.trim().to_string()));
    }
    if args.verify {
        return Ok(RememberIntent::VerifyOnly);
    }
    let mut buffer = String::new();
    stdin.read_to_string(&mut buffer)?;
    Ok(RememberIntent::Store(buffer.trim().to_string()))
}

pub fn run_remember_with<W: Write>(
    args: &RememberArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    stdin: &mut dyn Read,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    if !cfg.memory.enabled {
        return Err(
            "zirv ctx remember: memory is disabled (memory.enabled = false); nothing was remembered"
                .into(),
        );
    }

    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);

    match resolve_remember(args, stdin)? {
        RememberIntent::VerifyOnly => {
            if verify(&state, &slug, &args.key)? {
                writeln!(w, "zirv ctx remember: verified '{}'", args.key)?;
                Ok(0)
            } else {
                Err(format!("zirv ctx remember: no entry for key '{}'", args.key).into())
            }
        }
        RememberIntent::Store(body) => {
            if body.is_empty() {
                return Err(
                    "zirv ctx remember: no text given; pass --text, --text-file, or pipe one on stdin"
                        .into(),
                );
            }
            let now = now_secs();
            let entry = Entry {
                key: args.key.clone(),
                written_by: identity_or_unknown(env, AGENT_ENV),
                written: now,
                verified: now,
                source: "explicit".to_string(),
                body,
            };
            let path = remember(&state, &slug, &entry, &cfg)?;
            writeln!(
                w,
                "zirv ctx remember: stored '{}' at {}",
                args.key,
                path.display()
            )?;
            Ok(0)
        }
    }
}

pub fn run_remember<W: Write>(args: &RememberArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_remember_with(args, w, &repo, &env, &mut std::io::stdin())
}

pub fn run_recall_with<W: Write>(
    args: &RecallArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    if !cfg.memory.enabled {
        // Disabled means the bank reports empty, exactly like an empty one:
        // nothing is printed, exit 0.
        return Ok(0);
    }

    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let mut entries: Vec<Entry> = list(&state, &slug)?.into_iter().map(|(_, e)| e).collect();

    if let Some(key) = &args.key {
        entries.retain(|entry| &entry.key == key);
    }
    if let Some(days) = args.stale {
        let threshold = now_secs().saturating_sub(days * 86_400);
        entries.retain(|entry| entry.verified < threshold);
    }

    for entry in &entries {
        if args.json {
            writeln!(w, "{}", serde_json::to_string(entry)?)?;
        } else {
            let now = now_secs();
            let written_days = now.saturating_sub(entry.written) / 86_400;
            let verified_days = now.saturating_sub(entry.verified) / 86_400;
            writeln!(
                w,
                "{} (written {}d ago, verified {}d ago)\n{}\n",
                entry.key, written_days, verified_days, entry.body
            )?;
        }
    }
    Ok(0)
}

pub fn run_recall<W: Write>(args: &RecallArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_recall_with(args, w, &repo, &env)
}

pub fn run_forget_with<W: Write>(
    args: &ForgetArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // Deliberately does not check `cfg.memory.enabled`: forgetting must
    // still work while the bank is disabled, the same way disabling a
    // feature must never trap data behind it.
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);

    if args.all {
        forget_all(&state, &slug)?;
        writeln!(w, "zirv ctx forget: cleared the memory bank")?;
        return Ok(0);
    }
    let Some(key) = &args.key else {
        return Err("zirv ctx forget: pass a key, or --all".into());
    };
    if forget(&state, &slug, key)? {
        writeln!(w, "zirv ctx forget: removed '{key}'")?;
    } else {
        writeln!(w, "zirv ctx forget: no entry for '{key}'")?;
    }
    Ok(0)
}

pub fn run_forget<W: Write>(args: &ForgetArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_forget_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::super::state;
    use super::*;

    fn sample(key: &str, written: u64) -> Entry {
        Entry {
            key: key.to_string(),
            written_by: "claude".to_string(),
            written,
            verified: written,
            source: "explicit".to_string(),
            body: "the staging DB creds live in 1Password.".to_string(),
        }
    }

    fn env_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn an_entry_round_trips_its_key_author_timestamps_and_body() {
        let entry = Entry {
            key: "staging-db-creds".to_string(),
            written_by: "claude".to_string(),
            written: 1_700_000_000,
            verified: 1_700_000_500,
            source: "handoff".to_string(),
            body: "The staging DB creds live in 1Password under 'staging-db'.".to_string(),
        };
        let parsed = parse_markdown(&entry.to_markdown());
        assert_eq!(parsed, entry);
    }

    #[test]
    fn an_unknown_header_or_section_is_skipped_rather_than_failing_the_read() {
        let md = "## Memory\n\
- Key: build-cmd\n\
- Written-by: claude\n\
- Priority: urgent\n\
- Written: 1700000000\n\
- Verified: 1700000000\n\
- Source: explicit\n\
\n\
Run `cargo build` before tests.\n\
\n\
## Footer\n\
This should not appear in the body.\n";

        let entry = parse_markdown(md);
        assert_eq!(entry.key, "build-cmd");
        assert_eq!(entry.written_by, "claude");
        assert_eq!(entry.written, 1_700_000_000);
        assert_eq!(entry.body, "Run `cargo build` before tests.");
    }

    #[test]
    fn remembering_an_existing_key_replaces_the_entry_rather_than_duplicating_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let mut first = sample("build-cmd", 1_700_000_000);
        first.body = "cargo build".to_string();
        remember(&state, "-work-repo", &first, &cfg).expect("remember first");

        let mut second = sample("build-cmd", 1_700_000_100);
        second.body = "cargo build --release".to_string();
        remember(&state, "-work-repo", &second, &cfg).expect("remember second");

        let listed = list(&state, "-work-repo").expect("list");
        assert_eq!(
            listed.len(),
            1,
            "the old entry must be replaced, not duplicated"
        );
        assert_eq!(listed[0].1.body, "cargo build --release");
    }

    #[test]
    fn an_oversized_entry_body_is_truncated_and_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.max_entry_bytes = 50;

        let mut entry = sample("huge", 1);
        entry.body = "x".repeat(500);

        let path = remember(&state, "-work-repo", &entry, &cfg)
            .expect("remember must not fail on oversize");
        let stored = parse_markdown(&std::fs::read_to_string(&path).expect("read"));
        assert!(
            stored.body.len() <= 50,
            "body respects the cap: {} bytes",
            stored.body.len()
        );
        assert!(
            stored.body.ends_with("[truncated]"),
            "says it was truncated: {}",
            stored.body
        );
    }

    #[test]
    fn the_bank_is_pruned_to_the_entry_cap_oldest_written_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.max_entries = 3;

        for i in 0..5u64 {
            let entry = sample(&format!("key-{i}"), 1_700_000_000 + i);
            remember(&state, "-work-repo", &entry, &cfg).expect("remember");
        }

        let remaining = list(&state, "-work-repo").expect("list");
        assert_eq!(remaining.len(), 3, "pruned down to the cap");
        let keys: Vec<&str> = remaining.iter().map(|(_, e)| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["key-2", "key-3", "key-4"],
            "the two oldest-Written entries are dropped, newest three remain"
        );
    }

    #[test]
    fn entries_never_leak_across_repositories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        remember(&state, "-work-a", &sample("only-in-a", 1_700_000_000), &cfg).expect("remember a");
        remember(&state, "-work-b", &sample("only-in-b", 1_700_000_000), &cfg).expect("remember b");

        let listed_a = list(&state, "-work-a").expect("list a");
        assert_eq!(listed_a.len(), 1);
        assert_eq!(listed_a[0].1.key, "only-in-a");

        let listed_b = list(&state, "-work-b").expect("list b");
        assert_eq!(listed_b.len(), 1);
        assert_eq!(listed_b[0].1.key, "only-in-b");
    }

    #[test]
    fn nothing_in_the_repository_checkout_can_seed_the_bank() {
        let repo = tempfile::tempdir().expect("tempdir");
        let slug = repo_slug(repo.path());
        let decoy_dir = repo.path().join(".zirv").join("memory").join(&slug);
        std::fs::create_dir_all(&decoy_dir).expect("mkdir decoy");
        std::fs::write(
            decoy_dir.join("0000000000-decoy.md"),
            sample("decoy", 1).to_markdown(),
        )
        .expect("write decoy");

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));

        let listed = list(&state, &slug).expect("list");
        assert!(
            listed.is_empty(),
            "a repo-side memory tree must never be consulted: {listed:?}"
        );
    }

    #[test]
    fn forget_removes_one_key_and_forget_all_empties_the_bank() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        remember(&state, "-work-repo", &sample("keep-me", 1), &cfg).expect("remember 1");
        remember(&state, "-work-repo", &sample("drop-me", 2), &cfg).expect("remember 2");

        let removed = forget(&state, "-work-repo", "drop-me").expect("forget");
        assert!(removed);
        let remaining = list(&state, "-work-repo").expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1.key, "keep-me");

        let missing = forget(&state, "-work-repo", "not-there").expect("forget missing");
        assert!(
            !missing,
            "forgetting an absent key reports false, not an error"
        );

        forget_all(&state, "-work-repo").expect("forget all");
        assert!(list(&state, "-work-repo").expect("list").is_empty());
    }

    #[test]
    fn verifying_a_key_refreshes_the_verified_stamp_without_rewriting_the_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let mut entry = sample("build-cmd", 1_700_000_000);
        entry.verified = 1_700_000_000;
        entry.body = "cargo build --release".to_string();
        remember(&state, "-work-repo", &entry, &cfg).expect("remember");

        let verified = verify(&state, "-work-repo", "build-cmd").expect("verify");
        assert!(verified);

        let stored = get(&state, "-work-repo", "build-cmd")
            .expect("get")
            .expect("entry present");
        assert_eq!(stored.written, 1_700_000_000, "written stamp untouched");
        assert_eq!(stored.body, "cargo build --release", "body untouched");
        assert!(
            stored.verified >= now_secs().saturating_sub(5),
            "verified was refreshed to roughly now: {}",
            stored.verified
        );

        assert!(
            !verify(&state, "-work-repo", "no-such-key").expect("verify missing"),
            "verifying an absent key reports false"
        );
    }

    #[test]
    fn recall_can_list_only_entries_older_than_a_staleness_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let repo = tempfile::tempdir().expect("tempdir");
        let slug = repo_slug(repo.path());

        let now = now_secs();
        let mut fresh = sample("fresh", now);
        fresh.verified = now;
        let mut stale = sample("stale", now.saturating_sub(20 * 86_400));
        stale.verified = now.saturating_sub(20 * 86_400);
        remember(&state, &slug, &fresh, &cfg).expect("remember fresh");
        remember(&state, &slug, &stale, &cfg).expect("remember stale");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RecallArgs {
            key: None,
            stale: Some(5),
            json: true,
        };
        let mut out = Vec::new();
        run_recall_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned()).expect("recall");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"key\":\"stale\""), "got {text}");
        assert!(!text.contains("\"key\":\"fresh\""), "got {text}");
    }

    #[test]
    fn recall_prints_nothing_and_exits_zero_when_the_bank_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RecallArgs {
            key: None,
            stale: None,
            json: false,
        };
        let mut out = Vec::new();
        let code =
            run_recall_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("recall");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "nothing to print: {out:?}");
    }

    #[test]
    fn remember_records_the_writing_agent_from_the_environment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            (AGENT_ENV, "claude"),
        ]);
        let args = RememberArgs {
            key: "build-cmd".to_string(),
            text: Some("cargo build --release".to_string()),
            text_file: None,
            verify: false,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_remember_with(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember");
        assert_eq!(code, 0);

        let state = StateDir::from_root(state_dir);
        let slug = repo_slug(tmp.path());
        let stored = get(&state, &slug, "build-cmd")
            .expect("get")
            .expect("entry present");
        assert_eq!(stored.written_by, "claude");
        assert_eq!(stored.body, "cargo build --release");
    }

    #[test]
    fn remember_with_verify_and_no_text_only_refreshes_the_stamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let slug = repo_slug(tmp.path());
        let mut entry = sample("build-cmd", 1_700_000_000);
        entry.verified = 1_700_000_000;
        remember(&state, &slug, &entry, &cfg).expect("remember");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RememberArgs {
            key: "build-cmd".to_string(),
            text: None,
            text_file: None,
            verify: true,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_remember_with(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("verify-only remember");

        let stored = get(&state, &slug, "build-cmd")
            .expect("get")
            .expect("entry present");
        assert_eq!(stored.written, 1_700_000_000, "written untouched");
        assert!(stored.verified > 1_700_000_000, "verified was refreshed");
    }

    #[test]
    fn memory_disabled_in_config_refuses_remember_and_reports_an_empty_recall_but_forget_still_works()
     {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let slug = repo_slug(tmp.path());
        remember(
            &state,
            &slug,
            &sample("build-cmd", 1_700_000_000),
            &CtxConfig::default(),
        )
        .expect("remember");

        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY", "false"),
        ]);

        let remember_args = RememberArgs {
            key: "new-key".to_string(),
            text: Some("should not be stored".to_string()),
            text_file: None,
            verify: false,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_remember_with(
            &remember_args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("memory is disabled");
        assert!(err.to_string().contains("disabled"), "got {err}");

        let recall_args = RecallArgs {
            key: None,
            stale: None,
            json: false,
        };
        let mut recall_out = Vec::new();
        let code = run_recall_with(&recall_args, &mut recall_out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("recall still succeeds, just empty");
        assert_eq!(code, 0);
        assert!(
            recall_out.is_empty(),
            "a disabled bank reports empty even with an entry sitting in storage: {recall_out:?}"
        );

        let forget_args = ForgetArgs {
            key: Some("build-cmd".to_string()),
            all: false,
        };
        let mut forget_out = Vec::new();
        run_forget_with(&forget_args, &mut forget_out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("forget still works while memory is disabled");
        assert!(
            list(&state, &slug).expect("list").is_empty(),
            "forget removed the entry even while disabled"
        );
    }

    #[test]
    fn forget_requires_a_key_or_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = ForgetArgs {
            key: None,
            all: false,
        };
        let mut out = Vec::new();
        let err = run_forget_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("neither a key nor --all was given");
        assert!(err.to_string().contains("--all"), "got {err}");
    }

    // N5: the memory prompt layer's own source, `render_for_prompt`.

    #[test]
    fn render_for_prompt_renders_the_key_and_age_of_every_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let now = 1_700_000_000u64;

        let mut fresh = sample("build-cmd", now.saturating_sub(3 * 86_400));
        fresh.verified = now.saturating_sub(86_400);
        fresh.body = "cargo build --release".to_string();
        remember(&state, "-work-repo", &fresh, &cfg).expect("remember");

        let rendered = render_for_prompt(&state, "-work-repo", &cfg, now);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].key, "build-cmd");
        assert_eq!(rendered[0].body, "cargo build --release");
        assert_eq!(rendered[0].age, "written 3d ago, verified 1d ago");
    }

    #[test]
    fn render_for_prompt_is_empty_when_memory_is_disabled_even_with_entries_stored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        remember(
            &state,
            "-work-repo",
            &sample("build-cmd", 1_700_000_000),
            &CtxConfig::default(),
        )
        .expect("remember");

        let disabled = CtxConfig {
            memory: super::super::config::MemoryConfig {
                enabled: false,
                ..super::super::config::MemoryConfig::default()
            },
            ..CtxConfig::default()
        };
        assert!(
            render_for_prompt(&state, "-work-repo", &disabled, 1_700_000_000).is_empty(),
            "a disabled bank must render nothing, however much is stored"
        );
    }
}
