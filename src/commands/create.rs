use dialoguer::{Confirm, Input};
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::{COMMANDS_DIR_NAME, SCRIPT_DIR_NAME, Shortcuts, home_dir, is_reserved_command};

const DEFAULT_TEMPLATE: &str = r#"name: "Name"
description: "Description"
#params:
#  - "commit_message"
#  - "optional_param?"
commands:
#  - command: example
#    description: Step description
#    options:
#      interactive: bool
#      operating_system: linux|windows|macos
#      proceed_on_failure: bool
#      delay_ms: int
#  - command: example2
#    description: Step 2 description
#    options:
#      interactive: bool
#      operating_system: linux|windows|macos
#      proceed_on_failure: bool
#      delay_ms: int
"#;

/// Pre-resolved answers to the `create` wizard's questions. Any field left
/// `None` falls back to the corresponding interactive prompt. When every
/// field is `Some`, `create_script` never touches stdin, so it can run in a
/// non-interactive/scripted context.
#[derive(Debug, Default, Clone)]
pub struct CreateOptions {
    /// The script name (the file will be named `<name>.yaml`).
    pub name: Option<String>,
    /// An optional shortcut key; empty string means "no shortcut".
    pub shortcut: Option<String>,
    /// Whether to create in the global `~/.zirv` folder rather than the
    /// current directory's `.zirv`.
    pub global: Option<bool>,
}

/// Creates a new script file, asking interactively for whichever of
/// name/shortcut/global was not supplied in `opts`.
///
/// This command asks the user for:
///  - The script name (the file will be named `<name>.yaml`)
///  - An optional shortcut key (if provided, the shortcut is appended to the .shortcuts.yaml file)
///  - Whether the file should be created in the global folder (home directory) or in the current directory
pub fn create_script(opts: CreateOptions) -> Result<(), Box<dyn std::error::Error>> {
    let non_interactive = opts.name.is_some() && opts.shortcut.is_some() && opts.global.is_some();

    let name = match opts.name {
        Some(n) => n,
        None => Input::new()
            .with_prompt("Enter the name for the new script")
            .interact_text()?,
    };

    let shortcut = match opts.shortcut {
        Some(s) => s,
        None => Input::new()
            .with_prompt("Enter a shortcut key (optional, leave empty if none)")
            .allow_empty(true)
            .interact_text()?,
    };

    let global = match opts.global {
        Some(g) => g,
        None => Confirm::new()
            .with_prompt("Create the script in the global .zirv folder (in your home directory)?")
            .default(false)
            .interact()?,
    };

    create_script_core(&name, &shortcut, global, non_interactive, |colliding| {
        Confirm::new()
            .with_prompt(format!("Use '{colliding}' anyway?"))
            .default(false)
            .interact()
            .map_err(Into::into)
    })
}

/// The name becomes a path under `.zirv`, so it has to stay there. `--name`
/// reaches this straight from argv, and `../../escaped` wrote a file two
/// directories above the folder the command is about to report creating.
fn validate_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    if name.trim().is_empty() {
        return Err("a script name is required".into());
    }
    if name.contains(std::path::is_separator) || name == ".." {
        return Err(format!(
            "'{name}' is not a script name: it must not contain a path separator or be '..'"
        )
        .into());
    }
    Ok(())
}

