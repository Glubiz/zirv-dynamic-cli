use std::path::{Path, PathBuf};

use clap::Parser;

use crate::utils::{
    SCRIPT_DIR_NAME, SUPPORTED_EXTENSIONS, Shortcuts, candidate_names_in_dir, home_dir,
    suggest_matches,
};

#[derive(Debug, Parser, Default)]
pub struct Input {
    /// A descriptive name for the script.
    pub command: String,
    /// Optional parameters (positional arguments) that will be mapped to the script's expected params.
    #[arg(num_args = 0..)]
    pub params: Vec<String>,
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// (`create` only) Script name; skips the interactive name prompt.
    #[arg(long)]
    pub name: Option<String>,
    /// (`create` only) Shortcut key; skips the interactive shortcut prompt.
    #[arg(long)]
    pub shortcut: Option<String>,
    /// (`create` only) Create in the global `~/.zirv` folder. Bare `--global`
    /// means true; pass `--global false` to skip the prompt with "no".
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub global: Option<bool>,
}

impl Input {
    /// The first `create`-only flag present, for a command that is not
    /// `create`. Clap cannot express "only valid for one command" on a shared
    /// struct, so the check lives here rather than in the parser.
    pub fn misplaced_create_flag(&self) -> Option<&'static str> {
        [
            ("--name", self.name.is_some()),
            ("--shortcut", self.shortcut.is_some()),
            ("--global", self.global.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, present)| present.then_some(flag))
    }
}

fn find_script_in_dir(
    dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    for ext in SUPPORTED_EXTENSIONS {
        let path = dir.join(format!("{name}.{ext}"));
        if path.exists() {
            return Ok(Some(path.canonicalize()?));
        }
    }

    let shortcuts_path = dir.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        let content = std::fs::read_to_string(&shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml_ng::from_str(&content)?;
        if let Some(mapped_file) = shortcuts.shortcuts.get(name) {
            let path = dir.join(mapped_file);
            if path.exists() {
                return Ok(Some(path.canonicalize()?));
            }
            for ext in SUPPORTED_EXTENSIONS {
                let path = dir.join(format!("{mapped_file}.{ext}"));
                if path.exists() {
                    return Ok(Some(path.canonicalize()?));
                }
            }
        }
    }

    Ok(None)
}

impl Input {
    pub fn get_file_path(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let cmd_path = PathBuf::from(&self.command);
        if cmd_path.exists() {
            return Ok(cmd_path.canonicalize()?);
        }

        let local_dir = PathBuf::from(SCRIPT_DIR_NAME);
        if let Some(path) = find_script_in_dir(&local_dir, &self.command)? {
            return Ok(path);
        }

        let global_dir = home_dir()?.join(SCRIPT_DIR_NAME);
        if let Some(path) = find_script_in_dir(&global_dir, &self.command)? {
            return Ok(path);
        }

        Err(not_found_error(&self.command))
    }
}

/// Builds the "no script or shortcut" error, enriched with up to 3 "did you
/// mean" suggestions drawn from local (then global) script names and
/// shortcut keys, plus a pointer to `zirv help`.
fn not_found_error(command: &str) -> Box<dyn std::error::Error> {
    let mut candidates = candidate_names_in_dir(&PathBuf::from(SCRIPT_DIR_NAME));
    if let Ok(home) = home_dir() {
        candidates.extend(candidate_names_in_dir(&home.join(SCRIPT_DIR_NAME)));
    }

    let suggestions = suggest_matches(command, candidates.iter().map(String::as_str));

    let mut message = format!("No script or shortcut found for '{command}'.");
    if !suggestions.is_empty() {
        message.push_str(&format!(" Did you mean: {}?", suggestions.join(", ")));
    }
    message.push_str(" Run `zirv help` to see available scripts and shortcuts.");

    message.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};
    use tempfile::tempdir;

    /// RAII, so a panicking assertion still restores HOME/USERPROFILE and the
    /// working directory. See `commands::ctx::testenv::EnvGuard`.
    fn with_fake_env<F, R>(fake_home: &Path, fake_cwd: &Path, test: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = crate::commands::ctx::testenv::EnvGuard::set(fake_home, Some(fake_cwd));
        test()
    }

    /// These live on the shared `Input` struct, so clap accepts them for every
    /// command and eats the argument after them: `zirv echo --name hello`
    /// handed `hello` to `--name` and then reported the script got no
    /// parameters.
    #[test]
    fn create_only_flags_are_recognised_as_misplaced_elsewhere() {
        let with_name = Input {
            command: "echo".to_string(),
            name: Some("hello".to_string()),
            ..Default::default()
        };
        assert_eq!(with_name.misplaced_create_flag(), Some("--name"));

        let plain = Input {
            command: "echo".to_string(),
            params: vec!["hello".to_string()],
            ..Default::default()
        };
        assert_eq!(plain.misplaced_create_flag(), None);
    }

    #[test]
    fn test_get_file_path_missing_script_suggests_closest_match() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("build.yaml"), "name: Build\ncommands: []\n").unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "biuld".to_string(),
                ..Default::default()
            };
            let err = input.get_file_path().unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("build"),
                "expected a suggestion for 'build', got: {message}"
            );
            assert!(
                message.contains("zirv help"),
                "expected a hint to run `zirv help`, got: {message}"
            );
        });
    }

    #[test]
    fn test_get_file_path_missing_script_no_suggestion_when_nothing_close() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("deploy.yaml"), "name: Deploy\ncommands: []\n").unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "zzzzzzzzzz".to_string(),
                ..Default::default()
            };
            let err = input.get_file_path().unwrap_err();
            let message = err.to_string();
            assert!(!message.contains("Did you mean"), "got: {message}");
            assert!(message.contains("zirv help"), "got: {message}");
        });
    }

    #[test]
    fn test_get_file_path_missing_script_suggests_shortcut_key() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("test.yaml"), "name: Test\ncommands: []\n").unwrap();
        write(
            zirv_dir.join(".shortcuts.yaml"),
            "shortcuts:\n  tst: test.yaml\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "tsst".to_string(),
                ..Default::default()
            };
            let err = input.get_file_path().unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("tst"),
                "expected the shortcut key 'tst' to be suggested, got: {message}"
            );
        });
    }
}
