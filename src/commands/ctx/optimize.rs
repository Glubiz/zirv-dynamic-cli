use std::path::{Path, PathBuf};

/// Deep enough for a workspace's crate directories, shallow enough that a
/// monorepo scan stays instant.
pub const MAX_NESTED_DEPTH: usize = 4;
pub const MAX_SURFACES: usize = 40;

const SKIP_DIRS: &[&str] = &["target", "node_modules", "vendor", "dist", "build"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    GlobalClaudeMd,
    RepoClaudeMd,
    NestedClaudeMd,
    UserSettings,
    ProjectSettings,
    LocalSettings,
}

impl Layer {
    pub fn label(&self) -> &'static str {
        match self {
            Layer::GlobalClaudeMd => "global CLAUDE.md",
            Layer::RepoClaudeMd => "repo CLAUDE.md",
            Layer::NestedClaudeMd => "nested CLAUDE.md",
            Layer::UserSettings => "user settings.json",
            Layer::ProjectSettings => "project settings.json",
            Layer::LocalSettings => "local settings.json",
        }
    }

    /// Whether this layer is a JSON settings file rather than prose. Their
    /// values are secrets often enough -- an `env` block with an API key is
    /// ordinary -- that they are never sent to a model verbatim.
    pub fn is_settings(&self) -> bool {
        matches!(
            self,
            Layer::UserSettings | Layer::ProjectSettings | Layer::LocalSettings
        )
    }

    /// Whether cloning the repository is enough to change this layer. Decides
    /// whether a proposed diff can use a repo-relative `a/`/`b/` header (see
    /// `diff_headers`) and whether a backticked path token can be resolved
    /// against the repo root (see `resolve_candidates`): a layer this returns
    /// false for has no fixed repo to be relative to.
    pub fn is_repo_owned(&self) -> bool {
        matches!(
            self,
            Layer::RepoClaudeMd
                | Layer::NestedClaudeMd
                | Layer::ProjectSettings
                | Layer::LocalSettings
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    pub layer: Layer,
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub surface: usize,
    /// 1-based, so evidence reads as `path:line` the way an editor jumps to it.
    pub line: usize,
    pub text: String,
    pub normalized: String,
}

fn read_capped(path: &Path, max_bytes: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(crate::utils::truncate_bytes(text, Some(max_bytes)))
}

fn push_surface(into: &mut Vec<Surface>, layer: Layer, path: PathBuf, max_bytes: usize) {
    if into.len() >= MAX_SURFACES {
        return;
    }
    if let Some(text) = read_capped(&path, max_bytes) {
        into.push(Surface { layer, path, text });
    }
}

fn nested_claude_files(repo: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(repo.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if depth >= MAX_NESTED_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `is_dir` follows symlinks, so a link in the checkout would walk
            // this out of the repository entirely -- and everything found that
            // way is read into the report and shipped to the model. Whatever a
            // link points at is not this repository's configuration.
            if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            let candidate = path.join("CLAUDE.md");
            if candidate.is_file() {
                found.push(candidate);
            }
            stack.push((path, depth + 1));
        }
    }

    // Sorted so two runs over the same tree report in the same order.
    found.sort();
    found
}

/// Every configuration surface that steers a session in this repo, in a fixed
/// order. Missing files are simply absent: most repos have only some of these.
pub fn collect_surfaces(home: Option<&Path>, repo: &Path, max_bytes: usize) -> Vec<Surface> {
    let mut surfaces = Vec::new();

    if let Some(home) = home {
        push_surface(
            &mut surfaces,
            Layer::GlobalClaudeMd,
            home.join("CLAUDE.md"),
            max_bytes,
        );
        push_surface(
            &mut surfaces,
            Layer::GlobalClaudeMd,
            home.join(".claude").join("CLAUDE.md"),
            max_bytes,
        );
    }

    push_surface(
        &mut surfaces,
        Layer::RepoClaudeMd,
        repo.join("CLAUDE.md"),
        max_bytes,
    );
    for path in nested_claude_files(repo) {
        push_surface(&mut surfaces, Layer::NestedClaudeMd, path, max_bytes);
    }

    if let Some(home) = home {
        push_surface(
            &mut surfaces,
            Layer::UserSettings,
            home.join(".claude").join("settings.json"),
            max_bytes,
        );
    }
    push_surface(
        &mut surfaces,
        Layer::ProjectSettings,
        repo.join(".claude").join("settings.json"),
        max_bytes,
    );
    push_surface(
        &mut surfaces,
        Layer::LocalSettings,
        repo.join(".claude").join("settings.local.json"),
        max_bytes,
    );

    surfaces
}

fn strip_bullet(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        return Some(trimmed[digits.len() + 2..].trim());
    }
    None
}

/// Comparable form of an instruction: formatting, punctuation and case carry no
/// meaning when asking whether two layers say the same thing.
pub fn normalize(line: &str) -> String {
    let cleaned: String = line
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bullet lines only. Prose paragraphs in a CLAUDE.md are context; the rules
/// that collide between layers are written as lists.
pub fn statements(surface_index: usize, surface: &Surface) -> Vec<Instruction> {
    surface
        .text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let text = strip_bullet(line)?;
            if text.is_empty() {
                return None;
            }
            Some(Instruction {
                surface: surface_index,
                line: index + 1,
                text: text.to_string(),
                normalized: normalize(text),
            })
        })
        .filter(|instruction| !instruction.normalized.is_empty())
        .collect()
}

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// `"redundancy"` and `"dead-reference"` are zirv's own deterministic
    /// lints; `"friction"` is deterministic too but never carries a diff;
    /// `"contradiction"` is the model's judgment pass (`parse_judgment`).
    /// `render_report` keys the git-appliable label off `kind == "redundancy"`
    /// specifically (N3): that is the only kind whose diff comes from zirv's
    /// own `deletion_diff` rather than unverified model output.
    pub kind: &'static str,
    pub severity: Severity,
    pub title: String,
    /// `path:line` entries, or transcript references for evidence-backed items.
    pub evidence: Vec<String>,
    pub detail: String,
    pub proposed_diff: Option<String>,
}

pub fn evidence_ref(surface: &Surface, line: usize) -> String {
    format!("{}:{}", surface.path.display(), line)
}

/// Header paths for a diff against `surface`. A repo-owned surface (its path
/// lives under `repo`) gets the conventional `a/`+`b/` prefixes over its
/// repo-relative path, which is what makes `git apply` from the repo root
/// work. A surface outside the repo (the global CLAUDE.md, user settings) has
/// no repo root to be relative to, so it gets its plain absolute path with no
/// prefix instead: a fabricated `a/`+`b/` pair over an absolute path is not a
/// path `git apply` can ever resolve, and has in practice applied the hunk to
/// the wrong file when a same-named path happened to exist under `-p3`.
fn diff_headers(surface: &Surface, repo: &Path) -> (String, String) {
    if surface.layer.is_repo_owned()
        && let Ok(relative) = surface.path.strip_prefix(repo)
    {
        let relative = relative.display();
        return (format!("a/{relative}"), format!("b/{relative}"));
    }
    let display = surface.path.display().to_string();
    (display.clone(), display)
}

/// A unified diff that deletes one line, in the form `git apply` accepts when
/// the surface is repo-owned (see `diff_headers`).
/// Whether a hunk built from `str::lines()` can be byte-exact for this file.
/// `lines()` drops `\r` and cannot express a missing final newline, so for a
/// CRLF file the context lines would not match what is on disk, and for an
/// unterminated last line the hunk would be missing its
/// `\ No newline at end of file` marker. `git apply` rejects both -- after the
/// report has already promised they would apply.
fn diff_can_be_exact(text: &str) -> bool {
    !text.contains('\r') && (text.is_empty() || text.ends_with('\n'))
}

fn deletion_diff(surface: &Surface, line: usize, repo: &Path) -> Option<String> {
    if !diff_can_be_exact(&surface.text) {
        return None;
    }
    let lines: Vec<&str> = surface.text.lines().collect();
    let index = line.checked_sub(1)?;
    let removed = lines.get(index)?;
    let (a_header, b_header) = diff_headers(surface, repo);
    let before = index.checked_sub(1).and_then(|i| lines.get(i));
    let after = lines.get(index + 1);

    let mut body = String::new();
    if let Some(context) = before {
        body.push_str(&format!(" {context}\n"));
    }
    body.push_str(&format!("-{removed}\n"));
    if let Some(context) = after {
        body.push_str(&format!(" {context}\n"));
    }

    let start = if before.is_some() { line - 1 } else { line };
    let old_count = 1 + usize::from(before.is_some()) + usize::from(after.is_some());
    let new_count = old_count - 1;

    Some(format!(
        "--- {a_header}\n+++ {b_header}\n@@ -{start},{old_count} +{start},{new_count} @@\n{body}"
    ))
}

/// Rules stated more than once, whether across layers or inside one file. The
/// first occurrence is treated as the home of the rule and every later copy is
/// what the proposed diff removes.
pub fn lint_redundancy(surfaces: &[Surface], repo: &Path) -> Vec<Finding> {
    let all: Vec<Instruction> = surfaces
        .iter()
        .enumerate()
        .flat_map(|(index, surface)| statements(index, surface))
        .collect();

    // Grouped by normalized text, keyed in first-seen order so the report does
    // not depend on hash iteration order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: hashbrown::HashMap<String, Vec<&Instruction>> = hashbrown::HashMap::new();
    for instruction in &all {
        let bucket = groups.entry(instruction.normalized.clone()).or_default();
        if bucket.is_empty() {
            order.push(instruction.normalized.clone());
        }
        bucket.push(instruction);
    }

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
            .map(|i| evidence_ref(&surfaces[i.surface], i.line))
            .collect();
        let duplicate = group[1];
        let where_stated = if group.iter().any(|i| i.surface != group[0].surface) {
            "in more than one layer"
        } else {
            "more than once in the same file"
        };

        findings.push(Finding {
            kind: "redundancy",
            severity: Severity::Info,
            title: format!("Stated {where_stated}: {}", group[0].text),
            evidence,
            detail: format!(
                "The same instruction appears {} times. Keeping one copy makes the rule easier to change later.",
                group.len()
            ),
            proposed_diff: deletion_diff(&surfaces[duplicate.surface], duplicate.line, repo),
        });
    }

    findings
}

fn looks_like_path(token: &str) -> bool {
    if token.is_empty() || token.len() > 200 {
        return false;
    }
    if token.contains("://") || token.contains('*') || token.contains(' ') {
        return false;
    }
    let has_extension = std::path::Path::new(token)
        .extension()
        .is_some_and(|e| !e.is_empty());
    // `and/or` is prose; `src/main.rs` and `Cargo.toml` are paths.
    has_extension && (token.contains('/') || token.contains('.'))
}

fn backticked(line: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        found.push(&after[..close]);
        rest = &after[close + 1..];
    }
    found
}

fn hook_commands(text: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entries in hooks.values() {
        for entry in entries.as_array().into_iter().flatten() {
            for inner in entry
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if inner.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                if let Some(command) = inner.get("command").and_then(Value::as_str) {
                    found.push(command.to_string());
                }
            }
        }
    }
    found
}

/// Expands a leading `~` or `$HOME` the way a shell would, using the same
/// home resolution the rest of the module uses. A hook command written as
/// `~/.claude/hooks/foo.sh` is a literal path once expanded; left unexpanded
/// it reads as a bare program name that is never on `PATH`, and every such
/// hook was reported as missing regardless of whether it existed.
fn expand_home(program: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return program.to_string();
    };
    let home = home.display().to_string();
    for prefix in ["~/", "$HOME/"] {
        if let Some(rest) = program.strip_prefix(prefix) {
            return format!("{home}/{rest}");
        }
    }
    match program {
        "~" | "$HOME" => home,
        _ => program.to_string(),
    }
}

/// Every path a backticked token in `surface` could plausibly name, scoped to
/// the surface that named it. A repo-owned surface's tokens are relative to
/// the repo, with one exception: a nested CLAUDE.md (N4) is written from its
/// own directory's vantage point, the same false-positive class I4 fixed for
/// the global CLAUDE.md (a relative token is read from where it was written,
/// not necessarily the repo root), so its own directory is tried first, with
/// the repo root kept as a fallback for tokens still written the old,
/// repo-root-relative way. A reference is only dead when every candidate
/// misses (`lint_dead_references` checks all of them before reporting).
///
/// The global CLAUDE.md applies to whatever repo a session happens to run in,
/// so a plain relative token there (e.g. `change-management.yml` in a
/// conditional "if the project has X" instruction) names no fixed path in
/// this repo: only a home-anchored (`~/...`) or absolute token can be
/// resolved from it. Anything else is left unresolved rather than checked
/// against a repo root the instruction was never written about.
fn resolve_candidates(
    surface: &Surface,
    token: &str,
    repo: &Path,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    if surface.layer == Layer::NestedClaudeMd {
        let mut candidates = Vec::new();
        if let Some(dir) = surface.path.parent() {
            candidates.push(dir.join(token));
        }
        candidates.push(repo.join(token));
        return candidates;
    }
    if surface.layer.is_repo_owned() {
        return vec![repo.join(token)];
    }
    if let Some(rest) = token.strip_prefix("~/") {
        return home.map(|home| vec![home.join(rest)]).unwrap_or_default();
    }
    if Path::new(token).is_absolute() {
        return vec![PathBuf::from(token)];
    }
    Vec::new()
}