/// The existing shortcuts, or `None` when the caller declined to replace a
/// file that could not be read.
///
/// The whole file is rewritten from what is parsed here, so treating a parse
/// failure as "no shortcuts" silently deleted every entry in it. A missing
/// file is genuinely empty; a malformed one is a question, and in a scripted
/// run with nobody to ask, an error.
fn read_shortcuts<F>(
    path: &Path,
    non_interactive: bool,
    confirm_fn: &F,
) -> Result<Option<Shortcuts>, Box<dyn std::error::Error>>
where
    F: Fn(&str) -> Result<bool, Box<dyn std::error::Error>>,
{
    if !path.exists() {
        return Ok(Some(Shortcuts::default()));
    }
    let content = fs::read_to_string(path)?;
    let e = match serde_yaml_ng::from_str::<Shortcuts>(&content) {
        Ok(shortcuts) => return Ok(Some(shortcuts)),
        Err(e) => e,
    };

    let message = format!("{} could not be parsed ({e})", path.display());
    if non_interactive {
        return Err(format!(
            "{message}; fix it or move it aside -- rewriting it would drop every \
             shortcut it holds"
        )
        .into());
    }
    crate::output::warn(&message);
    if !confirm_fn("replace it and lose every shortcut it holds")? {
        crate::output::note("Aborted: the shortcuts file was left untouched.");
        return Ok(None);
    }
    Ok(Some(Shortcuts::default()))
}

