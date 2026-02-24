use dialoguer::{Confirm, Input};
use std::fs;
use std::path::PathBuf;

use crate::utils::{SCRIPT_DIR_NAME, Shortcuts, home_dir};

const DEFAULT_TEMPLATE: &str = r#"name: "Name"
description: "Description"
#params:
#  - "commit_message"
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

/// Interactively creates a new script file using dialogue.
///
/// This command will ask the user for:
///  - The script name (the file will be named `<name>.yaml`)
///  - An optional shortcut key (if provided, the shortcut is appended to the .shortcuts.yaml file)
///  - Whether the file should be created in the global folder (home directory) or in the current directory
pub fn create_script_interactive() -> Result<(), Box<dyn std::error::Error>> {
    let name: String = Input::new()
        .with_prompt("Enter the name for the new script")
        .interact_text()?;

    let shortcut: String = Input::new()
        .with_prompt("Enter a shortcut key (optional, leave empty if none)")
        .allow_empty(true)
        .interact_text()?;

    let global: bool = Confirm::new()
        .with_prompt("Create the script in the global .zirv folder (in your home directory)?")
        .default(false)
        .interact()?;

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
            serde_yaml::from_str(&content).unwrap_or_default()
        } else {
            Shortcuts::default()
        };
        shortcuts
            .shortcuts
            .insert(shortcut.clone(), file_name.clone());
        let yaml_string = serde_yaml::to_string(&shortcuts)?;
        fs::write(&shortcuts_path, yaml_string)?;
        println!("Updated shortcuts file: {shortcuts_path:?}");
    }

    Ok(())
}
