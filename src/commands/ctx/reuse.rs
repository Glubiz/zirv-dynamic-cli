//! Issue #406, layer 1 of the overcomplication guard: a pre-write REUSE
//! PROBE. Before a `Write`/`Edit`/`MultiEdit` lands, the DEFINITIONS the
//! incoming text adds are extracted and checked against what the repository
//! already defines under the same -- or a closely equivalent -- name; a hit
//! comes back as `additionalContext` naming the existing `path:line`.
//!
//! Advisory only, by construction: nothing here ever denies a write, and a
//! probe that cannot finish inside its own byte/wall-clock budget says
//! nothing at all rather than delaying the tool call. Every failure -- an
//! unreadable file, non-UTF-8 bytes, a directory that will not open -- is
//! skipped, never propagated.
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
/// leading-dot rule in [`collect_sources`] rather than by name.
const SKIPPED_DIRS: [&str; 2] = ["target", "node_modules"];

/// Bytes of source this probe may read before it gives up. 16 MiB covers
/// this repository whole with room to spare; a monorepo that exceeds it gets
/// a logged skip, never a slow tool call.
pub const MAX_SCANNED_BYTES: usize = 16 * 1024 * 1024;

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

/// Names so common that an existing definition says nothing about reuse --
/// every module has its own `run`, `new`, `parse`. Compared in normalized
/// form ([`normalize`]), so `New`/`_new` are covered too.
const GENERIC_NAMES: [&str; 24] = [
    "new", "default", "main", "run", "fmt", "from", "into", "test", "tests", "mod", "build",
    "parse", "get", "set", "init", "drop", "clone", "next", "len", "name", "path", "value", "data",
    "args",
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
            r#"^[ \t]*(?:pub(?:\([^()]*\))?[ \t]+)?(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+"[^"]*"[ \t]+)?(fn|struct|enum|trait|type|mod|class|def|function|interface)[ \t]+([A-Za-z_$][A-Za-z0-9_$-]*)"#,
            None,
        ),
        // `const`/`static` declarations, deliberately UNINDENTED only: an
        // indented `const` is a body-local binding in every language here,
        // and probing for locals would fire on almost every edit.
        (
            r"^(?:pub(?:\([^()]*\))?[ \t]+)?(?:export[ \t]+)?(const|static)[ \t]+(?:mut[ \t]+)?([A-Za-z_$][A-Za-z0-9_$]*)",
            None,
        ),
        // Go, with or without a receiver.
        (
            r"^[ \t]*func[ \t]+(?:\([^()]*\)[ \t]*)?([A-Za-z_][A-Za-z0-9_]*)",
            Some("func"),
        ),
        // POSIX shell `name() {`.
        (
            r"^[ \t]*([A-Za-z_][A-Za-z0-9_]*)[ \t]*\([ \t]*\)[ \t]*\{",
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
/// what keeps a whole-repository scan inside [`PROBE_DEADLINE`].
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
    let trimmed = line.trim_start();
    DECL_STARTS.iter().any(|start| trimmed.starts_with(start)) || trimmed.contains("() {")
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

/// Every `(kind, name)` declared on one line, in pattern order.
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
        if let Some(name) = name {
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
    for line in added_text.lines() {
        for (_, name) in definitions_on_line(line) {
            if is_probeworthy(&name) && seen.insert(name.clone()) {
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

/// Every scannable source file under `repo`, sorted for a deterministic
/// walk. Symlinks (files and directories both), dot-prefixed directories,
/// [`SKIPPED_DIRS`] and excluded prefixes are never descended into or
/// returned; a directory that will not open is skipped, never an error.
fn collect_sources(dir: &Path, repo: &Path, exclude: &[String], out: &mut Vec<PathBuf>) {
    if is_symlink(dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if is_symlink(&path) || is_excluded(repo, &path, exclude) {
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
            collect_sources(&path, repo, exclude, out);
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
        {
            out.push(path);
        }
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
    let started = Instant::now();
    let wanted: Vec<(String, BTreeSet<String>)> = names
        .iter()
        .map(|name| (name.clone(), variants(name).into_iter().collect()))
        .collect();
    if wanted.is_empty() {
        return ProbeOutcome::Matches(Vec::new());
    }

    let mut files = Vec::new();
    collect_sources(repo, repo, exclude, &mut files);

    let mut matches: Vec<Match> = Vec::new();
    let mut reported: BTreeSet<String> = BTreeSet::new();
    let mut scanned = 0usize;
    for file in files {
        if file == target_file {
            continue;
        }
        if started.elapsed() > budget.deadline {
            return ProbeOutcome::Skipped(format!(
                "probe deadline of {} ms exceeded",
                budget.deadline.as_millis()
            ));
        }
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        scanned = scanned.saturating_add(bytes.len());
        if scanned > budget.max_bytes {
            return ProbeOutcome::Skipped(format!(
                "probe byte budget of {} exceeded",
                budget.max_bytes
            ));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let Some(path) = repo_relative(repo, &file) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            for (kind, existing) in definitions_on_line(line) {
                let existing_variants: BTreeSet<String> = variants(&existing).into_iter().collect();
                for (name, candidate_variants) in &wanted {
                    if reported.contains(name) || candidate_variants.is_disjoint(&existing_variants)
                    {
                        continue;
                    }
                    reported.insert(name.clone());
                    matches.push(Match {
                        kind,
                        name: existing.clone(),
                        path: path.clone(),
                        line: number + 1,
                    });
                }
            }
            if matches.len() >= MAX_MATCHES {
                return ProbeOutcome::Matches(matches);
            }
        }
    }
    ProbeOutcome::Matches(matches)
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
/// excluded prefix, and the payload must actually add a definition worth
/// looking for -- otherwise nothing is scanned at all.
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