/// Warns about (and, unless declined, proceeds past) any reserved-name
/// collision, then writes the script file and shortcut entry.
///
/// `confirm_fn` is asked to confirm a collision only when `non_interactive`
/// is false; when `non_interactive` is true a collision is a hard error
/// instead, since there is no prompt to fall back on.
fn create_script_core<F>(
    name: &str,
    shortcut: &str,
    global: bool,
    non_interactive: bool,
    confirm_fn: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn(&str) -> Result<bool, Box<dyn std::error::Error>>,
{
    validate_name(name)?;

    for value in [name, shortcut] {
        if !is_reserved_command(value) {
            continue;
        }

        let message = format!(
            "'{value}' collides with a built-in zirv command; a script or shortcut named '{value}' would be unreachable."
        );

        if non_interactive {
            return Err(message.into());
        }

        crate::output::warn(&message);
        if !confirm_fn(value)? {
            crate::output::note(format!(
                "Aborted: '{value}' collides with a built-in command."
            ));
            return Ok(());
        }
    }

    let zirv_root: PathBuf = if global {
        home_dir()?.join(SCRIPT_DIR_NAME)
    } else {
        std::env::current_dir()?.join(SCRIPT_DIR_NAME)
    };
    // Scripts live in `.zirv/commands/` as of zirv 3.0 (issue #212);
    // `.shortcuts.yaml` stays at `zirv_root` -- it is config, not a script.
    let target_dir = zirv_root.join(COMMANDS_DIR_NAME);

    if !target_dir.exists() {
        fs::create_dir_all(&target_dir)?;
        crate::output::note(format!("Created directory: {target_dir:?}"));
    } else {
        crate::output::note(format!("Directory already exists: {target_dir:?}"));
    }

    let file_name = format!("{name}.yaml");
    let script_path = target_dir.join(&file_name);
    if script_path.exists() {
        crate::output::note(format!("Script file already exists: {script_path:?}"));
    } else {
        fs::write(&script_path, DEFAULT_TEMPLATE)?;
        crate::output::note(format!("Created script file: {script_path:?}"));
    }

    if !shortcut.trim().is_empty() {
        let shortcuts_path = zirv_root.join(".shortcuts.yaml");
        let Some(mut shortcuts) = read_shortcuts(&shortcuts_path, non_interactive, &confirm_fn)?
        else {
            return Ok(());
        };
        shortcuts
            .shortcuts
            .insert(shortcut.trim().to_string(), file_name.clone());
        let yaml_string = serde_yaml_ng::to_string(&shortcuts)?;
        fs::write(&shortcuts_path, yaml_string)?;
        crate::output::note(format!("Updated shortcuts file: {shortcuts_path:?}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::read_to_string;
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

    fn unreachable_confirm(_: &str) -> Result<bool, Box<dyn std::error::Error>> {
        panic!("confirm_fn should not be called when there is no reserved-name collision")
    }

    /// `--name` reaches this straight from argv, and the name becomes a path.
    #[test]
    fn a_name_that_leaves_the_zirv_directory_is_rejected() {
        for bad in ["../../escaped", "nested/script", "", "   ", ".."] {
            assert!(
                create_script_core(bad, "", false, true, unreachable_confirm).is_err(),
                "{bad:?} must be refused before anything is written"
            );
        }
        // A dot in the middle is a perfectly ordinary name.
        assert!(validate_name("deploy.staging").is_ok());
    }

    /// The separator check alone lets a bare `..` through: splitting a string
    /// on `.` can never produce a part that itself contains a `.`, so
    /// `part == ".."` was always false and this name passed validation
    /// despite the error message's own promise to reject it.
    #[test]
    fn a_bare_parent_directory_name_is_rejected_on_its_own() {
        assert!(validate_name("..").is_err());
    }

    /// The whole file is rewritten from what is parsed, so treating a parse
    /// failure as "no shortcuts" deleted every entry in it. With nobody to
    /// ask, that has to be an error rather than a silent loss.
    #[test]
    fn a_malformed_shortcuts_file_is_never_silently_replaced() {
        let home = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let zirv = cwd.path().join(SCRIPT_DIR_NAME);
        fs::create_dir_all(&zirv).unwrap();
        let shortcuts = zirv.join(".shortcuts.yaml");
        fs::write(&shortcuts, "shortcuts:\n  b: build.yaml\n  : : broken\n").unwrap();

        let err = with_fake_env(home.path(), cwd.path(), || {
            create_script_core("new", "n", false, true, unreachable_confirm)
                .expect_err("a scripted run has nobody to ask")
        });

        assert!(err.to_string().contains("could not be parsed"), "got {err}");
        assert!(
            read_to_string(&shortcuts).unwrap().contains("build.yaml"),
            "the existing shortcuts must still be there"
        );
    }

    #[test]
    fn declining_to_replace_a_malformed_shortcuts_file_leaves_it_alone() {
        let home = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let zirv = cwd.path().join(SCRIPT_DIR_NAME);
        fs::create_dir_all(&zirv).unwrap();
        let shortcuts = zirv.join(".shortcuts.yaml");
        fs::write(&shortcuts, "shortcuts:\n  b: build.yaml\n  : : broken\n").unwrap();

        with_fake_env(home.path(), cwd.path(), || {
            create_script_core("new", "n", false, false, |_| Ok(false)).expect("declining is fine")
        });

        assert!(
            read_to_string(&shortcuts).unwrap().contains("build.yaml"),
            "declining must not rewrite the file"
        );
    }

    #[test]
    fn test_create_script_core_writes_local_script_and_shortcut() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            create_script_core("mytest", "mt", false, true, unreachable_confirm).unwrap();

            let script_path = fake_cwd
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("mytest.yaml");
            assert!(script_path.exists());

            let shortcuts_path = fake_cwd.path().join(".zirv").join(".shortcuts.yaml");
            let content = read_to_string(shortcuts_path).unwrap();
            assert!(content.contains("mt"));
            assert!(content.contains("mytest.yaml"));
        });
    }

    #[test]
    fn test_create_script_core_writes_to_global_dir() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            create_script_core("globaltest", "", true, true, unreachable_confirm).unwrap();

            let script_path = fake_home
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("globaltest.yaml");
            assert!(
                script_path.exists(),
                "script should land in the global .zirv/commands"
            );

            let local_path = fake_cwd
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("globaltest.yaml");
            assert!(!local_path.exists());
        });
    }

    #[test]
    fn test_create_script_core_no_shortcut_skips_shortcuts_file() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            create_script_core("noshortcut", "", false, true, unreachable_confirm).unwrap();

            let shortcuts_path = fake_cwd.path().join(".zirv").join(".shortcuts.yaml");
            assert!(!shortcuts_path.exists());
        });
    }

    #[test]
    fn test_create_script_core_reserved_name_errors_when_non_interactive() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let result = create_script_core("help", "", false, true, unreachable_confirm);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("help"));

            let script_path = fake_cwd
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("help.yaml");
            assert!(!script_path.exists(), "must not write the file on error");
        });
    }

    /// S4: NTFS (and APFS by default) resolve a file case-insensitively, so
    /// `Chat.yaml` and `chat.yaml` are the same collision risk even though
    /// `zirv Chat` (differently cased) is not literally intercepted as the
    /// `chat` alias the way `zirv chat` is. The collision guard has to catch
    /// it anyway, or `--name Chat` creates a script `zirv help` never marks
    /// as shadowed and a later rename/lookup can resolve unpredictably.
    #[test]
    fn create_refuses_a_reserved_name_in_any_case() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            for name in ["Help", "CHAT", "Agent", "CtX"] {
                let result = create_script_core(name, "", false, true, unreachable_confirm);
                assert!(
                    result.is_err(),
                    "'{name}' collides with a reserved command regardless of case"
                );
            }
        });
    }

    #[test]
    fn test_create_script_core_reserved_shortcut_errors_when_non_interactive() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let result = create_script_core("fine", "c", false, true, unreachable_confirm);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains('c'));
        });
    }

    #[test]
    fn test_create_script_core_reserved_name_confirmed_proceeds() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            create_script_core("help", "", false, false, |_| Ok(true)).unwrap();

            let script_path = fake_cwd
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("help.yaml");
            assert!(script_path.exists(), "confirmed collision should proceed");
        });
    }

    #[test]
    fn test_create_script_core_reserved_name_declined_aborts() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let result = create_script_core("help", "", false, false, |_| Ok(false));
            assert!(
                result.is_ok(),
                "declining is a graceful abort, not an error"
            );

            let script_path = fake_cwd
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("help.yaml");
            assert!(
                !script_path.exists(),
                "must not write the file when declined"
            );
        });
    }

    #[test]
    fn test_create_script_fully_non_interactive_end_to_end() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let opts = CreateOptions {
                name: Some("e2e".to_string()),
                shortcut: Some("e".to_string()),
                global: Some(false),
            };
            create_script(opts).unwrap();

            let script_path = fake_cwd
                .path()
                .join(".zirv")
                .join(COMMANDS_DIR_NAME)
                .join("e2e.yaml");
            assert!(script_path.exists());
        });
    }

    /// Issue #212: `create` writes into `.zirv/commands/`, creating the
    /// directory itself, while `.shortcuts.yaml` stays at the `.zirv` root.
    #[test]
    fn create_writes_into_commands_and_keeps_shortcuts_at_the_root() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            create_script_core("rooted", "r", false, true, unreachable_confirm).unwrap();

            let zirv = fake_cwd.path().join(".zirv");
            assert!(
                zirv.join(COMMANDS_DIR_NAME).is_dir(),
                "commands/ must be created"
            );
            assert!(zirv.join(COMMANDS_DIR_NAME).join("rooted.yaml").exists());
            assert!(
                !zirv.join("rooted.yaml").exists(),
                "the script must not also land at the .zirv root"
            );
            assert!(
                zirv.join(".shortcuts.yaml").exists(),
                ".shortcuts.yaml must stay at the .zirv root, not move into commands/"
            );
            assert!(
                !zirv
                    .join(COMMANDS_DIR_NAME)
                    .join(".shortcuts.yaml")
                    .exists()
            );
        });
    }

    #[test]
    fn test_create_script_fully_non_interactive_reserved_name_errors() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(fake_home.path(), fake_cwd.path(), || {
            let opts = CreateOptions {
                name: Some("version".to_string()),
                shortcut: Some(String::new()),
                global: Some(false),
            };
            // Must not hang on stdin: with all three opts given this is
            // fully non-interactive, so the collision is an error, not a
            // Confirm prompt.
            let result = create_script(opts);
            assert!(result.is_err());
        });
    }
}
