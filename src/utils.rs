use std::{
    env, fs,
    path::{Path, PathBuf},
};

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use crate::script_runner::script::Script;

pub const SUPPORTED_EXTENSIONS: &[&str] = &["yaml", "yml", "json", "toml"];
pub const SCRIPT_DIR_NAME: &str = ".zirv";

/// Subdirectory of `SCRIPT_DIR_NAME` that invocable scripts live in as of
/// zirv 3.0 (issue #212). Everything else directly under `.zirv` --
/// `ctx.toml`, `.settings.toml`, `verify.toml`, `.shortcuts.yaml`,
/// `system-prompt.md`, `context/`, `memory/`, `work/`, `agents/` -- is
/// zirv's own configuration/state, never a script, so splitting scripts into
/// their own subdirectory means the `.zirv` root no longer needs a
/// file-by-file carve-out to tell the two apart.
pub const COMMANDS_DIR_NAME: &str = "commands";

/// Top-level command names (and their short aliases) that are handled as
/// built-ins in `main.rs` before any script lookup happens. A script or
/// shortcut sharing one of these names can never be reached.
pub const RESERVED_COMMANDS: &[&str] = &[
    "help", "h", "version", "v", "init", "i", "create", "c", "ctx", "chat", "agent", "memory",
    "context", "setup", "report", "skill", "workflow", "test", "verify", "artifact", "frontend",
    "commands",
];