/// Instructions naming files that are gone, and hooks naming programs that are
/// not installed. `on_path` is injected so the test does not depend on what
/// happens to be installed on the machine running it.
pub fn lint_dead_references(
    surfaces: &[Surface],
    repo: &Path,
    home: Option<&Path>,
    on_path: &dyn Fn(&str) -> bool,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for surface in surfaces {
        if matches!(
            surface.layer,
            Layer::UserSettings | Layer::ProjectSettings | Layer::LocalSettings
        ) {
            for command in hook_commands(&surface.text) {
                let Some(program) = command.split_whitespace().next() else {
                    continue;
                };
                if on_path(&expand_home(program, home)) {
                    continue;
                }
                findings.push(Finding {
                    kind: "dead-reference",
                    severity: Severity::High,
                    title: format!("Hook runs a program that is not installed: {program}"),
                    evidence: vec![surface.path.display().to_string()],
                    detail: format!(
                        "The hook command `{command}` names `{program}`, which is not on PATH. \
                         A hook that cannot start fails on every turn it is meant to run."
                    ),
                    proposed_diff: None,
                });
            }
            continue;
        }

        for (index, line) in surface.text.lines().enumerate() {
            for token in backticked(line) {
                if !looks_like_path(token) {
                    continue;
                }
                let candidates = resolve_candidates(surface, token, repo, home);
                if candidates.is_empty() {
                    continue;
                }
                if candidates.iter().any(|path| path.exists()) {
                    continue;
                }
                let resolved = &candidates[0];
                findings.push(Finding {
                    kind: "dead-reference",
                    severity: Severity::Warning,
                    title: format!("Instruction names a path that does not exist: {token}"),
                    evidence: vec![evidence_ref(surface, index + 1)],
                    detail: format!(
                        "`{token}` was not found at {}. Either the file moved or the \
                         instruction outlived it.",
                        resolved.display()
                    ),
                    proposed_diff: None,
                });
            }
        }
    }

    findings
}

use super::adapters::AgentAdapter;
use super::config::{OptimizeConfig, ScoreConfig};
use super::event::{Capabilities, NormalizedEvent};
use super::log;
use super::rot::{self, Verdict};
use super::state::StateDir;

/// Openings that mark a user turn as a correction rather than a new request.
/// A heuristic shown to a human, never grounds for an automatic edit.
const CORRECTION_OPENERS: &[&str] = &[
    "no,",
    "no.",
    "don't",
    "do not",
    "stop",
    "wrong",
    "that's wrong",
    "actually",
    "i said",
    "not like that",
    "revert",
];

/// Decision-log actions that mean zirv had to intervene. Each one is a session
/// that did not simply run to completion, which is friction the transcripts do
/// not show on their own. `exec` and `loop` name the same event class
/// differently (`exec`'s `kill`/`stand-down` vs `loop`'s `rot-kill`/
/// `timeout-kill`/`nonzero-exit`); both supervisors' actions count. Routine
/// entries (`advise`, `pace-wait`, `report`, `forward`) are deliberately
/// absent.
pub const FRICTION_ACTIONS: &[&str] = &[
    "rot-kill",
    "kill",
    "stand-down",
    "timeout-kill",
    "nonzero-exit",
    "restart",
    "restart-failed",
    "inject",
    "inject-unverified",
    "degrade",
    "give-up",
];

/// The threshold above which supervisor interventions are worth a finding,
/// expressed per sampled session so a long history does not fire on volume.
const INTERVENTIONS_PER_SESSION: f64 = 1.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Evidence {
    pub sessions_sampled: usize,
    pub turns: usize,
    pub tool_failure_rate: f64,
    /// Error snippet to the number of times it recurred, most frequent first.
    pub repeated_errors: Vec<(String, usize)>,
    pub corrections: Vec<(String, usize)>,
    pub rot_sessions: usize,
    /// Decision-log action to count, most frequent first: what zirv had to do.
    pub supervisor_events: Vec<(String, usize)>,
    /// Distinct project directories the sampled transcripts came from (M1):
    /// `newest_transcripts` samples machine-wide across every project with a
    /// recent session, not just this repository, so the report has to say so
    /// rather than let a reader assume every session happened here. Sorted so
    /// two runs over the same disk state report in the same order.
    pub sampled_project_dirs: Vec<String>,
}

pub fn correction_phrase(text: &str) -> Option<&'static str> {
    let lowered = text.trim().to_lowercase();
    CORRECTION_OPENERS
        .iter()
        .find(|opener| {
            // Anchored at the start, and followed by a boundary so "actuality"
            // does not read as "actually".
            lowered.strip_prefix(**opener).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric())
            })
        })
        .copied()
}

/// The most recently modified transcripts under the projects root, including
/// subagent files, newest first.
pub fn newest_transcripts(projects_root: &Path, sample: usize) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut stack = vec![projects_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            found.push((modified, path));
        }
    }

    // Newest first, path as the tiebreak so equal timestamps stay deterministic.
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .take(sample)
        .map(|(_, path)| path)
        .collect()
}

/// The project directory a sampled transcript came from (its slug directory
/// under `projects_root`), for the report's M1 disclosure that sampling is
/// machine-wide. A subagent file sits one level deeper (`<project>/subagents/
/// <file>.jsonl`), so its parent's parent is the project instead.
fn project_dir_of(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let project_dir = if parent.file_name().and_then(|n| n.to_str()) == Some("subagents") {
        parent.parent()?
    } else {
        parent
    };
    project_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
}

fn ranked(counts: hashbrown::HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    // Count descending, then text, so the report never reorders between runs.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// Gathers corrections, tool failures and repeated errors through `adapter`
/// rather than a hardcoded parser, so a transcript from any registered agent
/// (not just claude) is read correctly.
pub fn evidence_from_transcripts(
    paths: &[PathBuf],
    cfg: &ScoreConfig,
    adapter: &dyn AgentAdapter,
) -> Evidence {
    let mut evidence = Evidence::default();
    let mut errors: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    let mut corrections: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    let mut project_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut results = 0usize;
    let mut failures = 0usize;

    for path in paths {
        let Ok(jsonl) = std::fs::read_to_string(path) else {
            continue;
        };
        evidence.sessions_sampled += 1;
        if let Some(dir) = project_dir_of(path) {
            project_dirs.insert(dir);
        }

        let events = adapter.parse_events(&jsonl);
        for event in &events {
            match event {
                NormalizedEvent::TurnStart => evidence.turns += 1,
                NormalizedEvent::ToolResult { is_error } => {
                    results += 1;
                    if *is_error {
                        failures += 1;
                    }
                }
                _ => {}
            }
        }

        // The whole session, not the trailing window: optimize is looking for
        // habits, not for the health of the last ten turns.
        let context = adapter.structural_context(&jsonl, usize::MAX);
        for message in &context.user_messages {
            if let Some(phrase) = correction_phrase(message) {
                *corrections.entry(phrase.to_string()).or_insert(0) += 1;
            }
        }
        for error in &context.tool_errors {
            let snippet: String = error
                .lines()
                .next()
                .unwrap_or(error)
                .chars()
                .take(120)
                .collect();
            *errors.entry(snippet).or_insert(0) += 1;
        }

        let caps = Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: false,
        };
        if rot::score_events(&events, caps, cfg).verdict == Verdict::Restart {
            evidence.rot_sessions += 1;
        }
    }

    if results > 0 {
        evidence.tool_failure_rate = failures as f64 / results as f64;
    }
    evidence.repeated_errors = ranked(errors);
    evidence.corrections = ranked(corrections);
    evidence.sampled_project_dirs = project_dirs.into_iter().collect();
    evidence
}

/// What zirv itself had to do, counted from the decision log. The log is the
/// second evidence source the spec names: it records interventions that never
/// appear in a transcript as a failure.
///
/// `sessions` scopes the count to the sessions actually sampled. The state dir
/// is machine-wide, so counting every entry in it put another repository's
/// interventions over this run's sample size and reported a rate no session
/// here produced.
pub fn supervisor_events(
    state: &StateDir,
    lines: usize,
    sessions: &std::collections::BTreeSet<String>,
) -> Vec<(String, usize)> {
    if sessions.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = log::tail(state, lines) else {
        return Vec::new();
    };

    let mut counts: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    for line in entries {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(action) = entry.get("action").and_then(|a| a.as_str()) else {
            continue;
        };
        if !FRICTION_ACTIONS.contains(&action) {
            continue;
        }
        let in_sample = entry
            .get("session")
            .and_then(|s| s.as_str())
            .is_some_and(|session| sessions.contains(session));
        if !in_sample {
            continue;
        }
        *counts.entry(action.to_string()).or_insert(0) += 1;
    }
    ranked(counts)
}

/// A transcript is named after its session, which is how a decision-log entry
/// is matched back to a sampled session.
fn session_ids_of(paths: &[PathBuf]) -> std::collections::BTreeSet<String> {
    paths
        .iter()
        .filter_map(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().to_string())
        .collect()
}

/// Both evidence sources in one place: transcripts for what happened inside
/// sessions, the decision log for what zirv had to do about it.
pub fn collect_evidence(
    paths: &[PathBuf],
    state: Option<&StateDir>,
    cfg: &ScoreConfig,
    log_lines: usize,
    adapter: &dyn AgentAdapter,
) -> Evidence {
    let mut evidence = evidence_from_transcripts(paths, cfg, adapter);
    if let Some(state) = state {
        evidence.supervisor_events = supervisor_events(state, log_lines, &session_ids_of(paths));
    }
    evidence
}

/// Instruction gaps that the evidence points at. These carry no diff: what to
/// write is a judgment call, and that is the model's job in Task F4.
pub fn friction_findings(evidence: &Evidence, cfg: &OptimizeConfig) -> Vec<Finding> {
    let mut findings = Vec::new();
    let sample = format!("{} sessions sampled", evidence.sessions_sampled);

    if evidence.tool_failure_rate >= cfg.recommend_tool_failure_rate
        && let Some((error, count)) = evidence.repeated_errors.first()
    {
        findings.push(Finding {
            kind: "friction",
            severity: Severity::Warning,
            title: format!(
                "Tools fail on {:.0}% of results across the sample",
                evidence.tool_failure_rate * 100.0
            ),
            evidence: vec![sample.clone()],
            detail: format!(
                "The most repeated failure appeared {count} times: {error}. An instruction that \
                 prevents this class of failure would pay for itself."
            ),
            proposed_diff: None,
        });
    }

    let correction_total: usize = evidence.corrections.iter().map(|(_, count)| count).sum();
    if correction_total >= cfg.recommend_corrections
        && let Some((phrase, count)) = evidence.corrections.first()
    {
        findings.push(Finding {
            kind: "friction",
            severity: Severity::Warning,
            title: format!("{correction_total} user corrections across the sample"),
            evidence: vec![sample.clone()],
            detail: format!(
                "The most common opening was \"{phrase}\" ({count} times). Repeated corrections \
                 usually mean an unwritten expectation that belongs in an instruction file."
            ),
            proposed_diff: None,
        });
    }

    let interventions: usize = evidence
        .supervisor_events
        .iter()
        .map(|(_, count)| count)
        .sum();
    let per_session = if evidence.sessions_sampled == 0 {
        0.0
    } else {
        interventions as f64 / evidence.sessions_sampled as f64
    };
    if per_session > INTERVENTIONS_PER_SESSION {
        let breakdown = evidence
            .supervisor_events
            .iter()
            .map(|(action, count)| format!("{action} x{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        findings.push(Finding {
            kind: "friction",
            severity: Severity::Warning,
            title: format!("zirv intervened {interventions} times across the sample"),
            evidence: vec![format!("{sample}, from the decision log")],
            detail: format!(
                "Interventions: {breakdown}. Sessions that have to be compacted, restarted or \
                 killed usually carry more instruction than they can hold, or repeat work the \
                 instructions never told them to avoid."
            ),
            proposed_diff: None,
        });
    }

    findings
}

// N7: a read-only summary of this repo's memory bank for the report's own
// "Memory bank" section. Deliberately NOT folded into `collect_surfaces` or
// `judgment_prompt`: a memory entry's key or body must never reach the
// judgment model's prompt, so this is read straight from `memory::list` and
// rendered to counts-and-ages text right here, kept entirely separate from
// every surface/evidence path the model call above actually sees.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemorySummary {
    pub count: usize,
    pub total_bytes: usize,
    /// Age in days of the oldest/newest entry by its `Written` stamp. `None`
    /// when the bank is empty.
    pub oldest_written_days: Option<u64>,
    pub newest_written_days: Option<u64>,
    /// Entries whose `Verified` stamp is more than 30 days old.
    pub stale_count: usize,
    /// How many entries share a key with an earlier one in the listing.
    /// `remember` already de-duplicates on write, so this is normally zero;
    /// it stays a defensive check rather than an assumption.
    pub duplicate_keys: usize,
}

const MEMORY_STALE_SECS: u64 = 30 * 86_400;

/// Read-only: only ever calls `memory::list`, never `memory::remember` or
/// any other write path, matching the report-only guarantee the rest of
/// this module already holds itself to.
pub fn memory_bank_summary(state: &StateDir, slug: &str, now: u64) -> MemorySummary {
    let entries = super::memory::list(state, slug).unwrap_or_default();
    let mut summary = MemorySummary {
        count: entries.len(),
        ..MemorySummary::default()
    };
    let mut seen_keys: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (_, entry) in &entries {
        summary.total_bytes += entry.body.len();
        if !seen_keys.insert(entry.key.as_str()) {
            summary.duplicate_keys += 1;
        }
        let written_days = now.saturating_sub(entry.written) / 86_400;
        summary.oldest_written_days = Some(
            summary
                .oldest_written_days
                .map_or(written_days, |d| d.max(written_days)),
        );
        summary.newest_written_days = Some(
            summary
                .newest_written_days
                .map_or(written_days, |d| d.min(written_days)),
        );
        if now.saturating_sub(entry.verified) > MEMORY_STALE_SECS {
            summary.stale_count += 1;
        }
    }
    summary
}

