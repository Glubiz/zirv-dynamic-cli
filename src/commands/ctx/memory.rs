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
    /// Coarse, free-form priority signal for later retrieval ranking (issue
    /// #35) and lifecycle/staleness decisions (issue #38) -- by convention
    /// "high"/"normal"/"low", but not enforced: like `source`, a hand-edited
    /// value that doesn't match the convention is still kept as-is rather
    /// than rejected. `None` when unset. `skip_serializing_if` keeps
    /// `zirv ctx recall --json` emitting the pre-issue-#32 shape for any
    /// entry that doesn't use this field -- every entry before this change,
    /// and every entry since that never sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
    /// Coarse, free-form confidence signal, same shape and parsing rules as
    /// `importance`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Free-form labels for keyword-based retrieval (issue #35). Rendered as
    /// one comma-separated header line; empty when unset.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Repository paths this entry is about, for path-aware retrieval (issue
    /// #35) and dead-reference checks (issue #38). Free text, not validated
    /// against the filesystem here: a path that no longer exists is exactly
    /// the kind of fact a later task wants to *detect*, not something this
    /// store should silently reject. Rendered as one comma-separated header
    /// line; empty when unset.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl Entry {
    /// Renders the `## Memory` header block (Key, Written-by, Written,
    /// Verified, Source as list items, plus Importance/Confidence/Tags/Paths
    /// when set) followed by the free markdown body. The optional fields are
    /// omitted entirely when unset rather than rendered empty, so an entry
    /// that doesn't use them reads exactly as it did before they existed --
    /// this is the "versionable" part of the schema (issue #32): a header
    /// line absent from an old entry, or one a future task adds that this
    /// parser doesn't know about, is simply not there or is skipped, never a
    /// parse failure (see `parse_markdown`'s unknown-header tolerance).
    pub fn to_markdown(&self) -> String {
        let mut header = format!(
            "## Memory\n- Key: {}\n- Written-by: {}\n- Written: {}\n- Verified: {}\n- Source: {}\n",
            self.key, self.written_by, self.written, self.verified, self.source
        );
        if let Some(importance) = &self.importance {
            header.push_str(&format!("- Importance: {importance}\n"));
        }
        if let Some(confidence) = &self.confidence {
            header.push_str(&format!("- Confidence: {confidence}\n"));
        }
        if !self.tags.is_empty() {
            header.push_str(&format!("- Tags: {}\n", self.tags.join(", ")));
        }
        if !self.paths.is_empty() {
            header.push_str(&format!("- Paths: {}\n", self.paths.join(", ")));
        }
        format!("{header}\n{}\n", self.body)
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
        importance: None,
        confidence: None,
        tags: Vec::new(),
        paths: Vec::new(),
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
            // N2: the header block ends at the FIRST blank line after the
            // `## Memory` heading -- the one `to_markdown` always writes
            // after the last bullet. This used to `continue`, leaving the
            // parser in header mode, so a body whose first line happened to
            // be a `- key: value` bullet was absorbed as header. That let an
            // entry's own body rewrite the Key it is filed under, or promote
            // itself from `handoff` to `explicit`; it also silently ate any
            // honest bulleted body (`- build: cargo build`). Bullets are
            // header only until this line; everything after it is body,
            // verbatim.
            if trimmed.is_empty() {
                in_header = false;
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
                    "importance" => entry.importance = Some(value.trim().to_string()),
                    "confidence" => entry.confidence = Some(value.trim().to_string()),
                    "tags" => entry.tags = super::config::split_csv_list(value),
                    "paths" => entry.paths = super::config::split_csv_list(value),
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

/// Which memory bank an operation targets. `Private` is this operator's
/// pre-existing machine-local bank -- `<state>/memory/<repo_slug>/`,
/// unchanged by the introduction of scopes. `Shared` is a second,
/// independent bank meant to be committed with the repository itself:
/// `<repo>/.zirv/memory/`. Both scopes store the same `Entry` markdown
/// format through the same parser; only the storage root and trust level
/// differ. Shared content is repository-owned and therefore UNTRUSTED,
/// exactly like `.zirv/ctx.toml`'s repo layer: a checkout can fill it with
/// any text it likes, but nothing here ever reads that text back as
/// configuration (see `shared_scope_content_is_never_read_back_as_
/// configuration` below), and `CtxConfig`'s `REPO_FORBIDDEN` list keeps a
/// repo from even switching the scope on for itself (`MemoryConfig::
/// shared_enabled`). A shared `Entry`'s header fields (`Written`, `Verified`,
/// `Written-By`, `Source`) are themselves attacker-supplied repo content, the
/// same as the body -- a later task wiring this scope into ranking or
/// recency decisions must not treat any of them as trustworthy signal
/// without its own independent check. Issue #37's `write_durable` is a
/// deliberate, narrow exception for overwrite protection specifically: it
/// DOES treat an existing shared entry's own `Source == "explicit"` as the
/// signal that a harvest must never silently overwrite it (see
/// `write_durable`'s own doc comment, "N4, restored for the shared scope") --
/// the same trust the pre-#37 private-scope guard already placed in this
/// exact field, and the one the project's coordinator explicitly re-asserted
/// over "git-diff review is a sufficient safety net" for this decision. A
/// hostile commit could in principle mark its own entry `Source: explicit`
/// to make it harvest-immune; the operator's own git-diff review remains the
/// backstop for *that* residual risk, on top of (not instead of) this check.
// Consumed by the `zirv memory` CLI (`memory_cli.rs`, issue #33) and any
// later injection work built on top of this store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Shared,
    Private,
}

impl MemoryScope {
    /// Whether this scope may be used at all. `cfg.memory.enabled` is a
    /// MASTER switch: `false` disables both scopes outright, so an operator
    /// who turned memory off before the shared scope existed does not
    /// silently start receiving repo-controlled prompt content on upgrade.
    /// `shared_enabled` is a second, shared-only toggle underneath that
    /// master switch -- `Shared` needs both flags on; `Private` only needs
    /// the master switch, same as before scopes existed. Both flags are
    /// `REPO_FORBIDDEN`.
    pub fn enabled(self, cfg: &CtxConfig) -> bool {
        if !cfg.memory.enabled {
            return false;
        }
        match self {
            MemoryScope::Private => true,
            MemoryScope::Shared => cfg.memory.shared_enabled,
        }
    }

    /// Names which flag is responsible for `!self.enabled(cfg)`, for an
    /// error or advisory that wants to say why rather than just that.
    /// Callers only ever call this after already checking `!enabled(cfg)`
    /// -- meaningless otherwise, since then neither flag is actually at
    /// fault. The master switch is checked first because it always wins:
    /// if it is off, that is the reason regardless of `shared_enabled`'s
    /// own value.
    pub fn disabled_reason(self, cfg: &CtxConfig) -> &'static str {
        if !cfg.memory.enabled {
            "memory.enabled = false"
        } else {
            "memory.shared_enabled = false"
        }
    }

    /// This scope's canonical storage directory, or `None` when the location
    /// cannot be trusted (`Shared` only -- see `safe_shared_dir`). `Private`
    /// always resolves; the directory may simply not exist yet, same as
    /// before scopes existed.
    pub fn dir(self, repo: &Path, state: &StateDir, slug: &str) -> Option<PathBuf> {
        match self {
            MemoryScope::Private => Some(state.memory().join(slug)),
            MemoryScope::Shared => safe_shared_dir(repo),
        }
    }
}

/// `<repo>/.zirv/memory/`, refused (returned as `None`) if either it or
/// `<repo>/.zirv` itself is a symlink. A repository checkout can commit a
/// symlink at either level pointing anywhere on the filesystem; following it
/// would read (and, once a later task adds writes, write) outside the
/// repository the operator thinks they are trusting -- the same escape
/// `optimize.rs`'s `nested_claude_files` refuses for `CLAUDE.md` discovery.
/// `None` reads the same as "does not exist" to every caller here
/// (`read_entries`'s own `!dir.is_dir()` short circuit), so an unsafe
/// location is silently treated as an empty bank rather than an error. This
/// is also the write path's traversal defense at the directory level;
/// `upsert_shared`'s own `validate_shared_key` is the matching defense at the
/// file-name level, since a shared entry's file name is its key.
fn safe_shared_dir(repo: &Path) -> Option<PathBuf> {
    let zirv_dir = repo.join(crate::utils::SCRIPT_DIR_NAME);
    let dir = zirv_dir.join("memory");
    let is_symlink = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
    };
    if is_symlink(&zirv_dir) || is_symlink(&dir) {
        return None;
    }
    Some(dir)
}

/// Core directory scan shared by both scopes: reads every `.md` file in
/// `dir`, parses it, and skips anything that fails to read -- the same
/// tolerance every caller of `list` has always had. Symlinked entry files are
/// always skipped rather than followed: a no-op for the private scope's own
/// 0700 directory (nothing legitimate ever puts a symlink there), but
/// load-bearing for the shared scope, where a committed symlink could
/// otherwise make an arbitrary file on this machine read back as if it were a
/// memory entry. One rule for both scopes rather than a per-call toggle: the
/// private scope pays nothing for it, and the shared scope can never
/// accidentally be called without it.
fn read_entries(dir: &Path) -> CtxResult<Vec<(PathBuf, Entry)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|entry| !entry.file_type().is_ok_and(|kind| kind.is_symlink()))
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

/// Lists every entry stored for `slug`, oldest-written-first by filename
/// order (the zero-padded seconds prefix each file name carries). Files that
/// cannot be read are skipped rather than failing the whole listing. Reads
/// only `state.memory().join(slug)` -- no repository path is ever consulted,
/// so nothing checked into a repo can seed, alter, or hide what this
/// returns.
pub fn list(state: &StateDir, slug: &str) -> CtxResult<Vec<(PathBuf, Entry)>> {
    read_entries(&state.memory().join(slug))
}

/// Lists every entry in `scope`'s bank for this repository, gated on that
/// scope's own `cfg.memory` switch (see `MemoryScope::enabled`) and, for
/// `Shared`, on the directory's own safety (`safe_shared_dir`). Both
/// conditions read the same as an absent bank -- an empty vector, never an
/// error -- the same "disabled/missing means nothing" contract `list` and
/// `render_for_prompt` already follow.
///
/// Consumed by the `zirv memory status`/`list`/`recall` verbs
/// (`memory_cli.rs`, issue #33) and, since issue #34, by `render_for_prompt`
/// for the shared half of the core prompt layer.
///
/// NOT a drop-in replacement for `list`: `list` ignores `cfg` entirely and
/// always reads the private directory regardless of `memory.enabled`, which
/// is why callers like `forget`/`forget_all` can still operate on a disabled
/// bank (`prune_to_cap` similarly never consults `cfg` at all -- it does its
/// own `read_dir`, not a call through `list`). `list_scoped` is gated on
/// `scope.enabled(cfg)` and returns empty when the scope is off -- swapping
/// one call for the other silently changes that "disabling reads must never
/// trap data" behavior.
pub fn list_scoped(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
) -> CtxResult<Vec<(PathBuf, Entry)>> {
    if !scope.enabled(cfg) {
        return Ok(Vec::new());
    }
    let Some(dir) = scope.dir(repo, state, slug) else {
        return Ok(Vec::new());
    };
    read_entries(&dir)
}

/// UNGATED sibling of `list_scoped`: ignores `scope.enabled(cfg)` entirely,
/// reading whatever the scope's directory actually holds. Exists only for
/// `zirv memory status` (`memory_cli.rs`), so a disabled scope still
/// reports its counts and bytes rather than hiding them -- the same
/// "disabling a scope must never trap data" rule `forget_scoped`/
/// `verify_scoped` already follow for maintenance verbs. Not a
/// replacement for `list_scoped` anywhere reads are meant to respect the
/// operator's switch (recall, prompt injection).
pub fn list_scoped_unchecked(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
) -> CtxResult<Vec<(PathBuf, Entry)>> {
    let Some(dir) = scope.dir(repo, state, slug) else {
        return Ok(Vec::new());
    };
    read_entries(&dir)
}

/// **Collision policy for every per-key shared-scope operation below**
/// (`get_scoped`, `forget_scoped`, `verify_scoped`): the shared scope's
/// canonical file for key `k` is always `dir.join(k + ".md")` (see
/// `shared_canonical_path`) -- that file, and only that file, is what a
/// per-key lookup, forget, or verify ever reads or writes. A *different*
/// file elsewhere in the directory whose own `Key:` header happens to claim
/// the same value is a canonical-key collision: it can only exist from a
/// hand edit, a merge, or copy-pasted content, since `upsert_shared` itself
/// refuses to create one (see its own doc comment). Such a stray file is
/// never scanned for, matched against, silently included in, or destroyed by
/// a per-key operation here -- `duplicate_keys` (above, against a full
/// `list_scoped`) is the only way to learn one exists, and `forget_scoped`
/// additionally logs a `forget-collision-left` decision when it notices one
/// survives after removing the canonical file (see its own doc comment). A
/// key that fails `validate_shared_key` can never have had a canonical file
/// in the first place, so every operation below reports "not found" for one
/// rather than erroring -- consistent with `forget`/`verify`'s existing "an
/// absent key is not a failure" contract for the private scope.
///
/// Two more rules on the READ side specifically (`get_scoped`/`verify_scoped`,
/// fix round 2): the canonical path is refused outright if it is a symlink
/// rather than a regular file (`is_regular_file`) -- unlike a directory scan
/// through `read_entries`, a single-path `read_to_string` does not filter
/// symlinks out on its own, and following one here would read (`get_scoped`)
/// or read-then-rewrite (`verify_scoped`) an arbitrary file elsewhere on the
/// machine as if it were this repository's own committed content. And
/// `get_scoped` additionally requires the parsed entry's OWN `key` to equal
/// the key requested -- a mismatch (a hand-edited file whose header
/// disagrees with its file name, or any readable non-entry file, which
/// parses to `key == ""`) is refused and logged as `get-key-mismatch` rather
/// than trusted or silently returned as a phantom entry.
///
/// `Private` is unaffected by any of this: it has no canonical per-key file
/// name at all (a key's file name always carries its `Written` timestamp
/// too), so it keeps routing through its own pre-existing, unchanged
/// `get`/`forget`/`verify`, which scan by embedded `Key:` -- the only way
/// private lookup has ever worked.
///
/// `get_scoped` itself: the `_scoped` sibling of the private-only `get`
/// above. Gated on `scope.enabled(cfg)` for both scopes (unlike
/// `forget_scoped`/`verify_scoped`, which are deliberately ungated -- see
/// their own doc comments): `None` (never an error) when the scope is
/// disabled, the key's canonical file does not exist, or (for `Shared`) the
/// key or directory is unsafe.
///
/// Two more `Shared`-only refusals, both fix round 2: the canonical path is
/// refused if it is a symlink rather than a regular file (`is_regular_file`
/// -- see its own doc comment for why `read_to_string` alone is not enough
/// here, unlike a `read_entries`-based scan, which already filters symlinks
/// out); and the parsed entry's OWN `key` must equal the `key` requested --
/// a mismatch (a file whose header disagrees with its file name, or any
/// readable non-entry file, which parses to `key == ""`) is refused too,
/// logged as `get-key-mismatch` (naming both the requested key and the
/// header's actual key) rather than silently trusting the file name or
/// silently returning a phantom entry. The parsed key is never overwritten
/// to paper over the disagreement -- masking a repo-committed inconsistency
/// would be worse than refusing it outright.
///
/// Was dormant between issues #35 and #37: `zirv memory recall`'s
/// exact-match fast path (its one production caller since issue #33) was
/// replaced by the ranking engine (`retrieval::select`), which scores every
/// candidate itself rather than checking for an exact key first. Live again
/// as of issue #37: `write_
/// durable` calls this before every upsert to check whether the key it is
/// about to write already belongs to a deliberate, human-authored entry
/// (`Source == "explicit"`) that a harvest must never silently overwrite --
/// see `write_durable`'s own doc comment for the full N4-for-the-shared-scope
/// rule this backs.
pub fn get_scoped(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    key: &str,
) -> CtxResult<Option<Entry>> {
    if !scope.enabled(cfg) {
        return Ok(None);
    }
    match scope {
        MemoryScope::Private => get(state, slug, key),
        MemoryScope::Shared => {
            let Some(path) = shared_canonical_path(repo, key) else {
                return Ok(None);
            };
            if !is_regular_file(&path) {
                return Ok(None);
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Ok(None);
            };
            let parsed = parse_markdown(&text);
            if parsed.key != key {
                let _ = super::log::append(
                    state,
                    &super::log::Decision {
                        ts: now_secs(),
                        session: "n/a",
                        verb: "memory",
                        verdict: "n/a",
                        score: 0,
                        action: "get-key-mismatch",
                        detail: &format!(
                            "canonical file for '{key}' has header Key '{}': refusing to return it",
                            parsed.key
                        ),
                    },
                );
                return Ok(None);
            }
            Ok(Some(parsed))
        }
    }
}

