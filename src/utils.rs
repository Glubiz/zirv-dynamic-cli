use std::{collections::HashMap, path::PathBuf};

use crate::{shortcuts::Shortcuts, OperatingSystem};

pub fn operating_system(op: OperatingSystem) -> String {
    match op {
        OperatingSystem::Linux => "linux".to_string(),
        OperatingSystem::Windows => "windows".to_string(),
        OperatingSystem::MacOS => "macos".to_string(),
    }
}

/// Substitutes parameter placeholders in the command string.
/// Placeholders are of the form `${param_name}`.
pub fn substitute_params(command: &str, params: &HashMap<String, String>) -> String {
    let mut result = command.to_string();
    for (key, value) in params {
        let placeholder = format!("${{{}}}", key);
        result = result.replace(&placeholder, value);
    }
    result
}

/// Attempts to find a script file for the given command name in the .zirv directory.
/// First, it looks for a file named "{command}.yaml" (or .yml, .json, .toml). If not found,
/// it then loads .zirv/shortcuts.yaml to see if there is a mapping for that command.
/// If found, the file specified by the shortcut is returned.
pub fn find_script_file(base_name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base_dir = PathBuf::from(".zirv");
    let extensions = ["yaml", "yml", "json", "toml"];

    // First, try to find a file named "<base_name>.<ext>".
    for ext in &extensions {
        let path = base_dir.join(format!("{}.{}", base_name, ext));
        if path.exists() {
            return Ok(path);
        }
    }

    // If not found, try to load the shortcuts file.
    let shortcuts_path = base_dir.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        let content = std::fs::read_to_string(shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
        if let Some(mapped_file) = shortcuts.shortcuts.get(base_name) {
            // If the mapped file already has an extension, use it.
            let path = base_dir.join(mapped_file);
            if path.exists() {
                return Ok(path);
            }
            // Otherwise, try appending supported extensions.
            for ext in &extensions {
                let path = base_dir.join(format!("{}.{}", mapped_file, ext));
                if path.exists() {
                    return Ok(path);
                }
            }
        }
    }

    Err(format!("No script or shortcut found for '{}'", base_name).into())
}
