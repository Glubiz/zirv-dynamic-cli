use std::fs::File;
use std::io::Write;
use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{command::Command, script::Script};
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
        tx: Option<tokio::sync::mpsc::Sender<super::UiEvent>>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => cmd.clone().execute(context, tx).await,
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

                let temp_script = Script {
                    name: "zirv_concurrency".to_string(),
                    description: None,
                    params: None,
                    secrets: None,
                    commands: substituted.into_iter().map(CommandTypes::Command).collect(),
                };

                let yaml = serde_yaml::to_string(&temp_script).map_err(|e| e.to_string())?;
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_nanos();
                let path = std::env::temp_dir().join(format!("zirv_{timestamp}.yaml"));
                let mut file = File::create(&path).map_err(|e| e.to_string())?;
                file.write_all(yaml.as_bytes()).map_err(|e| e.to_string())?;

                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let cwd = context.get("cwd").cloned().unwrap_or_else(|| {
                    std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        .to_string_lossy()
                        .to_string()
                });

                let exec_cmd = if cfg!(windows) {
                    format!(
                        "cd /d \"{}\" && \"{}\" \"{}\"",
                        cwd,
                        exe.display(),
                        path.display()
                    )
                } else {
                    format!(
                        "cd \"{}\" && \"{}\" \"{}\"",
                        cwd,
                        exe.display(),
                        path.display()
                    )
                };

                spawn_terminal(&exec_cmd)?;
                Ok(None)
            }
        }
    }
}

fn spawn_terminal(command: &str) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        StdCommand::new("cmd")
            .args(["/C", "start", "cmd", "/K", command])
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
        // Try gnome-terminal, x-terminal-emulator, then xterm
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
