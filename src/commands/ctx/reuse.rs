//! Issue #406, layer 1 of the overcomplication guard: a pre-write REUSE
//! PROBE. Before a `Write`/`Edit`/`MultiEdit` lands, the DEFINITIONS the
//! incoming text adds are extracted and checked against what the repository
//! already defines under the same -- or a closely equivalent -- name; a hit
//! comes back as `additionalContext` naming the existing `path:line`.
//!
//! Advisory only, by construction: nothing here ever denies a write, and a
//! probe that cannot finish inside its budget -- an oversized payload, a
//! spent wall-clock deadline, a tree bigger than the byte allowance -- says
//! nothing at all rather than delaying the tool call or answering partially.
//! Every failure -- an unreadable file, non-UTF-8 bytes, a directory that
//! will not open -- is skipped, never propagated.
//!
//! CLAUDE-ONLY: the probe hangs off claude's `PreToolUse` contract
//! (`hook::run_pretool`), and codex exposes no PreToolUse seam at all (see
//! `adapters/codex.rs`), so there is deliberately no codex plumbing here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::hook::{PreToolInput, PreToolPayload};

/// The tools whose payload this probe understands. `NotebookEdit` is
/// deliberately absent: its payload carries a cell, not source text this
/// module's definition patterns are written for.
pub const PROBE_TOOLS: [&str; 3] = ["Write", "Edit", "MultiEdit"];

/// Extensions the walk scans. Anything else -- data, docs, lockfiles,
/// binaries -- is skipped outright, which is most of what makes the walk
/// affordable on a hot path.
const SOURCE_EXTENSIONS: [&str; 13] = [
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java", "kt", "cs", "ps1", "sh",
];

/// Directory names never descended into. Every dot-prefixed directory
/// (`.git`, `.zirv` and therefore `.zirv/work`, ...) is skipped by the
/// leading-dot rule in [`Scan::walk`] rather than by name.
const SKIPPED_DIRS: [&str; 2] = ["target", "node_modules"];

/// Bytes of source this probe may read before it gives up. 16 MiB covers
/// this repository whole with room to spare; a monorepo that exceeds it gets
/// a logged skip, never a slow tool call.
pub const MAX_SCANNED_BYTES: usize = 16 * 1024 * 1024;

/// Largest single file the scan will open. A file above this is skipped
/// WITHOUT being charged to the budget: a generated bundle or a checked-in
/// blob is not where a hand-written definition lives, and letting one of
/// them consume the whole allowance would end the scan for every real file
/// behind it.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Largest incoming payload whose definitions are extracted at all. The
/// extraction runs a regex over every line of the text a `Write`/`Edit`
/// carries, so an unbounded payload is unbounded work on a hook hot path.
/// Beyond this the probe SKIPS rather than truncating: a partial candidate
/// list silently under-reports, which is worse than saying nothing.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// Wall-clock ceiling on one probe. The hook runs in front of every write,
/// so the ceiling is what keeps a cold page cache or a network filesystem
/// from being felt as editor latency.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(250);

/// Most locations one note names, and the note's own byte cap. Both exist
/// for the same reason: `additionalContext` is injected into the session's
/// own context window, so this must stay a nudge, never a report.
const MAX_MATCHES: usize = 5;
const MAX_NOTE_BYTES: usize = 1024;

/// Most names one payload contributes. A large `Write` declaring dozens of
/// items would otherwise turn one probe into dozens of them.
const MAX_CANDIDATES: usize = 10;

/// Shortest name worth probing for. Two-character names (`ok`, `id`) match
/// something in every repository and prove nothing.
const MIN_NAME_LEN: usize = 3;

/// Names so common that a definition under one says nothing about reuse:
/// the trait methods every type implements (`new`, `default`, `fmt`,
/// `from`, `into`, `drop`, `clone`, `eq`, `hash`, `len`, `is_empty`) and the
/// entry points every module has (`run`, `main`, `parse`). Compared in
/// normalized form ([`normalize`]), so `New`/`_new` are covered too, and
/// applied to BOTH sides of a comparison -- to the names a payload adds and
/// to the definitions found in the repository -- so neither half can raise
/// a match on one.
const GENERIC_NAMES: [&str; 27] = [
    "new", "default", "main", "run", "fmt", "from", "into", "test", "tests", "mod", "build",
    "parse", "get", "set", "init", "drop", "clone", "next", "len", "isempty", "eq", "hash", "name",
    "path", "value", "data", "args",
];

/// One definition-site pattern. `kind` is `None` when the regex captures the
/// declaring keyword itself in group 1 and the name in group 2; `Some(word)`
/// when the pattern has no keyword to capture and group 1 is the name.
struct DefPattern {
    re: Regex,
    kind: Option<&'static str>,
}