/// Compared case-insensitively, the same way `is_reserved_zirv_file` compares
/// against `RESERVED_ZIRV_FILES`: NTFS (and APFS by default) resolve a file
/// name case-insensitively, and `main.rs`'s dispatch matches `input.command`
/// as typed, so `zirv Chat`/a script named `Chat.yaml` would otherwise slip
/// past this guard while `zirv chat` (lowercase) is still intercepted as the
/// built-in -- one name, two different answers about whether it is reachable.
pub fn is_reserved_command(name: &str) -> bool {
    RESERVED_COMMANDS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

/// File names inside a `.zirv` directory that are zirv's own configuration
/// rather than an invocable script: the shortcuts map plus `ctx.toml`,
/// `.settings.toml`, and workflow verification config. Shared by script listing
/// (`candidate_names_in_dir`, `help.rs`) and script lookup (`input.rs`'s
/// `find_script_in_dir`), so a name like `.settings` can never resolve to
/// `.settings.toml` by way of the usual `{name}.{ext}` search.
pub const RESERVED_ZIRV_FILES: &[&str] = &[
    ".shortcuts.yaml",
    "ctx.toml",
    ".settings.toml",
    "verify.toml",
];

/// Whether `name` is one of `RESERVED_ZIRV_FILES`, compared the way the
/// filesystem that put it there will resolve it: case-insensitively on
/// platforms (NTFS, APFS by default) where `Path::exists` already is, so a
/// repo file like `.Settings.toml` is caught by the same guard a lowercase
/// `.settings.toml` is. A case-sensitive comparison here would let a
/// differently-cased reserved file be honored by `AgentGate`/`CtxConfig`
/// (which resolve it via `exists()`) while still being listed as an
/// invocable script and resolvable as one -- the filesystem and zirv's own
/// guards disagreeing about what the file even is. This is deliberately
/// stricter than every filesystem requires -- on ext4, a file literally
/// named `CTX.toml` is a different, ordinary file from `ctx.toml`, and this
/// check excludes it anyway -- because the point is one rule that behaves
/// the same on every platform zirv runs on, not the minimum each one demands.
pub fn is_reserved_zirv_file(name: &str) -> bool {
    RESERVED_ZIRV_FILES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
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

/// Truncates to `cap` bytes on a char boundary, so the result stays valid
/// UTF-8. `None` means no cap. Shared by the prompt layers and the optimize
/// surfaces, which both cap what they read from disk the same way.
pub fn truncate_bytes(text: String, cap: Option<usize>) -> String {
    let Some(cap) = cap else {
        return text;
    };
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
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
        // Two names whose lengths differ by more than the threshold cannot be
        // within it -- deleting the difference already costs that much -- and
        // the full matrix is O(n*m). Without this, a mistyped multi-megabyte
        // argv is compared character by character against every script.
        let candidate_lower = candidate.to_lowercase();
        if target_lower
            .chars()
            .count()
            .abs_diff(candidate_lower.chars().count())
            > threshold
        {
            continue;
        }
        let distance = levenshtein(&target_lower, &candidate_lower);
        if distance <= threshold {
            scored.push((distance, candidate.to_string()));
        }
    }

    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(3).map(|(_, name)| name).collect()
}

/// Collects the invocable names available under a `.zirv` root: the file
/// stem of every supported script file in its `commands/` subdirectory,
/// plus every shortcut key from the root's `.shortcuts.yaml` (shortcuts stay
/// at the root -- they are config, not scripts). Used to power "did you
/// mean" suggestions; missing/unreadable directories yield an empty list
/// rather than an error.
pub fn candidate_names_in_dir(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let commands_dir = dir.join(COMMANDS_DIR_NAME);

    if let Ok(entries) = fs::read_dir(&commands_dir) {
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
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_reserved_zirv_file)
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

/// File names (not full paths) of script-like files (`SUPPORTED_EXTENSIONS`,
/// minus `RESERVED_ZIRV_FILES`) sitting directly at a `.zirv` root rather
/// than in its `commands/` subdirectory -- the pre-3.0 layout issue #212
/// replaced. Sorted for deterministic output. Used to build the loud,
/// actionable "scripts moved" error in `input.rs`'s `not_found_error`:
/// missing/unreadable directories yield an empty list rather than an error,
/// the same as `candidate_names_in_dir`.
pub fn script_like_files_at_root(dir: &Path) -> Vec<String> {
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
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_reserved_zirv_file(file_name) {
                continue;
            }
            names.push(file_name.to_string());
        }
    }

    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};
    use tempfile::tempdir;

    /// The matrix is O(n*m) and the threshold is at most 3, so a mistyped
    /// megabyte of argv used to be compared character by character against
    /// every script name. The short-circuit must not change any answer.
    #[test]
    fn a_wildly_long_name_is_dismissed_without_building_the_matrix() {
        let huge = "x".repeat(400_000);
        let started = std::time::Instant::now();
        assert!(suggest_matches(&huge, ["build", "deploy", "test"]).is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "the length check has to come before the matrix"
        );

        // Same answers as before, for names that are actually close.
        assert_eq!(suggest_matches("biuld", ["build", "deploy"]), vec!["build"]);
        assert_eq!(suggest_matches("Build", ["build"]), vec!["build"]);
    }

    #[test]
    fn truncate_bytes_cuts_on_a_char_boundary() {
        assert_eq!(truncate_bytes("hello".to_string(), None), "hello");
        assert_eq!(truncate_bytes("hello".to_string(), Some(99)), "hello");
        assert_eq!(truncate_bytes("hello".to_string(), Some(2)), "he");
        // 'é' is two bytes: a cap landing inside it drops the whole char
        // rather than producing invalid UTF-8.
        assert_eq!(truncate_bytes("é".to_string(), Some(1)), "");
        assert_eq!(truncate_bytes("aé".to_string(), Some(2)), "a");
    }

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
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join("build.yaml"),
            "name: Build\ncommands: []\n",
        )
        .unwrap();
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

    /// A script left at the `.zirv` root (the pre-3.0 layout) must not be
    /// picked up as a candidate any more: only `commands/` is searched.
    #[test]
    fn test_candidate_names_in_dir_ignores_scripts_left_at_the_root() {
        let temp_dir = tempdir().unwrap();
        let zirv_dir = temp_dir.path().join(".zirv");
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("build.yaml"), "name: Build\ncommands: []\n").unwrap();

        let names = candidate_names_in_dir(&zirv_dir);
        assert!(!names.contains(&"build".to_string()));
    }

    #[test]
    fn test_is_reserved_command() {
        assert!(is_reserved_command("help"));
        assert!(is_reserved_command("c"));
        assert!(is_reserved_command("frontend"));
        assert!(is_reserved_command("setup"));
        assert!(!is_reserved_command("build"));
    }

    /// S4: NTFS (and APFS by default) resolve a file name case-insensitively,
    /// so a script `Chat.yaml` is exactly as unreachable as `chat.yaml`
    /// would be. The guard has to agree, the same way `is_reserved_zirv_file`
    /// already does for `RESERVED_ZIRV_FILES`.
    #[test]
    fn is_reserved_command_is_case_insensitive() {
        assert!(is_reserved_command("Help"));
        assert!(is_reserved_command("CHAT"));
        assert!(is_reserved_command("Agent"));
        assert!(is_reserved_command("CtX"));
        assert!(is_reserved_command("FRONTEND"));
        assert!(is_reserved_command("SeTuP"));
        assert!(!is_reserved_command("Build"));
    }

    /// `.settings.toml` is zirv's own configuration file, not a script: it
    /// must not show up as a candidate name (and, via `RESERVED_ZIRV_FILES`,
    /// must not be resolvable as one either -- see the matching guard in
    /// `input.rs`) even if one ends up inside `commands/` alongside real
    /// scripts.
    #[test]
    fn the_settings_file_is_not_an_invocable_script_name() {
        let temp_dir = tempdir().unwrap();
        let zirv_dir = temp_dir.path().join(".zirv");
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join(".settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .unwrap();
        write(
            commands_dir.join("build.yaml"),
            "name: Build\ncommands: []\n",
        )
        .unwrap();

        let names = candidate_names_in_dir(&zirv_dir);
        assert!(!names.contains(&".settings".to_string()));
        assert!(names.contains(&"build".to_string()));
    }

    /// Review finding 2: NTFS (and APFS by default) resolve a file by name
    /// case-insensitively, so `Path::exists` finds `.Settings.toml` when
    /// asked for `.settings.toml`. The reserved-file guard has to agree, or
    /// a differently-cased settings/ctx-config file is honored by
    /// `AgentGate`/`CtxConfig` while still being listed as an invocable
    /// script name.
    #[test]
    fn a_differently_cased_reserved_file_is_still_skipped() {
        let temp_dir = tempdir().unwrap();
        let zirv_dir = temp_dir.path().join(".zirv");
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join(".Settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .unwrap();
        write(commands_dir.join("CTX.toml"), "[score]\nwindow = 4\n").unwrap();
        write(
            commands_dir.join("build.yaml"),
            "name: Build\ncommands: []\n",
        )
        .unwrap();

        let names = candidate_names_in_dir(&zirv_dir);
        assert!(!names.contains(&".Settings".to_string()));
        assert!(!names.contains(&"CTX".to_string()));
        assert!(names.contains(&"build".to_string()));
    }

    /// The migration-error helper (issue #212): script-like files at the
    /// `.zirv` root are named, config files are not, and a missing directory
    /// yields an empty list rather than an error.
    #[test]
    fn test_script_like_files_at_root_excludes_config_and_lists_scripts() {
        let temp_dir = tempdir().unwrap();
        let zirv_dir = temp_dir.path().join(".zirv");
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("claude.yaml"), "name: Claude\ncommands: []\n").unwrap();
        write(zirv_dir.join("commit.yaml"), "name: Commit\ncommands: []\n").unwrap();
        write(zirv_dir.join("ctx.toml"), "[score]\nwindow = 4\n").unwrap();
        write(
            zirv_dir.join(".settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .unwrap();
        write(zirv_dir.join("verify.toml"), "").unwrap();
        write(zirv_dir.join(".shortcuts.yaml"), "shortcuts: {}\n").unwrap();

        let names = script_like_files_at_root(&zirv_dir);
        assert_eq!(
            names,
            vec!["claude.yaml".to_string(), "commit.yaml".to_string()]
        );

        let missing = script_like_files_at_root(&temp_dir.path().join("does-not-exist"));
        assert!(missing.is_empty());
    }

    #[test]
    fn is_reserved_zirv_file_is_case_insensitive() {
        assert!(is_reserved_zirv_file(".settings.toml"));
        assert!(is_reserved_zirv_file(".Settings.toml"));
        assert!(is_reserved_zirv_file(".SETTINGS.TOML"));
        assert!(is_reserved_zirv_file("CTX.toml"));
        assert!(is_reserved_zirv_file("VERIFY.toml"));
        assert!(is_reserved_zirv_file(".Shortcuts.YAML"));
        assert!(!is_reserved_zirv_file("build.yaml"));
    }
}
