use dialoguer::Confirm;
use std::fs;
use std::path::{Path, PathBuf};

// Default shortcuts file content.
const DEFAULT_SHORTCUTS: &str = r#"shortcuts:
  e: "example.yaml"
"#;

/// Creates `~/.zirv` and its default `.shortcuts.yaml` if either is missing,
/// leaving an existing one untouched. The global half of `init_zirv_with`,
/// pulled out so the first-run setup wizard (`commands::setup::run_first_run`)
/// can scaffold the operator's home layer without duplicating this logic.
pub fn scaffold_global_zirv() -> Result<(), Box<dyn std::error::Error>> {
    // Instead of using dirs::home_dir(), use the HOME or USERPROFILE env variable.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| "Could not determine home directory")?;
    let home_zirv = home.join(".zirv");

    if !home_zirv.exists() {
        fs::create_dir_all(&home_zirv)?;
        println!("Created .zirv in home directory: {home_zirv:?}");
    }
    // Create default .shortcuts.yaml in home folder if not present.
    let home_shortcuts = home_zirv.join(".shortcuts.yaml");
    if !home_shortcuts.exists() {
        fs::write(&home_shortcuts, DEFAULT_SHORTCUTS)?;
        println!("Created default .shortcuts.yaml in home directory: {home_shortcuts:?}");
    }
    Ok(())
}

/// Creates `<current_dir>/.zirv` and its default `.shortcuts.yaml`. The local
/// half of `init_zirv_with`'s confirmed branch, pulled out for the same
/// reason `scaffold_global_zirv` is: the first-run wizard's project-layer
/// step reuses it rather than duplicating the creation logic. Callers are
/// expected to have already checked `current_dir.join(".zirv")` does not
/// exist; this always (re-)creates it.
pub fn scaffold_local_zirv(current_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let current_zirv = current_dir.join(".zirv");
    fs::create_dir_all(&current_zirv)?;
    println!("Created .zirv in current directory: {current_zirv:?}");
    let current_shortcuts = current_zirv.join(".shortcuts.yaml");
    if !current_shortcuts.exists() {
        fs::write(&current_shortcuts, DEFAULT_SHORTCUTS)?;
        println!("Created default .shortcuts.yaml in current directory: {current_shortcuts:?}");
    }
    Ok(())
}

/// Initializes the global .zirv folder in the home directory and (optionally)
/// the .zirv folder in the current directory based on the confirmation function.
///
/// The `confirm_fn` closure is called to determine if the user wants to initialize
/// in the current directory. In production, you can pass a closure that uses `dialoguer::Confirm`.
pub fn init_zirv_with<F>(confirm_fn: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn() -> Result<bool, Box<dyn std::error::Error>>,
{
    scaffold_global_zirv()?;

    // Get the current directory.
    let current_dir = std::env::current_dir()?;
    let current_zirv = current_dir.join(".zirv");

    if !current_zirv.exists() {
        let init_current = confirm_fn()?;
        if init_current {
            scaffold_local_zirv(&current_dir)?;
        } else {
            println!(".zirv not created in current directory.");
        }
    } else {
        println!(".zirv already exists in current directory.");
    }

    Ok(())
}

/// Production version: calls init_zirv_with using dialoguer to ask the user.
pub fn init_zirv() -> Result<(), Box<dyn std::error::Error>> {
    init_zirv_with(|| {
        Confirm::new()
            .with_prompt("Would you like to initialize .zirv in the current directory?")
            .default(false)
            .interact()
            .map_err(|e| e.into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{read_to_string, remove_dir_all};
    use tempfile::tempdir;

    const DEFAULT_SHORTCUTS_CONTENT: &str = r#"shortcuts:
  e: "example.yaml"
"#;

    /// Test that only the home directory .zirv folder (with default .shortcuts.yaml) is created,
    /// if the user declines to initialize in the current directory.
    #[test]
    fn test_init_zirv_only_home() -> Result<(), Box<dyn std::error::Error>> {
        // Create temporary directories for fake home and current directory.
        let fake_home_dir = tempdir()?;
        let fake_home_path = fake_home_dir.path().to_path_buf();
        let fake_current_dir = tempdir()?;
        let fake_current_path = fake_current_dir.path().to_path_buf();

        // Overrides HOME, USERPROFILE, and the working directory, and puts
        // all three back on drop -- including when an assertion below
        // panics, which is exactly when a leaked HOME or working directory
        // would otherwise break every later test in this process.
        let _guard =
            crate::commands::ctx::testenv::EnvGuard::set(&fake_home_path, Some(&fake_current_path));

        // Ensure no .zirv exists in current directory.
        let current_zirv = fake_current_path.join(".zirv");
        if current_zirv.exists() {
            remove_dir_all(&current_zirv).unwrap();
        }

        // Call init_zirv_with with a confirmation function that returns false.
        init_zirv_with(|| Ok(false)).unwrap();

        // Verify that .zirv exists in the fake home directory.
        let home_zirv = fake_home_path.join(".zirv");

        assert!(
            home_zirv.exists(),
            ".zirv should be created in the home directory"
        );

        let home_shortcuts = home_zirv.join(".shortcuts.yaml");
        assert!(
            home_shortcuts.exists(),
            ".shortcuts.yaml should be created in the home directory"
        );

        let content = read_to_string(&home_shortcuts).unwrap();
        assert_eq!(content, DEFAULT_SHORTCUTS_CONTENT);

        // Verify that .zirv was NOT created in the current directory.
        assert!(
            !current_zirv.exists(),
            ".zirv should not be created in the current directory"
        );

        Ok(())
    }

    /// Test that both the home directory and the current directory .zirv folders (with default .shortcuts.yaml)
    /// are created when the user agrees to initialize in the current directory.
    #[test]
    fn test_init_zirv_home_and_current() -> Result<(), Box<dyn std::error::Error>> {
        let fake_home_dir = tempdir()?;
        let fake_home_path = fake_home_dir.path().to_path_buf();
        let fake_current_dir = tempdir()?;
        let fake_current_path = fake_current_dir.path().to_path_buf();

        let _guard =
            crate::commands::ctx::testenv::EnvGuard::set(&fake_home_path, Some(&fake_current_path));

        // Ensure no .zirv exists in the current directory.
        let current_zirv = fake_current_path.join(".zirv");
        if current_zirv.exists() {
            remove_dir_all(&current_zirv).unwrap();
        }

        // Call init_zirv_with with a confirmation function that returns true.
        init_zirv_with(|| Ok(true)).unwrap();

        // Verify that .zirv exists in the home directory.
        let home_zirv = fake_home_path.join(".zirv");

        assert!(
            home_zirv.exists(),
            ".zirv should be created in the home directory"
        );

        let home_shortcuts = home_zirv.join(".shortcuts.yaml");

        assert!(
            home_shortcuts.exists(),
            "Global .shortcuts.yaml should be created"
        );

        let home_content = read_to_string(home_shortcuts).unwrap();
        assert_eq!(home_content, DEFAULT_SHORTCUTS_CONTENT);

        // Verify that .zirv exists in the current directory.
        assert!(
            current_zirv.exists(),
            ".zirv should be created in the current directory"
        );

        let current_shortcuts = current_zirv.join(".shortcuts.yaml");

        assert!(
            current_shortcuts.exists(),
            "Local .shortcuts.yaml should be created"
        );

        let current_content = read_to_string(current_shortcuts).unwrap();
        assert_eq!(current_content, DEFAULT_SHORTCUTS_CONTENT);

        Ok(())
    }
}
