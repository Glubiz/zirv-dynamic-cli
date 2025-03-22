use std::{fs, path::PathBuf};

use crate::{shortcuts::Shortcuts, Script};

pub fn show_help() -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = PathBuf::from(".zirv");
    if !base_dir.exists() {
        println!("No .zirv directory found.");
        return Ok(());
    }
    println!("Available Scripts:");

    let extensions = ["yaml", "yml", "json", "toml"];

    for entry in fs::read_dir(&base_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if extensions.contains(&ext) && path.file_name().unwrap() != ".shortcuts.yaml" {
                    let content = fs::read_to_string(&path)?;

                    // Parse the script.
                    let script: Script = if ext == "yaml" || ext == "yml" {
                        serde_yaml::from_str(&content)?
                    } else if ext == "json" {
                        serde_json::from_str(&content)?
                    } else if ext == "toml" {
                        toml::from_str(&content)?
                    } else {
                        return Err(format!("Unsupported file extension: {}", ext).into());
                    };

                    let file_name = path.file_name().unwrap().to_string_lossy();

                    println!("-------------------------------------------------");
                    println!("File: {}", file_name);

                    println!("  Name: {}", script.name);

                    if let Some(desc) = script.description {
                        println!("  Description: {}", desc);
                    }

                    if let Some(params) = &script.params {
                        println!("  Required Parameters:");
                        for param in params {
                            println!("    {}", param);
                        }
                    }

                    if !script.commands.is_empty() {
                        println!("  Commands:");
                        for (i, cmd) in script.commands.iter().enumerate() {
                            println!("    {}. {}", i + 1, cmd.command);
                            if let Some(d) = &cmd.description {
                                println!("       Description: {}", d);
                            }
                        }
                    }
                }
            }
        }
    }

    // List shortcuts if present.
    let shortcuts_path = base_dir.join(".shortcuts.yaml");

    if shortcuts_path.exists() {
        println!("\nAvailable Shortcuts:");

        let content = fs::read_to_string(shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;

        for (key, value) in shortcuts.shortcuts {
            println!("  {} -> {}", key, value);
        }

        println!("  h -> help");
    }

    Ok(())
}
