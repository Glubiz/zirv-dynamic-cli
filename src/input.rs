use std::path::{Path, PathBuf};

use clap::Parser;

use crate::utils::{
    COMMANDS_DIR_NAME, SCRIPT_DIR_NAME, SUPPORTED_EXTENSIONS, Shortcuts, candidate_names_in_dir,
    home_dir, is_reserved_zirv_file, script_like_files_at_root, suggest_matches,
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

/// `root_dir` is a `.zirv` root: scripts resolve from its `commands/`
/// subdirectory (issue #212), while `.shortcuts.yaml` -- config, not a
/// script -- is read from the root itself, same as it always has been. A
/// shortcut's mapped file is resolved from `commands/` too, since that is
/// where the script it points at now lives.
fn find_script_in_dir(
    root_dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let commands_dir = root_dir.join(COMMANDS_DIR_NAME);
    for ext in SUPPORTED_EXTENSIONS {
        let file_name = format!("{name}.{ext}");
        // A name like ".settings" would otherwise resolve to
        // ".settings.toml" -- zirv's own configuration, not a script -- by
        // the exact same `{name}.{ext}` search a real script uses. Compared
        // case-insensitively: NTFS resolves `.Settings.toml` to the same
        // file `Path::exists` would find below, so the guard has to agree.
        if is_reserved_zirv_file(&file_name) {
            continue;
        }
        let path = commands_dir.join(&file_name);
        if path.exists() {
            return Ok(Some(path.canonicalize()?));
        }
    }

    let shortcuts_path = root_dir.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        // A malformed `.shortcuts.yaml` must not take down every script
        // lookup: `create` already treats this file as recoverable rather
        // than fatal (`read_shortcuts`), and a lookup has somewhere else to
        // fall through to (the other extensions, then the other directory)
        // even when this one file cannot be parsed.
        let shortcuts = match std::fs::read_to_string(&shortcuts_path) {
            Ok(content) => match serde_yaml_ng::from_str::<Shortcuts>(&content) {
                Ok(shortcuts) => Some(shortcuts),
                Err(err) => {
                    crate::output::warn(format!(
                        "{} could not be parsed ({err}); ignoring its shortcuts for this lookup",
                        shortcuts_path.display()
                    ));
                    None
                }
            },
            Err(err) => {
                crate::output::warn(format!(
                    "{} could not be read ({err}); ignoring its shortcuts for this lookup",
                    shortcuts_path.display()
                ));
                None
            }
        };
        if let Some(mapped_file) = shortcuts.as_ref().and_then(|s| s.shortcuts.get(name)) {
            // Review finding (post-#212): `.shortcuts.yaml` is repo-owned,
            // untrusted config, and since #212 script resolution only ever
            // looks inside `commands_dir` -- a target like `../legacy.yaml`
            // must not be allowed to resolve outside it (it would otherwise
            // land back on the very `.zirv` root the hard cutover removed
            // from lookup) or via an absolute/rooted path escape further
            // still. Checked before ever joining, so a malicious target is
            // refused loudly rather than silently resolving somewhere else.
            if !shortcut_target_is_confined(mapped_file) {
                return Err(format!(
                    "shortcut '{name}' in {} maps to '{mapped_file}', which would resolve \
                     outside .zirv/commands/ -- shortcut targets must stay inside that \
                     directory (zirv 3.0 moved scripts to .zirv/commands/, issue #212); fix \
                     the mapping to a plain file name or a path relative to commands/",
                    shortcuts_path.display()
                )
                .into());
            }
            let path = commands_dir.join(mapped_file);
            if path.exists() {
                return Ok(Some(path.canonicalize()?));
            }
            for ext in SUPPORTED_EXTENSIONS {
                let path = commands_dir.join(format!("{mapped_file}.{ext}"));
                if path.exists() {
                    return Ok(Some(path.canonicalize()?));
                }
            }
        }
    }

    Ok(None)
}