/// Renders the "Memory bank" section: counts and ages only, never a key or a
/// body. Appended to the report text separately from `render_report`
/// (never folded into its findings), so the never-quoted guarantee holds
/// regardless of what `render_report` does with its own inputs.
pub fn render_memory_section(summary: &MemorySummary) -> String {
    if summary.count == 0 {
        return "## Memory bank\n\nEmpty.\n\n".to_string();
    }
    format!(
        "## Memory bank\n\n{count} entries, {bytes} bytes total, oldest {oldest}d, newest \
         {newest}d, {stale} stale (verified over 30d ago), {dupes} duplicate keys.\n\n",
        count = summary.count,
        bytes = summary.total_bytes,
        oldest = summary.oldest_written_days.unwrap_or(0),
        newest = summary.newest_written_days.unwrap_or(0),
        stale = summary.stale_count,
        dupes = summary.duplicate_keys,
    )
}

pub const OPTIMIZE_PROMPT_VERSION: &str = "v1";

/// Every surface contributes at most this many statement lines to the prompt,
/// so one enormous CLAUDE.md cannot crowd out the others.
const DEFAULT_EXCERPT_LINES: usize = 40;

/// Keys and structure survive; every string value becomes `<redacted>`. The
/// contradiction check reads the shape of a settings file -- which hooks are
/// registered, what is permitted -- and never needs the values, which
/// routinely include an `env` block holding an API key. Unparseable input is
/// withheld rather than passed through: a settings file truncated by the byte
/// cap must not fall back to raw bytes.
fn redact_json_strings(text: &str) -> String {
    fn walk(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = "<redacted>".to_string(),
            serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
            serde_json::Value::Object(map) => map.values_mut().for_each(walk),
            _ => {}
        }
    }

    let withheld = "(not valid JSON on its own; contents withheld)".to_string();
    let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return withheld;
    };
    walk(&mut parsed);
    serde_json::to_string_pretty(&parsed).unwrap_or(withheld)
}

pub fn judgment_prompt(surfaces: &[Surface], evidence: &Evidence, excerpt_lines: usize) -> String {
    let mut prompt = format!(
        "You are reviewing the instruction files that steer an AI coding agent in one \
repository ({OPTIMIZE_PROMPT_VERSION}). Find contradictions between or within these layers, \
including cases where a configured hook contradicts a written instruction, and propose concrete \
rewrites.\n\n\
Answer with zero or more blocks in exactly this format and nothing else:\n\n\
### FINDING\n\
kind: contradiction\n\
severity: high | warning | info\n\
title: one line\n\
evidence: path:line, path:line\n\
detail: one paragraph\n\
diff:\n\
```diff\n\
a unified diff that git apply accepts\n\
```\n\n\
The diff is optional. Do not invent files or line numbers that are not shown below. Report \
nothing rather than guessing.\n\n"
    );

    for surface in surfaces {
        prompt.push_str(&format!(
            "## {} ({})\n",
            surface.path.display(),
            surface.layer.label()
        ));
        if surface.layer.is_settings() {
            prompt
                .push_str("(string values redacted; this file's structure is what matters here)\n");
        }
        let body = if surface.layer.is_settings() {
            redact_json_strings(&surface.text)
        } else {
            surface.text.clone()
        };
        for line in body.lines().take(excerpt_lines) {
            prompt.push_str(line);
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    prompt.push_str("## Evidence from recent sessions\n");
    prompt.push_str(&format!(
        "- sessions sampled: {}\n- turns: {}\n- tool failure rate: {:.2}\n- sessions that rotted: {}\n",
        evidence.sessions_sampled, evidence.turns, evidence.tool_failure_rate, evidence.rot_sessions
    ));
    for (error, count) in evidence.repeated_errors.iter().take(5) {
        prompt.push_str(&format!("- repeated error ({count}x): {error}\n"));
    }
    for (phrase, count) in evidence.corrections.iter().take(5) {
        prompt.push_str(&format!(
            "- user correction opening \"{phrase}\" ({count}x)\n"
        ));
    }
    for (action, count) in evidence.supervisor_events.iter().take(5) {
        prompt.push_str(&format!("- supervisor intervention {action} ({count}x)\n"));
    }

    prompt
}

fn severity_from(raw: &str) -> Severity {
    match raw.trim().to_lowercase().as_str() {
        "high" => Severity::High,
        "info" => Severity::Info,
        // Anything unrecognised lands in the middle rather than being dropped
        // or promoted: the model does not get to invent a severity scale.
        _ => Severity::Warning,
    }
}

/// Parses the block format the prompt asks for. A block without a title is
/// discarded: a finding nobody can read is worse than no finding.
pub fn parse_judgment(markdown: &str) -> Vec<Finding> {
    let mut findings = Vec::new();

    for block in markdown.split("### FINDING").skip(1) {
        let mut severity = Severity::Warning;
        let mut title = String::new();
        let mut detail = String::new();
        let mut evidence: Vec<String> = Vec::new();
        let mut diff = String::new();
        let mut in_diff = false;

        for line in block.lines() {
            if in_diff {
                if line.trim_start().starts_with("```") {
                    in_diff = false;
                    continue;
                }
                diff.push_str(line);
                diff.push('\n');
                continue;
            }
            if line.trim_start().starts_with("```") {
                in_diff = true;
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_lowercase().as_str() {
                "severity" => severity = severity_from(value),
                "title" => title = value.to_string(),
                "detail" => detail = value.to_string(),
                "evidence" => {
                    evidence = value
                        .split(',')
                        .map(|item| item.trim().to_string())
                        .filter(|item| !item.is_empty())
                        .collect();
                }
                _ => {}
            }
        }

        if title.is_empty() {
            continue;
        }
        findings.push(Finding {
            kind: "contradiction",
            severity,
            title,
            evidence,
            detail,
            proposed_diff: if diff.trim().is_empty() {
                None
            } else {
                Some(diff)
            },
        });
    }

    findings
}

/// M1: `newest_transcripts` samples machine-wide across every project with a
/// recent session, not just this repository, and the report used to say
/// nothing about it. Named here so a reader is not surprised by a finding
/// whose evidence came from a different checkout entirely.
fn sampling_disclosure(evidence: &Evidence) -> String {
    if evidence.sampled_project_dirs.is_empty() {
        return "Sessions are sampled machine-wide across every project with a recent \
                transcript, not just this repository. Supervisor interventions are counted from \
                the decision log, restricted to those same sampled sessions.\n"
            .to_string();
    }
    format!(
        "Sessions are sampled machine-wide across every project with a recent transcript, not \
         just this repository. Projects sampled: {}. Supervisor interventions are counted from \
         the decision log, restricted to those same sampled sessions.\n",
        evidence.sampled_project_dirs.join(", ")
    )
}

pub fn render_report(findings: &[Finding], evidence: &Evidence, model_used: bool) -> String {
    let mut report = String::from("# zirv ctx optimize report\n\n");

    report.push_str(&format!(
        "Analysed {} recent sessions ({} turns, {:.0}% tool failures, {} rotted).\n",
        evidence.sessions_sampled,
        evidence.turns,
        evidence.tool_failure_rate * 100.0,
        evidence.rot_sessions
    ));
    if evidence.sessions_sampled > 0 {
        report.push_str(&sampling_disclosure(evidence));
    }
    if !model_used {
        report.push_str(
            "Deterministic checks only: no model call was made, so contradictions were not \
             reviewed.\n",
        );
    }
    report.push_str(
        "\nThis report changes nothing. Each proposed diff below says whether to apply it \
         with `git apply` or by hand.\n\n",
    );

    if findings.is_empty() {
        report.push_str("No findings.\n");
        return report;
    }

    let mut ordered: Vec<&Finding> = findings.iter().collect();
    // Most severe first, then a stable tiebreak so two runs read the same.
    ordered.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.kind.cmp(b.kind))
            .then_with(|| a.title.cmp(&b.title))
    });

    for finding in ordered {
        report.push_str(&format!(
            "## [{}] {}\n\n{}\n\n",
            finding.severity.as_str(),
            finding.title,
            finding.detail
        ));
        if !finding.evidence.is_empty() {
            report.push_str("Evidence:\n");
            for item in &finding.evidence {
                report.push_str(&format!("- {item}\n"));
            }
            report.push('\n');
        }
        if let Some(diff) = &finding.proposed_diff {
            // N3: the git-appliable label is restricted to zirv's own
            // deterministic diffs (`kind == "redundancy"`, built by
            // `deletion_diff`), whose header `diff_headers` writes and whose
            // shape is therefore trustworthy. A model-produced diff (`kind ==
            // "contradiction"`, from `parse_judgment`) can start with the same
            // `--- a/` header just by following the prompt's instructions, but
            // nothing has verified it actually applies, so it never earns the
            // label, header or not.
            let note = if finding.kind != "redundancy" {
                "Proposed change (model-produced diff, not verified to apply; apply by hand and \
                 review carefully):"
            } else if diff.starts_with("--- a/") {
                "Proposed change (apply with `git apply` from the repository root, or by hand):"
            } else {
                "Proposed change (path is outside the repository; apply by hand):"
            };
            report.push_str(note);
            report.push_str("\n\n```diff\n");
            report.push_str(diff);
            if !diff.ends_with('\n') {
                report.push('\n');
            }
            report.push_str("```\n\n");
        }
    }

    report
}

use std::io::Write;
use std::time::Duration;

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::state::{now_secs, repo_slug};
use super::{CtxResult, adapters, handoff, window};

/// Long enough for a careful answer over a few excerpted files, short enough
/// that a wedged model does not own the terminal.
const JUDGMENT_TIMEOUT: Duration = Duration::from_secs(120);

/// How far back in the decision log to read for supervisor interventions.
const LOG_LINES_SAMPLED: usize = 500;

#[derive(Debug, clap::Args)]
pub struct OptimizeArgs {
    /// Adapter name: claude or codex. Defaults to config, then claude.
    #[arg(long)]
    pub agent: Option<String>,
    /// Skip the judgment model call and report deterministic findings only.
    #[arg(long, default_value_t = false)]
    pub no_model: bool,
    /// How many recent sessions to sample for evidence.
    #[arg(long)]
    pub sessions: Option<usize>,
    /// Write the report here as well as to stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

fn on_path(program: &str) -> bool {
    // An absolute or relative path is checked directly; a bare name is looked
    // up the way a shell would.
    if program.contains('/') {
        return Path::new(program).exists();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return true;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(program).exists())
}

pub fn run_with<W: Write>(
    args: &OptimizeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // A findings run must never fail on a bad config: a malformed ctx.toml or
    // a forbidden key degrades to defaults instead, same spirit as the model
    // call below. The gate is the one exception: it does not fall back to
    // `CtxConfig::default()`'s permissive `AgentGate`, because that would let
    // a malformed *repo* `.settings.toml` silently void an *operator*
    // disable and launch the agent the operator turned off. Falling back to
    // `AgentGate::load_operator_only` keeps the operator's policy in force
    // even when the rest of the config could not be read.
    let cfg = match CtxConfig::load(repo, env) {
        Ok(cfg) => cfg,
        Err(e) => {
            writeln!(
                w,
                "zirv ctx optimize: config load failed, using defaults ({e})"
            )?;
            CtxConfig {
                agents: crate::settings::AgentGate::load_operator_only(env),
                ..CtxConfig::default()
            }
        }
    };
    let home = crate::utils::home_dir().ok();
    let surfaces = collect_surfaces(home.as_deref(), repo, cfg.optimize.max_surface_bytes);

    // Resolved once and reused for both evidence-gathering and the judgment
    // call below, the same adapter `score`/`exec`/`wrap` would use to parse
    // this agent's transcripts: a future codex parser needs no separate
    // wiring in this verb.
    let adapter = adapters::select(args.agent.as_deref().or(cfg.agent.as_deref()), &[], &cfg);

    let sample = args.sessions.unwrap_or(cfg.optimize.sessions_sampled);
    let transcripts = window::projects_root()
        .ok()
        .map(|root| newest_transcripts(&root, sample))
        .unwrap_or_default();
    // Both sources: the transcripts and zirv's own record of what it had to do.
    let state_for_evidence = StateDir::resolve(env).ok();
    let evidence = match &adapter {
        Ok(adapter) => collect_evidence(
            &transcripts,
            state_for_evidence.as_ref(),
            &cfg.score,
            LOG_LINES_SAMPLED,
            adapter.as_ref(),
        ),
        Err(e) => {
            writeln!(
                w,
                "zirv ctx optimize: no adapter available, evidence skipped ({e})"
            )?;
            Evidence::default()
        }
    };

    let mut findings = lint_redundancy(&surfaces, repo);
    findings.extend(lint_dead_references(
        &surfaces,
        repo,
        home.as_deref(),
        &on_path,
    ));
    findings.extend(friction_findings(&evidence, &cfg.optimize));

    // One model call, and only if the caller wants one and an adapter is
    // available. A dead model degrades the report; it never fails the run.
    let mut model_used = false;
    if !args.no_model {
        match &adapter {
            Ok(adapter) => {
                let model = if cfg.optimize.model.is_empty() {
                    cfg.handoff.model.clone()
                } else {
                    cfg.optimize.model.clone()
                };
                let prompt = judgment_prompt(&surfaces, &evidence, DEFAULT_EXCERPT_LINES);
                match handoff::run_model(adapter.as_ref(), &model, &prompt, JUDGMENT_TIMEOUT) {
                    Ok(answer) => {
                        findings.extend(parse_judgment(&answer));
                        model_used = true;
                    }
                    Err(e) => {
                        writeln!(w, "zirv ctx optimize: judgment pass skipped ({e})")?;
                    }
                }
            }
            Err(e) => {
                writeln!(w, "zirv ctx optimize: judgment pass skipped ({e})")?;
            }
        }
    }

    let mut report = render_report(&findings, &evidence, model_used);
    // N7: appended, not folded into `render_report`'s own findings -- see
    // `MemorySummary`'s doc comment for why its content stays out of every
    // surface/evidence path the judgment model call above actually sees.
    let memory_slug = repo_slug(repo);
    let memory_summary = state_for_evidence
        .as_ref()
        .map(|state| memory_bank_summary(state, &memory_slug, now_secs()))
        .unwrap_or_default();
    report.push_str(&render_memory_section(&memory_summary));
    write!(w, "{report}")?;

    let stored = store_report(env, repo, &report);
    if let Some(path) = &args.out {
        // M6: an explicit --out path can carry the same transcript excerpts as
        // the state copy, so it gets the same 0600 permissions rather than the
        // world/group-readable default from a bare `std::fs::write`.
        super::state::write_private(path, &report)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }

    if let Ok(state) = StateDir::resolve(env) {
        let detail = stored
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "report not stored".to_string());
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: "optimize",
                verb: "optimize",
                verdict: if model_used {
                    "analysed"
                } else {
                    "deterministic"
                },
                score: findings.len() as u32,
                action: "report",
                detail: &detail,
            },
        );
    }

    Ok(0)
}