/// Every key that appears on more than one file in `entries`, sorted for a
/// deterministic result. A collision can only arise from something outside
/// `upsert_shared`'s own writes -- a hand-edited file, a merge, or
/// copy-pasted content -- since `upsert_shared` itself refuses to create one
/// (see below). Exposed so a caller (a future `zirv memory status`/`optimize`
/// surface) can report an existing collision rather than the read path
/// picking one silently.
pub fn duplicate_keys(entries: &[(PathBuf, Entry)]) -> Vec<String> {
    let mut counts = std::collections::HashMap::<&str, usize>::new();
    for (_, entry) in entries {
        *counts.entry(entry.key.as_str()).or_insert(0) += 1;
    }
    let mut dups: Vec<String> = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(key, _)| key.to_string())
        .collect();
    dups.sort();
    dups
}

/// Cap on a shared entry's key: generous for a descriptive slug, short enough
/// that `<key>.md` is always a reasonable file name.
const MAX_SHARED_KEY_LEN: usize = 80;

/// Windows reserved device names: forbidden as a base file name regardless
/// of case or extension (`CON`, `Con.txt`, `con.md` all name the same
/// device). Every key `validate_shared_key` accepts is already
/// lowercase-only, so comparing against this lowercase set is exhaustive.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validates a key meant to address a *shared* entry's file directly (see
/// `upsert_shared` below): non-empty, at most `MAX_SHARED_KEY_LEN` bytes,
/// composed only of lowercase ASCII letters, digits, and hyphens -- the same
/// charset `parse_harvest` already requires of a harvested key -- containing
/// at least one letter or digit (rejects `-`, `--`, and the like: a key with
/// nothing but hyphens is not a meaningful identifier), and not a Windows
/// reserved device name. Unlike `slug_key` (used by the private scope's
/// `remember`, which silently sanitizes any key into a safe file name), this
/// REJECTS an invalid key outright: a shared entry's file name *is* its key,
/// so silently rewriting an invalid key into a different, sanitized one
/// would silently write to a different file than the one the caller named
/// -- exactly the ambiguity a canonical, key-addressed store must not allow.
///
/// This charset is also the write path's traversal defense at the file-name
/// level: a key restricted to `[a-z0-9-]` can never contain `/`, `\`, `..`,
/// or a null byte, so `dir.join(format!("{key}.md"))` can never resolve
/// outside `dir` no matter what `dir` is.
fn validate_shared_key(key: &str) -> CtxResult<()> {
    if key.is_empty() {
        return Err("a memory key must not be empty".into());
    }
    if key.len() > MAX_SHARED_KEY_LEN {
        return Err(
            format!("memory key '{key}' is too long (max {MAX_SHARED_KEY_LEN} bytes)").into(),
        );
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "memory key '{key}' is invalid: only lowercase letters, digits, and hyphens are allowed"
        )
        .into());
    }
    if key.chars().all(|c| c == '-') {
        return Err(format!("memory key '{key}' must contain at least one letter or digit").into());
    }
    if WINDOWS_RESERVED_NAMES.contains(&key) {
        return Err(format!(
            "memory key '{key}' is a reserved Windows device name and cannot be used as a file name"
        )
        .into());
    }
    Ok(())
}

/// Header-rendered string fields must not contain `\n`/`\r`: `to_markdown`
/// writes each of these directly into its own `## Memory` header line, and
/// `parse_markdown`'s header parser recognizes any line starting with a
/// bullet prefix as a new header entry -- so a value containing an embedded
/// newline can inject an entirely different header line (a different `Key`,
/// `Source`, etc.) that a later read parses back as if it were legitimate
/// (demonstrated directly, independent of this guard, by
/// `a_header_rendered_field_with_an_embedded_newline_would_inject_a_fake_
/// header_line`). Checked for every field `to_markdown` interpolates into a
/// header line: `written_by`, `source`, `importance`/`confidence` (if set),
/// and every individual tag/path (each one still ends up on the SAME
/// rendered line, joined by `", "`, so a newline inside any single one of
/// them still breaks that line in two). `key` is excluded: `validate_shared_
/// key`'s charset already rules out `\n`/`\r` there. `body` is excluded too:
/// N2's header-terminates-at-the-first-blank-line rule already means
/// anything after the header, including a body that itself contains
/// newlines, can never be read back as a header line.
fn validate_shared_entry_fields(entry: &Entry) -> CtxResult<()> {
    let no_newline = |value: &str, field: &str| -> CtxResult<()> {
        if value.contains(['\n', '\r']) {
            return Err(format!(
                "memory entry field `{field}` must not contain a newline (it would inject a fake header line into the stored file)"
            )
            .into());
        }
        Ok(())
    };
    no_newline(&entry.written_by, "written_by")?;
    no_newline(&entry.source, "source")?;
    if let Some(importance) = &entry.importance {
        no_newline(importance, "importance")?;
    }
    if let Some(confidence) = &entry.confidence {
        no_newline(confidence, "confidence")?;
    }
    for tag in &entry.tags {
        no_newline(tag, "tags")?;
    }
    for path in &entry.paths {
        no_newline(path, "paths")?;
    }
    Ok(())
}

/// The shared scope's canonical file for `key` -- `<repo>/.zirv/memory/
/// <key>.md` -- or `None` when the key is invalid (never a valid file name;
/// see `validate_shared_key`) or the directory itself is unsafe (see
/// `safe_shared_dir`). Every per-key shared-scope operation (`get_scoped`,
/// `forget_scoped`, `verify_scoped`) builds its path through this helper
/// rather than by hand, so none of them can ever construct a path from
/// caller-controlled input that resolves outside `.zirv/memory/`.
fn shared_canonical_path(repo: &Path, key: &str) -> Option<PathBuf> {
    validate_shared_key(key).ok()?;
    let dir = safe_shared_dir(repo)?;
    Some(dir.join(format!("{key}.md")))
}

/// Whether `path` is an ordinary regular file, not a symlink -- the
/// single-path analogue of `read_entries`'s own symlink skip (fix round 2).
/// `get_scoped`/`verify_scoped`'s canonical-file reads bypass `read_entries`
/// entirely (each reads exactly one known path, not a directory listing), so
/// neither inherited that skip on its own. A symlinked canonical file (a
/// repo-committed `some-key.md -> /etc/passwd`) must never be followed:
/// `parse_markdown` only ever recognizes text between a `## Memory` heading
/// and the next one, so an arbitrary target with no such heading (a literal
/// `/etc/passwd`) parses to an empty phantom entry, not its actual bytes --
/// but a target an attacker specifically crafts to look like a well-formed,
/// key-matching entry is not so limited: `get_scoped` would read it back as
/// if it were a genuine memory entry, and `verify_scoped` would go further
/// and rewrite that crafted content into the repository as ordinary,
/// committable text (`stamp_verified_in_place` still operates on whatever
/// `read_to_string` handed it). `read_to_string` follows a symlink
/// transparently, unlike `write_shared`'s `rename`, which replaces one
/// without ever reading through it.
fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Writes `entry` to the shared bank's canonical, key-addressed file
/// (`<repo>/.zirv/memory/<key>.md`) -- stable rather than timestamp-addressed
/// (issue #32): updating a key rewrites this same file, so `git diff` shows
/// an ordinary content change rather than a delete-and-add pair, and two
/// unrelated keys always land in two different files by construction.
///
/// Gated on `cfg.memory.shared_enabled` and on the directory's own safety
/// (`safe_shared_dir`, via `MemoryScope::Shared::dir`), same as every other
/// shared-scope entry point. Rejects an invalid key (`validate_shared_key`)
/// or a header-rendered field containing a newline
/// (`validate_shared_entry_fields`) before touching the filesystem, and
/// refuses to write if some *other* file already claims the same key (a
/// canonical-key collision -- see `duplicate_keys` above for the read-side
/// counterpart): only a hand-edited or merged directory can produce that
/// state, since this function itself never does. The write is atomic
/// (`state::write_shared`, temp sibling + `rename`), so a concurrent reader
/// never observes a partial file, and two concurrent upserts of the same key
/// each write in full before either `rename` lands -- the result is always
/// one of the two complete entries, never a mix of both.
fn upsert_shared(
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    entry: &Entry,
) -> CtxResult<PathBuf> {
    if !MemoryScope::Shared.enabled(cfg) {
        let reason = MemoryScope::Shared.disabled_reason(cfg);
        return Err(format!("shared memory is disabled ({reason}); nothing was stored").into());
    }
    validate_shared_key(&entry.key)?;
    validate_shared_entry_fields(entry)?;
    let Some(dir) = MemoryScope::Shared.dir(repo, state, slug) else {
        return Err(
            "the shared memory directory is unsafe (a symlink) and cannot be written to".into(),
        );
    };
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}.md", entry.key));
    for (other_path, other) in read_entries(&dir)? {
        if other.key == entry.key && other_path != path {
            return Err(format!(
                "memory key '{}' is already claimed by {} (its canonical file is {}); refusing to create a duplicate",
                entry.key,
                other_path.display(),
                path.display(),
            )
            .into());
        }
    }

    super::state::write_shared(&path, &entry.to_markdown())?;
    Ok(path)
}

/// Scope-aware upsert: `Private` delegates unchanged to `remember` above;
/// `Shared` writes through the new key-addressed `upsert_shared`.
///
/// **Gating asymmetry, deliberate, pin the contract before building a CLI
/// verb on this:** `remember` (and so `Private` here) has never itself
/// consulted `cfg.memory.enabled` -- only the `zirv ctx remember` CLI
/// wrapper (`run_remember_with`) checks that before calling `remember`. So
/// `upsert_scoped(Private, ...)` WRITES even while `memory.enabled` is
/// false; a caller that wants the CLI's refusal behavior must check
/// `cfg.memory.enabled` itself before calling this, the same way `run_
/// remember_with` does. `Shared`, by contrast, DOES check its own gate
/// (`cfg.memory.shared_enabled`) internally, inside `upsert_shared`. This
/// asymmetry is pinned by a test
/// (`upsert_scoped_private_writes_even_when_memory_enabled_is_false_
/// unlike_shared`) precisely so it is never "discovered" as a surprise by a
/// later CLI verb.
pub fn upsert_scoped(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    entry: &Entry,
) -> CtxResult<PathBuf> {
    match scope {
        MemoryScope::Private => remember(state, slug, entry, cfg),
        MemoryScope::Shared => upsert_shared(repo, state, slug, cfg, entry),
    }
}

/// Scope-aware forget: `Private` delegates unchanged to `forget` above.
/// `Shared` removes only the canonical file (see the collision policy on
/// `get_scoped` above) -- nothing else in the directory is ever touched, so
/// a human-named notes file that happens to have its own `Key:` header set
/// to the same value can never be swept up as collateral damage. If, after
/// removing the canonical file, some OTHER file still claims this key (a
/// pre-existing collision `upsert_shared` could never have created itself),
/// that fact is written to the decision log as `forget-collision-left`
/// (naming the key and the surviving path) rather than passed over in
/// silence -- the boolean return value still only reports whether the
/// canonical file itself was removed, since it carries no room for a
/// structured warning. Deliberately does not gate on `cfg.memory.
/// shared_enabled` either, the same "disabling a feature must never trap
/// data" contract the private scope's own `forget` already follows --
/// forgetting must still work while the scope is switched off.
pub fn forget_scoped(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    key: &str,
) -> CtxResult<bool> {
    match scope {
        MemoryScope::Private => forget(state, slug, key),
        MemoryScope::Shared => {
            let Some(path) = shared_canonical_path(repo, key) else {
                return Ok(false);
            };
            let removed = if path.is_file() {
                std::fs::remove_file(&path)?;
                true
            } else {
                false
            };

            if let Some(dir) = safe_shared_dir(repo) {
                // Best-effort (fix round 2): this is an ADVISORY scan after
                // the canonical delete already succeeded -- a scan failure
                // (e.g. the directory becomes unreadable mid-call) must not
                // turn an already-completed forget into an `Err`.
                let stray: Vec<String> = read_entries(&dir)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(other_path, other)| other.key == key && *other_path != path)
                    .map(|(other_path, _)| other_path.display().to_string())
                    .collect();
                if !stray.is_empty() {
                    let _ = super::log::append(
                        state,
                        &super::log::Decision {
                            ts: now_secs(),
                            session: "n/a",
                            verb: "memory",
                            verdict: "n/a",
                            score: 0,
                            action: "forget-collision-left",
                            detail: &format!("'{key}' still claimed by: {}", stray.join(", ")),
                        },
                    );
                }
            }

            Ok(removed)
        }
    }
}

/// Scope-aware verify: `Private` delegates unchanged to `verify` above,
/// which still refreshes via a parse-then-`to_markdown` re-render (unchanged
/// contract: refreshes only the `Verified` stamp, leaving `Written`/body
/// untouched).
///
/// `Shared` touches only the canonical file (same "canonical claimant only"
/// policy as `get_scoped`/`forget_scoped` above; a pre-existing collision
/// from some other file is left untouched, exactly as `duplicate_keys` would
/// report it). Ungated on `shared_enabled` for the same "must not trap data"
/// reason `forget_scoped` is. Refuses (reports `false`, touches nothing) if
/// the canonical path is a symlink rather than a regular file (fix round 2,
/// `is_regular_file`) -- see that function's doc comment for exactly what
/// following one could and couldn't expose.
///
/// **Key agreement (fix round 3: extended from `get_scoped` -- the round-2
/// ruling that scoped this check to `get_scoped` alone was an
/// under-scoping, corrected here):** after parsing, if the entry's own `key`
/// disagrees with the requested `key`, this logs a `verify-key-mismatch`
/// decision (same shape as `get_scoped`'s `get-key-mismatch`) and returns
/// `Ok(false)` WITHOUT writing anything. Before this fix, `verify_scoped`
/// would stamp false freshness onto a mismatched file, answer `true` for a
/// key `get_scoped` already refuses to return, and (see the next point)
/// destroy a readable non-entry file by overwriting it with an empty
/// phantom stub.
///
/// **In-place stamping (fix round 3, data-loss fix):** once the key is
/// confirmed to agree, the `Verified` bullet is refreshed via
/// `stamp_verified_in_place` -- NOT a parse-then-`to_markdown` re-render
/// (what this used to do, and what `Private`'s `verify` still does for the
/// machine-local bank). A re-render silently drops anything `parse_markdown`
/// doesn't recognize into its own fixed field set: an unknown header bullet
/// (`- Priority: urgent`) or a trailing `## Notes` section a human added
/// would both vanish. Issue #32 requires shared entries to stay "readable
/// and editable without Zirv" -- a hand-edited file must survive a `verify`
/// call with everything it wasn't asked to change intact, so the write path
/// edits the raw text directly instead.
pub fn verify_scoped(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    key: &str,
) -> CtxResult<bool> {
    match scope {
        MemoryScope::Private => verify(state, slug, key),
        MemoryScope::Shared => {
            let Some(path) = shared_canonical_path(repo, key) else {
                return Ok(false);
            };
            if !is_regular_file(&path) {
                return Ok(false);
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Ok(false);
            };
            let parsed = parse_markdown(&text);
            if parsed.key != key {
                let _ = super::log::append(
                    state,
                    &super::log::Decision {
                        ts: now_secs(),
                        session: "n/a",
                        verb: "memory",
                        verdict: "n/a",
                        score: 0,
                        action: "verify-key-mismatch",
                        detail: &format!(
                            "canonical file for '{key}' has header Key '{}': refusing to verify it",
                            parsed.key
                        ),
                    },
                );
                return Ok(false);
            }
            let stamped = stamp_verified_in_place(&text, now_secs());
            super::state::write_shared(&path, &stamped)?;
            Ok(true)
        }
    }
}

