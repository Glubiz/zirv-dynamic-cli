use std::path::{Path, PathBuf};

use clap::Parser;

use crate::utils::{SCRIPT_DIR_NAME, SUPPORTED_EXTENSIONS, Shortcuts, home_dir};

#[derive(Debug, Parser)]
pub struct Input {
    /// A descriptive name for the script.
    pub command: String,
    /// Optional parameters (positional arguments) that will be mapped to the script's expected params.
    #[arg(num_args = 0..)]
    pub params: Vec<String>,
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
        let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
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

        Err(format!("No script or shortcut found for '{}'", self.command).into())
    }
}
