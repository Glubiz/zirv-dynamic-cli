use std::{env, path::PathBuf};

use clap::Parser;
use serde::Deserialize;

use hashbrown::HashMap;

/// Structure representing the shortcuts mapping file.
#[derive(Debug, Deserialize)]
struct Shortcuts {
    /// A mapping of shortcut keys to script file names.
    shortcuts: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Parser)]
pub struct Input {
    /// A descriptive name for the script.
    pub command: String,
    /// Optional parameters (positional arguments) that will be mapped to the script's expected params.
    #[arg(num_args = 0..)]
    pub params: Vec<String>,
}

impl Input {
    pub fn get_file_path(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let cmd_path = PathBuf::from(&self.command);
        if cmd_path.exists() {
            return Ok(cmd_path.canonicalize()?);
        }

        let base_dir = PathBuf::from(".zirv");
        let extensions = ["yaml", "yml", "json", "toml"];

        for ext in &extensions {
            let path = base_dir.join(format!("{}.{}", &self.command, ext));
            if path.exists() {
                return Ok(path.canonicalize()?);
            }
        }

        let shortcuts_path = base_dir.join(".shortcuts.yaml");
        if shortcuts_path.exists() {
            let content = std::fs::read_to_string(&shortcuts_path)?;
            let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
            if let Some(mapped_file) = shortcuts.shortcuts.get(&self.command) {
                let path = base_dir.join(mapped_file);
                if path.exists() {
                    return Ok(path.canonicalize()?);
                }
                for ext in &extensions {
                    let path = base_dir.join(format!("{mapped_file}.{ext}"));
                    if path.exists() {
                        return Ok(path.canonicalize()?);
                    }
                }
            }
        }
        let home_dir = PathBuf::from(env::var("HOME").or_else(|_| env::var("USERPROFILE"))?);

        let root = home_dir.join(".zirv");
        for ext in &extensions {
            let path = root.join(format!("{}.{}", self.command, ext));
            if path.exists() {
                return Ok(path.canonicalize()?);
            }
        }

        let shortcuts_path = root.join(".shortcuts.yaml");
        if shortcuts_path.exists() {
            let content = std::fs::read_to_string(&shortcuts_path)?;
            let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
            if let Some(mapped_file) = shortcuts.shortcuts.get(&self.command) {
                let path = root.join(mapped_file);
                if path.exists() {
                    return Ok(path.canonicalize()?);
                }
                for ext in &extensions {
                    let path = root.join(format!("{mapped_file}.{ext}"));
                    if path.exists() {
                        return Ok(path.canonicalize()?);
                    }
                }
            }
        }

        Err(format!("No script or shortcut found for '{}'", self.command).into())
    }
}