/// Refreshes the `## Memory` header's `Verified` bullet IN PLACE, leaving
/// every other byte of `text` untouched -- the write-time half of
/// `verify_scoped`'s "hand-edited files must survive a verify call" contract
/// (see that function's own doc comment for the full reasoning).
///
/// Mirrors `parse_markdown`'s own header-boundary rule exactly (the header
/// block runs from the line after `## Memory` up to the first blank line, or
/// the first line that is neither blank nor a well-formed `key: value`
/// bullet, if there is no blank separator) so the two can never disagree
/// about where the header ends. If an existing bullet whose key is
/// `Verified` (case-insensitive, matching `parse_markdown`'s own lookup) is
/// found in that range, its WHOLE line is replaced with a canonical
/// `- Verified: {verified}`; otherwise a new such line is appended at the
/// end of the header block. Every other line -- every other header bullet
/// (recognized or not), the body, and any later `## ` section -- is copied
/// through unchanged. Falls back to returning `text` unchanged if no
/// `## Memory` heading is found at all (unreachable through `verify_scoped`
/// itself, since a file with no such heading parses to `key == ""`, which
/// the key-agreement check above already refuses before this is called; kept
/// as a safe no-op for any other caller).
fn stamp_verified_in_place(text: &str, verified: u64) -> String {
    let had_trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();

    let Some(memory_idx) = lines.iter().position(|line| {
        line.trim()
            .strip_prefix("## ")
            .is_some_and(|rest| rest.trim().eq_ignore_ascii_case("Memory"))
    }) else {
        return text.to_string();
    };

    let mut header_end = memory_idx + 1;
    let mut verified_idx = None;
    for (offset, line) in lines[memory_idx + 1..].iter().enumerate() {
        let idx = memory_idx + 1 + offset;
        if line.trim().is_empty() {
            header_end = idx;
            break;
        }
        match strip_bullet(line).and_then(|bullet| {
            bullet
                .split_once(':')
                .map(|(key, _)| key.trim().to_string())
        }) {
            Some(bullet_key) => {
                if bullet_key.eq_ignore_ascii_case("verified") {
                    verified_idx = Some(idx);
                }
                header_end = idx + 1;
            }
            None => {
                header_end = idx;
                break;
            }
        }
    }

    let new_line = format!("- Verified: {verified}");
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);
    match verified_idx {
        Some(idx) => {
            for (i, line) in lines.iter().enumerate() {
                out.push(if i == idx {
                    new_line.clone()
                } else {
                    (*line).to_string()
                });
            }
        }
        None => {
            for (i, line) in lines.iter().enumerate() {
                if i == header_end {
                    out.push(new_line.clone());
                }
                out.push((*line).to_string());
            }
            if header_end == lines.len() {
                out.push(new_line.clone());
            }
        }
    }

    let mut result = out.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    result
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
            let written = parse_markdown(&text).written;
            // A file that does not parse to a real `Written` timestamp reads
            // as `written == 0`: an empty or partial read (a rewrite window),
            // or a malformed file. Never select such a file for deletion --
            // mirror `mail::list`, which skips parse failures rather than
            // acting on them. Sorting a half-written entry to the front as the
            // "oldest" and deleting it was silent data loss; a real entry
            // always carries a non-zero `Written`.
            (written > 0).then_some((written, path))
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

    // LOW: `remember` is list -> remove-old -> write, so two concurrent
    // `remember`s on one key can each miss the other's not-yet-written file,
    // both write, and leave two entries under the key (the second lands on a
    // `_NNN` suffix). Collapse them deterministically: keep only the entry
    // with the greatest `(written, path)` -- a rule every racing writer
    // computes identically from the same on-disk state, and which can never
    // remove the globally-greatest file, so the bank converges to exactly one
    // entry for the key rather than to zero. Best-effort, like every other
    // removal here.
    let dups: Vec<(PathBuf, Entry)> = list(state, slug)?
        .into_iter()
        .filter(|(_, existing)| existing.key == entry.key)
        .collect();
    if dups.len() > 1
        && let Some((keep, _)) = dups
            .iter()
            .max_by(|a, b| a.1.written.cmp(&b.1.written).then_with(|| a.0.cmp(&b.0)))
    {
        for (other, _) in &dups {
            if other != keep {
                let _ = std::fs::remove_file(other);
            }
        }
    }

    prune_to_cap(&dir, cfg.memory.max_entries);
    Ok(path)
}

/// The single entry for `key`, if any.
// Consumed by the memory prompt layer and harvest (next waves); recall's own
// key filter goes through `list` so its output ordering matches the bank.
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
/// (`prompt::MemoryLine`) for the **core** layer (issue #34): both the
/// private and shared scopes, merged, each gated on its own switch --
/// `cfg.memory.enabled` for private (via `list`, unchanged), `list_scoped`'s
/// own `scope.enabled(cfg)` check for shared (`cfg.memory.shared_enabled`,
/// already returns an empty vec rather than an error when disabled or
/// unsafe, never a hard failure).
///
/// Renders compact key/body pairs only -- no `Written`/`Verified` storage
/// metadata is spent into the prompt by default (issue #34) -- but keeps the
/// raw timestamps on `MemoryLine` for `prompt::select_memory_within_cap`'s
/// ranking and for Task 5's retrieval ranking to reuse.
///
/// Precedence between the two scopes is NOT decided here: this is a plain
/// "list everything eligible" read that tags each line with which scope it
/// came from (`MemoryLine::shared`). `prompt::with_memory_layer`/
/// `select_memory_within_cap` is what enforces "private outranks shared"
/// structurally, using that tag -- see its own doc comment for why a shared
/// entry's own `verified`/`written` fields must never be trusted to compete
/// with a private one directly. That same function also drops any shared
/// line whose `key` collides with a private line's `key` before either is
/// ranked, so a repo-controlled entry can never shadow a private one by
/// reusing its key -- this function returns both, unfiltered, and lets the
/// prompt layer make that call.
pub fn render_for_prompt(
    state: &StateDir,
    repo: &Path,
    slug: &str,
    cfg: &CtxConfig,
) -> Vec<super::prompt::MemoryLine> {
    let mut lines: Vec<super::prompt::MemoryLine> = if cfg.memory.enabled {
        list(state, slug)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, entry)| render_prompt_line(entry, false))
            .collect()
    } else {
        Vec::new()
    };
    lines.extend(
        list_scoped(MemoryScope::Shared, repo, state, slug, cfg)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, entry)| render_prompt_line(entry, true)),
    );
    lines
}

fn render_prompt_line(entry: Entry, shared: bool) -> super::prompt::MemoryLine {
    super::prompt::MemoryLine {
        key: entry.key,
        body: entry.body,
        verified: entry.verified,
        written: entry.written,
        shared,
    }
}

// Issue #37: harvesting durable, repository-wide facts at the end of a
// session's useful life -- not just a *distilled* mid-session restart (the
// pre-existing N6 seam) but a genuinely completed session too. Two distinct
// paths now feed the same one entry point (`harvest_durable`), so a session
// never makes two competing harvest model calls at the same boundary:
//   - the pre-existing rot/timeout-restart seams in exec.rs/wrap.rs already
//     have a freshly *distilled* handoff (`source == "distilled"`, never the
//     mechanical structural fallback -- see the gate at both call sites) and
//     hand it straight to `harvest_durable`;
//   - the new clean-session-end seams (also exec.rs/wrap.rs) have no handoff
//     yet, so `harvest_at_session_end` distills the transcript itself first,
//     under the exact same "distilled only" rule, then calls the same
//     `harvest_durable`.
// A nudge relaunch deliberately gets neither: it is not rot and not a real
// session end, just an interruption.
//
// Opt-in via `cfg.memory.harvest` (default false: an entry worth keeping
// across sessions is a deliberate act, not an inferred one). Unlike the
// pre-issue-#37 harvest, this writes into the SHARED (repo-owned,
// git-diff-reviewable) bank rather than the private one -- the issue's own
// acceptance criterion ("shared-memory changes are reviewable with `git
// diff`") only holds for that scope -- so every harvest call additionally
// requires `cfg.memory.shared_enabled`, and never writes anywhere but
// through the ordinary `upsert_scoped(Shared, ...)` path: no `git add`, no
// commit, ever (see `a_harvest_never_invokes_git` below).
//
// N4 carries over unchanged in spirit, restored for the shared scope: a
// harvested fact never overwrites an entry whose `Source` is `"explicit"`
// (`write_durable`'s `get_scoped` check) -- model-proposed output silently
// clobbering deliberate human knowledge is exactly the failure this project
// guards against everywhere else, and git-diff reviewability is a second,
// independent safety net on top of this check, never a substitute for it.
//
// Every harvest call is best-effort at every one of its four call sites: the
// result is discarded with `let _ =`, so a failed or timed-out distiller
// call, or a write refused by `upsert_shared`, can never turn a successful
// restart or a clean exit into a failed one.
//
// Reuses `handoff::run_model` -- the same one fresh, cheap-model call
// handoff distillation itself makes, with the same timeout/kill-deadline
// shape -- rather than inventing a second call mechanism, and reuses the
// same strict `key: body` line parser (`parse_harvest`) `zirv memory init`
// already established. What issue #37 actually adds is
// `filter_durable_candidates`: a PURE, deterministic function -- no model,
// no filesystem, no clock -- applied to the model's raw candidates before
// anything is written. The model's own prompt (`durable_harvest_prompt`)
// already asks for durable facts only and names issue #37's reject
// categories explicitly, but a cheap distiller model can be confidently
// wrong: the filter is the deterministic backstop that actually enforces
// "task progress and temporary TODOs, debugging narration, speculative
// ideas, generic summaries, and cheaply discoverable facts never reach the
// bank" -- the model proposes, the filter disposes -- and it is directly
// testable with no model involved at all.

pub const HARVEST_PROMPT_VERSION: &str = "v2";

/// Same shape as `handoff::bullets`, duplicated locally for the same reason
/// `strip_bullet` above is: this file's edits stay isolated from a module
/// other tasks are actively working in.
fn harvest_bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "(none)\n".to_string();
    }
    items.iter().map(|i| format!("- {i}\n")).collect()
}

/// The durable-harvest prompt: repository facts only, drawn from `Gotchas
/// learned` and `Files touched` -- never `Task`, `Done`, `Remaining` or `Next
/// step`, which describe *this* task rather than the repository itself. This
/// is the handoff-vs-memory separation issue #37 requires: the prompt is its
/// own extraction over a narrow slice of the handoff, never a request to
/// store the handoff note itself (see `a_handoff_shaped_note_yields_no_
/// durable_candidates` below for the parser-level guarantee that backs this
/// even if a future edit here got it wrong). Names issue #37's reject list
/// verbatim, and explicitly told to answer with nothing when nothing below
/// is durable: task state slipping into a cross-session bank is worse than
/// an empty answer, and a cheap model can be confidently wrong, so the
/// instruction errs toward silence -- `filter_durable_candidates` is the
/// deterministic backstop for whenever it doesn't.
pub fn durable_harvest_prompt(handoff: &super::handoff::Handoff) -> String {
    format!(
        "You are extracting durable REPOSITORY FACTS ({HARVEST_PROMPT_VERSION}) from a handoff \
note, for a long-lived SHARED memory bank that outlives any single task or session. A durable \
fact is true about this repository regardless of which task is in progress: an architectural \
decision, a convention, an important constraint or invariant, non-obvious discovered behavior, a \
durable project preference, or a rejected approach and the reason it was rejected.\n\n\
Exclude: task progress and temporary TODOs, debugging narration, speculative ideas, generic \
summaries of what happened this session, and facts cheaply or obviously discoverable from the \
source itself. Task state -- what was done, what remains, or what to do next for the current \
task -- is NOT durable and must not appear in your answer, even though it is shown below for \
context.\n\n\
Answer with zero or more lines, each exactly `key: body`, one fact per line: key a short, \
lowercase, kebab-case slug (letters, digits, hyphens only), body one plain sentence. If nothing \
below is a durable repository fact, answer with nothing at all -- an empty answer is correct and \
expected far more often than not. Do not invent a fact that is not evidenced below, and do not \
answer in markdown, headings, or prose.\n\n\
### Gotchas learned\n{gotchas}\
### Files touched\n{files}",
        gotchas = harvest_bullets(&handoff.gotchas),
        files = harvest_bullets(&handoff.files_touched),
    )
}