/// Every definition form this module recognises, line-oriented (each regex
/// is matched against ONE line, so `^` anchors to that line and a match
/// carries its own line number for free).
///
/// Every pattern anchors at COLUMN 0, and that is the whole scope model:
/// this probe compares bare names, with no notion of the type or block a
/// name belongs to, so an indented definition -- a method inside an `impl`
/// or a `class`, a nested helper, a body-local `const` -- would be matched
/// scope-blind and reported as though a free function of that name already
/// existed. Only top-level definitions are comparable on name alone, so
/// only top-level definitions count, on the incoming side and the
/// repository side alike.
///
/// Built with `Regex::new(...).ok()` rather than `expect`: this runs inside
/// a `PreToolUse` hook under `panic = "abort"`, where a panic takes the tool
/// call with it. A pattern that somehow failed to compile disables itself
/// instead (`def_patterns_all_compile` proves none does).
static DEF_PATTERNS: LazyLock<Vec<DefPattern>> = LazyLock::new(|| {
    [
        // Rust/TS/JS/Python/PowerShell keyword declarations. The optional
        // modifier chain is ordered as the languages spell it: `pub async
        // unsafe fn`, `pub unsafe extern "C" fn`, `export default class`.
        (
            r#"^(?:pub(?:\([^()]*\))?[ \t]+)?(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+"[^"]*"[ \t]+)?(fn|struct|enum|trait|type|mod|class|def|function|interface)[ \t]+([A-Za-z_$][A-Za-z0-9_$-]*)"#,
            None,
        ),
        // `const`/`static` declarations.
        (
            r"^(?:pub(?:\([^()]*\))?[ \t]+)?(?:export[ \t]+)?(const|static)[ \t]+(?:mut[ \t]+)?([A-Za-z_$][A-Za-z0-9_$]*)",
            None,
        ),
        // Go, with or without a receiver.
        (
            r"^func[ \t]+(?:\([^()]*\)[ \t]*)?([A-Za-z_][A-Za-z0-9_]*)",
            Some("func"),
        ),
        // POSIX shell `name() {`.
        (
            r"^([A-Za-z_][A-Za-z0-9_]*)[ \t]*\([ \t]*\)[ \t]*\{",
            Some("function"),
        ),
    ]
    .into_iter()
    .filter_map(|(pattern, kind)| Regex::new(pattern).ok().map(|re| DefPattern { re, kind }))
    .collect()
});

/// Line prefixes that can possibly begin a declaration. A cheap byte-level
/// gate in front of [`DEF_PATTERNS`]: the overwhelming majority of lines in
/// any file start with none of these, and skipping the regexes for them is
/// what keeps a whole-repository scan inside [`PROBE_DEADLINE`]. Tested
/// against the RAW line, never a trimmed one, so it rejects every indented
/// line up front for the same reason the patterns themselves anchor at
/// column 0.
const DECL_STARTS: [&str; 19] = [
    "pub ",
    "export ",
    "async ",
    "unsafe ",
    "extern ",
    "default ",
    "fn ",
    "func ",
    "struct ",
    "enum ",
    "trait ",
    "type ",
    "mod ",
    "class ",
    "def ",
    "function ",
    "interface ",
    "const ",
    "static ",
];

fn may_declare(line: &str) -> bool {
    DECL_STARTS.iter().any(|start| line.starts_with(start)) || line.contains("() {")
}

/// The declaring keyword a group-1 capture names, or `None` for a capture
/// this module has no kind word for.
fn kind_word(captured: Option<&str>) -> Option<&'static str> {
    Some(match captured? {
        "fn" => "fn",
        "struct" => "struct",
        "enum" => "enum",
        "trait" => "trait",
        "type" => "type",
        "mod" => "mod",
        "class" => "class",
        "def" => "def",
        "function" => "function",
        "interface" => "interface",
        "const" => "const",
        "static" => "static",
        _ => return None,
    })
}

/// The multi-line quotings [`BlockState`] tracks, as `(opener, closer)`.
/// Ordered longest-first only for readability -- the earliest opener on a
/// line wins regardless.
const BLOCK_DELIMITERS: [(&str, &str); 3] = [("/*", "*/"), ("\"\"\"", "\"\"\""), ("'''", "'''")];

/// Review round 2, finding 3: whether the scan is currently inside a
/// `/* ... */` block or a `"""`/`'''` string, carried line to line across
/// ONE file (or one payload). [`DEF_PATTERNS`] anchor at column 0 and know
/// nothing of context, so a `def foo():` in a Python module docstring, or a
/// `pub fn example()` in a Rust block comment, was extracted as a real
/// definition on the payload side and on the repository side alike, and
/// advised on.
///
/// Deliberately a delimiter counter rather than a lexer: it only has to be
/// right about text that would otherwise LOOK like a top-level definition,
/// and both of its error directions cost at most one advisory this probe
/// never owed anyone.
#[derive(Debug, Default)]
struct BlockState {
    /// The closer being looked for, when a block is open.
    open: Option<&'static str>,
}

