use std::{env, fs, path::PathBuf};

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use crate::script_runner::script::Script;

pub const SUPPORTED_EXTENSIONS: &[&str] = &["yaml", "yml", "json", "toml"];
pub const SCRIPT_DIR_NAME: &str = ".zirv";

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct Shortcuts {
    pub shortcuts: HashMap<String, String>,
}

pub fn home_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| "Could not determine home directory".into())
}

pub fn parse_script_content(
    content: &str,
    ext: &str,
) -> Result<Script, Box<dyn std::error::Error>> {
    let script: Script = match ext {
        "yaml" | "yml" => serde_yaml::from_str(content)?,
        "json" => serde_json::from_str(content)?,
        "toml" => toml::from_str(content)?,
        other => return Err(format!("Unsupported extension: {other}").into()),
    };
    Ok(script)
}

pub fn file_to_script(path: &PathBuf) -> Result<Script, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    parse_script_content(&content, &ext)
}