/// Strict `key: body` line parser: a line that is blank, has no colon, or
/// whose key is not a lowercase kebab-case slug (the exact shape the prompt
/// asks for) is dropped rather than guessed at -- the same "anything
/// unparseable is nothing" rule `parse_markdown` applies to a whole entry,
/// applied here per line.
pub fn parse_harvest(answer: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in answer.lines() {
        let Some((key, body)) = line.trim().split_once(':') else {
            continue;
        };
        let key = key.trim();
        let body = body.trim();
        if key.is_empty() || body.is_empty() {
            continue;
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        out.push((key.to_string(), body.to_string()));
    }
    out
}

/// Pattern-based rejection signals for `filter_durable_candidates`, grouped
/// by issue #37's own reject-list categories (checked case-insensitively as
/// a plain substring of a candidate's body). A blunt, deliberately
/// conservative instrument, not a semantic classifier: the model's own
/// prompt (`durable_harvest_prompt`) already asks for durable facts only, so
/// this is the deterministic backstop for whenever a cheap distiller model
/// answers with task narration anyway. A false positive here (rejecting a
/// genuinely durable fact that happens to use one of these words) costs
/// only that one candidate for this one session; a false negative pollutes
/// the shared bank forever -- ties go to rejecting.
const REJECT_PATTERNS: &[&str] = &[
    // Task progress and temporary TODOs.
    "todo",
    "to-do",
    "next step",
    "still need",
    "still needs",
    "still needed",
    "remains to be",
    "remaining:",
    "in progress",
    "work in progress",
    " wip ",
    "left to do",
    "not yet done",
    "not done yet",
    // Debugging narration.
    "i debugged",
    "after debugging",
    "while debugging",
    "turns out",
    "stack trace",
    "traceback",
    "the error was",
    "the bug was",
    "fixed a bug",
    "debugging session",
    "root cause was",
    // Speculative ideas.
    "might want",
    "could try",
    "could consider",
    "maybe we",
    "perhaps we",
    "possibly should",
    "we could",
    "it would be nice",
    "consider switching",
    "consider trying",
    // Generic "what happened" summaries.
    "this session",
    "in this session",
    "during this session",
    "the session",
    "we implemented",
    "we added",
    "we changed",
    "we ran",
    "changes were made",
    "was updated to",
    // Facts cheaply or obviously discoverable from the source itself.
    "as seen in the source",
    "as shown in the code",
    "you can see in",
    "the source code shows",
    "is written in rust",
    "is a rust project",
];

/// Whether `body` reads as temporary session state or generic, cheaply
/// discoverable narration rather than a durable repository fact -- a plain
/// case-insensitive substring match against `REJECT_PATTERNS`.
///
/// `pub(super)`, not private: `memory_optimize`'s low-value-entry finding
/// (issue #38) reuses this exact heuristic rather than duplicating a second
/// copy of `REJECT_PATTERNS`'s judgment calls -- an entry that reads like
/// task narration is exactly the kind of low-value content issue #38 asks
/// `zirv memory optimize` to flag, whether it slipped in through a harvest
/// or was typed by hand.
pub(super) fn is_temporary_or_generic(body: &str) -> bool {
    let lower = format!(" {} ", body.to_lowercase());
    REJECT_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// The durability filter (issue #37): a PURE, deterministic function over
/// already-parsed `(key, body)` candidates -- no model, no filesystem, no
/// clock -- so it is directly testable against every reject-list category
/// with no model involved. This is where "reject temporary/session state"
/// is actually enforced: the model proposes, this disposes.
///
/// Applied in order, each check independent of the others: a malformed key
/// (`validate_shared_key` -- not lowercase kebab-case, too long, all-hyphen,
/// or a reserved Windows device name) is dropped; a body matching
/// `is_temporary_or_generic` is dropped; the body is then truncated to
/// `cfg.memory.max_entry_bytes` (the same per-entry cap every other entry in
/// this store gets) and dropped entirely if truncation leaves nothing but
/// whitespace. Only survivors count against the two NEW per-session caps
/// issue #37 asks for: `cfg.memory.harvest_max_entries` (stops accepting
/// once reached, keeping the model's own answer order as a priority order)
/// and `cfg.memory.harvest_max_bytes` (a cumulative budget over
/// every accepted body; a single candidate that alone would blow the
/// remaining budget is skipped rather than truncated further, so a later,
/// smaller candidate still gets its chance).
pub fn filter_durable_candidates(
    candidates: &[(String, String)],
    cfg: &CtxConfig,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for (key, body) in candidates {
        if validate_shared_key(key).is_err() {
            continue;
        }
        if is_temporary_or_generic(body) {
            continue;
        }
        let body = crate::utils::truncate_bytes(body.clone(), Some(cfg.memory.max_entry_bytes));
        if body.trim().is_empty() {
            continue;
        }
        if bytes + body.len() > cfg.memory.harvest_max_bytes {
            continue;
        }
        bytes += body.len();
        out.push((key.clone(), body));
        if out.len() >= cfg.memory.harvest_max_entries {
            break;
        }
    }
    out
}

/// Writes an already-filtered batch to the SHARED bank, each entry through
/// `upsert_scoped(Shared, ...)` -- an existing key (harvested before, or from
/// `zirv memory init`) is updated in place rather than duplicated, exactly
/// the "stable keys, upsert, never append-only" rule issue #37 asks for.
/// Never touches git: `upsert_shared` (via `state::write_shared`) only ever
/// writes an ordinary file with a temp-sibling rename, so a harvest leaves
/// nothing but visible, unstaged working-tree changes for the operator's own
/// `git add`/`git diff`/commit (see `a_harvest_never_invokes_git` below).
///
/// **N4, restored for the shared scope:** a harvested fact never overwrites
/// an entry whose `Source` is `"explicit"` -- something a human or a session
/// deliberately asked to remember, via `zirv ctx remember` or a hand-committed
/// shared entry. `Source == "explicit"` marks a DELIBERATE act; a harvest is
/// an INFERRED one, and the deliberate entry has by far the stronger claim to
/// be right -- model-proposed output silently overwriting human-curated
/// knowledge is exactly the failure this project guards against everywhere
/// else (`REPO_FORBIDDEN` keys, fail-closed policy, `zirv memory init`'s own
/// refusal to overwrite a populated bank without `--merge`/`--force`). Git-diff
/// reviewability is a second, independent safety net on top of this one, not
/// a replacement for it: an invariant enforced in code does not depend on an
/// operator reading every diff carefully before committing. Checked with a
/// fresh `get_scoped` read right before each write (not a single batch read
/// up front), so a harvest never has to worry about acting on stale
/// knowledge of what the bank looked like when the batch started. A skip is
/// logged by key (`harvest-skipped`), the same as the pre-#37 private-scope
/// guard did, so the behavior is observable rather than silent. Re-harvesting
/// an entry that is itself already `source == "harvest"` (or `"init"`) is
/// NOT protected this way -- only a deliberate `"explicit"` entry is -- so a
/// previously-harvested key still refreshes normally when project truth
/// changes, exactly as issue #37 asks.
///
/// Returns how many entries actually landed (skipped keys do not count); a
/// write refused partway through (a canonical-key collision from a
/// hand-edited file, for instance) propagates as `Err`, same as every other
/// scoped write in this module.
fn write_durable(
    repo: &Path,
    state: &StateDir,
    slug: &str,
    accepted: &[(String, String)],
    cfg: &CtxConfig,
    now: u64,
) -> CtxResult<usize> {
    let mut written = 0usize;
    for (key, body) in accepted {
        if let Some(existing) = get_scoped(MemoryScope::Shared, repo, state, slug, cfg, key)?
            && existing.source == "explicit"
        {
            let _ = super::log::append(
                state,
                &super::log::Decision {
                    ts: now,
                    session: "n/a",
                    verb: "memory",
                    verdict: "n/a",
                    score: 0,
                    action: "harvest-skipped",
                    detail: &format!("'{key}' is already an explicit entry"),
                },
            );
            continue;
        }
        let entry = Entry {
            key: key.clone(),
            written_by: "harvest".to_string(),
            written: now,
            verified: now,
            source: "harvest".to_string(),
            body: body.clone(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        upsert_scoped(MemoryScope::Shared, repo, state, slug, cfg, &entry)?;
        written += 1;
    }
    Ok(written)
}

/// THE single durable-harvest entry point (issue #37): every one of the four
/// call sites (exec.rs/wrap.rs, each with a restart seam and a
/// clean-session-end seam) funnels through this one function, so a session
/// never makes two competing harvest model calls at the same boundary.
///
/// Gated on `cfg.memory.enabled && cfg.memory.harvest && cfg.memory.
/// shared_enabled` *first*, before anything about the model is touched, so a
/// disabled operator (or one who left harvest on but the shared scope off)
/// never pays for a spawn. Any failure or timeout from the model call
/// propagates as an `Err` and nothing
/// is written: the answer is parsed, filtered, and stored only after the
/// whole call has already succeeded.
pub fn harvest_durable(
    adapter: &dyn super::adapters::AgentAdapter,
    model: &str,
    handoff: &super::handoff::Handoff,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
) -> CtxResult<usize> {
    if !cfg.memory.enabled || !cfg.memory.harvest || !cfg.memory.shared_enabled {
        return Ok(0);
    }
    let timeout = std::time::Duration::from_secs(cfg.handoff.timeout_secs);
    let answer =
        super::handoff::run_model(adapter, model, &durable_harvest_prompt(handoff), timeout)?;
    let candidates = parse_harvest(&answer);
    let accepted = filter_durable_candidates(&candidates, cfg);
    write_durable(repo, state, slug, &accepted, cfg, now_secs())
}

/// The clean-session-end companion to `harvest_durable` (issue #37): a
/// session that simply ends -- no rot, no timeout, no restart -- previously
/// never harvested at all. This is the seam `exec.rs`'s and `wrap.rs`'s
/// clean-exit arms call: it distills the just-ended transcript itself (there
/// is no already-distilled handoff lying around at a clean exit the way
/// there is at a restart) and, only when that distillation genuinely
/// succeeded (`source == "distilled"` -- never the mechanical structural
/// fallback, which has nothing durable to offer, the exact same rule the
/// restart seams already apply), hands the result to `harvest_durable`.
///
/// Gated on `cfg.memory.enabled && cfg.memory.harvest && cfg.memory.
/// shared_enabled` first, before the distiller is even touched -- the same
/// three-way gate `harvest_durable` itself checks, matched here rather than
/// left to that inner call alone (round-2 finding). Without this, a session
/// with `harvest = true` but `shared_enabled = false` still paid for a real
/// distiller spawn (up to `handoff_timeout`) on every clean exit, only for
/// `harvest_durable`'s own gate to discard the result. So a disabled
/// operator never pays for either model call this function can make.
#[allow(clippy::too_many_arguments)]
pub fn harvest_at_session_end(
    adapter: &dyn super::adapters::AgentAdapter,
    model: &str,
    ctx: &super::event::StructuralContext,
    handoff_timeout: std::time::Duration,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
) -> CtxResult<usize> {
    if !cfg.memory.enabled || !cfg.memory.harvest || !cfg.memory.shared_enabled {
        return Ok(0);
    }
    let (note, source) =
        super::handoff::distill_or_structural(adapter, model, ctx, handoff_timeout);
    if source != "distilled" {
        return Ok(0);
    }
    harvest_durable(adapter, model, &note, repo, state, slug, cfg)
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
    /// NOT a CLI flag on `zirv ctx remember` -- `#[arg(skip)]` always
    /// leaves this at its default (`None`) here. `zirv memory remember`'s
    /// private arm (`memory_cli.rs`, the only surface with `--importance`)
    /// builds this struct directly (not through clap parsing) and sets it,
    /// reusing this same write path rather than duplicating it.
    #[arg(skip)]
    pub importance: Option<String>,
    /// See `importance`'s doc comment -- same reasoning, for `--confidence`.
    #[arg(skip)]
    pub confidence: Option<String>,
    /// See `importance`'s doc comment -- same reasoning, for `--tag`.
    #[arg(skip)]
    pub tags: Vec<String>,
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
                importance: args.importance.clone(),
                confidence: args.confidence.clone(),
                tags: args.tags.clone(),
                // Deliberately unwritable, unlike importance/confidence/tags
                // above: a path signal is inert until issue #44 wires it up
                // (see retrieval.rs's module doc), so no `--path` flag
                // exists to set it yet.
                paths: Vec::new(),
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
        // Saturating, not plain multiplication: `--stale` is an operator-typed
        // `u64`, and anything past `u64::MAX / 86_400` overflowed -- a panic in
        // a debug build, a wrapped (tiny) threshold in a release one, which
        // silently reported every entry as fresh.
        let threshold = now_secs().saturating_sub(days.saturating_mul(86_400));
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

    // N2: the header block ends at the first blank line. Before this, a
    // blank line only `continue`d, so the parser stayed in header mode and
    // any body whose first line happened to be a `- key: value` bullet was
    // absorbed as header -- letting an entry's own body rewrite the Key,
    // Source or Written-by it is filed under.

    #[test]
    fn a_body_bullet_cannot_rewrite_the_header() {
        let hijack = concat!(
            "## Memory\n",
            "- Key: real-key\n",
            "- Written-by: claude\n",
            "- Written: 100\n",
            "- Verified: 100\n",
            "- Source: handoff\n",
            "\n",
            "- Key: hijacked\n",
            "- Source: explicit\n",
            "- Written-by: somebody-else\n",
            "the rest of the body\n",
        );
        let entry = parse_markdown(hijack);

        assert_eq!(entry.key, "real-key", "the body must not rewrite the key");
        assert_eq!(
            entry.source, "handoff",
            "the body must not promote itself to an explicit entry"
        );
        assert_eq!(entry.written_by, "claude");
        assert!(
            entry.body.starts_with("- Key: hijacked"),
            "the would-be header lines stay in the body verbatim: {:?}",
            entry.body
        );
        assert!(entry.body.contains("the rest of the body"));
    }

    /// The honest case the same bug broke: a perfectly ordinary body that
    /// happens to be a bulleted list of `name: value` pairs.
    #[test]
    fn a_body_of_bullet_lines_round_trips_intact() {
        let entry = Entry {
            key: "commands".to_string(),
            written_by: "claude".to_string(),
            written: 42,
            verified: 42,
            source: "explicit".to_string(),
            body: "- build: cargo build\n- test: cargo test".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        let parsed = parse_markdown(&entry.to_markdown());
        assert_eq!(parsed, entry, "a bulleted body must survive a round trip");
    }

    /// A document with no blank line after the header (hand-written, or an
    /// older writer) still has to parse: the first non-bullet line ends the
    /// block, exactly as before.
    #[test]
    fn a_header_with_no_blank_separator_still_ends_at_the_first_prose_line() {
        let md = concat!(
            "## Memory\n",
            "- Key: k\n",
            "- Source: explicit\n",
            "plain prose body\n",
        );
        let entry = parse_markdown(md);
        assert_eq!(entry.key, "k");
        assert_eq!(entry.body, "plain prose body");
    }

    fn sample(key: &str, written: u64) -> Entry {
        Entry {
            key: key.to_string(),
            written_by: "claude".to_string(),
            written,
            verified: written,
            source: "explicit".to_string(),
            body: "the staging DB creds live in 1Password.".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
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
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
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

    /// MED: `prune_to_cap` must never select a present-but-unparseable file
    /// (an empty/partial read mid-rewrite, or a malformed file) for deletion.
    /// Before the fix it read as `written == 0`, sorted first as the "oldest",
    /// and was deleted -- silent data loss racing a concurrent write.
    #[test]
    fn prune_never_deletes_a_present_but_unparseable_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        // An unparseable file: empty, so `parse_markdown` yields `written: 0`.
        let junk = dir.join("0000000000-junk.md");
        std::fs::write(&junk, "").expect("write empty");

        // Plus several real entries, more than the cap.
        for i in 0..5u64 {
            let entry = sample(&format!("k{i}"), 1_700_000_000 + i);
            std::fs::write(
                dir.join(format!("{:010}-k{i}.md", 1_700_000_000 + i)),
                entry.to_markdown(),
            )
            .expect("write entry");
        }

        prune_to_cap(dir, 2);

        assert!(
            junk.exists(),
            "an unparseable file is never chosen for pruning"
        );
        let real: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "0000000000-junk.md")
            .collect();
        assert_eq!(
            real.len(),
            2,
            "the real entries are still pruned down to the cap: {real:?}"
        );
    }

    /// LOW: a key that ended up with two entries (as a racing pair of
    /// `remember`s could leave) resolves the same way on every read, and a
    /// fresh `remember` collapses the key back down to exactly one entry.
    #[test]
    fn duplicate_entries_for_one_key_resolve_deterministically_and_remember_converges_to_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = "-work-repo";
        let dir = state.memory().join(slug);
        super::super::state::create_private_dir_all(&dir).expect("mkdir");

        // Two entries under one key: same base name, the second bumped to a
        // `_NNN` suffix -- exactly the shape claim_and_write's collision path
        // produces for a concurrent second writer.
        let mut a = sample("build-cmd", 1_700_000_000);
        a.body = "cargo build".to_string();
        let mut b = sample("build-cmd", 1_700_000_000);
        b.body = "cargo build --release".to_string();
        std::fs::write(dir.join("1700000000-build-cmd.md"), a.to_markdown()).expect("write a");
        std::fs::write(dir.join("1700000000-build-cmd_001.md"), b.to_markdown()).expect("write b");

        // Deterministic read: `get` returns the same entry on every call.
        let first = get(&state, slug, "build-cmd")
            .expect("get")
            .expect("present");
        let again = get(&state, slug, "build-cmd")
            .expect("get")
            .expect("present");
        assert_eq!(first, again, "a duplicated key resolves to a stable entry");

        // A fresh `remember` collapses the key back to exactly one entry.
        let mut c = sample("build-cmd", 1_700_000_100);
        c.body = "cargo build --locked".to_string();
        remember(&state, slug, &c, &cfg).expect("remember");

        let for_key: Vec<_> = list(&state, slug)
            .expect("list")
            .into_iter()
            .filter(|(_, e)| e.key == "build-cmd")
            .collect();
        assert_eq!(
            for_key.len(),
            1,
            "remember converges the key to a single entry: {for_key:?}"
        );
        assert_eq!(for_key[0].1.body, "cargo build --locked");
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

    // N8: this test used to be named `nothing_in_the_repository_checkout_
    // can_seed_the_bank`, back when there was only one bank. `MemoryScope`
    // below makes that no longer true system-wide: the whole point of
    // `Shared` is that a checkout DOES seed a bank. The invariant this test
    // actually guards -- narrowed, not weakened -- is that the *private*
    // scope specifically still never reads anything a repo checkout wrote;
    // see `shared_scope_reads_memory_committed_in_the_repository_checkout`
    // just below for the shared scope's deliberately opposite behavior, and
    // `shared_scope_content_is_never_read_back_as_configuration` for the
    // boundary that still holds for both: repo-owned memory content can
    // never reach `CtxConfig`.
    #[test]
    fn nothing_in_the_repository_checkout_can_seed_the_private_bank() {
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
            "a repo-side memory tree must never seed the private scope: {listed:?}"
        );
    }

    // MemoryScope: `Shared` (repo-owned, untrusted, `<repo>/.zirv/memory/`)
    // vs. `Private` (machine-local, unchanged by this task).

    /// `memory.enabled` is a MASTER switch (fix round: memory review): off,
    /// it disables `Shared` too, however `shared_enabled` is set.
    /// `shared_enabled` is a second, shared-only toggle underneath it --
    /// `Private` never consults it, and `Shared` needs both flags on.
    #[test]
    fn memory_scope_enabled_treats_the_master_switch_as_a_floor_for_shared() {
        let mut cfg = CtxConfig::default();
        cfg.memory.enabled = false;
        cfg.memory.shared_enabled = true;
        assert!(!MemoryScope::Private.enabled(&cfg));
        assert!(
            !MemoryScope::Shared.enabled(&cfg),
            "the master switch must suppress shared too, even with shared_enabled on"
        );

        cfg.memory.enabled = true;
        cfg.memory.shared_enabled = false;
        assert!(MemoryScope::Private.enabled(&cfg));
        assert!(!MemoryScope::Shared.enabled(&cfg));
    }

    #[test]
    fn shared_scope_resolves_to_a_deterministic_repo_relative_directory() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        assert_eq!(
            MemoryScope::Shared.dir(repo.path(), &state, "-irrelevant"),
            Some(repo.path().join(".zirv").join("memory"))
        );
    }

    #[test]
    fn private_scope_dir_is_unchanged_from_before_scopes_existed() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let slug = repo_slug(repo.path());
        assert_eq!(
            MemoryScope::Private.dir(repo.path(), &state, &slug),
            Some(state.memory().join(&slug))
        );
    }

    /// A repository checkout can commit a symlink at `.zirv` pointing
    /// anywhere on the filesystem. Following it would treat an arbitrary
    /// directory elsewhere on this machine as this repo's shared memory --
    /// the same escape `optimize.rs`'s `nested_claude_files` refuses for
    /// `CLAUDE.md` discovery.
    #[cfg(unix)]
    #[test]
    fn shared_scope_refuses_a_symlinked_zirv_directory() {
        let repo = crate::commands::ctx::testenv::repo();
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(outside.path().join("memory")).expect("mkdir");
        std::os::unix::fs::symlink(outside.path(), repo.path().join(".zirv")).expect("symlink");

        let state = StateDir::from_root(repo.path().join("state"));
        assert_eq!(
            MemoryScope::Shared.dir(repo.path(), &state, "-irrelevant"),
            None,
            "a symlinked .zirv must never be followed"
        );
    }

    /// Same escape one level deeper: `.zirv` itself is real, but
    /// `.zirv/memory` is the symlink.
    #[cfg(unix)]
    #[test]
    fn shared_scope_refuses_a_symlinked_memory_directory() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("secret.md"), "leak").expect("write");
        std::os::unix::fs::symlink(outside.path(), repo.path().join(".zirv").join("memory"))
            .expect("symlink");

        let state = StateDir::from_root(repo.path().join("state"));
        assert_eq!(
            MemoryScope::Shared.dir(repo.path(), &state, "-irrelevant"),
            None,
            "a symlinked .zirv/memory must never be followed"
        );
    }

    /// The deliberate counterpart of `nothing_in_the_repository_checkout_
    /// can_seed_the_private_bank` above: the whole point of the shared scope
    /// is that a checkout's own committed content is read.
    #[test]
    fn shared_scope_reads_memory_committed_in_the_repository_checkout() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("0000000001-shared-fact.md"),
            sample("shared-fact", 1).to_markdown(),
        )
        .expect("write");

        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let listed = list_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
        )
        .expect("list shared");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.key, "shared-fact");
    }

    #[test]
    fn list_scoped_is_empty_when_the_shared_scope_is_disabled() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("0000000001-shared-fact.md"),
            sample("shared-fact", 1).to_markdown(),
        )
        .expect("write");

        let state = StateDir::from_root(repo.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.shared_enabled = false;
        let listed = list_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
        )
        .expect("list shared");
        assert!(
            listed.is_empty(),
            "a disabled shared scope must report empty even with entries on disk: {listed:?}"
        );
    }

    /// A committed symlink inside an otherwise-legitimate shared memory
    /// directory (rather than at the directory itself) is a narrower version
    /// of the same escape: `foo.md -> /etc/passwd` would read an arbitrary
    /// file on this machine back as if it were an innocuous memory entry.
    #[cfg(unix)]
    #[test]
    fn list_scoped_skips_a_symlinked_entry_file_in_the_shared_bank() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("0000000001-real.md"),
            sample("real", 1).to_markdown(),
        )
        .expect("write real entry");

        let outside = tempfile::tempdir().expect("tempdir");
        let leaked = outside.path().join("leaked.md");
        std::fs::write(&leaked, sample("leaked", 2).to_markdown()).expect("write outside file");
        std::os::unix::fs::symlink(&leaked, dir.join("0000000002-linked.md")).expect("symlink");

        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let listed = list_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
        )
        .expect("list shared");

        assert_eq!(
            listed.len(),
            1,
            "the symlinked entry is skipped: {listed:?}"
        );
        assert_eq!(listed[0].1.key, "real");
    }

    /// The acceptance-criterion test for issue #31: shared memory is
    /// repo-owned but must never be able to alter agent binary/model/
    /// security settings. Reading it back only ever produces `Entry`
    /// markdown (key/body text) -- `CtxConfig::load` never looks inside
    /// `.zirv/memory/` at all, so a body engineered to look like forbidden
    /// `ctx.toml` keys still round-trips as inert text, and loading config
    /// from the same repository is unaffected.
    #[test]
    fn shared_scope_content_is_never_read_back_as_configuration() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let malicious = Entry {
            key: "sneaky".to_string(),
            written_by: "attacker".to_string(),
            written: 1,
            verified: 1,
            source: "explicit".to_string(),
            body:
                "agent_bin = \"/malicious\"\nagent = \"codex\"\nworker.claude = \"attacker-model\""
                    .to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        std::fs::write(dir.join("0000000001-sneaky.md"), malicious.to_markdown()).expect("write");

        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let listed = list_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
        )
        .expect("list shared");
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].1.body.contains("agent_bin"),
            "planted content survives only as inert text: {:?}",
            listed[0].1.body
        );

        let empty = env_map(&[]);
        let loaded = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load config");
        assert_eq!(
            loaded.agent, None,
            "no agent was chosen by planted shared-memory content"
        );
        assert_eq!(
            loaded.agent_bin, None,
            "no binary was chosen by planted shared-memory content"
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

    /// R5: `--stale` is an operator-typed `u64` and the day-to-second
    /// conversion used to be a plain multiplication -- a panic in a debug
    /// build, a wrapped (tiny) threshold in a release one, where "everything
    /// is stale" quietly became "nothing is".
    #[test]
    fn an_absurd_staleness_threshold_saturates_rather_than_overflowing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let repo = tempfile::tempdir().expect("tempdir");
        let slug = repo_slug(repo.path());

        let now = now_secs();
        let mut ancient = sample("ancient", now.saturating_sub(900 * 86_400));
        ancient.verified = now.saturating_sub(900 * 86_400);
        remember(&state, &slug, &ancient, &cfg).expect("remember");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        for stale in [u64::MAX / 2, u64::MAX] {
            let args = RecallArgs {
                key: None,
                stale: Some(stale),
                json: true,
            };
            let mut out = Vec::new();
            run_recall_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned())
                .expect("recall must not panic on an absurd --stale");
            assert!(
                String::from_utf8(out).expect("utf8").is_empty(),
                "the threshold saturates to zero, so nothing is older than it"
            );
        }
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
            importance: None,
            confidence: None,
            tags: Vec::new(),
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
            importance: None,
            confidence: None,
            tags: Vec::new(),
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
            importance: None,
            confidence: None,
            tags: Vec::new(),
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

    // N5/issue #34: the memory prompt layer's own source, `render_for_prompt`
    // -- now the merged **core** layer (private + shared).

    #[test]
    fn render_for_prompt_renders_the_key_and_body_of_every_private_entry() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        let mut fresh = sample("build-cmd", 1_700_000_000);
        fresh.body = "cargo build --release".to_string();
        remember(&state, "-work-repo", &fresh, &cfg).expect("remember");

        let rendered = render_for_prompt(&state, repo.path(), "-work-repo", &cfg);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].key, "build-cmd");
        assert_eq!(rendered[0].body, "cargo build --release");
        assert!(!rendered[0].shared, "a private entry is tagged as such");
    }

    #[test]
    fn render_for_prompt_is_empty_when_both_scopes_are_disabled_even_with_entries_stored() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
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
                shared_enabled: false,
                ..super::super::config::MemoryConfig::default()
            },
            ..CtxConfig::default()
        };
        assert!(
            render_for_prompt(&state, repo.path(), "-work-repo", &disabled).is_empty(),
            "a fully disabled bank must render nothing, however much is stored"
        );
    }

    /// Issue #34: the core layer merges both scopes, tagging each line with
    /// where it came from.
    #[test]
    fn render_for_prompt_merges_private_and_shared_entries_with_provenance() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        remember(
            &state,
            "-work-repo",
            &sample("private-fact", 1_700_000_000),
            &cfg,
        )
        .expect("remember private");

        let shared_dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&shared_dir).expect("mkdir");
        std::fs::write(
            shared_dir.join("shared-fact.md"),
            sample("shared-fact", 1_700_000_000).to_markdown(),
        )
        .expect("write shared");

        let rendered = render_for_prompt(&state, repo.path(), "-work-repo", &cfg);
        assert_eq!(rendered.len(), 2, "both scopes are merged: {rendered:?}");
        let private_line = rendered
            .iter()
            .find(|l| l.key == "private-fact")
            .expect("private entry present");
        assert!(!private_line.shared);
        let shared_line = rendered
            .iter()
            .find(|l| l.key == "shared-fact")
            .expect("shared entry present");
        assert!(shared_line.shared);
    }

    /// `memory.enabled = false` is a MASTER switch (fix round: memory
    /// review), superseding the old behavior this test used to pin (a
    /// disabled private scope leaving shared untouched): an operator who
    /// disabled memory before the shared scope existed must not silently
    /// start receiving repo-controlled prompt content on upgrade. Disabling
    /// `shared_enabled` specifically (leaving the master switch on) is the
    /// still-independent toggle -- see `render_for_prompt_merges_private_
    /// and_shared_entries_with_provenance` for the "both on" case this test
    /// used to contrast against.
    #[test]
    fn disabling_memory_master_switch_suppresses_shared_entries_too() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let shared_dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&shared_dir).expect("mkdir");
        std::fs::write(
            shared_dir.join("shared-fact.md"),
            sample("shared-fact", 1_700_000_000).to_markdown(),
        )
        .expect("write shared");

        let cfg = CtxConfig {
            memory: super::super::config::MemoryConfig {
                enabled: false,
                // shared_enabled stays at its default (true): the master
                // switch alone must be enough to suppress shared too.
                ..super::super::config::MemoryConfig::default()
            },
            ..CtxConfig::default()
        };
        assert!(
            render_for_prompt(&state, repo.path(), "-work-repo", &cfg).is_empty(),
            "memory.enabled = false must suppress the shared scope too, not just private"
        );
    }

    #[test]
    fn memory_enabled_false_and_shared_enabled_true_injects_nothing() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        remember(
            &state,
            "-work-repo",
            &sample("private-fact", 1_700_000_000),
            &CtxConfig::default(),
        )
        .expect("remember private");
        let shared_dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&shared_dir).expect("mkdir");
        std::fs::write(
            shared_dir.join("shared-fact.md"),
            sample("shared-fact", 1_700_000_000).to_markdown(),
        )
        .expect("write shared");

        let cfg = CtxConfig {
            memory: super::super::config::MemoryConfig {
                enabled: false,
                shared_enabled: true,
                ..super::super::config::MemoryConfig::default()
            },
            ..CtxConfig::default()
        };
        assert!(
            render_for_prompt(&state, repo.path(), "-work-repo", &cfg).is_empty(),
            "enabled=false must win over an explicit shared_enabled=true"
        );
    }

    // Issue #37: durable shared-memory harvest at session end.

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn fake_model_adapter() -> crate::commands::ctx::adapters::claude::ClaudeAdapter {
        crate::commands::ctx::adapters::claude::ClaudeAdapter::new(Some(
            fixture("fake-model.sh").to_str().expect("utf8 path"),
        ))
    }

    fn sample_handoff() -> super::super::handoff::Handoff {
        super::super::handoff::Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec!["Added the route".to_string()],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec!["The provider sends two events per charge".to_string()],
        }
    }

    /// Checked before anything about the model is touched: an adapter that
    /// would fail to spawn at all is enough to prove nothing was spawned.
    #[test]
    fn harvesting_is_off_unless_the_operator_turns_it_on() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        assert!(!cfg.memory.harvest, "sanity: harvest defaults to off");
        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(Some(
            "/nonexistent/model-binary",
        ));

        let count = harvest_durable(
            &adapter,
            "haiku",
            &sample_handoff(),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        )
        .expect("a disabled harvest is not an error, just a no-op");
        assert_eq!(count, 0);
        assert!(
            list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn a_harvest_names_the_issue_37_reject_list_and_excludes_task_state() {
        let handoff = sample_handoff();
        let prompt = durable_harvest_prompt(&handoff);
        assert!(prompt.contains("Gotchas learned"), "got {prompt}");
        assert!(prompt.contains("Files touched"), "got {prompt}");
        assert!(
            prompt.contains("The provider sends two events per charge"),
            "carries the gotcha context: {prompt}"
        );
        assert!(
            prompt.to_lowercase().contains("not durable")
                || prompt.to_lowercase().contains("must not appear"),
            "the prompt must instruct that task state is excluded: {prompt}"
        );
        assert!(
            prompt.to_lowercase().contains("answer with nothing"),
            "the prompt must invite an empty answer: {prompt}"
        );
        assert!(
            !prompt.contains("## Task"),
            "no handoff task section leaks in"
        );
        for needle in [
            "temporary todo",
            "debugging narration",
            "speculative ideas",
            "generic",
            "cheaply or obviously discoverable",
        ] {
            assert!(
                prompt.to_lowercase().contains(needle),
                "the prompt must name issue #37's own reject category '{needle}': {prompt}"
            );
        }

        // Strict parse: only well-formed `key: body` lines with a lowercase
        // kebab-case key survive; everything else -- prose, a capitalized
        // key that looks like a handoff section, a missing key or body -- is
        // dropped rather than guessed at.
        let parsed = parse_harvest(
            "build-cmd: cargo build --release\n\
             Not a fact, just prose.\n\
             Next Step: keep going\n\
             : missing key\n\
             trailing-colon:\n",
        );
        assert_eq!(
            parsed,
            vec![("build-cmd".to_string(), "cargo build --release".to_string())],
            "only the one well-formed line survives: {parsed:?}"
        );
    }

    /// Issue #37, design decision 6: the durable pass is its own extraction,
    /// never a dump of the handoff note itself. Proven at the parser
    /// boundary: a handoff's own `to_markdown()` output -- `## Task`, `##
    /// Done`, bulleted lists, no `key: body` lines anywhere in it -- must
    /// parse to zero durable candidates. No model involved: if a distiller
    /// ever answered with the handoff note verbatim instead of extracting
    /// from it, this is the deterministic reason nothing would land in the
    /// bank anyway.
    #[test]
    fn a_handoff_shaped_note_yields_no_durable_candidates() {
        let note = sample_handoff();
        let markdown = note.to_markdown();
        let candidates = parse_harvest(&markdown);
        assert!(
            candidates.is_empty(),
            "a handoff's own markdown must never parse as durable key:body facts: {candidates:?}"
        );
    }

    // Pure filter tests (`filter_durable_candidates`): no model, no
    // filesystem, no clock -- these are the real local signal for issue
    // #37's "reject temporary/session state" requirement.

    #[test]
    fn filter_rejects_task_progress_and_temporary_todos() {
        let cfg = CtxConfig::default();
        let candidates = vec![(
            "auth-rate-limit".to_string(),
            "TODO: still need to add rate limiting before merging".to_string(),
        )];
        assert!(filter_durable_candidates(&candidates, &cfg).is_empty());
    }

    #[test]
    fn filter_rejects_debugging_narration() {
        let cfg = CtxConfig::default();
        let candidates = vec![(
            "webhook-bug".to_string(),
            "After debugging for a while, turns out the root cause was a race condition."
                .to_string(),
        )];
        assert!(filter_durable_candidates(&candidates, &cfg).is_empty());
    }

    #[test]
    fn filter_rejects_speculative_ideas() {
        let cfg = CtxConfig::default();
        let candidates = vec![(
            "db-choice".to_string(),
            "We might want to consider switching to postgres eventually.".to_string(),
        )];
        assert!(filter_durable_candidates(&candidates, &cfg).is_empty());
    }

    #[test]
    fn filter_rejects_generic_what_happened_summaries() {
        let cfg = CtxConfig::default();
        let candidates = vec![(
            "session-recap".to_string(),
            "In this session we implemented the payments webhook and ran the tests.".to_string(),
        )];
        assert!(filter_durable_candidates(&candidates, &cfg).is_empty());
    }

    #[test]
    fn filter_rejects_facts_cheaply_discoverable_from_source() {
        let cfg = CtxConfig::default();
        let candidates = vec![(
            "project-language".to_string(),
            "This is a Rust project, as seen in the source code.".to_string(),
        )];
        assert!(filter_durable_candidates(&candidates, &cfg).is_empty());
    }

    #[test]
    fn filter_accepts_a_genuinely_durable_fact() {
        let cfg = CtxConfig::default();
        let candidates = vec![(
            "staging-db-creds".to_string(),
            "The staging DB creds live in 1Password under staging-db.".to_string(),
        )];
        let accepted = filter_durable_candidates(&candidates, &cfg);
        assert_eq!(accepted.len(), 1, "a real fact must survive: {accepted:?}");
    }

    #[test]
    fn filter_drops_a_malformed_key() {
        let cfg = CtxConfig::default();
        let candidates = vec![("Not Kebab Case".to_string(), "a real fact".to_string())];
        assert!(filter_durable_candidates(&candidates, &cfg).is_empty());
    }

    #[test]
    fn filter_enforces_the_per_session_entry_cap_in_priority_order() {
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest_max_entries = 2;
        let candidates: Vec<(String, String)> = (0..5)
            .map(|i| (format!("fact-{i}"), "a genuinely durable fact".to_string()))
            .collect();
        let accepted = filter_durable_candidates(&candidates, &cfg);
        assert_eq!(
            accepted,
            vec![
                ("fact-0".to_string(), "a genuinely durable fact".to_string()),
                ("fact-1".to_string(), "a genuinely durable fact".to_string()),
            ],
            "capped to harvest_max_entries, keeping the model's own priority order: {accepted:?}"
        );
    }

    #[test]
    fn filter_enforces_the_per_session_byte_cap() {
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest_max_bytes = 10;
        let candidates = vec![
            ("fact-a".to_string(), "0123456789".to_string()),
            ("fact-b".to_string(), "more text".to_string()),
        ];
        let accepted = filter_durable_candidates(&candidates, &cfg);
        assert_eq!(
            accepted,
            vec![("fact-a".to_string(), "0123456789".to_string())],
            "the second candidate would blow the total budget: {accepted:?}"
        );
    }

    #[test]
    fn filter_skips_an_oversized_candidate_rather_than_blocking_smaller_ones_after_it() {
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest_max_bytes = 10;
        let candidates = vec![
            (
                "fact-big".to_string(),
                "this body alone is already too long".to_string(),
            ),
            ("fact-small".to_string(), "tiny".to_string()),
        ];
        let accepted = filter_durable_candidates(&candidates, &cfg);
        assert_eq!(
            accepted,
            vec![("fact-small".to_string(), "tiny".to_string())],
            "an oversized candidate is skipped, not a hard stop: {accepted:?}"
        );
    }

    /// Issue #37: shared writes must remain ordinary visible working-tree
    /// changes -- Zirv must never auto-commit them. No model involved: this
    /// exercises the write path directly against a real `git init`-ed
    /// repository and asserts no commit was ever created.
    #[test]
    fn a_harvest_never_invokes_git() {
        let repo = crate::commands::ctx::testenv::repo();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("init")
            .arg("--quiet")
            .status();
        if !matches!(init, Ok(status) if status.success()) {
            eprintln!("skipping a_harvest_never_invokes_git: no usable git binary");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let accepted = vec![("build-cmd".to_string(), "cargo build --release".to_string())];
        write_durable(repo.path(), &state, "-work-repo", &accepted, &cfg, 1_000)
            .expect("write_durable");

        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["log", "--oneline"])
            .output()
            .expect("git log");
        assert!(
            !log.status.success() || log.stdout.is_empty(),
            "a harvest must never create a commit: {}",
            String::from_utf8_lossy(&log.stdout)
        );

        // `--untracked-files=all` (not the default `normal`, which collapses
        // a wholly-untracked directory to one `?? .zirv/` line) so the
        // harvested file itself shows up in the listing, not just its parent
        // directory.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .expect("git status");
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(
            status_text.contains("build-cmd.md"),
            "the write must land as an ordinary, unstaged working-tree change: {status_text}"
        );
    }

    /// N4: `harvest` is the opt-in for harvesting, but `enabled`/
    /// `shared_enabled` govern the bank as a whole. With either off,
    /// `render_for_prompt`/`list_scoped` return nothing for the shared scope
    /// -- so harvesting into it was writing to a store nobody reads.
    ///
    /// Runs everywhere: the gate short-circuits before `run_model`, so no
    /// model is ever spawned (which is also why the fake-model adapter is
    /// deliberately left unarmed here).
    #[test]
    fn harvest_is_inert_when_the_bank_is_disabled() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.enabled = false;
        cfg.memory.harvest = true;
        let adapter = fake_model_adapter();

        let count = harvest_durable(
            &adapter,
            "haiku",
            &sample_handoff(),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        )
        .expect("a disabled bank is not an error, just a no-op");

        assert_eq!(count, 0);
        assert!(
            list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
                .expect("list")
                .is_empty(),
            "nothing may be written into a bank the operator turned off"
        );

        // The other two halves of the gate hold on their own too.
        for (enabled, harvest, shared_enabled) in [(true, false, true), (true, true, false)] {
            let mut variant = CtxConfig::default();
            variant.memory.enabled = enabled;
            variant.memory.harvest = harvest;
            variant.memory.shared_enabled = shared_enabled;
            assert_eq!(
                harvest_durable(
                    &adapter,
                    "haiku",
                    &sample_handoff(),
                    repo.path(),
                    &state,
                    "-work-repo",
                    &variant,
                )
                .expect("no-op"),
                0
            );
        }
    }

    /// **N4, restored for the shared scope (issue #37 fix round):** a
    /// harvested fact must never overwrite an entry whose `Source` is
    /// `"explicit"` -- the same invariant the pre-#37 private-scope harvest
    /// enforced (`write_harvested`'s own `existing_explicit` guard), restored
    /// here as `write_durable`'s `get_scoped` check. Pure: exercises
    /// `write_durable` directly, no model involved, so it is real local
    /// signal on every platform including this Windows workstation.
    #[test]
    fn a_harvested_fact_never_overwrites_an_explicit_entry() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let explicit = Entry {
            key: "build-cmd".to_string(),
            written_by: "claude".to_string(),
            written: 1_000,
            verified: 1_000,
            source: "explicit".to_string(),
            body: "cargo build --release   # the operator said so".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            &explicit,
        )
        .expect("seed");

        let accepted = vec![
            (
                "build-cmd".to_string(),
                "cargo build (inferred)".to_string(),
            ),
            ("test-cmd".to_string(), "cargo test (inferred)".to_string()),
        ];
        let written = write_durable(repo.path(), &state, "-work-repo", &accepted, &cfg, 2_000)
            .expect("harvest writes");
        assert_eq!(
            written, 1,
            "only the fact with no explicit entry is written"
        );

        let kept = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            "build-cmd",
        )
        .expect("read")
        .expect("the explicit entry is still there");
        assert_eq!(
            kept.body, explicit.body,
            "the deliberate entry survives completely unchanged"
        );
        assert_eq!(kept.source, "explicit");
        assert_eq!(kept.written, 1_000, "not even its timestamps are disturbed");
        assert_eq!(kept.verified, 1_000);

        let added = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            "test-cmd",
        )
        .expect("read")
        .expect("the un-contested fact is written");
        assert_eq!(added.source, "harvest");

        // The skip is visible rather than silent, the same as the pre-#37
        // private-scope guard.
        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains("harvest-skipped") && log.contains("build-cmd"),
            "the skipped key must be named in the decision log: {log}"
        );
    }

    /// Both halves of the restored N4 rule in one pass, so neither can
    /// regress unnoticed by "fixing" the other: a key that already belongs
    /// to a previously-*harvested* entry still gets refreshed when project
    /// truth changes (issue #37's own "existing keys updated, not
    /// duplicated" requirement), while a key that belongs to a deliberate
    /// `"explicit"` entry is left completely untouched in the very same
    /// batch. Pure: exercises `write_durable` directly, no model involved.
    #[test]
    fn a_harvest_updates_a_previously_harvested_key_but_never_an_explicit_one() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let explicit = Entry {
            key: "staging-db-creds".to_string(),
            written_by: "claude".to_string(),
            written: 1_000,
            verified: 1_000,
            source: "explicit".to_string(),
            body: "the staging DB creds live in 1Password.".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        let previously_harvested = Entry {
            key: "build-cmd".to_string(),
            written_by: "harvest".to_string(),
            written: 1_000,
            verified: 1_000,
            source: "harvest".to_string(),
            body: "cargo build".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        for entry in [&explicit, &previously_harvested] {
            upsert_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-work-repo",
                &cfg,
                entry,
            )
            .expect("seed");
        }

        let accepted = vec![
            (
                "staging-db-creds".to_string(),
                "creds moved to Vault under staging-db (inferred)".to_string(),
            ),
            ("build-cmd".to_string(), "cargo build --release".to_string()),
        ];
        let written = write_durable(repo.path(), &state, "-work-repo", &accepted, &cfg, 2_000)
            .expect("harvest writes");
        assert_eq!(written, 1, "only the previously-harvested key lands");

        let still_explicit = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            "staging-db-creds",
        )
        .expect("read")
        .expect("still there");
        assert_eq!(
            still_explicit.body, explicit.body,
            "the explicit entry is never overwritten"
        );

        let refreshed = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            "build-cmd",
        )
        .expect("read")
        .expect("still there");
        assert_eq!(
            refreshed.body, "cargo build --release",
            "the previously-harvested entry IS refreshed"
        );
        assert_eq!(refreshed.source, "harvest");
    }

    #[cfg(unix)]
    #[test]
    fn a_harvest_that_returns_nothing_writes_nothing() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest = true;
        let adapter = fake_model_adapter();

        // C10: a guard, not a bare `set_var`/`remove_var` pair -- an
        // assertion failure below must not leak `FAKE_MODEL_MODE` into every
        // later test in this process.
        let _mode =
            crate::commands::ctx::testenv::VarGuard::set(&[("FAKE_MODEL_MODE", Some("garbage"))]);
        let result = harvest_durable(
            &adapter,
            "haiku",
            &sample_handoff(),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        );
        assert_eq!(
            result.expect("prose with no colon still succeeds, just empty"),
            0
        );
        assert!(
            list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn a_distiller_failure_or_timeout_leaves_the_bank_untouched() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest = true;
        let adapter = fake_model_adapter();

        let _mode =
            crate::commands::ctx::testenv::VarGuard::set(&[("FAKE_MODEL_MODE", Some("fail"))]);
        let result = harvest_durable(
            &adapter,
            "haiku",
            &sample_handoff(),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        );

        assert!(
            result.is_err(),
            "a failing distiller call must surface as an error"
        );
        assert!(
            list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
                .expect("list")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_harvested_entry_is_marked_as_coming_from_a_harvest_and_lands_in_the_shared_bank() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest = true;
        let adapter = fake_model_adapter();

        let _mode =
            crate::commands::ctx::testenv::VarGuard::set(&[("FAKE_MODEL_MODE", Some("harvest"))]);
        let count = harvest_durable(
            &adapter,
            "haiku",
            &sample_handoff(),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        )
        .expect("harvests");

        assert!(count > 0, "the fixture answers with well-formed facts");
        let listed = list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
            .expect("list");
        assert!(!listed.is_empty());
        assert!(
            listed.iter().all(|(_, e)| e.source == "harvest"),
            "every harvested entry is marked as coming from a harvest: {listed:?}"
        );
        // And it is the SHARED bank, not the private one -- the private
        // bank must stay untouched by a durable harvest.
        assert!(
            list(&state, "-work-repo").expect("list").is_empty(),
            "a durable harvest must never write into the private scope"
        );
    }

    #[cfg(unix)]
    #[test]
    fn harvesting_an_existing_key_refreshes_it_rather_than_duplicating_it() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest = true;

        let mut existing = sample("build-cmd", 1_700_000_000);
        existing.body = "cargo build".to_string();
        existing.source = "harvest".to_string();
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
            &existing,
        )
        .expect("seed");

        let adapter = fake_model_adapter();
        let _mode =
            crate::commands::ctx::testenv::VarGuard::set(&[("FAKE_MODEL_MODE", Some("harvest"))]);
        harvest_durable(
            &adapter,
            "haiku",
            &sample_handoff(),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        )
        .expect("harvests");

        let listed = list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
            .expect("list");
        let build_cmd: Vec<_> = listed
            .iter()
            .filter(|(_, e)| e.key == "build-cmd")
            .collect();
        assert_eq!(build_cmd.len(), 1, "refreshed, not duplicated: {listed:?}");
        assert_eq!(
            build_cmd[0].1.source, "harvest",
            "the refreshed entry is still marked as harvested"
        );
        assert_ne!(
            build_cmd[0].1.body, "cargo build",
            "the body was refreshed too"
        );
    }

    /// Issue #37's clean-session-end seam: `harvest_at_session_end` is
    /// gated exactly like `harvest_durable` itself, before either model call
    /// it can make is ever touched.
    #[test]
    fn harvest_at_session_end_is_a_no_op_when_harvest_is_disabled() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        assert!(!cfg.memory.harvest, "sanity: harvest defaults to off");
        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(Some(
            "/nonexistent/model-binary",
        ));
        let ctx = super::super::event::StructuralContext::default();

        let count = harvest_at_session_end(
            &adapter,
            "haiku",
            &ctx,
            std::time::Duration::from_secs(5),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        )
        .expect("a disabled harvest is not an error, just a no-op");
        assert_eq!(count, 0);
        assert!(
            list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
                .expect("list")
                .is_empty()
        );
    }

    /// A mock adapter whose `distiller_cmd` panics if it is ever called --
    /// used by the `shared_enabled` gating test below in place of the
    /// `/nonexistent/model-binary` pattern the sibling test above uses.
    /// That pattern doesn't work here: `distill_or_structural` swallows a
    /// spawn failure into a silent `"structural"` fallback (see its own doc
    /// comment), so `harvest_at_session_end` would return `Ok(0)` either
    /// way, whether or not it actually tried to spawn the distiller first --
    /// which is exactly the gap the round-2 finding this test covers is
    /// about. `capabilities().events = true` so a caller that reaches
    /// `distill_or_structural` proceeds past its own capability check
    /// straight into the call this must never make.
    #[derive(Debug)]
    struct PanicOnDistillAdapter;

    impl super::super::adapters::AgentAdapter for PanicOnDistillAdapter {
        fn name(&self) -> &'static str {
            "panic-on-distill"
        }

        fn program(&self) -> &str {
            "panic-on-distill"
        }

        fn provider(&self) -> &'static str {
            "panic-on-distill"
        }

        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }

        fn detect(&self, _command: &[String]) -> bool {
            false
        }

        fn headless_cmd(
            &self,
            _prompt: &str,
            _session: &super::super::event::SessionId,
            _extra: &[String],
        ) -> std::process::Command {
            std::process::Command::new("true")
        }

        fn interactive_cmd(
            &self,
            _initial_prompt: Option<&str>,
            _extra: &[String],
        ) -> std::process::Command {
            std::process::Command::new("true")
        }

        fn distiller_cmd(&self, _model: &str) -> std::process::Command {
            panic!(
                "distiller_cmd must never be called when memory.shared_enabled is false: \
                 harvest_at_session_end must gate before touching the model"
            );
        }

        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &super::super::event::SessionRef) -> PathBuf {
            PathBuf::new()
        }

        fn parse_events(&self, _jsonl: &str) -> Vec<super::super::event::NormalizedEvent> {
            Vec::new()
        }

        fn structural_context(
            &self,
            _jsonl: &str,
            _last_n: usize,
        ) -> super::super::event::StructuralContext {
            super::super::event::StructuralContext::default()
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> super::super::event::Capabilities {
            super::super::event::Capabilities {
                events: true,
                ..Default::default()
            }
        }

        fn register_turn_signal(
            &self,
            _session: &super::super::event::SessionRef,
            _socket: &Path,
        ) -> super::super::adapters::TurnSignalSetup {
            super::super::adapters::TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// Round-2 finding: `harvest_at_session_end` gated on `cfg.memory.
    /// enabled && cfg.memory.harvest` but NOT `cfg.memory.shared_enabled`,
    /// unlike `harvest_durable` itself (see that function's own three-way
    /// gate). With `harvest = true, shared_enabled = false`, every clean
    /// session exit paid for a real distiller spawn whose result `harvest_
    /// durable`'s own gate then discarded. Fixed by matching `harvest_
    /// durable`'s gate exactly, checked before the distiller is ever
    /// touched.
    #[test]
    fn harvest_at_session_end_is_a_no_op_when_shared_is_disabled_even_with_harvest_on() {
        let repo = crate::commands::ctx::testenv::repo();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.harvest = true;
        cfg.memory.shared_enabled = false;

        let adapter = PanicOnDistillAdapter;
        let ctx = super::super::event::StructuralContext::default();

        let count = harvest_at_session_end(
            &adapter,
            "haiku",
            &ctx,
            std::time::Duration::from_secs(5),
            repo.path(),
            &state,
            "-work-repo",
            &cfg,
        )
        .expect("gated before the model is ever touched, so this is not an error either");
        assert_eq!(count, 0);
        assert!(
            list_scoped(MemoryScope::Shared, repo.path(), &state, "-work-repo", &cfg)
                .expect("list")
                .is_empty()
        );
    }

    // Issue #32: the shared memory store's own schema and CRUD.

    /// The full extended schema -- importance, confidence, tags, and paths,
    /// on top of the fields Task 1 already had -- round-trips intact. This is
    /// the "versionable" half of issue #32: each new field is its own header
    /// line, omitted entirely when unset, so an entry that never sets them
    /// (every private-scope entry so far) renders exactly as before.
    #[test]
    fn an_entry_with_the_extended_schema_fields_round_trips_intact() {
        let mut entry = sample("architecture-invariant", 1_700_000_000);
        entry.importance = Some("high".to_string());
        entry.confidence = Some("medium".to_string());
        entry.tags = vec!["architecture".to_string(), "invariant".to_string()];
        entry.paths = vec!["src/main.rs".to_string(), "src/lib.rs".to_string()];

        let parsed = parse_markdown(&entry.to_markdown());
        assert_eq!(parsed, entry, "the extended schema must round-trip intact");
    }

    /// Minor fix (review round 1): `zirv ctx recall --json` must keep
    /// emitting the pre-issue-#32 shape for an entry that never sets the new
    /// fields -- no `importance`/`confidence`/`tags`/`paths` keys at all,
    /// not `null`/`[]`. `#[serde(skip_serializing_if = ...)]` on each new
    /// field is what guarantees this.
    #[test]
    fn json_serialization_omits_the_new_fields_entirely_when_unset() {
        let entry = sample("build-cmd", 1);
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(!json.contains("importance"), "got {json}");
        assert!(!json.contains("confidence"), "got {json}");
        assert!(!json.contains("\"tags\""), "got {json}");
        assert!(!json.contains("\"paths\""), "got {json}");
    }

    /// The other half: when the new fields ARE set, they do appear.
    #[test]
    fn json_serialization_includes_the_new_fields_once_set() {
        let mut entry = sample("build-cmd", 1);
        entry.importance = Some("high".to_string());
        entry.tags = vec!["ci".to_string()];
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"importance\":\"high\""), "got {json}");
        assert!(json.contains("\"tags\":[\"ci\"]"), "got {json}");
        assert!(!json.contains("confidence"), "still unset: {json}");
    }

    /// Tags and paths are comma-separated on one header line; stray spaces
    /// and empty items (a trailing comma, doubled commas) are normalized away
    /// rather than producing a blank tag.
    #[test]
    fn tags_and_paths_are_split_on_commas_and_trimmed() {
        let md = concat!(
            "## Memory\n",
            "- Key: k\n",
            "- Source: explicit\n",
            "- Tags:  build ,  ci ,, \n",
            "- Paths: src/a.rs ,src/b.rs\n",
            "\n",
            "body\n",
        );
        let entry = parse_markdown(md);
        assert_eq!(entry.tags, vec!["build".to_string(), "ci".to_string()]);
        assert_eq!(
            entry.paths,
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
    }

    /// The "versionable" property in practice: a header line this parser
    /// doesn't know about yet (as a later task's schema extension, e.g. issue
    /// #38's lifecycle states, would add) is simply skipped, never a parse
    /// failure -- the same unknown-header tolerance
    /// `an_unknown_header_or_section_is_skipped_rather_than_failing_the_read`
    /// already covers for Task 1's fields, exercised here against a
    /// plausible future field name.
    #[test]
    fn a_future_schema_field_this_parser_does_not_know_about_yet_is_skipped_not_fatal() {
        let md = concat!(
            "## Memory\n",
            "- Key: k\n",
            "- Source: explicit\n",
            "- Lifecycle: archived\n",
            "\n",
            "body\n",
        );
        let entry = parse_markdown(md);
        assert_eq!(entry.key, "k");
        assert_eq!(entry.body, "body");
    }

    #[test]
    fn validate_shared_key_rejects_an_empty_key() {
        assert!(validate_shared_key("").is_err());
    }

    #[test]
    fn validate_shared_key_rejects_a_key_over_the_length_cap() {
        let long = "a".repeat(MAX_SHARED_KEY_LEN + 1);
        assert!(validate_shared_key(&long).is_err());
        let ok = "a".repeat(MAX_SHARED_KEY_LEN);
        assert!(validate_shared_key(&ok).is_ok());
    }

    /// The traversal defense at the file-name level: anything outside
    /// `[a-z0-9-]` is rejected outright, which rules out `/`, `\`, `..`,
    /// uppercase, spaces, and null bytes in one charset check.
    #[test]
    fn validate_shared_key_rejects_path_traversal_and_separators() {
        for bad in [
            "../../etc/passwd",
            "foo/bar",
            "foo\\bar",
            "..",
            "Build-Cmd",
            "has space",
            "trailing.",
        ] {
            assert!(
                validate_shared_key(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn validate_shared_key_accepts_lowercase_kebab_case() {
        assert!(validate_shared_key("build-cmd").is_ok());
        assert!(validate_shared_key("a").is_ok());
        assert!(validate_shared_key("has-123-digits").is_ok());
    }

    #[test]
    fn validate_shared_key_rejects_pure_dash_keys() {
        for bad in ["-", "--", "---"] {
            assert!(
                validate_shared_key(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn validate_shared_key_rejects_windows_reserved_device_names() {
        for bad in ["con", "nul", "aux", "prn", "com1", "com9", "lpt1", "lpt9"] {
            assert!(
                validate_shared_key(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
        // A reserved word as part of a longer name is not itself reserved --
        // Windows matches the whole base name, never a substring.
        assert!(validate_shared_key("console").is_ok());
        assert!(validate_shared_key("con-fig").is_ok());
    }

    /// IMPORTANT fix (review round 1): `upsert_shared` validated only
    /// `entry.key`, but `to_markdown` interpolates `written_by`/`source`/
    /// `importance`/`confidence`/`tags`/`paths` directly into `## Memory`
    /// header lines. A newline embedded in any of those fields would inject
    /// a fake header line that `parse_markdown` reads back as legitimate --
    /// demonstrated directly (independent of the write-time guard) by
    /// `a_header_rendered_field_with_an_embedded_newline_would_inject_a_
    /// fake_header_line` below. This test proves the guard actually stops it
    /// at write time.
    #[test]
    fn upsert_scoped_shared_rejects_a_header_field_containing_a_newline() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        let mut entry = sample("build-cmd", 1);
        entry.paths = vec!["src/a.rs\n- Key: hijacked".to_string()];

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect_err("a header field containing a newline must be rejected");
        assert!(err.to_string().contains("newline"), "got {err}");
        assert!(
            !repo
                .path()
                .join(".zirv")
                .join("memory")
                .join("build-cmd.md")
                .exists(),
            "nothing is written when a header field is rejected"
        );
    }

    /// Same rejection, for each other header-rendered field individually --
    /// `written_by`, `source`, `importance`, `confidence`, and `tags`.
    #[test]
    fn upsert_scoped_shared_rejects_a_newline_in_any_header_rendered_field() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        let base = sample("build-cmd", 1);
        let variants: Vec<Entry> = vec![
            Entry {
                written_by: "claude\n- Key: hijacked".to_string(),
                ..base.clone()
            },
            Entry {
                source: "explicit\n- Key: hijacked".to_string(),
                ..base.clone()
            },
            Entry {
                importance: Some("high\n- Key: hijacked".to_string()),
                ..base.clone()
            },
            Entry {
                confidence: Some("high\n- Key: hijacked".to_string()),
                ..base.clone()
            },
            Entry {
                tags: vec!["ok".to_string(), "bad\n- Key: hijacked".to_string()],
                ..base.clone()
            },
        ];

        for entry in variants {
            let err = upsert_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                &cfg,
                &entry,
            )
            .expect_err("a newline in a header-rendered field must be rejected");
            assert!(err.to_string().contains("newline"), "got {err}");
        }
    }

    /// Documents the exact injection mechanism the guard above exists to
    /// block, at the parser level, independent of the write-time guard: if a
    /// header-rendered field's value ever reached `to_markdown` uninspected,
    /// an embedded newline would render as a second, fake header line that
    /// `parse_markdown` reads back as legitimate -- the exact scenario the
    /// review flagged (a `paths` value living at `build-cmd.md` round-trips
    /// with key `hijacked`).
    #[test]
    fn a_header_rendered_field_with_an_embedded_newline_would_inject_a_fake_header_line() {
        let mut entry = sample("build-cmd", 1);
        entry.paths = vec!["src/a.rs\n- Key: hijacked".to_string()];
        // `to_markdown` itself has no opinion on this -- the guard lives one
        // layer up, in `upsert_shared`'s `validate_shared_entry_fields` call,
        // which is why this test builds the markdown directly.
        let rendered = entry.to_markdown();
        let parsed = parse_markdown(&rendered);
        assert_eq!(
            parsed.key, "hijacked",
            "an embedded newline in a header field is read back as a new header line: {rendered:?}"
        );
    }

    /// Pins the deliberate gating asymmetry documented on `upsert_scoped`:
    /// `Private` (via `remember`) has never itself consulted
    /// `cfg.memory.enabled` -- only the `zirv ctx remember` CLI wrapper does
    /// that before calling `remember`. `Shared` (via `upsert_shared`) DOES
    /// check its own gate internally. A future CLI verb built directly on
    /// `upsert_scoped` must add its own `memory.enabled` check for the
    /// Private path; this test exists so that requirement is never
    /// "discovered" as a surprise.
    #[test]
    fn upsert_scoped_private_writes_even_when_memory_enabled_is_false_unlike_shared() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let slug = repo_slug(repo.path());
        let mut cfg = CtxConfig::default();
        cfg.memory.enabled = false;

        upsert_scoped(
            MemoryScope::Private,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect(
            "upsert_scoped(Private) writes even while memory.enabled is false, same as remember always has",
        );
        assert!(get(&state, &slug, "build-cmd").expect("get").is_some());

        cfg.memory.shared_enabled = false;
        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect_err("upsert_scoped(Shared) refuses while shared_enabled is false");
        assert!(err.to_string().contains("disabled"), "got {err}");
    }

    /// Acceptance criterion: two unrelated memories modify different files.
    #[test]
    fn upsert_scoped_shared_writes_two_unrelated_keys_to_two_different_files() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");

        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert a");
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("staging-db-creds", 2),
        )
        .expect("upsert b");

        assert!(dir.join("build-cmd.md").exists());
        assert!(dir.join("staging-db-creds.md").exists());
    }

    /// Acceptance criterion: updating a key updates its stable file rather
    /// than appending another historical copy.
    #[test]
    fn upsert_scoped_shared_updating_a_key_rewrites_the_same_file() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");

        let mut entry = sample("build-cmd", 1);
        entry.body = "cargo build".to_string();
        let first_path = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect("first upsert");

        entry.written = 2;
        entry.body = "cargo build --release".to_string();
        let second_path = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect("second upsert");

        assert_eq!(first_path, second_path, "the same key -> the same file");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            files,
            vec!["build-cmd.md".to_string()],
            "no historical copy is left behind: {files:?}"
        );
        assert_eq!(
            parse_markdown(&std::fs::read_to_string(&second_path).expect("read")).body,
            "cargo build --release"
        );
    }

    #[test]
    fn upsert_scoped_private_still_delegates_unchanged_to_remember() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let slug = repo_slug(repo.path());
        let cfg = CtxConfig::default();

        upsert_scoped(
            MemoryScope::Private,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert private");

        let listed = list(&state, &slug).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.key, "build-cmd");
        assert!(
            listed[0]
                .0
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .starts_with("0000000001-"),
            "private upserts still use the timestamp-addressed naming remember always has: {:?}",
            listed[0].0
        );
    }

    #[test]
    fn upsert_scoped_shared_is_refused_when_shared_enabled_is_false() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.shared_enabled = false;

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect_err("a disabled shared scope must refuse the write");
        assert!(err.to_string().contains("disabled"), "got {err}");
        assert!(!repo.path().join(".zirv").join("memory").exists());
    }

    #[test]
    fn upsert_scoped_shared_rejects_an_invalid_key_and_writes_nothing() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("../escape", 1),
        )
        .expect_err("an invalid key must be rejected");
        assert!(err.to_string().contains("invalid"), "got {err}");
        assert!(
            !repo.path().join(".zirv").join("memory").exists(),
            "nothing is written when the key is rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn upsert_scoped_shared_refuses_to_write_through_a_symlinked_zirv_directory() {
        let repo = crate::commands::ctx::testenv::repo();
        let outside = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path(), repo.path().join(".zirv")).expect("symlink");

        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect_err("a symlinked .zirv must refuse the write");
        assert!(err.to_string().contains("symlink"), "got {err}");
        assert!(
            !outside.path().join("memory").exists(),
            "nothing must ever be written outside the repository"
        );
    }

    /// The write-path traversal test the Task 1 review explicitly flagged as
    /// missing: `rename` replaces the directory entry rather than following
    /// it, so writing over a canonical path that is currently a symlink
    /// (e.g. a repo-committed `some-key.md -> /etc/passwd`) must replace the
    /// link with a regular file, never write through it.
    #[cfg(unix)]
    #[test]
    fn upsert_scoped_shared_replaces_a_symlinked_target_file_rather_than_writing_through_it() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "leak-me").expect("write secret");
        std::os::unix::fs::symlink(&secret, dir.join("build-cmd.md")).expect("symlink");

        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let path = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert must succeed, replacing the symlink");

        assert_eq!(
            std::fs::read_to_string(&secret).expect("read secret"),
            "leak-me",
            "the symlink target must never be written through"
        );
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the symlink itself must be replaced by a regular file"
        );
    }

    /// The write-side counterpart of `duplicate_keys` below: a canonical-key
    /// collision from a mismatched file name (hand-edited, or copy-pasted
    /// content into the wrong file) is refused rather than silently creating
    /// a second file claiming the same key.
    #[test]
    fn upsert_scoped_shared_detects_a_canonical_key_collision_from_a_mismatched_filename() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mismatched = sample("build-cmd", 1);
        std::fs::write(dir.join("renamed-by-hand.md"), mismatched.to_markdown())
            .expect("write mismatched");

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 2),
        )
        .expect_err("a canonical-key collision must be refused");
        assert!(err.to_string().contains("build-cmd"), "got {err}");
        assert!(
            !dir.join("build-cmd.md").exists(),
            "the canonical file must not be created when a collision is refused"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("renamed-by-hand.md")).expect("read"),
            mismatched.to_markdown(),
            "the pre-existing mismatched file is untouched"
        );
    }

    #[test]
    fn duplicate_keys_reports_keys_claimed_by_more_than_one_file() {
        let a = (PathBuf::from("a.md"), sample("build-cmd", 1));
        let b = (PathBuf::from("b.md"), sample("build-cmd", 2));
        let c = (PathBuf::from("c.md"), sample("staging-db-creds", 3));
        assert_eq!(duplicate_keys(&[a, b, c]), vec!["build-cmd".to_string()]);
    }

    #[test]
    fn duplicate_keys_is_empty_when_every_key_is_unique() {
        let a = (PathBuf::from("a.md"), sample("build-cmd", 1));
        let b = (PathBuf::from("b.md"), sample("staging-db-creds", 2));
        assert!(duplicate_keys(&[a, b]).is_empty());
    }

    #[test]
    fn get_scoped_finds_an_entry_written_through_upsert_scoped_in_either_scope() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let slug = repo_slug(repo.path());
        let cfg = CtxConfig::default();

        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("shared-key", 1),
        )
        .expect("upsert shared");
        upsert_scoped(
            MemoryScope::Private,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("private-key", 1),
        )
        .expect("upsert private");

        let shared = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            &slug,
            &cfg,
            "shared-key",
        )
        .expect("get shared")
        .expect("present");
        assert_eq!(shared.key, "shared-key");

        let private = get_scoped(
            MemoryScope::Private,
            repo.path(),
            &state,
            &slug,
            &cfg,
            "private-key",
        )
        .expect("get private")
        .expect("present");
        assert_eq!(private.key, "private-key");

        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                &slug,
                &cfg,
                "private-key"
            )
            .expect("get")
            .is_none(),
            "a private-scope key must not be visible through the shared scope"
        );
    }

    #[test]
    fn get_scoped_returns_none_when_the_scope_is_disabled_even_with_an_entry_on_disk() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert");

        let mut disabled = cfg.clone();
        disabled.memory.shared_enabled = false;
        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                &disabled,
                "build-cmd"
            )
            .expect("get")
            .is_none()
        );
    }

    #[test]
    fn forget_scoped_removes_a_shared_entry_by_key() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert");

        assert!(
            forget_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("forget")
        );
        assert!(
            !repo
                .path()
                .join(".zirv")
                .join("memory")
                .join("build-cmd.md")
                .exists()
        );
        assert!(
            !forget_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "no-such-key"
            )
            .expect("forget missing"),
            "forgetting an absent key reports false, not an error"
        );
    }

    /// Same "disabling a feature must never trap data" contract the private
    /// scope's own `forget` already gives: forgetting a shared entry must
    /// still work even while `shared_enabled` is off.
    #[test]
    fn forget_scoped_shared_still_works_when_shared_enabled_is_false() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert");

        let mut disabled = cfg.clone();
        disabled.memory.shared_enabled = false;
        assert!(
            forget_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("forget while disabled")
        );
    }

    /// Minor fix (review round 1): `forget_scoped` used to scan the whole
    /// directory and delete EVERY file whose own `Key:` header matched --
    /// so a human-named notes file that happened to carry the same key
    /// would be deleted as collateral damage. It must now touch only the
    /// canonical `<key>.md` file, and report (via the decision log) rather
    /// than silently ignore a stray file left behind.
    #[test]
    fn forget_scoped_shared_never_deletes_a_stray_file_with_a_matching_header_key() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert canonical");

        // A human-named notes file that happens to carry the same Key
        // header -- exactly the state a hand edit or a merge could produce.
        let stray = sample("build-cmd", 2);
        std::fs::write(dir.join("notes.md"), stray.to_markdown()).expect("write stray");

        let removed = forget_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            "build-cmd",
        )
        .expect("forget");
        assert!(removed, "the canonical file was removed");
        assert!(
            !dir.join("build-cmd.md").exists(),
            "the canonical file is gone"
        );
        assert!(
            dir.join("notes.md").exists(),
            "a stray file with a matching header key must never be deleted as collateral damage"
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains("forget-collision-left") && log.contains("build-cmd"),
            "the surviving collision is reported in the decision log: {log}"
        );
    }

    #[test]
    fn verify_scoped_refreshes_only_the_verified_stamp_for_a_shared_entry() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let mut entry = sample("build-cmd", 1_700_000_000);
        entry.verified = 1_700_000_000;
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect("upsert");

        assert!(
            verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify")
        );

        let refreshed = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            "build-cmd",
        )
        .expect("get")
        .expect("present");
        assert_eq!(refreshed.written, 1_700_000_000, "written stamp untouched");
        assert!(
            refreshed.verified >= now_secs().saturating_sub(5),
            "verified was refreshed to roughly now: {}",
            refreshed.verified
        );

        assert!(
            !verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "no-such-key"
            )
            .expect("verify missing"),
            "verifying an absent key reports false"
        );
    }

    #[test]
    fn verify_scoped_shared_still_works_when_shared_enabled_is_false() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert");

        let mut disabled = cfg.clone();
        disabled.memory.shared_enabled = false;
        assert!(
            verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify while disabled")
        );
    }

    /// Minor fix (review round 1): `verify_scoped` must touch only the
    /// canonical `<key>.md` file, same collision policy as `forget_scoped` --
    /// a stray file elsewhere claiming the same key is left completely
    /// untouched, never refreshed or read as if it were the real entry.
    #[test]
    fn verify_scoped_shared_only_touches_the_canonical_file_even_with_a_stray_collision_present() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut canonical = sample("build-cmd", 1_700_000_000);
        canonical.verified = 1_700_000_000;
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &canonical,
        )
        .expect("upsert canonical");

        let stray = sample("build-cmd", 5);
        std::fs::write(dir.join("notes.md"), stray.to_markdown()).expect("write stray");

        assert!(
            verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify")
        );

        let refreshed = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            "build-cmd",
        )
        .expect("get")
        .expect("present");
        assert_eq!(
            refreshed.written, 1_700_000_000,
            "the canonical entry's written stamp is untouched"
        );
        assert!(refreshed.verified >= now_secs().saturating_sub(5));

        let stray_text = std::fs::read_to_string(dir.join("notes.md")).expect("read stray");
        assert_eq!(
            parse_markdown(&stray_text).verified,
            5,
            "the stray collision is never touched by verify"
        );
    }

    /// `get_scoped` follows the same canonical-file-only policy: a key only
    /// claimed by a stray, non-canonically-named file is not found, even
    /// though a full directory scan would have surfaced it.
    #[test]
    fn get_scoped_shared_does_not_find_a_key_only_claimed_by_a_non_canonical_file() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        std::fs::write(dir.join("notes.md"), sample("build-cmd", 1).to_markdown())
            .expect("write stray");

        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                &cfg,
                "build-cmd"
            )
            .expect("get")
            .is_none(),
            "only the canonical file is ever consulted for a per-key lookup"
        );
    }

    /// IMPORTANT fix (review round 2): `get_scoped` used to read the
    /// canonical path with a plain `std::fs::read_to_string`, which follows
    /// a symlink transparently -- bypassing the symlink skip `read_entries`
    /// gives every directory scan. A repo-committed `build-cmd.md ->
    /// /etc/passwd` must be refused, not read back as an entry.
    #[cfg(unix)]
    #[test]
    fn get_scoped_shared_refuses_a_symlinked_canonical_file() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        // Deliberately a WELL-FORMED entry for the requested key, not
        // garbage: this isolates the symlink defense from the separate
        // key-agreement check below -- content that fails to parse as
        // "build-cmd" would be refused by that other check regardless of
        // whether the symlink is followed, which would make this test pass
        // for the wrong reason.
        let outside = tempfile::tempdir().expect("tempdir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, sample("build-cmd", 1).to_markdown()).expect("write secret");
        std::os::unix::fs::symlink(&secret, dir.join("build-cmd.md")).expect("symlink");

        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                &cfg,
                "build-cmd"
            )
            .expect("get")
            .is_none(),
            "a symlinked canonical file must never be read back as an entry, even one that would otherwise match"
        );
    }

    /// IMPORTANT fix (review round 2), the write-side counterpart:
    /// `verify_scoped` used to read the canonical path the same unguarded
    /// way, and would then WRITE the linked file's own content back into the
    /// repository via `write_shared` -- reading a symlink's target and then
    /// materializing it as ordinary, committable content. Must refuse
    /// instead, touching neither the link nor its target.
    ///
    /// Deliberately a WELL-FORMED entry for the requested key at the symlink
    /// target (not garbage), same reasoning as `get_scoped`'s analogous test:
    /// this isolates the symlink defense from the separate key-agreement
    /// check (fix round 3) -- content that fails to parse as "build-cmd"
    /// would be refused by that other check regardless of whether the
    /// symlink is followed, which would make this test pass for the wrong
    /// reason.
    #[cfg(unix)]
    #[test]
    fn verify_scoped_shared_refuses_a_symlinked_canonical_file_and_never_materializes_its_target() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let outside = tempfile::tempdir().expect("tempdir");
        let secret = outside.path().join("secret.txt");
        let secret_contents = sample("build-cmd", 1).to_markdown();
        std::fs::write(&secret, &secret_contents).expect("write secret");
        std::os::unix::fs::symlink(&secret, dir.join("build-cmd.md")).expect("symlink");

        assert!(
            !verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify"),
            "a symlinked canonical file must never be verified, even one that would otherwise match"
        );

        assert_eq!(
            std::fs::read_to_string(&secret).expect("read secret"),
            secret_contents,
            "the symlink target must never be read-then-rewritten"
        );
        assert!(
            std::fs::symlink_metadata(dir.join("build-cmd.md"))
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the symlink itself must be left completely untouched, not replaced"
        );
    }

    /// IMPORTANT fix (review round 2): `get_scoped` used to return
    /// `Some(parse_markdown(&text))` unconditionally. A hand-edited or
    /// merged file whose header `Key:` disagrees with its own file name must
    /// be refused, not returned as if it were an honest entry for the
    /// requested key -- and the mismatch is logged, not silently masked by
    /// overwriting the parsed key.
    #[test]
    fn get_scoped_shared_refuses_a_canonical_file_whose_header_key_disagrees_with_its_name() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        // The file named build-cmd.md, but its own header claims a
        // different key -- a hand edit or a merge, not something
        // upsert_shared itself can produce.
        std::fs::write(
            dir.join("build-cmd.md"),
            sample("something-else", 1).to_markdown(),
        )
        .expect("write mismatched");

        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                &cfg,
                "build-cmd"
            )
            .expect("get")
            .is_none(),
            "a canonical file whose header key disagrees with its file name must be refused"
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains("get-key-mismatch")
                && log.contains("build-cmd")
                && log.contains("something-else"),
            "the mismatch is logged, naming both keys: {log}"
        );
    }

    /// The phantom-entry case falls out of the same key-agreement check: any
    /// readable file with no `## Memory` header at all parses to
    /// `key == ""`, which can never equal a real requested key.
    #[test]
    fn get_scoped_shared_refuses_a_non_entry_file_at_the_canonical_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        std::fs::write(dir.join("build-cmd.md"), "not a memory entry at all\n")
            .expect("write garbage");

        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                &cfg,
                "build-cmd"
            )
            .expect("get")
            .is_none(),
            "a readable non-entry file must never surface as a phantom entry"
        );
    }

    /// IMPORTANT fix (review round 3): `verify_scoped` gets the same
    /// key-agreement rule as `get_scoped` (the round-2 ruling scoping it to
    /// `get_scoped` alone was an under-scoping). A canonical file whose
    /// header key disagrees with its file name must be refused -- not
    /// stamped with false freshness -- and the mismatch is logged. The file
    /// on disk is byte-identical afterward: nothing was written.
    #[test]
    fn verify_scoped_shared_refuses_a_canonical_file_whose_header_key_disagrees_with_its_name() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mismatched = sample("something-else", 1).to_markdown();
        std::fs::write(dir.join("build-cmd.md"), &mismatched).expect("write mismatched");

        assert!(
            !verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify"),
            "a canonical file whose header key disagrees with its file name must be refused"
        );

        assert_eq!(
            std::fs::read_to_string(dir.join("build-cmd.md")).expect("read"),
            mismatched,
            "nothing is written when the key disagrees"
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains("verify-key-mismatch")
                && log.contains("build-cmd")
                && log.contains("something-else"),
            "the mismatch is logged, naming both keys: {log}"
        );
    }

    /// The phantom-entry case, same key-agreement check: a readable
    /// non-entry file must never be "verified" -- doing so used to destroy
    /// it by overwriting it with an empty phantom stub via the old
    /// parse-then-`to_markdown` re-render.
    #[test]
    fn verify_scoped_shared_refuses_a_non_entry_file_and_leaves_it_byte_identical() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let garbage = "not a memory entry at all\n";
        std::fs::write(dir.join("build-cmd.md"), garbage).expect("write garbage");

        assert!(
            !verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify"),
            "a readable non-entry file must never be verified"
        );

        assert_eq!(
            std::fs::read_to_string(dir.join("build-cmd.md")).expect("read"),
            garbage,
            "the file must be left byte-identical, not overwritten with an empty phantom stub"
        );
    }

    /// FOLDED fix (review round 3, data-loss hazard): a hand-edited shared
    /// entry with an unknown header bullet and a trailing `## Notes` section
    /// must survive `verify_scoped` byte-identical except for the `Verified`
    /// line -- the old parse-then-`to_markdown` re-render would have
    /// silently dropped both, violating issue #32's "readable and editable
    /// without Zirv" contract.
    #[test]
    fn verify_scoped_shared_preserves_unknown_header_lines_and_trailing_sections() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let original = concat!(
            "## Memory\n",
            "- Key: build-cmd\n",
            "- Written-by: claude\n",
            "- Written: 1700000000\n",
            "- Verified: 1700000000\n",
            "- Source: explicit\n",
            "- Priority: urgent\n",
            "\n",
            "cargo build --release\n",
            "\n",
            "## Notes\n",
            "A human wrote this section; Zirv has never read it.\n",
        );
        std::fs::write(dir.join("build-cmd.md"), original).expect("write hand-edited entry");

        let before = now_secs();
        assert!(
            verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify")
        );
        let after_call = now_secs();

        let after = std::fs::read_to_string(dir.join("build-cmd.md")).expect("read");
        // Read the actual stamped value back rather than assuming a second
        // `now_secs()` call matches the one `verify_scoped` itself made --
        // avoids a test that flakes if the clock ticks over between calls.
        let new_verified = parse_markdown(&after).verified;
        assert!(
            (before..=after_call).contains(&new_verified),
            "the Verified stamp was refreshed to roughly now: {new_verified}"
        );

        let expected = original.replace(
            "- Verified: 1700000000",
            &format!("- Verified: {new_verified}"),
        );
        assert_eq!(
            after, expected,
            "only the Verified line may change; every other byte, including the \
             unknown Priority bullet and the trailing Notes section, must survive: {after:?}"
        );
    }

    /// The same in-place stamping also handles the common case: no
    /// pre-existing `Verified` bullet at all (e.g. a hand-written file that
    /// never had one) gets one appended at the end of the header, rather
    /// than failing or fabricating the rest of the entry.
    #[test]
    fn verify_scoped_shared_appends_a_verified_bullet_when_none_exists() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let dir = repo.path().join(".zirv").join("memory");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let original = concat!(
            "## Memory\n",
            "- Key: build-cmd\n",
            "- Source: explicit\n",
            "\n",
            "cargo build\n",
        );
        std::fs::write(dir.join("build-cmd.md"), original).expect("write");

        let before = now_secs();
        assert!(
            verify_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-irrelevant",
                "build-cmd"
            )
            .expect("verify")
        );
        let after_call = now_secs();

        let after = std::fs::read_to_string(dir.join("build-cmd.md")).expect("read");
        let new_verified = parse_markdown(&after).verified;
        assert!(
            (before..=after_call).contains(&new_verified),
            "a Verified bullet is appended and refreshed to roughly now: {after:?}"
        );
        assert!(
            after.contains("- Key: build-cmd"),
            "existing lines survive: {after:?}"
        );
        assert!(
            after.contains("cargo build"),
            "the body survives: {after:?}"
        );
    }

    /// Concurrent-write test required by issue #32: distinct keys racing each
    /// other must never interfere -- each lands in its own canonical file.
    #[test]
    fn concurrent_upserts_to_different_shared_keys_never_interfere() {
        let repo = crate::commands::ctx::testenv::repo();
        let repo_path = repo.path().to_path_buf();
        let state = StateDir::from_root(repo_path.join("state"));
        let cfg = CtxConfig::default();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let repo_path = repo_path.clone();
                let state = state.clone();
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    let entry = sample(&format!("key-{i}"), i as u64 + 1);
                    upsert_scoped(
                        MemoryScope::Shared,
                        &repo_path,
                        &state,
                        "-irrelevant",
                        &cfg,
                        &entry,
                    )
                    .expect("upsert")
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread must not panic");
        }

        let listed = list_scoped(MemoryScope::Shared, &repo_path, &state, "-irrelevant", &cfg)
            .expect("list");
        assert_eq!(listed.len(), 8, "each key gets its own file: {listed:?}");
        let mut keys: Vec<&str> = listed.iter().map(|(_, e)| e.key.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "key-0", "key-1", "key-2", "key-3", "key-4", "key-5", "key-6", "key-7"
            ]
        );
    }

    /// Concurrent-write test required by issue #32: several writers racing
    /// the SAME key must never corrupt the file (each write is atomic, temp
    /// sibling + `rename`) and must converge to exactly one, whole entry --
    /// never two files for one key, unlike the private scope's timestamp-
    /// addressed naming, which needs its own best-effort dedup pass for
    /// exactly this race.
    #[test]
    fn concurrent_upserts_to_the_same_shared_key_always_leave_one_whole_entry() {
        let repo = crate::commands::ctx::testenv::repo();
        let repo_path = repo.path().to_path_buf();
        let state = StateDir::from_root(repo_path.join("state"));
        let cfg = CtxConfig::default();
        let canonical_path = repo_path
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join("memory")
            .join("race-key.md");

        // Deliberately different lengths per body (not just a trailing digit
        // that leaves every rendered entry the same size): a reader
        // observing any length outside the known-whole set can only be a
        // torn read, never a coincidence.
        let bodies: Vec<String> = (0..8)
            .map(|i| format!("body-variant-{}", "x".repeat(i * 37)))
            .collect();
        // Minor fix (review round 1): precompute every racer's exact rendered
        // length up front, so a reader thread racing the writers can assert
        // each read is either absent or one of these WHOLE lengths -- never
        // any other length, which would mean a torn (partial) read was
        // observed mid-`rename`. Asserting only the final state (as before)
        // proves the race converges, but not that no reader ever saw a torn
        // write along the way.
        let whole_lengths: std::collections::HashSet<usize> = bodies
            .iter()
            .map(|body| {
                let mut entry = sample("race-key", 1);
                entry.body = body.clone();
                entry.to_markdown().len()
            })
            .collect();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader_path = canonical_path.clone();
        let reader_lengths = whole_lengths.clone();
        let reader = std::thread::spawn(move || {
            while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(contents) = std::fs::read_to_string(&reader_path) {
                    assert!(
                        reader_lengths.contains(&contents.len()),
                        "a reader must never observe a torn write: saw {} bytes, expected one of {:?}",
                        contents.len(),
                        reader_lengths
                    );
                }
            }
        });

        let handles: Vec<_> = bodies
            .iter()
            .cloned()
            .map(|body| {
                let repo_path = repo_path.clone();
                let state = state.clone();
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    let mut entry = sample("race-key", 1);
                    entry.body = body;
                    upsert_scoped(
                        MemoryScope::Shared,
                        &repo_path,
                        &state,
                        "-irrelevant",
                        &cfg,
                        &entry,
                    )
                    .expect("upsert")
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread must not panic");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader
            .join()
            .expect("reader thread panicked on a torn read");

        let listed = list_scoped(MemoryScope::Shared, &repo_path, &state, "-irrelevant", &cfg)
            .expect("list");
        assert_eq!(
            listed.len(),
            1,
            "one key must never become two files, even under a race: {listed:?}"
        );
        assert_eq!(listed[0].1.key, "race-key");
        assert!(
            bodies.contains(&listed[0].1.body),
            "the surviving body must be one of the raced writes, whole and uncorrupted: {:?}",
            listed[0].1.body
        );
    }
}