impl BlockState {
    /// Advances the state over `line` and answers whether that line's own
    /// text may be read for definitions -- true only when the line BEGAN
    /// outside every block, because a column-0 definition inside one is
    /// prose, not code.
    ///
    /// Issue #435: a delimiter-shaped substring INSIDE a string literal
    /// (`"see /* usage"`) or after a `//` line comment (`// see /* usage`)
    /// is not a real opener, so a single character-level pass scans `line`
    /// while OUTSIDE a block, skipping whole string literals and anything
    /// from an unquoted `//` to the end of the line before ever looking for
    /// [`BLOCK_DELIMITERS`]. None of that applies while INSIDE a block --
    /// once inside a `/* ... */` comment or a `"""`/`'''` docstring, quotes
    /// and `//` are just prose, so the closer is searched for in the raw
    /// text, exactly as before.
    fn admits(&mut self, line: &str) -> bool {
        let outside = self.open.is_none();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if let Some(closer) = self.open {
                if matches_at(&chars, i, closer) {
                    self.open = None;
                    i += closer.chars().count();
                } else {
                    i += 1;
                }
                continue;
            }
            // Outside a block: a `//` not inside a string ends the line's
            // relevance right here -- nothing after it can open a block.
            if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
                break;
            }
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                // A triple quote is a BLOCK delimiter to this module, not a
                // string literal -- checked here, before any stripping, so
                // it falls through to the delimiter search below instead of
                // being consumed as an (empty) string plus a stray quote.
                let is_triple =
                    chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote);
                // No matching close on this line means it is not a string
                // at all -- a Rust lifetime or bare char tick like `'a` --
                // so it falls through and is left as ordinary text rather
                // than swallowing the rest of the line.
                if !is_triple && let Some(close) = find_closing_quote(&chars, i + 1, quote) {
                    // The whole literal, quotes included, is skipped: its
                    // content may contain `/*`, `*/` or `//`, and none of
                    // those are real here.
                    i = close + 1;
                    continue;
                }
            }
            if let Some((opener, closer)) = BLOCK_DELIMITERS
                .iter()
                .find(|(opener, _)| matches_at(&chars, i, opener))
            {
                self.open = Some(closer);
                i += opener.chars().count();
                continue;
            }
            i += 1;
        }
        outside
    }
}

/// Whether `pat` occurs in `chars` starting at exactly `i`.
fn matches_at(chars: &[char], i: usize, pat: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    i + pat.len() <= chars.len() && chars[i..i + pat.len()] == pat[..]
}

/// The index of the unescaped `quote` character that closes a literal
/// opened just before `start`, if one exists at or after `start` on the
/// same line. A backslash escapes the following character, so `\'` or `\"`
/// never closes the literal early -- this is what lets `"a \" /* b"` still
/// find its real closing quote instead of the escaped one.
fn find_closing_quote(chars: &[char], start: usize, quote: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            c if c == quote => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Every `(kind, name)` declared on one line, in pattern order, filtered by
/// [`is_probeworthy`] so a name too short or too generic to prove anything
/// is dropped on BOTH sides of a comparison rather than only on the
/// incoming one.
fn definitions_on_line(line: &str) -> Vec<(&'static str, String)> {
    if !may_declare(line) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for pattern in DEF_PATTERNS.iter() {
        let Some(captures) = pattern.re.captures(line) else {
            continue;
        };
        let (kind, name) = match pattern.kind {
            Some(kind) => (kind, captures.get(1)),
            None => {
                let Some(kind) = kind_word(captures.get(1).map(|m| m.as_str())) else {
                    continue;
                };
                (kind, captures.get(2))
            }
        };
        if let Some(name) = name
            && is_probeworthy(name.as_str())
        {
            out.push((kind, name.as_str().to_string()));
        }
    }
    out
}

/// Whether a name is worth probing for at all: long enough to be specific,
/// and not one of the [`GENERIC_NAMES`] every module already has.
fn is_probeworthy(name: &str) -> bool {
    let normalized = normalize(name);
    normalized.len() >= MIN_NAME_LEN && !GENERIC_NAMES.contains(&normalized.as_str())
}

/// The names of the definitions `added_text` DECLARES, deduplicated in first
/// appearance order. Pure: no filesystem, no clock.
pub fn added_definitions(added_text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut blocks = BlockState::default();
    for line in added_text.lines() {
        if !blocks.admits(line) {
            continue;
        }
        for (_, name) in definitions_on_line(line) {
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }
    out
}

/// A name reduced to the form two spellings of the same idea share: case
/// folded, `_`/`-` removed, so `fooBar`, `foo_bar` and `foo-bar` all become
/// `foobar`.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Suffixes stripped from a normalized name before comparison -- the shapes
/// a second implementation of an existing thing is habitually named with.
/// `v2` is tried before `2` so `parsethingv2` loses the whole suffix.
const STRIPPED_SUFFIXES: [&str; 4] = ["impl", "v2", "new", "2"];

fn strip_known_suffix(normalized: &str) -> Option<String> {
    STRIPPED_SUFFIXES.iter().find_map(|suffix| {
        normalized
            .strip_suffix(suffix)
            .filter(|rest| rest.len() >= MIN_NAME_LEN)
            .map(str::to_string)
    })
}

/// Every normalized form `name` is considered equivalent to: itself, the
/// same name with a known re-implementation suffix stripped, and the
/// singular/plural of both. Two names match when their variant sets
/// intersect, which makes the relation symmetric. Pure.
pub fn variants(name: &str) -> Vec<String> {
    let base = normalize(name);
    if base.is_empty() {
        return Vec::new();
    }
    let mut forms = BTreeSet::new();
    let stems: Vec<String> = std::iter::once(base.clone())
        .chain(strip_known_suffix(&base))
        .collect();
    for stem in stems {
        if let Some(singular) = stem
            .strip_suffix('s')
            .filter(|rest| rest.len() >= MIN_NAME_LEN)
        {
            forms.insert(singular.to_string());
        }
        forms.insert(format!("{stem}s"));
        forms.insert(stem);
    }
    forms.into_iter().collect()
}

/// One existing definition the probe found, as the note renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The declaring keyword at the existing definition (`fn`, `struct`, ...).
    pub kind: &'static str,
    /// The existing definition's own name, which may be a variant spelling
    /// of the incoming one.
    pub name: String,
    /// Repository-relative path, forward-slashed so the note reads the same
    /// on every platform.
    pub path: String,
    /// 1-based line number.
    pub line: usize,
}

/// What one probe produced: the locations it found (possibly none), or the
/// budget it ran out of. `Skipped` deliberately discards partial matches --
/// a truncated answer presented as a whole one is worse than silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Matches(Vec<Match>),
    Skipped(String),
}