/// M4: two runs within the same wall-clock second must not silently overwrite
/// each other's report. `{secs}-report.md` is tried first so the common case
/// keeps its familiar name; a taken name gets a numeric suffix bumped until
/// one is free.
fn unique_report_path(dir: &Path, secs: u64) -> PathBuf {
    let base = dir.join(format!("{secs}-report.md"));
    if !base.exists() {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = dir.join(format!("{secs}-report-{suffix}.md"));
        if !candidate.exists() {
            return candidate;
        }
        suffix += 1;
    }
}

/// Best effort: a report the operator can already read on stdout is not worth
/// failing over a state dir that cannot be written.
fn store_report(env: EnvLookup<'_>, repo: &Path, report: &str) -> Option<PathBuf> {
    let state = StateDir::resolve(env).ok()?;
    let dir = state.optimize_reports().join(repo_slug(repo));
    super::state::create_private_dir_all(&dir).ok()?;
    let path = unique_report_path(&dir, now_secs());
    super::state::write_private(&path, report).ok()?;
    Some(path)
}

pub fn run<W: Write>(args: &OptimizeArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

use super::rot::Score;

pub const RECOMMEND_ACTION: &str = "optimize-recommended";

/// Below this a session is too short to say anything about habits.
const MIN_TURNS_FOR_RECOMMENDATION: usize = 8;

/// Which signal justified an optimize recommendation. The Stop hook's
/// user-facing wording needs this: a session that only corrected the assistant
/// must not be told it "hit tools hard".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecommendReason {
    ToolFailures,
    Corrections,
}

/// Corrections in one transcript, through `adapter` rather than a hardcoded
/// parser. Cheap enough for a hook: one pass over a file the hook has already
/// caused to be read.
pub fn count_corrections(adapter: &dyn AgentAdapter, jsonl: &str) -> usize {
    // Only real user turns count: a tool result is not the user speaking, and
    // an assistant saying "no," is not a correction of itself.
    adapter
        .structural_context(jsonl, usize::MAX)
        .user_messages
        .iter()
        .filter(|message| correction_phrase(message).is_some())
        .count()
}

/// Either signal is enough, both need a mature session. A clean run the user had
/// to steer five times is exactly as interesting as a failing one. Tool
/// failures are checked first: when both signals fired the tools did in fact
/// fail, so blaming them is still accurate.
fn recommend_reason(
    score: &Score,
    corrections: usize,
    cfg: &OptimizeConfig,
) -> Option<RecommendReason> {
    if !cfg.enabled || score.signals.turns < MIN_TURNS_FOR_RECOMMENDATION {
        return None;
    }
    if score.signals.tool_failure_rate >= cfg.recommend_tool_failure_rate {
        return Some(RecommendReason::ToolFailures);
    }
    if corrections >= cfg.recommend_corrections {
        return Some(RecommendReason::Corrections);
    }
    None
}