/// Whether a `.shortcuts.yaml` mapped target stays confined to the directory
/// it is about to be joined against: never rooted/absolute (Unix `/x`,
/// Windows `C:\x` or a driveless `\x`, both caught by `Path::has_root`) and
/// never containing a `..` component. `.shortcuts.yaml` is repo-owned,
/// untrusted config (see `Untrusted Configuration` in the vault) that must
/// only ever narrow, never widen, what a lookup can reach.
///
/// Before issue #212, the mapped file was joined straight against the
/// `.zirv` root itself, so the same kind of target could already escape
/// `.zirv` entirely (a `..` walked one directory above it; an absolute path
/// bypassed the join altogether) -- this was already a latent path-traversal
/// gap, just bounded one level higher up. After #212 moved script lookup
/// into `.zirv/commands/`, joining unchecked against that narrower directory
/// means `../legacy.yaml` resolves to `.zirv/legacy.yaml` -- silently
/// reviving the exact pre-3.0 root layout the hard cutover was meant to
/// retire -- while a longer `../../x.yaml` or an absolute path can still
/// walk out past `.zirv` altogether. This closes both at the new boundary.
fn shortcut_target_is_confined(mapped_file: &str) -> bool {
    let path = Path::new(mapped_file);
    if path.has_root() {
        return false;
    }
    !path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
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

/// How many stray root-level script files `not_found_error` names before it
/// stops listing them -- an operator with a large pre-3.0 `.zirv/` gets a
/// useful sample, not a wall of text.
const MAX_STRAY_SCRIPTS_LISTED: usize = 10;

/// Builds the "no script or shortcut" error, enriched with up to 3 "did you
/// mean" suggestions drawn from local (then global) script names and
/// shortcut keys, plus a pointer to `zirv help`.
///
/// Issue #212 (zirv 3.0, hard cutover): scripts moved from the `.zirv` root
/// into `.zirv/commands/`, with no transitional root lookup. When the root
/// still has script-like files -- almost certainly a pre-3.0 layout rather
/// than an unrelated typo -- the error names them (relative to the `.zirv`
/// they were found under) and says where they need to move, instead of
/// leaving the operator to guess why a script that is plainly right there
/// doesn't resolve.
fn not_found_error(command: &str) -> Box<dyn std::error::Error> {
    let local_root = PathBuf::from(SCRIPT_DIR_NAME);
    let home = home_dir().ok();

    let mut candidates = candidate_names_in_dir(&local_root);
    if let Some(home) = &home {
        candidates.extend(candidate_names_in_dir(&home.join(SCRIPT_DIR_NAME)));
    }
    let suggestions = suggest_matches(command, candidates.iter().map(String::as_str));

    let mut stray: Vec<String> = script_like_files_at_root(&local_root)
        .into_iter()
        .map(|name| format!(".zirv/{name}"))
        .collect();
    if let Some(home) = &home {
        stray.extend(
            script_like_files_at_root(&home.join(SCRIPT_DIR_NAME))
                .into_iter()
                .map(|name| format!("~/.zirv/{name}")),
        );
    }

    let mut message = format!("No script or shortcut found for '{command}'.");
    if !stray.is_empty() {
        let shown: Vec<&String> = stray.iter().take(MAX_STRAY_SCRIPTS_LISTED).collect();
        let listed = shown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let more = stray.len().saturating_sub(shown.len());
        message.push_str(&format!(
            " zirv 3.0 moved scripts from the .zirv root into .zirv/commands/: {listed}",
        ));
        if more > 0 {
            message.push_str(&format!(" (+{more} more)"));
        }
        message.push_str(" still need to move there.");
    }
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
        let commands_dir = fake_cwd
            .path()
            .join(SCRIPT_DIR_NAME)
            .join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join("build.yaml"),
            "name: Build\ncommands: []\n",
        )
        .unwrap();

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
        let commands_dir = fake_cwd
            .path()
            .join(SCRIPT_DIR_NAME)
            .join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join("deploy.yaml"),
            "name: Deploy\ncommands: []\n",
        )
        .unwrap();

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
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(commands_dir.join("test.yaml"), "name: Test\ncommands: []\n").unwrap();
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

    /// Before this, a malformed local `.shortcuts.yaml` propagated its parse
    /// error through `?`, so a lookup that did not even need shortcuts (the
    /// script exists as a plain file) still failed. `create` already treats
    /// this file as recoverable rather than fatal; the read path has to
    /// match, and fall through to a direct extension match in the same
    /// directory instead of hard-erroring.
    #[test]
    fn a_malformed_local_shortcuts_file_does_not_break_a_direct_match() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join("build.yaml"),
            "name: Build\ncommands: []\n",
        )
        .unwrap();
        write(zirv_dir.join(".shortcuts.yaml"), "not: [valid, yaml for,").unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "build".to_string(),
                ..Default::default()
            };
            let path = input
                .get_file_path()
                .expect("a direct extension match must still resolve");
            assert!(path.ends_with("build.yaml"), "got: {}", path.display());
        });
    }

    /// Review finding (post-#212): `.shortcuts.yaml` is repo-owned, untrusted
    /// config, and since #212 script resolution only ever looks inside
    /// `commands_dir` -- a `../legacy.yaml` mapping must not be allowed to
    /// resolve outside it (landing back on the `.zirv` root the hard cutover
    /// removed from lookup, or escaping further still) and must error
    /// clearly instead. A sibling shortcut whose target genuinely lives
    /// inside `commands/` must keep working exactly as before.
    #[test]
    fn a_shortcut_target_escaping_commands_errors_instead_of_resolving_at_the_root() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(commands_dir.join("safe.yaml"), "name: Safe\ncommands: []\n").unwrap();
        // The "legacy" script sitting right at the `.zirv` root -- the
        // pre-3.0 location `../legacy.yaml` (relative to `commands/`) would
        // land on.
        write(zirv_dir.join("legacy.yaml"), "name: Legacy\ncommands: []\n").unwrap();
        write(
            zirv_dir.join(".shortcuts.yaml"),
            "shortcuts:\n  legacy: ../legacy.yaml\n  ok: safe.yaml\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let escaping = Input {
                command: "legacy".to_string(),
                ..Default::default()
            };
            let err = escaping
                .get_file_path()
                .expect_err("a shortcut escaping commands/ must not resolve");
            let message = err.to_string();
            assert!(
                message.contains("legacy") && message.contains("../legacy.yaml"),
                "expected the shortcut and its target to be named, got: {message}"
            );
            assert!(
                message.contains("commands/"),
                "expected the confinement boundary to be named, got: {message}"
            );

            let confined = Input {
                command: "ok".to_string(),
                ..Default::default()
            };
            let path = confined
                .get_file_path()
                .expect("a shortcut whose target lives inside commands/ must still resolve");
            assert!(path.ends_with("safe.yaml"), "got: {}", path.display());
        });
    }

    /// `zirv .settings` must not resolve to `.zirv/.settings.toml`: that file
    /// is zirv's own agent switchboard, reached the same way a script
    /// extension search would ("{name}.{ext}") if the guard did not exist.
    #[test]
    fn the_settings_file_cannot_be_invoked_as_a_script_name() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(
            zirv_dir.join(".settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: ".settings".to_string(),
                ..Default::default()
            };
            let err = input
                .get_file_path()
                .expect_err("the settings file must not be an invocable script");
            assert!(
                err.to_string().contains("zirv help"),
                "a normal not-found error, not a resolved config file: {err}"
            );
        });
    }

    /// Review finding 2: `zirv .Settings` builds the candidate name
    /// `.Settings.toml`, which NTFS (and APFS by default) would resolve to
    /// the very same on-disk `.settings.toml` file `Path::exists` finds --
    /// so the reserved-name guard has to match case-insensitively too, not
    /// just the lowercase spelling.
    #[test]
    fn a_differently_cased_settings_invocation_is_still_blocked() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(
            zirv_dir.join(".settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: ".Settings".to_string(),
                ..Default::default()
            };
            let err = input
                .get_file_path()
                .expect_err("a differently-cased invocation must not resolve the settings file");
            assert!(
                err.to_string().contains("zirv help"),
                "a normal not-found error, not a resolved config file: {err}"
            );
        });
    }

    /// Same file, but this time the lookup genuinely needs the shortcut: it
    /// must report "not found" rather than propagating the parse error.
    #[test]
    fn a_malformed_local_shortcuts_file_falls_through_to_not_found() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join(".shortcuts.yaml"), "not: [valid, yaml for,").unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "tst".to_string(),
                ..Default::default()
            };
            let err = input.get_file_path().unwrap_err();
            assert!(
                err.to_string().contains("zirv help"),
                "a normal not-found error, not a parse error: {err}"
            );
        });
    }

    /// Issue #212, core behavior: a script in `.zirv/commands/` resolves,
    /// and the identical file sitting at the `.zirv` root (the pre-3.0
    /// layout) does not -- with no transitional root lookup.
    #[test]
    fn a_script_resolves_from_commands_but_not_from_the_zirv_root() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let commands_dir = fake_cwd
            .path()
            .join(SCRIPT_DIR_NAME)
            .join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        write(
            commands_dir.join("build.yaml"),
            "name: Build\ncommands: []\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "build".to_string(),
                ..Default::default()
            };
            let path = input
                .get_file_path()
                .expect("a script in commands/ must resolve");
            assert!(path.ends_with("build.yaml"), "got: {}", path.display());
        });
    }

    /// Same script, but left at the `.zirv` root instead of moved into
    /// `commands/`: it must not resolve, and the error must name the stray
    /// file and say where it needs to move.
    #[test]
    fn a_script_left_at_the_zirv_root_does_not_resolve_and_is_named_in_the_error() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("build.yaml"), "name: Build\ncommands: []\n").unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "build".to_string(),
                ..Default::default()
            };
            let err = input
                .get_file_path()
                .expect_err("a script at the .zirv root must not resolve any more");
            let message = err.to_string();
            assert!(
                message.contains("build.yaml"),
                "expected the stray file to be named, got: {message}"
            );
            assert!(
                message.contains(".zirv/commands"),
                "expected the move instruction to name the new location, got: {message}"
            );
        });
    }

    /// Global `~/.zirv/commands/` resolves with the same local-first
    /// precedence as before the move.
    #[test]
    fn a_global_commands_script_resolves_and_local_still_takes_precedence() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let global_commands = fake_home
            .path()
            .join(SCRIPT_DIR_NAME)
            .join(COMMANDS_DIR_NAME);
        create_dir_all(&global_commands).unwrap();
        write(
            global_commands.join("deploy.yaml"),
            "name: Global Deploy\ncommands: []\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            // No local .zirv at all: falls through to the global commands/.
            let input = Input {
                command: "deploy".to_string(),
                ..Default::default()
            };
            let path = input
                .get_file_path()
                .expect("a global commands/ script must resolve");
            assert!(path.ends_with("deploy.yaml"), "got: {}", path.display());
        });

        // A local script of the same name still wins over the global one.
        let local_commands = fake_cwd
            .path()
            .join(SCRIPT_DIR_NAME)
            .join(COMMANDS_DIR_NAME);
        create_dir_all(&local_commands).unwrap();
        write(
            local_commands.join("deploy.yaml"),
            "name: Local Deploy\ncommands: []\n",
        )
        .unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "deploy".to_string(),
                ..Default::default()
            };
            let path = input.get_file_path().unwrap();
            let resolved = std::fs::read_to_string(&path).unwrap();
            assert!(
                resolved.contains("Local Deploy"),
                "the local script must take precedence over the global one, got: {resolved}"
            );
        });
    }

    /// Config files at the `.zirv` root (`ctx.toml`, `.settings.toml`,
    /// `verify.toml`, `.shortcuts.yaml`) must never be listed as "script-like"
    /// in the migration error -- they still belong at the root.
    #[test]
    fn config_files_at_the_root_are_never_listed_as_stray_scripts() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();
        let zirv_dir = fake_cwd.path().join(SCRIPT_DIR_NAME);
        create_dir_all(&zirv_dir).unwrap();
        write(zirv_dir.join("ctx.toml"), "[score]\nwindow = 4\n").unwrap();
        write(
            zirv_dir.join(".settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .unwrap();
        write(zirv_dir.join("verify.toml"), "").unwrap();
        write(zirv_dir.join(".shortcuts.yaml"), "shortcuts: {}\n").unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let input = Input {
                command: "nope".to_string(),
                ..Default::default()
            };
            let err = input.get_file_path().unwrap_err();
            let message = err.to_string();
            assert!(
                !message.contains("moved scripts"),
                "config-only root must not trigger the migration hint, got: {message}"
            );
            assert!(message.contains("zirv help"), "got: {message}");
        });
    }
}
