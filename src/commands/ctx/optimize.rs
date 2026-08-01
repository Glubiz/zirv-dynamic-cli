// Consumed by the `optimize` verb wired up in Task F4; nothing calls this yet
// outside tests, so dead_code is silenced module-wide until then, matching
// the scaffolding pattern config.rs/state.rs/log.rs/event.rs/handoff.rs used
// (see 4ff2410, later dropped once its caller landed in the same way).
#![allow(dead_code)]

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

    /// Whether cloning the repository is enough to change this layer. Findings
    /// about repo-owned layers are the ones a reviewer should read first.
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
    if text.len() <= max_bytes {
        return Some(text);
    }
    // Truncating on a char boundary keeps the excerpt valid UTF-8.
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_string())
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

/// A unified diff that deletes one line, in the form `git apply` accepts.
fn deletion_diff(surface: &Surface, line: usize) -> Option<String> {
    let lines: Vec<&str> = surface.text.lines().collect();
    let index = line.checked_sub(1)?;
    let removed = lines.get(index)?;
    let display = surface.path.display().to_string();
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
        "--- a{display}\n+++ b{display}\n@@ -{start},{old_count} +{start},{new_count} @@\n{body}"
    ))
}

/// Rules stated more than once, whether across layers or inside one file. The
/// first occurrence is treated as the home of the rule and every later copy is
/// what the proposed diff removes.
pub fn lint_redundancy(surfaces: &[Surface]) -> Vec<Finding> {
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
            proposed_diff: deletion_diff(&surfaces[duplicate.surface], duplicate.line),
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

/// Instructions naming files that are gone, and hooks naming programs that are
/// not installed. `on_path` is injected so the test does not depend on what
/// happens to be installed on the machine running it.
pub fn lint_dead_references(
    surfaces: &[Surface],
    repo: &Path,
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
                if on_path(program) {
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
                if repo.join(token).exists() {
                    continue;
                }
                findings.push(Finding {
                    kind: "dead-reference",
                    severity: Severity::Warning,
                    title: format!("Instruction names a path that does not exist: {token}"),
                    evidence: vec![evidence_ref(surface, index + 1)],
                    detail: format!(
                        "`{token}` was not found under {}. Either the file moved or the \
                         instruction outlived it.",
                        repo.display()
                    ),
                    proposed_diff: None,
                });
            }
        }
    }

    findings
}

use super::adapters::claude;
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
/// not show on their own. Routine entries (`advise`, `pace-wait`, `report`,
/// `forward`) are deliberately absent.
pub const FRICTION_ACTIONS: &[&str] = &[
    "rot-kill",
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

fn ranked(counts: hashbrown::HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    // Count descending, then text, so the report never reorders between runs.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

pub fn evidence_from_transcripts(paths: &[PathBuf], cfg: &ScoreConfig) -> Evidence {
    let mut evidence = Evidence::default();
    let mut errors: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    let mut corrections: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    let mut results = 0usize;
    let mut failures = 0usize;

    for path in paths {
        let Ok(jsonl) = std::fs::read_to_string(path) else {
            continue;
        };
        evidence.sessions_sampled += 1;

        let events = claude::parse_events(&jsonl);
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
        let context = claude::structural_context(&jsonl, usize::MAX);
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
    evidence
}

/// What zirv itself had to do, counted from the decision log. The log is the
/// second evidence source the spec names: it records interventions that never
/// appear in a transcript as a failure.
pub fn supervisor_events(state: &StateDir, lines: usize) -> Vec<(String, usize)> {
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
        *counts.entry(action.to_string()).or_insert(0) += 1;
    }
    ranked(counts)
}

/// Both evidence sources in one place: transcripts for what happened inside
/// sessions, the decision log for what zirv had to do about it.
pub fn collect_evidence(
    paths: &[PathBuf],
    state: Option<&StateDir>,
    cfg: &ScoreConfig,
    log_lines: usize,
) -> Evidence {
    let mut evidence = evidence_from_transcripts(paths, cfg);
    if let Some(state) = state {
        evidence.supervisor_events = supervisor_events(state, log_lines);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let findings = lint_redundancy(&surfaces);
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
        let diff = lint_redundancy(&surfaces)[0]
            .proposed_diff
            .clone()
            .expect("diff");

        assert!(diff.contains("--- a/repo/CLAUDE.md"), "got {diff}");
        assert!(diff.contains("+++ b/repo/CLAUDE.md"), "got {diff}");
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

    #[test]
    fn a_rule_repeated_within_one_file_is_redundant_too() {
        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- run fmt before pushing\n- something else\n- Run fmt before pushing!\n",
        )];
        let findings = lint_redundancy(&surfaces);
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
        assert!(lint_redundancy(&surfaces).is_empty());
    }

    #[test]
    fn findings_are_ordered_deterministically() {
        let surfaces = vec![surface_of(
            Layer::RepoClaudeMd,
            "/repo/CLAUDE.md",
            "- zebra rule\n- alpha rule\n- zebra rule\n- alpha rule\n",
        )];
        let first = lint_redundancy(&surfaces);
        for _ in 0..10 {
            assert_eq!(
                lint_redundancy(&surfaces),
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
        let findings = lint_dead_references(&surfaces, tmp.path(), &|_| true);

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
            lint_dead_references(&surfaces, tmp.path(), &|_| true).is_empty(),
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
        let findings =
            lint_dead_references(&surfaces, tmp.path(), &|program| program != "nosuchbin");

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
        assert!(lint_dead_references(&surfaces, tmp.path(), &|_| true).is_empty());
    }

    #[test]
    fn malformed_settings_json_does_not_panic_the_linter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let surfaces = vec![surface_of(
            Layer::ProjectSettings,
            "/repo/.claude/settings.json",
            "{ not json at all",
        )];
        assert!(lint_dead_references(&surfaces, tmp.path(), &|_| true).is_empty());
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

        let evidence = evidence_from_transcripts(&paths, &ScoreConfig::default());

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
        let evidence = evidence_from_transcripts(&[], &ScoreConfig::default());
        assert_eq!(evidence, Evidence::default());
        assert_eq!(evidence.tool_failure_rate, 0.0);
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

        let events = supervisor_events(&state, 200);

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

    #[test]
    fn a_missing_or_empty_decision_log_contributes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("absent"));
        assert!(supervisor_events(&state, 200).is_empty());
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

        let evidence = collect_evidence(&paths, Some(&state), &ScoreConfig::default(), 200);
        assert_eq!(evidence.sessions_sampled, 1, "transcript side");
        assert_eq!(
            evidence.supervisor_events,
            vec![("rot-kill".to_string(), 1)],
            "decision-log side"
        );

        let without_log = collect_evidence(&paths, None, &ScoreConfig::default(), 200);
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
}
