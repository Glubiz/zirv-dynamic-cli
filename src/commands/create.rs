use dialoguer::{Confirm, Input};
use std::fs;
use std::path::PathBuf;

use crate::utils::{SCRIPT_DIR_NAME, Shortcuts, home_dir, is_reserved_command};

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
            println!("Aborted: '{value}' collides with a built-in command.");
            return Ok(());
        }
    }

    let target_dir: PathBuf = if global {
        home_dir()?.join(SCRIPT_DIR_NAME)
    } else {
        std::env::current_dir()?.join(SCRIPT_DIR_NAME)
    };

    if !target_dir.exists() {
        fs::create_dir_all(&target_dir)?;
        println!("Created directory: {target_dir:?}");
    } else {
        println!("Directory already exists: {target_dir:?}");
    }

    let file_name = format!("{name}.yaml");
    let script_path = target_dir.join(&file_name);
    if script_path.exists() {
        println!("Script file already exists: {script_path:?}");
    } else {
        fs::write(&script_path, DEFAULT_TEMPLATE)?;
        println!("Created script file: {script_path:?}");
    }

    if !shortcut.trim().is_empty() {
        let shortcuts_path = target_dir.join(".shortcuts.yaml");
        let mut shortcuts: Shortcuts = if shortcuts_path.exists() {
            let content = fs::read_to_string(&shortcuts_path)?;
            serde_yaml_ng::from_str(&content).unwrap_or_default()
        } else {
            Shortcuts::default()
        };
        shortcuts
            .shortcuts
            .insert(shortcut.trim().to_string(), file_name.clone());
        let yaml_string = serde_yaml_ng::to_string(&shortcuts)?;
        fs::write(&shortcuts_path, yaml_string)?;
        println!("Updated shortcuts file: {shortcuts_path:?}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::read_to_string;
    use tempfile::tempdir;

    /// Overrides HOME/USERPROFILE and the current directory for `test`,
    /// mirroring `commands::init::tests::with_fake_home`.
    fn with_fake_env<F, R>(fake_home: &PathBuf, fake_cwd: &PathBuf, test: F) -> R
    where
        F: FnOnce() -> R,
    {
        let original_home = env::var("HOME").ok();
        let original_userprofile = env::var("USERPROFILE").ok();
        let original_dir = env::current_dir().unwrap();

        unsafe {
            env::set_var("HOME", fake_home);
            env::set_var("USERPROFILE", fake_home);
        }
        env::set_current_dir(fake_cwd).unwrap();

        let result = test();

        env::set_current_dir(original_dir).unwrap();
        unsafe {
            match original_home {
                Some(home) => env::set_var("HOME", home),
                None => env::remove_var("HOME"),
            }
            match original_userprofile {
                Some(up) => env::set_var("USERPROFILE", up),
                None => env::remove_var("USERPROFILE"),
            }
        }

        result
    }

    fn unreachable_confirm(_: &str) -> Result<bool, Box<dyn std::error::Error>> {
        panic!("confirm_fn should not be called when there is no reserved-name collision")
    }

    #[test]
    fn test_create_script_core_writes_local_script_and_shortcut() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                create_script_core("mytest", "mt", false, true, unreachable_confirm).unwrap();

                let script_path = fake_cwd.path().join(".zirv").join("mytest.yaml");
                assert!(script_path.exists());

                let shortcuts_path = fake_cwd.path().join(".zirv").join(".shortcuts.yaml");
                let content = read_to_string(shortcuts_path).unwrap();
                assert!(content.contains("mt"));
                assert!(content.contains("mytest.yaml"));
            },
        );
    }

    #[test]
    fn test_create_script_core_writes_to_global_dir() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                create_script_core("globaltest", "", true, true, unreachable_confirm).unwrap();

                let script_path = fake_home.path().join(".zirv").join("globaltest.yaml");
                assert!(
                    script_path.exists(),
                    "script should land in the global .zirv"
                );

                let local_path = fake_cwd.path().join(".zirv").join("globaltest.yaml");
                assert!(!local_path.exists());
            },
        );
    }

    #[test]
    fn test_create_script_core_no_shortcut_skips_shortcuts_file() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                create_script_core("noshortcut", "", false, true, unreachable_confirm).unwrap();

                let shortcuts_path = fake_cwd.path().join(".zirv").join(".shortcuts.yaml");
                assert!(!shortcuts_path.exists());
            },
        );
    }

    #[test]
    fn test_create_script_core_reserved_name_errors_when_non_interactive() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                let result = create_script_core("help", "", false, true, unreachable_confirm);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("help"));

                let script_path = fake_cwd.path().join(".zirv").join("help.yaml");
                assert!(!script_path.exists(), "must not write the file on error");
            },
        );
    }

    #[test]
    fn test_create_script_core_reserved_shortcut_errors_when_non_interactive() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                let result = create_script_core("fine", "c", false, true, unreachable_confirm);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains('c'));
            },
        );
    }

    #[test]
    fn test_create_script_core_reserved_name_confirmed_proceeds() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                create_script_core("help", "", false, false, |_| Ok(true)).unwrap();

                let script_path = fake_cwd.path().join(".zirv").join("help.yaml");
                assert!(script_path.exists(), "confirmed collision should proceed");
            },
        );
    }

    #[test]
    fn test_create_script_core_reserved_name_declined_aborts() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                let result = create_script_core("help", "", false, false, |_| Ok(false));
                assert!(
                    result.is_ok(),
                    "declining is a graceful abort, not an error"
                );

                let script_path = fake_cwd.path().join(".zirv").join("help.yaml");
                assert!(
                    !script_path.exists(),
                    "must not write the file when declined"
                );
            },
        );
    }

    #[test]
    fn test_create_script_fully_non_interactive_end_to_end() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
                let opts = CreateOptions {
                    name: Some("e2e".to_string()),
                    shortcut: Some("e".to_string()),
                    global: Some(false),
                };
                create_script(opts).unwrap();

                let script_path = fake_cwd.path().join(".zirv").join("e2e.yaml");
                assert!(script_path.exists());
            },
        );
    }

    #[test]
    fn test_create_script_fully_non_interactive_reserved_name_errors() {
        let fake_home = tempdir().unwrap();
        let fake_cwd = tempdir().unwrap();

        with_fake_env(
            &fake_home.path().to_path_buf(),
            &fake_cwd.path().to_path_buf(),
            || {
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
            },
        );
    }
}