/// The two ceilings one probe runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_bytes: usize,
    pub deadline: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_bytes: MAX_SCANNED_BYTES,
            deadline: PROBE_DEADLINE,
        }
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// `path` relative to `repo`, forward-slashed. `None` when `path` is not
/// under `repo` at all.
fn repo_relative(repo: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(repo)
        .ok()
        .map(|rest| rest.to_string_lossy().replace('\\', "/"))
}

/// Whether a repository-relative path sits under one of the excluded
/// prefixes (`hooks.reuse_exclude`). Prefixes are matched on path-segment
/// boundaries, so `src/a` never excludes `src/ab`.
pub fn is_excluded(repo: &Path, path: &Path, exclude: &[String]) -> bool {
    let Some(relative) = repo_relative(repo, path) else {
        return false;
    };
    exclude.iter().any(|prefix| {
        let prefix = prefix.trim_matches('/');
        !prefix.is_empty() && (relative == prefix || relative.starts_with(&format!("{prefix}/")))
    })
}

/// Why a walk stopped before it ran out of tree. `Deadline`/`Bytes` are the
/// two budget halves and become a [`ProbeOutcome::Skipped`]; `Enough` means
/// [`MAX_MATCHES`] were already found, which is a complete answer, not a
/// truncated one.
enum Halt {
    Deadline,
    Bytes,
    Enough,
}

/// One probe in progress. The walk is LAZY -- it descends and scans in the
/// same pass, checking the deadline on every entry -- so a tree far larger
/// than the budget costs one directory listing per level visited before the
/// deadline stops it, never a full recursive file list built up front.
struct Scan<'a> {
    repo: &'a Path,
    target_file: &'a Path,
    exclude: &'a [String],
    /// Each incoming name with its own [`variants`] set, precomputed once.
    wanted: &'a [(String, BTreeSet<String>)],
    budget: Budget,
    started: Instant,
    scanned: usize,
    /// Incoming names already reported, so one duplication is named once.
    reported: BTreeSet<String>,
    matches: Vec<Match>,
}

impl Scan<'_> {
    /// Whether the wall-clock half of the budget is spent. Checked per
    /// directory entry -- the finest granularity that costs nothing -- so
    /// an over-large tree stops mid-walk rather than after it.
    fn out_of_time(&self) -> bool {
        self.started.elapsed() > self.budget.deadline
    }

    /// Descends one directory. Symlinks (files and directories both),
    /// dot-prefixed directories (`.git`, `.zirv`, ...), [`SKIPPED_DIRS`] and
    /// excluded prefixes are never entered; a directory that will not open
    /// is skipped, never an error. Entries are sorted so a walk is
    /// deterministic, one directory at a time.
    fn walk(&mut self, dir: &Path) -> Result<(), Halt> {
        if is_symlink(dir) {
            return Ok(());
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if self.out_of_time() {
                return Err(Halt::Deadline);
            }
            if is_symlink(&path) || is_excluded(self.repo, &path, self.exclude) {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if path.is_dir() {
                if name.starts_with('.') || SKIPPED_DIRS.contains(&name) {
                    continue;
                }
                self.walk(&path)?;
            } else if path != self.target_file
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
            {
                self.scan_file(&path)?;
            }
        }
        Ok(())
    }

    /// Scans one file. Its SIZE is read from the directory metadata and
    /// charged to the budget BEFORE any content is read, so the cap cannot
    /// be overshot by the one file that crosses it; a file above
    /// [`MAX_FILE_BYTES`] is skipped outright and charged nothing.
    /// Unreadable files and non-UTF-8 bytes are skipped.
    fn scan_file(&mut self, file: &Path) -> Result<(), Halt> {
        let Ok(metadata) = std::fs::metadata(file) else {
            return Ok(());
        };
        if metadata.len() > MAX_FILE_BYTES {
            return Ok(());
        }
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if self.scanned.saturating_add(size) > self.budget.max_bytes {
            return Err(Halt::Bytes);
        }
        self.scanned = self.scanned.saturating_add(size);

        let Ok(bytes) = std::fs::read(file) else {
            return Ok(());
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(());
        };
        let Some(path) = repo_relative(self.repo, file) else {
            return Ok(());
        };
        // Review round 2, finding 3: the same block/docstring tracker the
        // payload side runs, so prose is not evidence that a definition
        // already exists here either. Per file, by construction.
        let mut blocks = BlockState::default();
        for (number, line) in text.lines().enumerate() {
            if blocks.admits(line) {
                for (kind, existing) in definitions_on_line(line) {
                    let existing_variants: BTreeSet<String> =
                        variants(&existing).into_iter().collect();
                    for (name, candidate_variants) in self.wanted {
                        if self.reported.contains(name)
                            || candidate_variants.is_disjoint(&existing_variants)
                        {
                            continue;
                        }
                        self.reported.insert(name.clone());
                        self.matches.push(Match {
                            kind,
                            name: existing.clone(),
                            path: path.clone(),
                            line: number + 1,
                        });
                    }
                }
            }
            if self.matches.len() >= MAX_MATCHES {
                return Err(Halt::Enough);
            }
        }
        Ok(())
    }
}

