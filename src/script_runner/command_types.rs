use std::process::Command as StdCommand;

use super::command::Command;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    Command(Command),
    Commands(Vec<Command>),
}

impl CommandTypes {
    pub async fn execute(
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => cmd.execute(context).await,
            CommandTypes::Commands(cmds) => {
                if cmds.is_empty() {
                    return Ok(None);
                }

                let mut substituted = cmds.clone();
                for cmd in &mut substituted {
                    for (key, value) in context.iter() {
                        let placeholder = format!("${{{key}}}");
                        cmd.command = cmd.command.replace(&placeholder, value);
                    }
                }

                let joined = substituted
                    .into_iter()
                    .map(|c| c.command)
                    .collect::<Vec<_>>()
                    .join(" && ");

                let cwd = context.get("cwd").cloned().unwrap_or_else(|| {
                    std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        .to_string_lossy()
                        .to_string()
                });

                if cfg!(target_os = "windows") {
                    spawn_terminal_windows(&joined, &cwd)
                } else if cfg!(target_os = "macos") {
                    let full_cmd = format!("cd '{}' ; {}", escape_single_quotes(&cwd), joined);
                    spawn_terminal_macos(&full_cmd)
                } else {
                    spawn_terminal_linux(&cwd, &joined)
                }?;

                Ok(None)
            }
        }
    }
}

fn spawn_terminal_windows(command: &str, working_dir: &str) -> Result<(), String> {
    StdCommand::new("cmd")
        .args(["/C", "start", "", "/D", working_dir, "cmd", "/K", command])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn spawn_terminal_macos(command: &str) -> Result<(), String> {
    let applescript_cmd = format!(
        r#"tell application "Terminal"
activate
do script "{}"
end tell"#,
        escape_for_applescript(command)
    );

    StdCommand::new("osascript")
        .arg("-e")
        .arg(applescript_cmd)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn spawn_terminal_linux(cwd: &str, joined: &str) -> Result<(), String> {
    let fallback_cmd = format!(
        "cd '{}' ; {} ; exec bash",
        escape_single_quotes(cwd),
        joined
    );

    if StdCommand::new("gnome-terminal")
        .args([
            "--working-directory",
            cwd,
            "--",
            "bash",
            "-lc",
            &format!("{} ; exec bash", joined),
        ])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }

    if StdCommand::new("x-terminal-emulator")
        .args(["-e", "bash", "-lc", &fallback_cmd])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }

    StdCommand::new("xterm")
        .args(["-hold", "-e", "bash", "-lc", &fallback_cmd])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn escape_for_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', r#"'\''"#)
}
