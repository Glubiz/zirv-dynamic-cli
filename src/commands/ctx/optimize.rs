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
}
