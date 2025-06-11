use std::{fs, path::PathBuf};

use crate::script_runner::script::Script;

pub fn file_to_script(path: &PathBuf) -> Result<Script, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    let script: Script = match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_str(&content)?,
        "json" => serde_json::from_str(&content)?,
        "toml" => toml::from_str(&content)?,
        other => return Err(format!("Unsupported extension: {}", other).into()),
    };

    Ok(script)
}
