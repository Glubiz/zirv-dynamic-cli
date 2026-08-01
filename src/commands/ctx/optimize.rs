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
}
