use std::{
    env, fs,
    path::{Path, PathBuf},
};

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use crate::script_runner::script::Script;

pub const SUPPORTED_EXTENSIONS: &[&str] = &["yaml", "yml", "json", "toml"];
pub const SCRIPT_DIR_NAME: &str = ".zirv";

/// Top-level command names (and their short aliases) that are handled as
/// built-ins in `main.rs` before any script lookup happens. A script or
/// shortcut sharing one of these names can never be reached.
pub const RESERVED_COMMANDS: &[&str] = &[
    "help", "h", "version", "v", "init", "i", "create", "c", "ctx",
];

pub fn is_reserved_command(name: &str) -> bool {
    RESERVED_COMMANDS.contains(&name)
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct Shortcuts {
    pub shortcuts: HashMap<String, String>,
}

pub fn home_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| "Could not determine home directory".into())
}

pub fn parse_script_content(
    content: &str,
    ext: &str,
) -> Result<Script, Box<dyn std::error::Error>> {
    let script: Script = match ext {
        "yaml" | "yml" => serde_yaml_ng::from_str(content)?,
        "json" => serde_json::from_str(content)?,
        "toml" => toml::from_str(content)?,
        other => return Err(format!("Unsupported extension: {other}").into()),
    };
    Ok(script)
}

pub fn file_to_script(path: &PathBuf) -> Result<Script, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    parse_script_content(&content, &ext)
}

/// Levenshtein (edit) distance between two strings, counted in chars rather
/// than bytes so it stays correct for non-ASCII script/shortcut names.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (len_a, len_b) = (a.len(), b.len());

    if len_a == 0 {
        return len_b;
    }
    if len_b == 0 {
        return len_a;
    }

    let mut prev: Vec<usize> = (0..=len_b).collect();
    let mut curr = vec![0usize; len_b + 1];

    for i in 1..=len_a {
        curr[0] = i;
        for j in 1..=len_b {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[len_b]
}

/// A sane edit-distance cutoff scaled to the length of the mistyped name:
/// short names tolerate fewer typos than long ones.
fn max_suggestion_distance(len: usize) -> usize {
    match len {
        0..=3 => 1,
        4..=5 => 2,
        _ => 3,
    }
}

/// Returns up to 3 candidates closest to `target` (by edit distance), within
/// a sane distance threshold, closest first. Ties break alphabetically for
/// deterministic output. Case-insensitive so `Build` still suggests `build`.
pub fn suggest_matches<'a, I>(target: &str, candidates: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let target_lower = target.to_lowercase();
    let threshold = max_suggestion_distance(target_lower.chars().count());

    let mut seen = std::collections::HashSet::new();
    let mut scored: Vec<(usize, String)> = Vec::new();
    for candidate in candidates {
        if candidate.is_empty() || candidate == target {
            continue;
        }
        if !seen.insert(candidate.to_string()) {
            continue;
        }
        let distance = levenshtein(&target_lower, &candidate.to_lowercase());
        if distance <= threshold {
            scored.push((distance, candidate.to_string()));
        }
    }

    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(3).map(|(_, name)| name).collect()
}

/// Collects the invocable names available in a `.zirv` directory: the file
/// stem of every supported script file, plus every shortcut key. Used to
/// power "did you mean" suggestions; missing/unreadable directories yield an
/// empty list rather than an error.
pub fn candidate_names_in_dir(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if !SUPPORTED_EXTENSIONS.contains(&ext) {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some(".shortcuts.yaml") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str())
                == Some(crate::commands::ctx::config::CTX_CONFIG_FILE)
            {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }

    let shortcuts_path = dir.join(".shortcuts.yaml");
    if let Ok(content) = fs::read_to_string(&shortcuts_path)
        && let Ok(shortcuts) = serde_yaml_ng::from_str::<Shortcuts>(&content)
    {
        names.extend(shortcuts.shortcuts.into_keys());
    }

    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};
    use tempfile::tempdir;

    #[test]
    fn test_levenshtein_identical() {
        assert_eq!(levenshtein("build", "build"), 0);
    }

    #[test]
    fn test_levenshtein_empty_strings() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn test_levenshtein_classic_example() {
        // Textbook example: kitten -> sitting is 3 edits.
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn test_levenshtein_single_typo() {
        assert_eq!(levenshtein("biuld", "build"), 2);
        assert_eq!(levenshtein("buld", "build"), 1);
    }

    #[test]
    fn test_suggest_matches_orders_by_distance() {
        let candidates = ["build", "bundle", "deploy", "test"];
        let suggestions = suggest_matches("biuld", candidates);
        assert_eq!(suggestions.first(), Some(&"build".to_string()));
    }

    #[test]
    fn test_suggest_matches_excludes_far_names() {
        let candidates = ["deploy", "release"];
        let suggestions = suggest_matches("biuld", candidates);
        assert!(
            suggestions.is_empty(),
            "expected no close matches, got {suggestions:?}"
        );
    }

    #[test]
    fn test_suggest_matches_caps_at_three() {
        let candidates = ["test1", "test2", "test3", "test4", "test5"];
        let suggestions = suggest_matches("test", candidates);
        assert_eq!(suggestions.len(), 3);
    }

    #[test]
    fn test_suggest_matches_dedups_and_skips_exact_match() {
        let candidates = ["build", "build", "build"];
        let suggestions = suggest_matches("build", candidates);
        assert!(
            suggestions.is_empty(),
            "an exact match should not be suggested as a typo fix"
        );
    }

    #[test]
    fn test_suggest_matches_is_case_insensitive() {
        let candidates = ["Build"];
        let suggestions = suggest_matches("biuld", candidates);
        assert_eq!(suggestions, vec!["Build".to_string()]);
    }

    #[test]
    fn test_candidate_names_in_dir_includes_scripts_and_shortcuts() {
        let temp_dir = tempdir().unwrap();
        let zirv_dir = temp_dir.path().join(".zirv");
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("build.yaml"), "name: Build\ncommands: []\n").unwrap();
        write(
            zirv_dir.join(".shortcuts.yaml"),
            "shortcuts:\n  b: build.yaml\n",
        )
        .unwrap();

        let names = candidate_names_in_dir(&zirv_dir);
        assert!(names.contains(&"build".to_string()));
        assert!(names.contains(&"b".to_string()));
    }

    #[test]
    fn test_candidate_names_in_dir_ignores_ctx_config_and_missing_dir() {
        let temp_dir = tempdir().unwrap();
        let zirv_dir = temp_dir.path().join(".zirv");
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("ctx.toml"), "[score]\nwindow = 4\n").unwrap();

        let names = candidate_names_in_dir(&zirv_dir);
        assert!(!names.contains(&"ctx".to_string()));

        let missing = candidate_names_in_dir(&temp_dir.path().join("does-not-exist"));
        assert!(missing.is_empty());
    }

    #[test]
    fn test_is_reserved_command() {
        assert!(is_reserved_command("help"));
        assert!(is_reserved_command("c"));
        assert!(!is_reserved_command("build"));
    }
}