/// Reads the tail of the decision log rather than keeping separate state: the
/// log is already the record of what zirv decided and when.
pub fn recently_recommended(state: &StateDir, now: u64, cooldown: u64) -> bool {
    let Ok(lines) = log::tail(state, 200) else {
        return false;
    };
    lines.iter().rev().any(|line| {
        if !line.contains(RECOMMEND_ACTION) {
            return false;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        if entry.get("action").and_then(|a| a.as_str()) != Some(RECOMMEND_ACTION) {
            return false;
        }
        let ts = entry.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
        now.saturating_sub(ts) < cooldown
    })
}

/// Queues the recommendation for a human to act on. Returns the signal that
/// justified it when it queued, so the hook can mention it exactly once and
/// word it from the real cause.
/// The gates that cost nothing, so the Stop hook can ask before it pays for a
/// correction count. Counting corrections re-reads and re-parses the whole
/// transcript, which on every turn is O(session) per turn and O(n²) over a
/// session -- exactly what the cached incremental score exists to avoid.
/// Nothing here touches the transcript: the score is already computed and the
/// cooldown is a short read of the decision log.
pub fn recommendation_possible(
    state: &StateDir,
    score: &Score,
    cfg: &OptimizeConfig,
    now: u64,
) -> bool {
    cfg.enabled
        && score.signals.turns >= MIN_TURNS_FOR_RECOMMENDATION
        && !recently_recommended(state, now, cfg.recommend_cooldown_secs)
}

pub fn queue_recommendation(
    state: &StateDir,
    session: &str,
    score: &Score,
    corrections: usize,
    cfg: &OptimizeConfig,
    now: u64,
) -> Option<RecommendReason> {
    let reason = recommend_reason(score, corrections, cfg)?;
    if recently_recommended(state, now, cfg.recommend_cooldown_secs) {
        return None;
    }

    // Name the signal that fired, so the log does not blame the tools for a
    // session where the tools were fine.
    let detail = format!(
        "tool failure rate {:.2}, {corrections} corrections over {} turns; run `zirv ctx optimize`",
        score.signals.tool_failure_rate, score.signals.turns
    );
    log::append(
        state,
        &log::Decision {
            ts: now,
            session,
            verb: "hook",
            verdict: score.verdict.as_str(),
            score: score.score,
            action: RECOMMEND_ACTION,
            detail: &detail,
        },
    )
    .ok()?;
    Some(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude;
    use crate::commands::ctx::event::{SessionId, SessionRef, StructuralContext};
    use clap::Parser;

    /// A fake adapter whose `parse_events`/`structural_context` return fixed,
    /// recognisable data regardless of the jsonl handed to them. Evidence
    /// built through this adapter must reflect that fixed data: a hardcoded
    /// `claude::` call would keep parsing whatever real jsonl is on disk and
    /// never see it, which is the proof that evidence-gathering routes
    /// through the `AgentAdapter` trait (item 1) rather than around it.
    #[derive(Debug)]
    struct SentinelAdapter;

    impl AgentAdapter for SentinelAdapter {
        fn name(&self) -> &'static str {
            "sentinel"
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
            _session: &SessionId,
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
            std::process::Command::new("true")
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
            PathBuf::new()
        }

        fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
            vec![NormalizedEvent::TurnStart; 7]
        }

        fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
            StructuralContext {
                user_messages: vec!["no, sentinel says stop".to_string()],
                tool_errors: vec!["sentinel boom".to_string()],
                ..Default::default()
            }
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn register_turn_signal(
            &self,
            _session: &SessionRef,
            _socket: &Path,
        ) -> super::adapters::TurnSignalSetup {
            super::adapters::TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// A repo tree with every surface kind the collector knows about.
    fn fixture_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");

        std::fs::create_dir_all(home.join(".claude")).expect("mkdir home");
        std::fs::write(home.join("CLAUDE.md"), "# global\n- always run tests\n").expect("write");
        std::fs::write(
            home.join(".claude/CLAUDE.md"),
            "# global claude dir\n- prefer rg over grep\n",
        )
        .expect("write");
        std::fs::write(
            home.join(".claude/settings.json"),
            "{\"hooks\":{\"Stop\":[]}}\n",
        )
        .expect("write");

        std::fs::create_dir_all(repo.join(".claude")).expect("mkdir repo");
        std::fs::create_dir_all(repo.join("crates/inner")).expect("mkdir nested");
        std::fs::create_dir_all(repo.join("target/debug")).expect("mkdir target");
        std::fs::create_dir_all(repo.join("node_modules/pkg")).expect("mkdir node_modules");
        std::fs::write(repo.join("CLAUDE.md"), "# repo\n- always run tests\n").expect("write");
        std::fs::write(
            repo.join("crates/inner/CLAUDE.md"),
            "# nested\n- inner rule\n",
        )
        .expect("write");
        std::fs::write(repo.join("target/debug/CLAUDE.md"), "# build junk\n").expect("write");
        std::fs::write(repo.join("node_modules/pkg/CLAUDE.md"), "# vendor\n").expect("write");
        std::fs::write(repo.join(".claude/settings.json"), "{}\n").expect("write");
        std::fs::write(repo.join(".claude/settings.local.json"), "{}\n").expect("write");

        (tmp, home, repo)
    }

    /// `is_dir` follows symlinks, so a link in the checkout walked the scan
    /// out of the repository -- and everything it found was printed in the
    /// report and embedded in the prompt sent to the agent.
    #[cfg(unix)]
    #[test]
    fn the_nested_scan_does_not_follow_a_symlink_out_of_the_repo() {
        let (tmp, home, repo) = fixture_tree();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("CLAUDE.md"), "# private notes\n").expect("write");
        std::os::unix::fs::symlink(&outside, repo.join("linked")).expect("symlink");

        let surfaces = collect_surfaces(Some(&home), &repo, 1_000_000);

        assert!(
            surfaces.iter().all(|s| !s.text.contains("private notes")),
            "a symlinked directory is not this repository's configuration"
        );
    }

    #[test]
    fn every_layer_is_collected_in_a_stable_order() {
        let (_tmp, home, repo) = fixture_tree();
        let surfaces = collect_surfaces(Some(&home), &repo, 1_000_000);

        let layers: Vec<&'static str> = surfaces.iter().map(|s| s.layer.label()).collect();
        assert_eq!(
            layers,
            vec![
                "global CLAUDE.md",
                "global CLAUDE.md",
                "repo CLAUDE.md",
                "nested CLAUDE.md",
                "user settings.json",
                "project settings.json",
                "local settings.json",
            ],
            "collection order must be deterministic so reports are comparable"
        );

        // Same inputs, same output, every time.
        let again = collect_surfaces(Some(&home), &repo, 1_000_000);
        assert_eq!(surfaces, again);
    }

    #[test]
    fn build_and_vendor_directories_are_skipped() {
        let (_tmp, home, repo) = fixture_tree();
        let surfaces = collect_surfaces(Some(&home), &repo, 1_000_000);
        for surface in &surfaces {
            let path = surface.path.display().to_string();
            assert!(!path.contains("/target/"), "found build output: {path}");
            assert!(
                !path.contains("node_modules"),
                "found vendored file: {path}"
            );
        }
    }

    #[test]
    fn missing_surfaces_are_simply_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = collect_surfaces(None, tmp.path(), 1_000_000);
        assert!(
            surfaces.is_empty(),
            "an empty tree analyses to nothing: {surfaces:?}"
        );
    }

    #[test]
    fn oversized_surfaces_are_truncated_not_dropped() {
        let (_tmp, home, repo) = fixture_tree();
        let big = "- rule\n".repeat(5000);
        std::fs::write(repo.join("CLAUDE.md"), &big).expect("write");

        let surfaces = collect_surfaces(Some(&home), &repo, 100);
        let repo_surface = surfaces
            .iter()
            .find(|s| s.layer == Layer::RepoClaudeMd)
            .expect("repo CLAUDE.md still present");
        assert!(
            repo_surface.text.len() <= 100,
            "truncated to the cap, got {}",
            repo_surface.text.len()
        );
    }

    #[test]
    fn layers_know_whether_a_checkout_owns_them() {
        assert!(!Layer::GlobalClaudeMd.is_repo_owned());
        assert!(!Layer::UserSettings.is_repo_owned());
        assert!(Layer::RepoClaudeMd.is_repo_owned());
        assert!(Layer::NestedClaudeMd.is_repo_owned());
        assert!(Layer::ProjectSettings.is_repo_owned());
        assert!(Layer::LocalSettings.is_repo_owned());
    }

    #[test]
    fn statements_are_bullets_with_their_line_numbers() {
        let surface = Surface {
            layer: Layer::RepoClaudeMd,
            path: PathBuf::from("/repo/CLAUDE.md"),
            text: "# Heading\n\n- first rule\n* second rule\n1. third rule\nplain prose line\n"
                .to_string(),
        };
        let found = statements(3, &surface);
        let texts: Vec<&str> = found.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["first rule", "second rule", "third rule"]);
        assert_eq!(
            found[0].line, 3,
            "line numbers are 1-based for file:line evidence"
        );
        assert_eq!(found[1].line, 4);
        assert_eq!(found[2].line, 5);
        assert!(found.iter().all(|i| i.surface == 3));
    }

    #[test]
    fn normalization_ignores_formatting_and_punctuation() {
        assert_eq!(
            normalize("- **Always** run `cargo test`."),
            normalize("always run cargo test")
        );
        assert_eq!(normalize("Use   rg,  not grep!"), "use rg not grep");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalization_keeps_genuinely_different_rules_apart() {
        assert_ne!(normalize("always run tests"), normalize("never run tests"));
        assert_ne!(normalize("use rg"), normalize("use grep"));
    }

    fn surface_of(layer: Layer, path: &str, text: &str) -> Surface {
        Surface {
            layer,
            path: PathBuf::from(path),
            text: text.to_string(),
        }
    }

    #[test]
    fn the_same_rule_in_two_layers_is_redundant() {
        let surfaces = vec![
            surface_of(
                Layer::GlobalClaudeMd,
                "/home/CLAUDE.md",
                "- Always run tests\n",
            ),
            surface_of(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- **always** run tests.\n",
            ),
        ];
        let findings = lint_redundancy(&surfaces, Path::new("/repo"));
        assert_eq!(findings.len(), 1, "got {findings:?}");

        let finding = &findings[0];
        assert_eq!(finding.kind, "redundancy");
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(
            finding.evidence,
            vec!["/home/CLAUDE.md:1", "/repo/CLAUDE.md:1"],
            "evidence is file:line so the reader can jump straight there"
        );
        assert!(finding.title.to_lowercase().contains("always run tests"));
        assert!(
            finding.proposed_diff.is_some(),
            "a redundancy has an obvious fix"
        );
    }

    #[test]
    fn the_proposed_diff_removes_the_later_copy_only() {
        let surfaces = vec![
            surface_of(
                Layer::GlobalClaudeMd,
                "/home/CLAUDE.md",
                "- Always run tests\n",
            ),
            surface_of(
                Layer::RepoClaudeMd,
                "/repo/CLAUDE.md",
                "- keep me\n- always run tests\n",
            ),
        ];
        let diff = lint_redundancy(&surfaces, Path::new("/repo"))[0]
            .proposed_diff
            .clone()
            .expect("diff");

        assert!(
            diff.starts_with("--- a/CLAUDE.md"),
            "repo-relative, so git apply works from the repo root: {diff}"
        );
        assert!(diff.contains("+++ b/CLAUDE.md"), "got {diff}");
        assert!(
            diff.contains("-- always run tests"),
            "the duplicate line is removed: {diff}"
        );
        assert!(
            !diff.contains("-- keep me"),
            "nothing else may be touched: {diff}"
        );
        assert!(
            !diff.contains("/home/CLAUDE.md"),
            "the first statement of a rule stays where it is: {diff}"
        );
    }

    /// I3: the whole point of a repo-relative header is that `git apply` can
    /// actually place the hunk. Proving this against a fixture path is not
    /// enough (the review found the old code only looked correct because its
    /// fixture paths happened to render that way); this runs the real `git`
    /// binary against a real repo.
    #[test]
    fn a_repo_owned_diff_actually_applies_with_git_apply() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::write(repo.join("CLAUDE.md"), "- keep me\n- always run tests\n")
            .expect("write CLAUDE.md");

        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .status()
            .expect("run git init");
        assert!(
            init.success(),
            "git init must succeed for this test to mean anything"
        );

        let surfaces = vec![
            surface_of(
                Layer::GlobalClaudeMd,
                "/home/CLAUDE.md",
                "- Always run tests\n",
            ),
            surface_of(
                Layer::RepoClaudeMd,
                &repo.join("CLAUDE.md").display().to_string(),
                "- keep me\n- always run tests\n",
            ),
        ];
        let diff = lint_redundancy(&surfaces, repo)[0]
            .proposed_diff
            .clone()
            .expect("diff");

        let patch = tmp.path().join("proposed.patch");
        std::fs::write(&patch, &diff).expect("write patch");

        let check = std::process::Command::new("git")
            .args(["apply", "--check"])
            .arg(&patch)
            .current_dir(repo)
            .output()
            .expect("run git apply --check");
        assert!(
            check.status.success(),
            "git apply --check failed: {}\ndiff was:\n{diff}",
            String::from_utf8_lossy(&check.stderr)
        );
    }

    /// I3: a surface outside the repo has no repo root to be relative to, so
    /// its diff must not claim a fake `a/`+`b/` pair that `git apply` would
    /// either reject or, worse, resolve against the wrong file.
    #[test]
    fn a_surface_outside_the_repo_gets_a_plain_absolute_header() {
        let global = surface_of(Layer::GlobalClaudeMd, "/home/CLAUDE.md", "- keep me\n");
        let (a, b) = diff_headers(&global, Path::new("/repo"));
        assert_eq!(a, "/home/CLAUDE.md", "no a/ prefix outside the repo");
        assert_eq!(b, "/home/CLAUDE.md", "no b/ prefix outside the repo");
    }

    #[test]
    fn a_rule_repeated_within_one_file_is_redundant_too() {
        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- run fmt before pushing\n- something else\n- Run fmt before pushing!\n",
        )];
        let findings = lint_redundancy(&surfaces, Path::new("/repo"));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].evidence,
            vec!["/repo/CLAUDE.md:1", "/repo/CLAUDE.md:3"]
        );
    }

    #[test]
    fn distinct_rules_are_not_redundant() {
        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- run fmt before pushing\n- run clippy before pushing\n",
        )];
        assert!(lint_redundancy(&surfaces, Path::new("/repo")).is_empty());
    }

    #[test]
    fn findings_are_ordered_deterministically() {
        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- zebra rule\n- alpha rule\n- zebra rule\n- alpha rule\n",
        )];
        let first = lint_redundancy(&surfaces, Path::new("/repo"));
        for _ in 0..10 {
            assert_eq!(
                lint_redundancy(&surfaces, Path::new("/repo")),
                first,
                "hash order must not leak out"
            );
        }
        assert_eq!(first.len(), 2);
    }

    #[test]
    fn a_referenced_file_that_does_not_exist_is_a_dead_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("real.rs"), "").expect("write");

        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- see `src/gone.rs` for details\n- and `real.rs` which exists\n",
        )];
        let findings = lint_dead_references(&surfaces, tmp.path(), None, &|_| true);

        assert_eq!(findings.len(), 1, "got {findings:?}");
        assert_eq!(findings[0].kind, "dead-reference");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(
            findings[0].detail.contains("src/gone.rs"),
            "got {:?}",
            findings[0]
        );
        assert_eq!(findings[0].evidence, vec!["/repo/CLAUDE.md:1"]);
    }

    #[test]
    fn urls_globs_and_prose_are_not_treated_as_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- read `https://example.com/docs` first\n\
             - touch `src/**/*.rs` carefully\n\
             - the `and/or` question\n\
             - run `cargo test`\n",
        )];
        assert!(
            lint_dead_references(&surfaces, tmp.path(), None, &|_| true).is_empty(),
            "only concrete paths count"
        );
    }

    #[test]
    fn a_hook_command_that_is_not_installed_is_a_dead_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = vec![surface_of(
            Layer::UserSettings,
            "/home/.claude/settings.json",
            "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"nosuchbin --flag\"}]}]}}",
        )];
        let findings = lint_dead_references(&surfaces, tmp.path(), None, &|program| {
            program != "nosuchbin"
        });

        assert_eq!(findings.len(), 1, "got {findings:?}");
        assert!(
            findings[0].detail.contains("nosuchbin"),
            "got {:?}",
            findings[0]
        );
        assert_eq!(
            findings[0].severity,
            Severity::High,
            "a dead hook fires on every turn"
        );
    }

    #[test]
    fn an_installed_hook_command_is_fine() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = vec![surface_of(
            Layer::UserSettings,
            "/home/.claude/settings.json",
            "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"zirv ctx hook stop\"}]}]}}",
        )];
        assert!(lint_dead_references(&surfaces, tmp.path(), None, &|_| true).is_empty());
    }

    /// I4: `~`-prefixed hook commands (`~/.claude/hooks/foo.sh`) are common.
    /// This exercises the real, production `on_path` (no fake closure), with
    /// a real executable inside a fabricated home, so the fix is proven
    /// against the actual existence check rather than an injected stand-in.
    #[test]
    fn a_tilde_prefixed_hook_command_is_resolved_against_its_real_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("hooks")).expect("mkdir hooks");
        let hook_path = home.join("hooks/lint.sh");
        std::fs::write(&hook_path, "#!/bin/sh\nexit 0\n").expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let surfaces = vec![surface_of(
            Layer::UserSettings,
            "/home/.claude/settings.json",
            "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"~/hooks/lint.sh\"}]}]}}",
        )];

        let findings = lint_dead_references(&surfaces, tmp.path(), Some(&home), &on_path);
        assert!(
            findings.is_empty(),
            "a real, executable ~-prefixed hook must not be reported missing: {findings:?}"
        );
    }

    #[test]
    fn a_tilde_prefixed_hook_command_that_really_is_missing_is_still_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");

        let surfaces = vec![surface_of(
            Layer::UserSettings,
            "/home/.claude/settings.json",
            "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"~/hooks/gone.sh\"}]}]}}",
        )];

        let findings = lint_dead_references(&surfaces, tmp.path(), Some(&home), &on_path);
        assert_eq!(
            findings.len(),
            1,
            "a genuinely missing tilde-prefixed hook is still caught: {findings:?}"
        );
    }

    /// I4: a conditional instruction in the global CLAUDE.md ("if the project
    /// has X") names a file that may or may not exist in whichever repo the
    /// session happens to run in. Checking it against this repo's root
    /// produced a false positive in every repo that lacked the file.
    #[test]
    fn a_relative_token_in_the_global_claude_md_is_not_checked_against_the_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = vec![surface_of(
            Layer::GlobalClaudeMd,
            "/home/CLAUDE.md",
            "- If the project has a `change-management.yml` in the repo root, do X\n",
        )];
        let findings = lint_dead_references(&surfaces, tmp.path(), None, &|_| true);
        assert!(
            findings.is_empty(),
            "a conditional global reference must not be checked against every repo: {findings:?}"
        );
    }

    #[test]
    fn a_home_anchored_token_in_the_global_claude_md_is_still_checked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let surfaces = vec![surface_of(
            Layer::GlobalClaudeMd,
            "/home/CLAUDE.md",
            "- see `~/gone.md` for the full policy\n",
        )];
        let findings = lint_dead_references(&surfaces, tmp.path(), Some(&home), &|_| true);
        assert_eq!(
            findings.len(),
            1,
            "a home-anchored reference is resolvable and genuinely missing: {findings:?}"
        );
    }

    /// N4: a nested CLAUDE.md is written from its own directory's vantage
    /// point. Checking `helper.rs` against the repo root (which does not have
    /// it) produced a false positive before the fix; the file's own directory
    /// (which does have it) must be tried first.
    #[test]
    fn a_nested_claude_md_reference_is_checked_against_its_own_directory_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("crates/inner")).expect("mkdir");
        std::fs::write(repo.join("crates/inner/helper.rs"), "").expect("write");

        let surfaces = vec![surface_of(
            Layer::NestedClaudeMd,
            &repo.join("crates/inner/CLAUDE.md").display().to_string(),
            "- see `helper.rs` for details\n",
        )];
        let findings = lint_dead_references(&surfaces, repo, None, &|_| true);
        assert!(
            findings.is_empty(),
            "a nested CLAUDE.md reference must resolve against its own directory first: {findings:?}"
        );
    }

    /// N4: a token still written the old, repo-root-relative way must keep
    /// resolving, so the repo root stays a fallback rather than being dropped.
    #[test]
    fn a_nested_claude_md_reference_falls_back_to_the_repo_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("crates/inner")).expect("mkdir");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
        std::fs::write(repo.join("src/main.rs"), "").expect("write");

        let surfaces = vec![surface_of(
            Layer::NestedClaudeMd,
            &repo.join("crates/inner/CLAUDE.md").display().to_string(),
            "- see `src/main.rs` for the entry point\n",
        )];
        let findings = lint_dead_references(&surfaces, repo, None, &|_| true);
        assert!(
            findings.is_empty(),
            "a nested CLAUDE.md reference must fall back to the repo root: {findings:?}"
        );
    }

    /// N4: a reference missing from both the nested file's own directory and
    /// the repo root is still genuinely dead.
    #[test]
    fn a_nested_claude_md_reference_missing_from_both_locations_is_still_dead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("crates/inner")).expect("mkdir");

        let surfaces = vec![surface_of(
            Layer::NestedClaudeMd,
            &repo.join("crates/inner/CLAUDE.md").display().to_string(),
            "- see `gone.rs` for details\n",
        )];
        let findings = lint_dead_references(&surfaces, repo, None, &|_| true);
        assert_eq!(findings.len(), 1, "got {findings:?}");
    }

    #[test]
    fn malformed_settings_json_does_not_panic_the_linter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = vec![surface_of(
            Layer::ProjectSettings,
            "/repo/.claude/settings.json",
            "{ not json at all",
        )];
        assert!(lint_dead_references(&surfaces, tmp.path(), None, &|_| true).is_empty());
    }

    /// A transcript with a chosen number of turns, tool errors and user lines.
    fn transcript(turns: usize, errors: bool, user_line: &str, tokens: u64) -> String {
        let mut text = String::new();
        for _ in 0..turns {
            text.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"{user_line}\"}}}}\n"
            ));
            text.push_str(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}],\"usage\":{\"input_tokens\":1}}}\n",
            );
            text.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"content\":\"boom: permission denied\",\"is_error\":{errors}}}]}}}}\n"
            ));
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"[zirv] ok\"}}],\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n"
            ));
        }
        text
    }

    fn write_transcripts(root: &Path, files: &[(&str, String)]) -> Vec<PathBuf> {
        let dir = root.join("-home-testuser-repo");
        std::fs::create_dir_all(dir.join("subagents")).expect("mkdir");
        let mut written = Vec::new();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("write transcript");
            written.push(path);
        }
        written
    }

    #[test]
    fn correction_phrases_are_recognised_at_the_start_of_a_message() {
        assert_eq!(correction_phrase("no, that is wrong"), Some("no,"));
        assert_eq!(correction_phrase("  Don't do that"), Some("don't"));
        assert_eq!(correction_phrase("actually, use rg"), Some("actually"));
        assert_eq!(correction_phrase("I said use rg"), Some("i said"));
        assert_eq!(correction_phrase("stop"), Some("stop"));
    }

    #[test]
    fn ordinary_requests_are_not_corrections() {
        for line in [
            "",
            "please add a test",
            "the stop hook needs work",
            "she said hello in the readme",
            "actuality is a word",
        ] {
            assert_eq!(correction_phrase(line), None, "false positive on {line:?}");
        }
    }

    #[test]
    fn newest_transcripts_are_sampled_including_subagents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let dir = root.join("-home-testuser-repo");
        std::fs::create_dir_all(dir.join("subagents")).expect("mkdir");

        for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
            std::fs::write(dir.join(name), "{}\n").expect("write");
        }
        std::fs::write(dir.join("subagents/s.jsonl"), "{}\n").expect("write");
        std::fs::write(dir.join("notes.txt"), "ignore").expect("write");

        let sampled = newest_transcripts(&root, 10);
        assert_eq!(
            sampled.len(),
            4,
            "three sessions plus one subagent: {sampled:?}"
        );
        assert!(
            sampled
                .iter()
                .all(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl")),
            "only transcripts: {sampled:?}"
        );

        assert_eq!(
            newest_transcripts(&root, 2).len(),
            2,
            "the sample is bounded"
        );
        assert!(newest_transcripts(Path::new("/nonexistent"), 5).is_empty());
    }

    #[test]
    fn evidence_counts_failures_corrections_and_rot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let paths = write_transcripts(
            &root,
            &[
                (
                    "rotten.jsonl",
                    transcript(12, true, "no, do it properly", 170_000),
                ),
                (
                    "healthy.jsonl",
                    transcript(2, false, "please add a test", 1_000),
                ),
            ],
        );

        let evidence = evidence_from_transcripts(
            &paths,
            &ScoreConfig::default(),
            &claude::ClaudeAdapter::new(None),
        );

        assert_eq!(evidence.sessions_sampled, 2);
        assert_eq!(evidence.turns, 14);
        assert!(
            evidence.tool_failure_rate > 0.8,
            "twelve of fourteen turns failed, got {}",
            evidence.tool_failure_rate
        );
        assert_eq!(
            evidence.rot_sessions, 1,
            "only the 170k-token rotted session counts"
        );

        let (phrase, count) = evidence
            .corrections
            .first()
            .cloned()
            .expect("corrections recorded");
        assert_eq!(phrase, "no,");
        assert_eq!(count, 12);

        let (error, count) = evidence
            .repeated_errors
            .first()
            .cloned()
            .expect("repeated errors recorded");
        assert!(error.contains("permission denied"), "got {error}");
        assert_eq!(count, 12);
    }

    #[test]
    fn evidence_from_nothing_is_empty_not_an_error() {
        let evidence = evidence_from_transcripts(
            &[],
            &ScoreConfig::default(),
            &claude::ClaudeAdapter::new(None),
        );
        assert_eq!(evidence, Evidence::default());
        assert_eq!(evidence.tool_failure_rate, 0.0);
    }

    /// Item 1: `evidence_from_transcripts` must read `adapter.parse_events`
    /// and `adapter.structural_context`, not hardcoded `claude::` calls. The
    /// transcript text below parses to nothing under claude's real parser
    /// (no recognisable `type` field), so only a routed call can still
    /// produce the sentinel's fixed turns, corrections and repeated error.
    #[test]
    fn evidence_gathering_routes_through_the_adapter_trait_not_claude_directly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let paths = write_transcripts(&root, &[("s.jsonl", "not claude jsonl at all".to_string())]);

        let evidence = evidence_from_transcripts(&paths, &ScoreConfig::default(), &SentinelAdapter);

        assert_eq!(evidence.sessions_sampled, 1);
        assert_eq!(
            evidence.turns, 7,
            "the sentinel's fixed TurnStart count, not claude's parse of this file"
        );
        assert_eq!(
            evidence
                .corrections
                .first()
                .map(|(phrase, _)| phrase.clone()),
            Some("no,".to_string()),
            "the sentinel's structural_context, not claude's"
        );
        assert!(
            evidence
                .repeated_errors
                .iter()
                .any(|(error, _)| error.contains("sentinel boom")),
            "got {:?}",
            evidence.repeated_errors
        );
    }

    /// M1: the report must be able to say which projects a machine-wide
    /// sample actually came from, including the grandparent of a subagent
    /// file whose transcript sits one directory deeper.
    #[test]
    fn evidence_records_which_project_directories_were_sampled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let project_a = root.join("-home-user-repo-a");
        let project_b = root.join("-home-user-repo-b");
        std::fs::create_dir_all(&project_a).expect("mkdir a");
        std::fs::create_dir_all(project_b.join("subagents")).expect("mkdir b");
        std::fs::write(project_a.join("a.jsonl"), "{}\n").expect("write");
        std::fs::write(project_b.join("subagents/b.jsonl"), "{}\n").expect("write");

        let paths = vec![
            project_a.join("a.jsonl"),
            project_b.join("subagents/b.jsonl"),
        ];
        let evidence = evidence_from_transcripts(
            &paths,
            &ScoreConfig::default(),
            &claude::ClaudeAdapter::new(None),
        );

        assert_eq!(
            evidence.sampled_project_dirs,
            vec![
                "-home-user-repo-a".to_string(),
                "-home-user-repo-b".to_string()
            ],
            "distinct project dirs, sorted, including the subagent's grandparent: {:?}",
            evidence.sampled_project_dirs
        );
    }

    #[test]
    fn the_decision_log_contributes_what_zirv_had_to_do() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        // A synthetic history: rot kills, a forced compaction, a restart, a
        // degradation, and entries that are not friction at all.
        for (index, action) in [
            "rot-kill",
            "rot-kill",
            "inject",
            "restart",
            "degrade",
            "advise",
            "pace-wait",
            "report",
        ]
        .iter()
        .enumerate()
        {
            crate::commands::ctx::log::append(
                &state,
                &crate::commands::ctx::log::Decision {
                    ts: 1_800_000_000 + index as u64,
                    session: "s",
                    verb: "loop",
                    verdict: "n/a",
                    score: 0,
                    action,
                    detail: "",
                },
            )
            .expect("append");
        }

        let events = supervisor_events(&state, 200, &["s".to_string()].into());

        assert_eq!(
            events.first().cloned(),
            Some(("rot-kill".to_string(), 2)),
            "most frequent first, got {events:?}"
        );
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert!(
            names.contains(&"inject"),
            "compactions count as friction: {names:?}"
        );
        assert!(names.contains(&"restart"));
        assert!(names.contains(&"degrade"));
        assert!(
            !names.contains(&"advise")
                && !names.contains(&"pace-wait")
                && !names.contains(&"report"),
            "routine entries are not friction: {names:?}"
        );
    }

    /// The state dir is machine-wide. Counting every intervention in it over
    /// this run's sample size reported a rate the sampled sessions never
    /// produced -- five kills from another repository over `--sessions 1`.
    #[test]
    fn interventions_are_counted_only_for_the_sampled_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        for session in ["mine", "someone-elses"] {
            crate::commands::ctx::log::append(
                &state,
                &crate::commands::ctx::log::Decision {
                    ts: 1_800_000_000,
                    session,
                    verb: "exec",
                    verdict: "rot",
                    score: 0,
                    action: "rot-kill",
                    detail: "",
                },
            )
            .expect("append");
        }

        assert_eq!(
            supervisor_events(&state, 200, &["mine".to_string()].into()),
            vec![("rot-kill".to_string(), 1)],
            "another session's interventions are not this sample's"
        );
        assert!(
            supervisor_events(&state, 200, &Default::default()).is_empty(),
            "no sample means no rate to report"
        );
    }

    /// A settings file's values are routinely credentials -- an `env` block
    /// with an API key is ordinary -- and the whole file was going verbatim to
    /// whatever binary `agent_bin` names.
    #[test]
    fn settings_values_are_redacted_before_reaching_the_model() {
        let surfaces = vec![
            Surface {
                layer: Layer::UserSettings,
                path: PathBuf::from("/home/u/.claude/settings.json"),
                text: "{\"env\":{\"ANTHROPIC_API_KEY\":\"sk-ant-secret\"},\"hooks\":{\"Stop\":[]}}"
                    .to_string(),
            },
            Surface {
                layer: Layer::RepoClaudeMd,
                path: PathBuf::from("/repo/CLAUDE.md"),
                text: "- always run tests".to_string(),
            },
        ];

        let prompt = judgment_prompt(&surfaces, &Evidence::default(), 40);

        assert!(
            !prompt.contains("sk-ant-secret"),
            "a settings value must never reach the model: {prompt}"
        );
        assert!(
            prompt.contains("ANTHROPIC_API_KEY") && prompt.contains("<redacted>"),
            "the structure is what the contradiction check needs: {prompt}"
        );
        assert!(
            prompt.contains("- always run tests"),
            "prose surfaces are still sent verbatim: {prompt}"
        );
    }

    /// `str::lines()` drops `\r` and cannot express a missing final newline,
    /// so the hunk would not match the file -- yet it still earned the "apply
    /// with `git apply`" label.
    #[test]
    fn a_diff_is_only_offered_when_it_can_be_byte_exact() {
        let surface = |text: &str| Surface {
            layer: Layer::RepoClaudeMd,
            path: PathBuf::from("/repo/CLAUDE.md"),
            text: text.to_string(),
        };
        let repo = Path::new("/repo");

        assert!(
            deletion_diff(&surface("one\ntwo\nthree\n"), 2, repo).is_some(),
            "an ordinary LF file still gets one"
        );
        assert!(
            deletion_diff(&surface("one\r\ntwo\r\nthree\r\n"), 2, repo).is_none(),
            "a CRLF file's context lines would not match"
        );
        assert!(
            deletion_diff(&surface("one\ntwo\nthree"), 2, repo).is_none(),
            "an unterminated last line needs a marker this cannot emit"
        );
    }

    #[test]
    fn a_missing_or_empty_decision_log_contributes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("absent"));
        assert!(supervisor_events(&state, 200, &["s".to_string()].into()).is_empty());
    }

    #[test]
    fn collect_evidence_joins_both_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let paths = write_transcripts(
            &root,
            &[(
                "s.jsonl",
                transcript(12, true, "no, do it properly", 170_000),
            )],
        );

        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        crate::commands::ctx::log::append(
            &state,
            &crate::commands::ctx::log::Decision {
                ts: 1_800_000_000,
                session: "s",
                verb: "loop",
                verdict: "n/a",
                score: 0,
                action: "rot-kill",
                detail: "",
            },
        )
        .expect("append");

        let claude_adapter = claude::ClaudeAdapter::new(None);
        let evidence = collect_evidence(
            &paths,
            Some(&state),
            &ScoreConfig::default(),
            200,
            &claude_adapter,
        );
        assert_eq!(evidence.sessions_sampled, 1, "transcript side");
        assert_eq!(
            evidence.supervisor_events,
            vec![("rot-kill".to_string(), 1)],
            "decision-log side"
        );

        let without_log =
            collect_evidence(&paths, None, &ScoreConfig::default(), 200, &claude_adapter);
        assert!(without_log.supervisor_events.is_empty());
        assert_eq!(
            without_log.sessions_sampled, 1,
            "the transcripts still count"
        );
    }

    #[test]
    fn unreadable_transcripts_are_skipped() {
        let evidence = evidence_from_transcripts(
            &[PathBuf::from("/nonexistent/x.jsonl")],
            &ScoreConfig::default(),
            &claude::ClaudeAdapter::new(None),
        );
        assert_eq!(evidence.sessions_sampled, 0);
    }

    #[test]
    fn friction_findings_fire_only_above_the_thresholds() {
        let cfg = OptimizeConfig::default();

        let quiet = Evidence {
            sessions_sampled: 5,
            turns: 40,
            tool_failure_rate: 0.05,
            repeated_errors: vec![("boom".to_string(), 1)],
            corrections: vec![("no,".to_string(), 1)],
            rot_sessions: 0,
            supervisor_events: vec![("rot-kill".to_string(), 1)],
            sampled_project_dirs: Vec::new(),
        };
        assert!(friction_findings(&quiet, &cfg).is_empty());

        let noisy = Evidence {
            sessions_sampled: 5,
            turns: 40,
            tool_failure_rate: 0.5,
            repeated_errors: vec![("boom: permission denied".to_string(), 9)],
            corrections: vec![("no,".to_string(), 7)],
            rot_sessions: 2,
            supervisor_events: vec![("rot-kill".to_string(), 6), ("inject".to_string(), 3)],
            sampled_project_dirs: Vec::new(),
        };
        let findings = friction_findings(&noisy, &cfg);
        assert_eq!(
            findings.len(),
            3,
            "one for failures, one for corrections, one for supervisor interventions: {findings:?}"
        );
        assert!(findings.iter().all(|f| f.kind == "friction"));
        assert!(
            findings.iter().all(|f| f.proposed_diff.is_none()),
            "friction findings describe evidence; the rewrite comes from the model call"
        );
        assert!(
            findings[0].detail.contains("permission denied"),
            "quote the actual error: {:?}",
            findings[0]
        );
        assert!(
            findings
                .iter()
                .any(|f| f.evidence.iter().any(|e| e.contains("sessions"))),
            "evidence names the sample it came from: {findings:?}"
        );

        let supervisor = findings
            .iter()
            .find(|f| f.title.to_lowercase().contains("intervened"))
            .expect("the decision-log finding");
        assert!(supervisor.detail.contains("rot-kill"), "got {supervisor:?}");
        assert!(
            supervisor
                .evidence
                .iter()
                .any(|e| e.contains("decision log")),
            "say where it came from: {supervisor:?}"
        );
    }

    #[test]
    fn a_quiet_decision_log_produces_no_supervisor_finding() {
        let cfg = OptimizeConfig::default();
        let evidence = Evidence {
            sessions_sampled: 5,
            supervisor_events: vec![("inject".to_string(), 2)],
            ..Evidence::default()
        };
        assert!(
            friction_findings(&evidence, &cfg).is_empty(),
            "two compactions across five sessions is ordinary"
        );
    }

    /// I5: exec's rot/timeout kill writes `kill`, not loop's `rot-kill`. Before
    /// the fix, ten rotted-and-restarted sessions landed at exactly 1.0
    /// interventions per session (only `restart` counted) and the strict
    /// `> 1.0` comparison suppressed the finding for exactly the sessions the
    /// check exists to catch.
    #[test]
    fn ten_sessions_each_killed_and_restarted_trigger_the_supervisor_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        for i in 0..10u64 {
            for action in ["kill", "restart"] {
                crate::commands::ctx::log::append(
                    &state,
                    &crate::commands::ctx::log::Decision {
                        ts: 1_800_000_000 + i,
                        session: &format!("s{i}"),
                        verb: "exec",
                        verdict: "rot",
                        score: 0,
                        action,
                        detail: "",
                    },
                )
                .expect("append");
            }
        }

        let sampled = (0..10u64).map(|i| format!("s{i}")).collect();
        let events = supervisor_events(&state, 200, &sampled);
        let kill_count = events
            .iter()
            .find(|(name, _)| name == "kill")
            .map(|(_, count)| *count)
            .unwrap_or(0);
        assert_eq!(
            kill_count, 10,
            "exec's kill action must be counted: {events:?}"
        );

        let evidence = Evidence {
            sessions_sampled: 10,
            supervisor_events: events,
            ..Evidence::default()
        };
        let findings = friction_findings(&evidence, &OptimizeConfig::default());
        assert!(
            findings
                .iter()
                .any(|f| f.title.to_lowercase().contains("intervened")),
            "kill + restart at 2.0 interventions/session must fire: {findings:?}"
        );
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn the_judgment_prompt_is_bounded_and_versioned() {
        let (_tmp, home, repo) = fixture_tree();
        let mut surfaces = collect_surfaces(Some(&home), &repo, 1_000_000);
        surfaces.push(surface_of(
            Layer::RepoClaudeMd,
            "/repo/big.md",
            &(1..=200)
                .map(|i| format!("- rule number {i}\n"))
                .collect::<String>(),
        ));

        let prompt = judgment_prompt(&surfaces, &Evidence::default(), 5);

        assert!(
            prompt.contains(OPTIMIZE_PROMPT_VERSION),
            "version the template"
        );
        assert!(prompt.contains("contradiction"), "name the job: {prompt}");
        assert!(prompt.contains("### FINDING"), "specify the answer format");
        assert!(
            prompt.contains("rule number 5") && !prompt.contains("rule number 6"),
            "each surface is excerpted to the line budget"
        );
        assert!(
            prompt.len() < 60_000,
            "the prompt must stay bounded, got {} bytes",
            prompt.len()
        );
    }

    #[test]
    fn the_judgment_prompt_carries_the_evidence_summary() {
        let evidence = Evidence {
            sessions_sampled: 4,
            turns: 30,
            tool_failure_rate: 0.4,
            repeated_errors: vec![("boom: permission denied".to_string(), 6)],
            corrections: vec![("no,".to_string(), 5)],
            rot_sessions: 1,
            supervisor_events: vec![("rot-kill".to_string(), 3)],
            sampled_project_dirs: Vec::new(),
        };
        let prompt = judgment_prompt(&[], &evidence, 5);
        assert!(prompt.contains("permission denied"), "got {prompt}");
        assert!(prompt.contains('4'), "the sample size is stated: {prompt}");
        assert!(
            prompt.contains("rot-kill"),
            "what zirv had to do is part of the evidence the model sees: {prompt}"
        );
    }

    #[test]
    fn findings_parse_out_of_the_model_answer() {
        let answer = std::process::Command::new("sh")
            .arg(fixture("fake-optimizer.sh"))
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run fake optimizer");
        let findings = parse_judgment(&String::from_utf8_lossy(&answer.stdout));

        assert_eq!(findings.len(), 2, "got {findings:?}");
        assert_eq!(findings[0].kind, "contradiction");
        assert_eq!(findings[0].severity, Severity::High);
        assert!(findings[0].title.contains("Commit message rules"));
        assert_eq!(
            findings[0].evidence,
            vec!["/repo/CLAUDE.md:4", "/home/CLAUDE.md:2"]
        );
        let diff = findings[0].proposed_diff.clone().expect("a diff");
        assert!(
            diff.starts_with("--- a/repo/CLAUDE.md"),
            "fences stripped: {diff}"
        );
        assert!(!diff.contains("```"), "fences stripped: {diff}");
        assert_eq!(findings[1].severity, Severity::Warning);
        assert_eq!(
            findings[1].proposed_diff, None,
            "not every finding has a diff"
        );
    }

    #[test]
    fn a_model_answer_with_no_findings_parses_to_nothing() {
        assert!(parse_judgment("Everything looks fine to me.").is_empty());
        assert!(parse_judgment("").is_empty());
    }

    #[test]
    fn a_finding_missing_its_title_is_dropped_rather_than_half_reported() {
        let answer = "### FINDING\nkind: contradiction\nseverity: high\ndetail: something\n";
        assert!(
            parse_judgment(answer).is_empty(),
            "a finding needs a title to be useful"
        );
    }

    #[test]
    fn an_unknown_severity_falls_back_to_warning() {
        let answer = "### FINDING\nkind: contradiction\nseverity: catastrophic\ntitle: T\n";
        assert_eq!(parse_judgment(answer)[0].severity, Severity::Warning);
    }

    #[test]
    fn the_report_groups_by_severity_and_states_its_basis() {
        let findings = vec![
            Finding {
                kind: "redundancy",
                severity: Severity::Info,
                title: "Stated twice".to_string(),
                evidence: vec!["/repo/CLAUDE.md:1".to_string()],
                detail: "detail one".to_string(),
                proposed_diff: Some("--- a\n+++ b\n".to_string()),
            },
            Finding {
                kind: "dead-reference",
                severity: Severity::High,
                title: "Hook program missing".to_string(),
                evidence: vec!["/home/.claude/settings.json".to_string()],
                detail: "detail two".to_string(),
                proposed_diff: None,
            },
        ];
        let report = render_report(&findings, &Evidence::default(), true);

        let high = report
            .find("Hook program missing")
            .expect("high finding present");
        let info = report.find("Stated twice").expect("info finding present");
        assert!(high < info, "most severe first:\n{report}");

        assert!(
            report.contains("```diff"),
            "diffs are fenced for git apply: {report}"
        );
        assert!(report.contains("/repo/CLAUDE.md:1"), "evidence is shown");
        assert!(!report.contains('\u{2014}'), "no em dashes in the report");
    }

    /// I3: the report must say, per finding, whether the diff is
    /// git-appliable or needs hand-application, not make one blanket claim
    /// that is only sometimes true.
    #[test]
    fn the_report_states_which_diffs_are_git_appliable_and_which_are_not() {
        let findings = vec![
            Finding {
                kind: "redundancy",
                severity: Severity::Info,
                title: "Repo-owned duplicate".to_string(),
                evidence: vec!["/repo/CLAUDE.md:1".to_string()],
                detail: "detail".to_string(),
                proposed_diff: Some("--- a/CLAUDE.md\n+++ b/CLAUDE.md\n".to_string()),
            },
            Finding {
                kind: "redundancy",
                severity: Severity::Info,
                title: "Global duplicate".to_string(),
                evidence: vec!["/home/CLAUDE.md:1".to_string()],
                detail: "detail".to_string(),
                proposed_diff: Some("--- /home/CLAUDE.md\n+++ /home/CLAUDE.md\n".to_string()),
            },
        ];
        let report = render_report(&findings, &Evidence::default(), true);

        assert!(
            report.contains("git apply"),
            "the repo-owned finding names its command: {report}"
        );
        assert!(
            report.contains("apply by hand"),
            "the outside-the-repo finding says to apply by hand: {report}"
        );
        assert!(!report.contains('\u{2014}'), "no em dashes in the report");
    }

    /// N3: a model-produced (`kind: "contradiction"`) diff must never earn the
    /// git-appliable label, even when it happens to carry the same
    /// repo-relative `--- a/` header zirv's own diffs use: the model was
    /// asked to write a diff `git apply` accepts, but nothing has verified
    /// that this one actually does.
    #[test]
    fn model_produced_diffs_are_never_labeled_git_appliable_even_with_a_repo_relative_header() {
        let findings = vec![Finding {
            kind: "contradiction",
            severity: Severity::High,
            title: "Model found a contradiction".to_string(),
            evidence: vec!["/repo/CLAUDE.md:1".to_string()],
            detail: "detail".to_string(),
            proposed_diff: Some("--- a/CLAUDE.md\n+++ b/CLAUDE.md\n".to_string()),
        }];
        let report = render_report(&findings, &Evidence::default(), true);

        assert!(
            !report.contains("apply with `git apply`"),
            "a model-produced diff must never be marked git-appliable, verified or not: {report}"
        );
        assert!(
            report.to_lowercase().contains("not verified to apply"),
            "the report must say this needs manual review: {report}"
        );
    }

    /// M1: sampling is machine-wide, not scoped to this repository, so the
    /// report must disclose it and, when it can, name the projects sampled.
    #[test]
    fn the_report_discloses_that_sessions_may_come_from_other_projects() {
        let evidence = Evidence {
            sessions_sampled: 3,
            sampled_project_dirs: vec![
                "-home-user-repo-a".to_string(),
                "-home-user-repo-b".to_string(),
            ],
            ..Evidence::default()
        };
        let report = render_report(&[], &evidence, true);

        assert!(
            report.to_lowercase().contains("machine-wide"),
            "the report must disclose machine-wide sampling: {report}"
        );
        assert!(
            report.contains("-home-user-repo-a") && report.contains("-home-user-repo-b"),
            "the report names the projects sampled: {report}"
        );
    }

    #[test]
    fn no_sessions_sampled_means_no_sampling_disclosure() {
        let report = render_report(&[], &Evidence::default(), true);
        assert!(
            !report.to_lowercase().contains("machine-wide"),
            "nothing to disclose when nothing was sampled: {report}"
        );
    }

    #[test]
    fn a_clean_report_says_so_and_a_model_free_run_admits_it() {
        let clean = render_report(&[], &Evidence::default(), true);
        assert!(clean.to_lowercase().contains("no findings"), "got {clean}");

        let deterministic = render_report(&[], &Evidence::default(), false);
        assert!(
            deterministic
                .to_lowercase()
                .contains("deterministic checks only"),
            "a --no-model run must not look like a full analysis: {deterministic}"
        );
    }

    fn verb_env(state: &Path, bin: &Path) -> std::collections::HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            ("ZIRV_CTX_AGENT_BIN".to_string(), bin.display().to_string()),
        ]
        .into()
    }

    fn tree_snapshot(root: &Path) -> Vec<(PathBuf, String)> {
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

    #[test]
    fn the_verb_prints_a_report_and_stores_a_copy() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: false,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert_eq!(code, 0, "findings are not failures");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("# zirv ctx optimize report"),
            "got {printed}"
        );
        assert!(
            printed.contains("Commit message rules"),
            "the model findings are included: {printed}"
        );

        let stored: Vec<PathBuf> = std::fs::read_dir(
            state
                .join("optimize")
                .join(crate::commands::ctx::state::repo_slug(&repo)),
        )
        .expect("report dir")
        .flatten()
        .map(|e| e.path())
        .collect();
        assert_eq!(stored.len(), 1, "one report per run: {stored:?}");
        assert!(
            std::fs::read_to_string(&stored[0])
                .expect("read")
                .contains("optimize report")
        );
    }

    /// M4: `unique_report_path` must not let a second run within the same
    /// wall-clock second silently overwrite the first run's report.
    #[test]
    fn unique_report_path_avoids_colliding_with_an_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::write(dir.join("1800000000-report.md"), "first").expect("write");

        let path = unique_report_path(dir, 1_800_000_000);
        assert_eq!(path, dir.join("1800000000-report-2.md"));

        std::fs::write(&path, "second").expect("write");
        let path2 = unique_report_path(dir, 1_800_000_000);
        assert_eq!(path2, dir.join("1800000000-report-3.md"));
    }

    /// M4: two runs against the same repo and state dir, close enough in time
    /// to land on the same `now_secs()`, must both keep their own report.
    #[test]
    fn two_runs_in_the_same_second_both_keep_their_report() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env_fn = |k: &str| env.get(k).cloned();

        let first = store_report(&env_fn, &repo, "first report body").expect("first store");
        let second = store_report(&env_fn, &repo, "second report body").expect("second store");

        assert_ne!(
            first, second,
            "two runs must not collide on the same filename"
        );
        assert_eq!(
            std::fs::read_to_string(&first).expect("read first"),
            "first report body"
        );
        assert_eq!(
            std::fs::read_to_string(&second).expect("read second"),
            "second report body"
        );
    }

    #[test]
    fn the_verb_never_modifies_an_analysed_file() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));
        let before_repo = tree_snapshot(&repo);
        let before_home = tree_snapshot(&home);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: false,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert_eq!(before_repo, tree_snapshot(&repo), "optimize is report-only");
        assert_eq!(
            before_home,
            tree_snapshot(&home),
            "and that includes the global layer"
        );
    }

    #[test]
    fn a_failing_model_still_reports_the_deterministic_findings() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // NEW-1: a guard, so a failing assertion below cannot leave
        // `FAKE_OPTIMIZER_MODE=fail` set for every later test.
        let _mode =
            crate::commands::ctx::testenv::VarGuard::set(&[("FAKE_OPTIMIZER_MODE", Some("fail"))]);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: false,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert_eq!(code, 0, "a dead model is not a failed analysis");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("always run tests"),
            "redundancy still found: {printed}"
        );
        assert!(
            printed.to_lowercase().contains("deterministic checks only"),
            "and the report admits the judgment pass did not happen: {printed}"
        );
    }

    #[test]
    fn no_model_skips_the_call_entirely() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let log = tmp.path().join("prompt.log");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // NEW-1: a guard; the old restore (if any) sat behind assertions.
        let _prompt_log = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_OPTIMIZER_PROMPT_LOG",
            log.to_str(),
        )]);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert!(!log.exists(), "--no-model must not spawn the model at all");
    }

    #[test]
    fn every_run_is_recorded_in_the_decision_log() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"optimize\""), "got {log}");
        assert!(log.contains("\"action\":\"report\""), "got {log}");
    }

    #[test]
    fn an_explicit_out_path_receives_the_report() {
        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let out_path = tmp.path().join("report.md");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: Some(out_path.clone()),
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert!(
            std::fs::read_to_string(&out_path)
                .expect("out file")
                .contains("optimize report")
        );
    }

    /// M6: the `--out` report can carry the same transcript excerpts as the
    /// state copy, so it must be just as private (0600), not the
    /// world/group-readable default a bare `std::fs::write` leaves behind.
    #[cfg(unix)]
    #[test]
    fn the_out_report_has_the_same_private_permissions_as_the_state_copy() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, home, repo) = fixture_tree();
        let state = tmp.path().join("state");
        let out_path = tmp.path().join("out-report.md");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: Some(out_path.clone()),
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        let mode = std::fs::metadata(&out_path)
            .expect("out metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the --out report must not be world/group readable, got {mode:o}"
        );
    }

    #[test]
    fn a_malformed_repo_config_falls_back_to_defaults_instead_of_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir zirv");
        std::fs::write(repo.join(".zirv/ctx.toml"), "this is not [ valid toml").expect("write");

        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert_eq!(code, 0, "a malformed config must not fail the command");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("# zirv ctx optimize report"),
            "the report still renders with defaults: {printed}"
        );
        assert!(
            printed.to_lowercase().contains("config load failed"),
            "the report admits the config could not be read: {printed}"
        );
    }

    /// Review finding 1: a malformed *repo* `.settings.toml` makes
    /// `CtxConfig::load` fail, and the fallback used to be
    /// `CtxConfig::default()` -- whose gate is permissive. That let one bad
    /// byte in the repo's own settings file silently void an *operator*
    /// disable and launch (or, here, parse transcripts through) the agent
    /// the operator turned off. The fallback gate must come from
    /// `AgentGate::load_operator_only` instead, so the operator's disable
    /// still holds even though the repo layer could not be read.
    #[test]
    fn a_malformed_repo_settings_file_does_not_void_an_operator_disable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir zirv");
        std::fs::write(repo.join(".zirv/.settings.toml"), "not [ valid toml").expect("write");

        let state = tmp.path().join("state");
        let env = verb_env(&state, &fixture("fake-optimizer.sh"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");

        assert_eq!(
            code, 0,
            "a malformed settings file must not fail the command"
        );
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("no adapter available"),
            "claude must not have been selected: {printed}"
        );
        assert!(
            printed.contains("claude") && printed.to_lowercase().contains("disabled"),
            "the operator's disable must still be reported: {printed}"
        );
    }

    #[test]
    fn the_verb_parses_with_its_flags() {
        let cli = crate::commands::ctx::CtxCli::try_parse_from([
            "zirv ctx",
            "optimize",
            "--no-model",
            "--sessions",
            "3",
        ])
        .expect("optimize should parse");
        match cli.verb {
            crate::commands::ctx::CtxVerb::Optimize(args) => {
                assert!(args.no_model);
                assert_eq!(args.sessions, Some(3));
            }
            other => panic!("expected Optimize, got {other:?}"),
        }
    }

    use crate::commands::ctx::rot::{Score, Signals, Verdict as RotVerdict};

    fn score_with(tool_failure_rate: f64, turns: usize) -> Score {
        Score {
            score: 50,
            verdict: RotVerdict::Advise,
            signals: Signals {
                turns,
                tool_failure_rate,
                repetition_hits: 0,
                max_repeat: 1,
                marker_miss_rate: None,
            },
            context_tokens: 120_000,
        }
    }

    #[test]
    fn a_failure_heavy_session_earns_a_recommendation() {
        let cfg = OptimizeConfig::default();
        assert!(recommend_reason(&score_with(0.4, 20), 0, &cfg).is_some());
        assert!(
            recommend_reason(&score_with(0.25, 20), 0, &cfg).is_some(),
            "the threshold is inclusive"
        );
    }

    #[test]
    fn a_correction_heavy_session_earns_one_even_with_clean_tools() {
        // The second trigger the spec names: nothing failed, but the user had
        // to steer repeatedly, which is an instruction gap by another route.
        let cfg = OptimizeConfig::default();
        assert!(recommend_reason(&score_with(0.0, 20), 3, &cfg).is_some());
        assert!(
            recommend_reason(&score_with(0.0, 20), 2, &cfg).is_none(),
            "below recommend_corrections, got a recommendation anyway"
        );
    }

    #[test]
    fn a_quiet_or_young_session_earns_nothing() {
        let cfg = OptimizeConfig::default();
        assert!(
            recommend_reason(&score_with(0.05, 20), 0, &cfg).is_none(),
            "few failures"
        );
        assert!(
            recommend_reason(&score_with(0.9, 2), 99, &cfg).is_none(),
            "two turns is not evidence of a habit, however it went"
        );
    }

    #[test]
    fn recommendations_can_be_switched_off() {
        let cfg = OptimizeConfig {
            enabled: false,
            ..OptimizeConfig::default()
        };
        assert!(recommend_reason(&score_with(0.9, 50), 50, &cfg).is_none());
    }

    #[test]
    fn corrections_are_counted_from_the_transcript() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"please add a test"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"no, not like that"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"no, ignore me","is_error":false}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"no, I disagree"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"actually use rg"}]}}"#,
            "\n"
        );
        let adapter = claude::ClaudeAdapter::new(None);
        assert_eq!(
            count_corrections(&adapter, jsonl),
            2,
            "user turns only: tool results and assistant text are not corrections"
        );
        assert_eq!(count_corrections(&adapter, ""), 0);
        assert_eq!(count_corrections(&adapter, "not json"), 0);
    }

    /// Item 1: `count_corrections` must read `adapter.structural_context`, not
    /// a hardcoded `claude::structural_context` call. Proven with jsonl that
    /// claude's real parser would see no user messages in at all: only a
    /// routed call can still find the sentinel's fixed correction.
    #[test]
    fn count_corrections_routes_through_the_adapter_trait_not_claude_directly() {
        assert_eq!(
            count_corrections(&SentinelAdapter, "not claude jsonl at all"),
            1,
            "the sentinel's structural_context supplies one correction regardless of the input"
        );
    }

    /// The Stop hook pays for a correction count only when this says the
    /// answer could matter, because counting corrections re-reads and
    /// re-parses the whole transcript. It has to agree with the gates
    /// `queue_recommendation` applies afterwards, or the hook would either
    /// skip a real recommendation or pay for one that gets discarded.
    #[test]
    fn the_free_gates_agree_with_what_queueing_would_decide() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");
        let cfg = OptimizeConfig::default();
        let now = 1_800_000_000;
        let mature = score_with(0.9, MIN_TURNS_FOR_RECOMMENDATION);

        assert!(
            !recommendation_possible(
                &state,
                &score_with(0.9, MIN_TURNS_FOR_RECOMMENDATION - 1),
                &cfg,
                now
            ),
            "a session too short to judge never costs a transcript re-read"
        );
        assert!(
            !recommendation_possible(
                &state,
                &mature,
                &OptimizeConfig {
                    enabled: false,
                    ..cfg.clone()
                },
                now
            ),
            "a disabled feature never costs a transcript re-read"
        );
        assert!(recommendation_possible(&state, &mature, &cfg, now));

        // Queueing one arms the cooldown, which closes the gate again.
        queue_recommendation(&state, "s", &mature, 0, &cfg, now).expect("queues");
        assert!(
            !recommendation_possible(&state, &mature, &cfg, now),
            "a recommendation still in cooldown never costs a transcript re-read"
        );
    }

    #[test]
    fn the_cooldown_reads_the_decision_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        assert!(
            !recently_recommended(&state, 1_800_000_000, 86_400),
            "an empty log has nothing to cool down from"
        );

        crate::commands::ctx::log::append(
            &state,
            &crate::commands::ctx::log::Decision {
                ts: 1_800_000_000,
                session: "s",
                verb: "hook",
                verdict: "advise",
                score: 50,
                action: RECOMMEND_ACTION,
                detail: "",
            },
        )
        .expect("append");

        assert!(
            recently_recommended(&state, 1_800_000_100, 86_400),
            "still inside the window"
        );
        assert!(
            !recently_recommended(&state, 1_800_000_000 + 86_401, 86_400),
            "the window expired"
        );
    }

    #[test]
    fn other_log_entries_do_not_trip_the_cooldown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");
        crate::commands::ctx::log::append(
            &state,
            &crate::commands::ctx::log::Decision {
                ts: 1_800_000_000,
                session: "s",
                verb: "hook",
                verdict: "advise",
                score: 50,
                action: "advise",
                detail: "",
            },
        )
        .expect("append");
        assert!(!recently_recommended(&state, 1_800_000_100, 86_400));
    }

    #[test]
    fn queueing_writes_one_entry_and_then_respects_its_own_cooldown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");
        let cfg = OptimizeConfig::default();
        let score = score_with(0.5, 20);

        assert!(queue_recommendation(&state, "sess", &score, 0, &cfg, 1_800_000_000).is_some());
        assert!(
            queue_recommendation(&state, "sess", &score, 0, &cfg, 1_800_000_060).is_none(),
            "a second session minutes later must not queue again"
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert_eq!(
            log.lines().filter(|l| l.contains(RECOMMEND_ACTION)).count(),
            1,
            "got {log}"
        );
        assert!(log.contains("\"verb\":\"hook\""), "got {log}");
    }

    #[test]
    fn the_queued_entry_says_which_signal_fired() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        let reason = queue_recommendation(
            &state,
            "sess",
            &score_with(0.0, 20),
            5,
            &OptimizeConfig::default(),
            1_800_000_000,
        );
        assert_eq!(
            reason,
            Some(RecommendReason::Corrections),
            "the returned reason must match the signal that fired"
        );
        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(
            log.contains("5 corrections"),
            "a corrections-driven recommendation must say so, not blame the tools: {log}"
        );
    }

    #[test]
    fn a_tool_failure_heavy_session_reports_the_tool_failure_reason() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");
        let reason = queue_recommendation(
            &state,
            "sess",
            &score_with(0.5, 20),
            0,
            &OptimizeConfig::default(),
            1_800_000_000,
        );
        assert_eq!(reason, Some(RecommendReason::ToolFailures));
    }

    #[test]
    fn a_quiet_session_queues_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");
        assert!(
            queue_recommendation(
                &state,
                "sess",
                &score_with(0.0, 20),
                0,
                &OptimizeConfig::default(),
                1_800_000_000
            )
            .is_none()
        );
    }

    // N7: the report's own memory-bank summary block.

    fn seed_memory_entry(
        state: &StateDir,
        slug: &str,
        key: &str,
        written: u64,
        verified: u64,
        body: &str,
    ) {
        let cfg = CtxConfig::default();
        let entry = crate::commands::ctx::memory::Entry {
            key: key.to_string(),
            written_by: "claude".to_string(),
            written,
            verified,
            source: "explicit".to_string(),
            body: body.to_string(),
        };
        crate::commands::ctx::memory::remember(state, slug, &entry, &cfg).expect("seed memory");
    }

    #[test]
    fn an_empty_bank_summarises_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let summary = memory_bank_summary(&state, "-work-repo", 1_800_000_000);
        assert_eq!(summary, MemorySummary::default());
        assert_eq!(
            render_memory_section(&summary),
            "## Memory bank\n\nEmpty.\n\n"
        );
    }

    #[test]
    fn the_bank_summary_reports_counts_ages_staleness_and_duplicates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let now = 1_800_000_000u64;

        seed_memory_entry(
            &state,
            "-work-repo",
            "fresh-fact",
            now - 2 * 86_400,
            now - 2 * 86_400,
            "a short fact",
        );
        seed_memory_entry(
            &state,
            "-work-repo",
            "stale-fact",
            now - 40 * 86_400,
            now - 40 * 86_400,
            "an older fact",
        );

        let summary = memory_bank_summary(&state, "-work-repo", now);
        assert_eq!(summary.count, 2);
        assert_eq!(summary.stale_count, 1, "only the one verified >30d ago");
        assert_eq!(summary.newest_written_days, Some(2));
        assert_eq!(summary.oldest_written_days, Some(40));
        assert_eq!(summary.duplicate_keys, 0, "remember never duplicates a key");
        assert!(summary.total_bytes > 0);

        let text = render_memory_section(&summary);
        assert!(text.contains("2 entries"), "got {text}");
        assert!(text.contains("1 stale"), "got {text}");
    }

    /// The report must say how big the bank is without ever quoting what is
    /// in it: a memory entry's body is repository-scoped, cross-session
    /// content that has nothing to do with what this report is reviewing.
    #[test]
    fn the_optimize_report_summarises_the_bank_without_quoting_it() {
        let (tmp, home, repo) = fixture_tree();
        let state_root = tmp.path().join("state");
        let state = StateDir::from_root(state_root.clone());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let distinctive_body = "the staging DB creds live in a very particular vault path";
        seed_memory_entry(
            &state,
            &slug,
            "staging-db-creds",
            1_700_000_000,
            1_700_000_000,
            distinctive_body,
        );

        let env = verb_env(&state_root, &fixture("fake-optimizer.sh"));
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = OptimizeArgs {
            agent: Some("claude".to_string()),
            no_model: true,
            sessions: Some(0),
            out: None,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");
        let printed = String::from_utf8(out).expect("utf8");

        assert!(printed.contains("Memory bank"), "got {printed}");
        assert!(printed.contains("1 entries"), "got {printed}");
        assert!(
            !printed.contains(distinctive_body),
            "the body must never be quoted in the report: {printed}"
        );
        assert!(
            !printed.contains("staging-db-creds"),
            "the key must never be quoted either: {printed}"
        );
    }
}