/// Walks `repo` looking for existing definitions whose name is equivalent to
/// one of `names`, returning at most [`MAX_MATCHES`] of them -- one per
/// incoming name, so five hits mean five distinct duplications.
///
/// `target_file` is excluded: the file being written is not evidence that
/// what it declares already exists. Exceeding either half of `budget`
/// returns [`ProbeOutcome::Skipped`]. Nothing here panics: unreadable files
/// and non-UTF-8 bytes are skipped.
pub fn probe(
    repo: &Path,
    target_file: &Path,
    names: &[String],
    exclude: &[String],
    budget: Budget,
) -> ProbeOutcome {
    let wanted: Vec<(String, BTreeSet<String>)> = names
        .iter()
        .map(|name| (name.clone(), variants(name).into_iter().collect()))
        .collect();
    if wanted.is_empty() {
        return ProbeOutcome::Matches(Vec::new());
    }

    let mut scan = Scan {
        repo,
        target_file,
        exclude,
        wanted: &wanted,
        budget,
        started: Instant::now(),
        scanned: 0,
        reported: BTreeSet::new(),
        matches: Vec::new(),
    };
    match scan.walk(repo) {
        Err(Halt::Deadline) => ProbeOutcome::Skipped(format!(
            "probe deadline of {} ms exceeded",
            budget.deadline.as_millis()
        )),
        Err(Halt::Bytes) => ProbeOutcome::Skipped(format!(
            "probe byte budget of {} exceeded",
            budget.max_bytes
        )),
        Err(Halt::Enough) | Ok(()) => ProbeOutcome::Matches(scan.matches),
    }
}

/// Bytes of payload text [`candidate_names`] would examine for `tool_name`.
/// Counted before any of it is scanned, so the cap in [`evaluate`] bounds
/// the work rather than reporting it afterwards.
fn payload_text_bytes(tool_name: &str, input: &PreToolInput) -> usize {
    match tool_name {
        "Write" => input.content.len(),
        "Edit" => input
            .old_string
            .len()
            .saturating_add(input.new_string.len()),
        "MultiEdit" => input.edits.iter().fold(0, |total, edit| {
            total
                .saturating_add(edit.old_string.len())
                .saturating_add(edit.new_string.len())
        }),
        _ => 0,
    }
}

/// Definitions `new_string` declares that `old_string` did not.
fn added_names(old_string: &str, new_string: &str) -> Vec<String> {
    let existing = added_definitions(old_string);
    added_definitions(new_string)
        .into_iter()
        .filter(|name| !existing.contains(name))
        .collect()
}

/// The names one payload ADDS, ready to probe for. `Write` contributes every
/// definition in the incoming content, plus the file stem when the file does
/// not exist yet (a new `src/foo.rs` proposes a `foo` module). `Edit` and
/// `MultiEdit` contribute the definitions in the replacement text MINUS the
/// ones the replaced text already declared -- an edit that reformats an
/// existing signature adds nothing.
pub fn candidate_names(tool_name: &str, target: &Path, input: &PreToolInput) -> Vec<String> {
    let mut names = match tool_name {
        "Write" => {
            let mut names = added_definitions(&input.content);
            if !target.exists()
                && let Some(stem) = target.file_stem().and_then(|stem| stem.to_str())
                && is_probeworthy(stem)
                && !names.iter().any(|name| name == stem)
            {
                names.push(stem.to_string());
            }
            names
        }
        "Edit" => added_names(&input.old_string, &input.new_string),
        "MultiEdit" => {
            let mut names = Vec::new();
            for edit in &input.edits {
                for name in added_names(&edit.old_string, &edit.new_string) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
            names
        }
        _ => Vec::new(),
    };
    names.truncate(MAX_CANDIDATES);
    names
}

/// What one evaluation says, as `hook::run_pretool` needs it: a note to
/// append to the advise envelope, a budget skip worth one log row, or
/// nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Advice(String),
    Skipped(String),
    Nothing,
}

/// One line per match, capped at [`MAX_MATCHES`] lines and
/// [`MAX_NOTE_BYTES`] bytes. The wording asks for reuse OR a one-line
/// reason, because a probe hit is evidence, not a verdict.
fn advice_note(matches: &[Match]) -> String {
    let mut note = String::new();
    for hit in matches.iter().take(MAX_MATCHES) {
        let line = format!(
            "reuse guard: {} {} already exists at {}:{}; reuse it or say in one line why it \
             does not fit",
            hit.kind, hit.name, hit.path, hit.line
        );
        if note.len() + line.len() + 1 > MAX_NOTE_BYTES {
            break;
        }
        if !note.is_empty() {
            note.push('\n');
        }
        note.push_str(&line);
    }
    note
}

