use dialoguer::Confirm;
use std::fs;
use std::path::PathBuf;

// Default shortcuts file content.
const DEFAULT_SHORTCUTS: &str = r#"shortcuts:
  e: "example.yaml"
"#;

/// Initializes the global .zirv folder in the home directory and (optionally)
/// the .zirv folder in the current directory based on the confirmation function.
///
/// The `confirm_fn` closure is called to determine if the user wants to initialize
/// in the current directory. In production, you can pass a closure that uses `dialoguer::Confirm`.
pub fn init_zirv_with<F>(confirm_fn: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn() -> Result<bool, Box<dyn std::error::Error>>,
{
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

    // Get the current directory.
    let current_dir = std::env::current_dir()?;
    let current_zirv = current_dir.join(".zirv");

    if !current_zirv.exists() {
        let init_current = confirm_fn()?;
        if init_current {
            fs::create_dir_all(&current_zirv)?;
            println!("Created .zirv in current directory: {current_zirv:?}");
            // Create default .shortcuts.yaml in current dicurrent_zirvrect
            let current_shortcuts = current_zirv.join(".shortcuts.yaml");
            if !current_shortcuts.exists() {
                fs::write(&current_shortcuts, DEFAULT_SHORTCUTS)?;
                println!(
                    "Created default .shortcuts.yaml in current directory: {current_shortcuts:?}"
                );
            }
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
    use std::env;
    use std::fs::{read_to_string, remove_dir_all};
    use std::path::PathBuf;
    use tempfile::tempdir;

    const DEFAULT_SHORTCUTS_CONTENT: &str = r#"shortcuts:
  e: "example.yaml"
"#;

    /// Helper function to temporarily override HOME and USERPROFILE.
    fn with_fake_home<F, R>(fake_home: &PathBuf, test: F) -> R
    where
        F: FnOnce() -> R,
    {
        let original_home = env::var("HOME").ok();
        let original_userprofile = env::var("USERPROFILE").ok();

        unsafe {
            env::set_var("HOME", fake_home);
            env::set_var("USERPROFILE", fake_home);
        }

        let result = test();

        unsafe {
            if let Some(home) = original_home {
                env::set_var("HOME", home);
            } else {
                env::remove_var("HOME");
            }
            if let Some(up) = original_userprofile {
                env::set_var("USERPROFILE", up);
            } else {
                env::remove_var("USERPROFILE");
            }
        }

        result
    }

    /// Test that only the home directory .zirv folder (with default .shortcuts.yaml) is created,
    /// if the user declines to initialize in the current directory.
    #[test]
    fn test_init_zirv_only_home() -> Result<(), Box<dyn std::error::Error>> {
        // Create temporary directories for fake home and current directory.
        let fake_home_dir = tempdir()?;
        let fake_home_path = fake_home_dir.path().to_path_buf();
        let fake_current_dir = tempdir()?;
        let fake_current_path = fake_current_dir.path().to_path_buf();

        // Override HOME and USERPROFILE.
        with_fake_home(&fake_home_path, || {
            // Change current directory to fake_current.
            let original_dir = env::current_dir().unwrap();
            env::set_current_dir(&fake_current_path).unwrap();

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

            env::set_current_dir(&original_dir).unwrap();
        });

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

        with_fake_home(&fake_home_path, || {
            let original_dir = env::current_dir().unwrap();
            env::set_current_dir(&fake_current_path).unwrap();

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

            env::set_current_dir(&original_dir).unwrap();
        });

        Ok(())
    }
}
