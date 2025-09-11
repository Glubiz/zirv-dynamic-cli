use std::process::Command as StdCommand;

use super::command::Command;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    /// A command defined in the script.
    Command(Command),
    /// A set of commands that should be executed together.
    Commands(Vec<Command>),
}

impl CommandTypes {
    pub async fn execute(
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => cmd.clone().execute(context).await,
            CommandTypes::Commands(cmds) => {
                if cmds.is_empty() {
                    return Ok(None);
                }

                // Substitute parameters for each command before spawning the shell
                let mut substituted = cmds.clone();
                for cmd in &mut substituted {
                    for (key, value) in context.iter() {
                        let placeholder = format!("${{{key}}}");
                        cmd.command = cmd.command.replace(&placeholder, value);
                    }
                }

                let command_str = substituted
                    .iter()
                    .map(|c| c.command.clone())
                    .collect::<Vec<String>>()
                    .join(" && ");

                spawn_terminal(&command_str)?;
                Ok(None)
            }
        }
    }
}

fn spawn_terminal(command: &str) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        // Run the entire chain inside the new cmd window:
        // cmd /C start "" cmd /K <command>
        // - Empty title "" avoids the first quoted token being treated as the title.
        // - Pass tokens separately to avoid mis-parsing.
        StdCommand::new("cmd")
            .args([
                "/C", "start", "", // window title
                "cmd", "/K",
                command, // full chained command, e.g. `cd src && dir && echo done`
            ])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else if cfg!(target_os = "macos") {
        StdCommand::new("osascript")
            .arg("-e")
            .arg(format!(
                "tell application \"Terminal\" to do script \"{}\"",
                command.replace('"', "\\\"")
            ))
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else {
        // Try common Linux terminals
        if StdCommand::new("gnome-terminal")
            .arg("--")
            .arg("bash")
            .arg("-c")
            .arg(format!("{command}; exec bash"))
            .spawn()
            .is_ok()
            || StdCommand::new("x-terminal-emulator")
                .arg("-e")
                .arg("bash")
                .arg("-c")
                .arg(format!("{command}; exec bash"))
                .spawn()
                .is_ok()
        {
            Ok(())
        } else {
            StdCommand::new("xterm")
                .arg("-hold")
                .arg("-e")
                .arg("bash")
                .arg("-c")
                .arg(format!("{command}; exec bash"))
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }
}