/// The whole layer-1 decision for one already-resolved write: the tool has
/// to be one this probe understands, the target must not sit under an
/// excluded prefix, the payload has to fit inside [`MAX_PAYLOAD_BYTES`], and
/// it must actually add a definition worth looking for -- otherwise nothing
/// is scanned at all.
pub fn evaluate(
    repo: &Path,
    target: &Path,
    payload: &PreToolPayload,
    exclude: &[String],
) -> Outcome {
    if !PROBE_TOOLS.contains(&payload.tool_name.as_str()) {
        return Outcome::Nothing;
    }
    if is_excluded(repo, target, exclude) {
        return Outcome::Nothing;
    }
    if payload_text_bytes(&payload.tool_name, &payload.tool_input) > MAX_PAYLOAD_BYTES {
        return Outcome::Skipped("payload too large".to_string());
    }
    let names = candidate_names(&payload.tool_name, target, &payload.tool_input);
    if names.is_empty() {
        return Outcome::Nothing;
    }
    match probe(repo, target, &names, exclude, Budget::default()) {
        ProbeOutcome::Skipped(reason) => Outcome::Skipped(reason),
        ProbeOutcome::Matches(matches) if matches.is_empty() => Outcome::Nothing,
        ProbeOutcome::Matches(matches) => Outcome::Advice(advice_note(&matches)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern in the table compiles. `DEF_PATTERNS` swallows a
    /// compile failure rather than panicking on a hook hot path, so nothing
    /// else would notice a pattern silently disabling itself.
    #[test]
    fn def_patterns_all_compile() {
        assert_eq!(DEF_PATTERNS.len(), 4, "a pattern failed to compile");
    }

    #[test]
    fn added_definitions_reads_each_supported_language() {
        let rust = added_definitions(
            "pub fn parse_widget() {}\npub struct WidgetSpec;\nconst WIDGET_LIMIT: usize = 3;\n",
        );
        assert_eq!(rust, vec!["parse_widget", "WidgetSpec", "WIDGET_LIMIT"]);

        assert_eq!(
            added_definitions("export function renderWidget() {}\nclass WidgetView {}\n"),
            vec!["renderWidget", "WidgetView"]
        );
        assert_eq!(
            added_definitions("def compute_widget(x):\n    return x\nclass WidgetBox:\n"),
            vec!["compute_widget", "WidgetBox"]
        );
        assert_eq!(
            added_definitions("func WidgetTotal() int {\nfunc (w *W) WidgetSum() int {\n"),
            vec!["WidgetTotal", "WidgetSum"]
        );
        assert_eq!(
            added_definitions("function Get-WidgetReport {\n"),
            vec!["Get-WidgetReport"]
        );
        assert_eq!(
            added_definitions("widget_report() {\n  echo hi\n}\n"),
            vec!["widget_report"]
        );
    }

    /// A body-local binding is not a definition: an indented `const`/`let`
    /// would otherwise fire the probe on almost every edit.
    #[test]
    fn added_definitions_ignores_indented_bindings_and_generic_names() {
        assert!(added_definitions("    const limit = 3;\n").is_empty());
        assert!(added_definitions("pub fn run() {}\npub fn new() {}\n").is_empty());
    }

    /// Review round 2, finding 3: prose is not a declaration. A column-0
    /// `pub fn` inside a `/* ... */` block, or a `def` inside a Python
    /// module docstring, used to be extracted exactly like real code.
    #[test]
    fn added_definitions_skip_block_comments_and_docstrings() {
        assert!(
            added_definitions("/*\npub fn parse_widget() {}\n*/\n").is_empty(),
            "a Rust block comment declares nothing"
        );
        assert!(
            added_definitions("\"\"\"\ndef compute_widget(x):\n\"\"\"\n").is_empty(),
            "nor does a Python module docstring"
        );
        assert!(
            added_definitions("'''\ndef compute_widget(x):\n'''\n").is_empty(),
            "in either spelling"
        );
        assert_eq!(
            added_definitions("/*\npub fn parse_widget() {}\n*/\npub fn render_widget() {}\n"),
            vec!["render_widget".to_string()],
            "and the real definition after the block still counts"
        );
        assert_eq!(
            added_definitions("/* opened and closed */\npub fn render_widget() {}\n"),
            vec!["render_widget".to_string()],
            "a delimiter pair on one line leaves nothing open"
        );
        assert_eq!(
            added_definitions("/// see the `/* ... */` above\npub fn render_widget() {}\n"),
            vec!["render_widget".to_string()],
            "and a line comment mentioning one opens nothing"
        );
    }

    /// Issue #435, item 2: a `/*`-shaped substring living inside a string
    /// literal or after an unquoted `//` is not a real block opener, so it
    /// must never leave the rest of the file skipped.
    #[test]
    fn added_definitions_ignores_delimiters_inside_strings_and_comments() {
        assert_eq!(
            added_definitions("let example = \"see /* usage\";\npub fn render_widget() {}\n"),
            vec!["render_widget".to_string()],
            "`/*` inside a string literal does not open a block"
        );
        assert_eq!(
            added_definitions("foo(); // see /* usage\npub fn render_widget() {}\n"),
            vec!["render_widget".to_string()],
            "an inline `// ... /*` does not open a block"
        );
        assert_eq!(
            added_definitions("let x = \"a \\\" /* b\";\npub fn render_widget() {}\n"),
            vec!["render_widget".to_string()],
            "an escaped quote does not end the string early and expose the `/*`"
        );
        assert!(
            added_definitions("pub fn lifetime_example<'a>(x: &'a str) {}\n")
                .contains(&"lifetime_example".to_string()),
            "a Rust lifetime tick is not mistaken for an unterminated string"
        );
        // A real block comment, and a real triple-quoted docstring, must
        // still behave exactly as before this fix.
        assert!(
            added_definitions("/*\npub fn parse_widget() {}\n*/\n").is_empty(),
            "a real block comment still opens"
        );
        assert!(
            added_definitions("\"\"\"\ndef compute_widget(x):\n\"\"\"\n").is_empty(),
            "a real docstring still opens"
        );
    }

    #[test]
    fn variants_equate_the_documented_spellings() {
        let intersects = |a: &str, b: &str| {
            let left: BTreeSet<String> = variants(a).into_iter().collect();
            let right: BTreeSet<String> = variants(b).into_iter().collect();
            !left.is_disjoint(&right)
        };
        assert!(intersects("fooBar", "foo_bar"));
        assert!(intersects("foo-bar", "foo_bar"));
        assert!(intersects("parse_thing2", "parse_thing"));
        assert!(intersects("parse_thing_v2", "parse_thing"));
        assert!(intersects("ParseThingImpl", "parse_thing"));
        assert!(intersects("helpers", "helper"));
        assert!(!intersects("parse_thing", "render_thing"));
    }

    fn probe_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join("src")).expect("src dir");
        std::fs::write(
            repo.path().join("src/x.rs"),
            "// header\npub fn foo_bar() -> u8 {\n    0\n}\n",
        )
        .expect("write x.rs");
        repo
    }

    #[test]
    fn probe_finds_an_existing_definition() {
        let repo = probe_repo();
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/other.rs"),
            &["fooBar".to_string()],
            &[],
            Budget::default(),
        );
        let ProbeOutcome::Matches(matches) = outcome else {
            panic!("expected matches, got {outcome:?}");
        };
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].kind, "fn");
        assert_eq!(matches[0].name, "foo_bar");
        assert_eq!(matches[0].path, "src/x.rs");
        assert_eq!(matches[0].line, 2);
    }

    /// Review round 2, finding 3, repository side: the same prose that
    /// declares nothing on the payload side is not evidence that a
    /// definition already exists here either.
    #[test]
    fn probe_ignores_definitions_inside_block_comments_and_docstrings() {
        let repo = tempfile::tempdir().expect("tempdir");
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).expect("src dir");
        std::fs::write(
            src.join("commented.rs"),
            "/*\npub fn widget_total() -> u8 {\n    0\n}\n*/\n",
        )
        .expect("write commented.rs");
        std::fs::write(
            src.join("docstring.py"),
            "\"\"\"\ndef widget_total():\n    pass\n\"\"\"\n",
        )
        .expect("write docstring.py");

        let wanted = ["widget_total".to_string()];
        let target = src.join("new.rs");
        assert_eq!(
            probe(repo.path(), &target, &wanted, &[], Budget::default()),
            ProbeOutcome::Matches(Vec::new()),
            "neither the comment nor the docstring is a definition"
        );

        std::fs::write(
            src.join("real.rs"),
            "pub fn widget_total() -> u8 {\n    0\n}\n",
        )
        .expect("write real.rs");
        let outcome = probe(repo.path(), &target, &wanted, &[], Budget::default());
        let ProbeOutcome::Matches(matches) = outcome else {
            panic!("expected matches, got {outcome:?}");
        };
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].path, "src/real.rs");
        assert_eq!(matches[0].kind, "fn");
    }

    /// The file being written is never its own evidence.
    #[test]
    fn probe_excludes_the_target_file() {
        let repo = probe_repo();
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/x.rs"),
            &["foo_bar".to_string()],
            &[],
            Budget::default(),
        );
        assert_eq!(outcome, ProbeOutcome::Matches(Vec::new()));
    }

    #[test]
    fn probe_honours_an_excluded_prefix() {
        let repo = probe_repo();
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/other.rs"),
            &["foo_bar".to_string()],
            &["src".to_string()],
            Budget::default(),
        );
        assert_eq!(outcome, ProbeOutcome::Matches(Vec::new()));
    }

    #[test]
    fn probe_skips_when_the_byte_budget_is_exhausted() {
        let repo = probe_repo();
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/other.rs"),
            &["foo_bar".to_string()],
            &[],
            Budget {
                max_bytes: 0,
                ..Budget::default()
            },
        );
        let ProbeOutcome::Skipped(reason) = outcome else {
            panic!("expected a skip, got {outcome:?}");
        };
        assert!(reason.contains("byte budget"), "got {reason}");
    }

    /// Non-UTF-8 bytes and an unreadable path are both skipped: a probe that
    /// cannot read a file says nothing about it rather than failing.
    #[test]
    fn probe_skips_a_non_utf8_file() {
        let repo = probe_repo();
        std::fs::write(repo.path().join("src/bad.rs"), [0xff, 0xfe, 0xfd]).expect("write bad.rs");
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/other.rs"),
            &["foo_bar".to_string()],
            &[],
            Budget::default(),
        );
        let ProbeOutcome::Matches(matches) = outcome else {
            panic!("expected matches, got {outcome:?}");
        };
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].path, "src/x.rs");
    }

    /// Issue #406 review: this probe compares bare names with no notion of
    /// scope, so an indented definition -- a method inside an `impl`, where
    /// `fn parse_widget` means "on this type", not "in this crate" -- is not
    /// a definition on either side of the comparison.
    #[test]
    fn an_indented_method_is_not_a_definition_on_either_side() {
        assert!(
            added_definitions("impl Widget {\n    fn parse_widget() {}\n    fn new() {}\n}\n")
                .is_empty(),
            "an impl block declares nothing this probe can compare by name"
        );

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join("src")).expect("src dir");
        std::fs::write(
            repo.path().join("src/x.rs"),
            "impl Widget {\n    fn parse_widget() {}\n}\n",
        )
        .expect("write x.rs");
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/y.rs"),
            &["parse_widget".to_string()],
            &[],
            Budget::default(),
        );
        assert_eq!(outcome, ProbeOutcome::Matches(Vec::new()));
    }

    /// Issue #406 review: a file above [`MAX_FILE_BYTES`] is skipped and
    /// charged NOTHING -- were it charged, the 1 MiB blob sorted ahead of
    /// `src/x.rs` here would exhaust the budget and the real match behind it
    /// would never be reached.
    #[test]
    fn probe_skips_an_oversized_file_without_charging_the_budget() {
        let repo = probe_repo();
        std::fs::write(
            repo.path().join("src/big.rs"),
            "a".repeat(usize::try_from(MAX_FILE_BYTES).expect("cap fits") + 1),
        )
        .expect("write big.rs");
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/other.rs"),
            &["foo_bar".to_string()],
            &[],
            Budget {
                max_bytes: 4096,
                ..Budget::default()
            },
        );
        let ProbeOutcome::Matches(matches) = outcome else {
            panic!("the oversized file must not end the scan, got {outcome:?}");
        };
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].path, "src/x.rs");
    }

    /// Issue #406 review: the walk is lazy, so a spent deadline stops it
    /// mid-tree -- it never collects the file list first and then discovers
    /// it had no time to scan it.
    #[test]
    fn probe_stops_walking_a_deep_tree_once_the_deadline_is_spent() {
        let repo = probe_repo();
        let mut dir = repo.path().join("src");
        for level in 0..6 {
            dir = dir.join(format!("level{level}"));
            std::fs::create_dir_all(&dir).expect("nested dir");
            std::fs::write(dir.join("m.rs"), "pub fn foo_bar() {}\n").expect("write m.rs");
        }
        let outcome = probe(
            repo.path(),
            &repo.path().join("src/other.rs"),
            &["foo_bar".to_string()],
            &[],
            Budget {
                deadline: Duration::ZERO,
                ..Budget::default()
            },
        );
        let ProbeOutcome::Skipped(reason) = outcome else {
            panic!("a spent deadline must skip, got {outcome:?}");
        };
        assert!(reason.contains("deadline"), "got {reason}");
    }

    /// Issue #406 review: an oversized payload is skipped whole rather than
    /// truncated -- a partial candidate list would under-report silently.
    #[test]
    fn evaluate_skips_an_oversized_payload() {
        let repo = probe_repo();
        let payload = PreToolPayload {
            tool_name: "Write".to_string(),
            tool_input: PreToolInput {
                content: "x".repeat(MAX_PAYLOAD_BYTES + 1),
                ..PreToolInput::default()
            },
            ..PreToolPayload::default()
        };
        assert_eq!(
            evaluate(repo.path(), &repo.path().join("src/y.rs"), &payload, &[]),
            Outcome::Skipped("payload too large".to_string())
        );
    }

    #[test]
    fn candidate_names_subtract_what_an_edit_already_declared() {
        let input = PreToolInput {
            old_string: "pub fn foo_bar() {}\n".to_string(),
            new_string: "pub fn foo_bar() {}\npub fn baz_quux() {}\n".to_string(),
            ..PreToolInput::default()
        };
        assert_eq!(
            candidate_names("Edit", Path::new("src/x.rs"), &input),
            vec!["baz_quux"]
        );
    }

    /// A `Write` of a file that does not exist yet also proposes its own
    /// stem: a new `src/widget_report.rs` is a claim that no
    /// `widget_report` exists.
    #[test]
    fn candidate_names_add_the_stem_of_a_new_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("widget_report.rs");
        let input = PreToolInput {
            content: "// nothing declared\n".to_string(),
            ..PreToolInput::default()
        };
        assert_eq!(
            candidate_names("Write", &target, &input),
            vec!["widget_report"]
        );
    }
}
