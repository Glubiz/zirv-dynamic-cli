use hashbrown::HashMap;
use serde::Deserialize;
use std::process::{Command as StdCommand, Stdio};

use super::command::Command;

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    /// A command defined in the script - runs in current shell
    Command(Command),
    /// A set of commands that should be executed together in a new child shell
    Commands(Vec<Command>),
}

impl CommandTypes {
    pub async fn execute(
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => {
                // Run single command in current shell (direct execution)
                self.execute_in_current_shell(cmd, context).await
            }
            CommandTypes::Commands(cmds) => {
                // Run multiple commands in a new child shell
                self.execute_in_child_shell(cmds, context).await
            }
        }
    }

    async fn execute_in_current_shell(
        &self,
        cmd: &Command,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        let mut shell = if cfg!(windows) {
            let mut c = StdCommand::new("cmd");
            c.arg("/C").arg(cmd.command.clone());
            c
        } else {
            let mut c = StdCommand::new("sh");
            c.arg("-c").arg(cmd.command.clone());
            c
        };

        println!("Executing command: {}", cmd.command);
        if let Some(description) = &cmd.description {
            println!("Description: {description}");
        }

        if let Some(options) = &cmd.options {
            if options.interactive {
                shell
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
        }

        if let Some(var) = &cmd.capture {
            let out = shell.output().ok();

            if out.is_none() {
                return Err(format!("Failed to execute command: {}", cmd.command));
            }

            let out = out.unwrap();
            
            if ! out.status.success() {
                return Err(format!("`{}` failed", cmd.command));
            }

            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();

            context.insert(var.clone(), val);

            Ok(None)
        } else {
            let status = shell.status();

            if !status.unwrap().success() {
                return Err(format!("`{}` failed", cmd.command));
            }

            Ok(None)
        }
    }

    async fn execute_in_child_shell(
        &self,
        cmds: &[Command],
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        // Create a single script with all commands
        let mut script_lines = Vec::new();

        for cmd in cmds {
            let mut command = cmd.clone();
            command.substitute_params(context);
            script_lines.push(command.command);
        }

        // Create the script content as a string
        let script_content = if cfg!(target_os = "windows") {
            // Windows batch commands
            let mut content = vec!["@echo off".to_string()];
            content.extend(script_lines);
            content.push("echo.".to_string());
            content.push("echo All commands completed.".to_string());
            content.push("pause".to_string()); // Keep window open
            content.join(" & ") // Chain commands with &
        } else {
            // Unix shell commands
            let mut content = script_lines;
            content.push("echo".to_string());
            content.push("echo 'All commands completed.'".to_string());
            content.push("read -p 'Press enter to close...'".to_string()); // Keep terminal open
            content.join(" && ") // Chain commands with &&
        };

        // Open new terminal and execute commands directly without temp files
        let _child = if cfg!(target_os = "windows") {
            // Windows: Execute commands directly in new command window
            StdCommand::new("cmd")
                .args(["/C", "start", "cmd", "/K", &script_content])
                .spawn()
                .map_err(|e| format!("Failed to open new command window: {e}"))?
        } else if cfg!(target_os = "macos") {
            // macOS: Execute commands directly in new Terminal
            let escaped_content = script_content.replace("\"", "\\\"");
            StdCommand::new("osascript")
                .args([
                    "-e",
                    &format!("tell application \"Terminal\" to do script \"{escaped_content}\""),
                ])
                .spawn()
                .map_err(|e| format!("Failed to open new terminal: {e}"))?
        } else {
            // Linux: Execute commands directly in new terminal
            let terminals = [
                ("gnome-terminal", vec!["--", "bash", "-c", &script_content]),
                ("xterm", vec!["-e", "bash", "-c", &script_content]),
                ("konsole", vec!["-e", "bash", "-c", &script_content]),
                ("terminator", vec!["-e", "bash", "-c", &script_content]),
                ("alacritty", vec!["-e", "bash", "-c", &script_content]),
            ];

            let mut child = None;

            for (terminal, args) in &terminals {
                if let Ok(c) = StdCommand::new(terminal).args(args).spawn() {
                    child = Some(c);
                    break;
                }
            }

            child.ok_or("No suitable terminal emulator found")?
        };

        println!(
            "All {} commands launched in ONE new terminal window (no temp files)",
            cmds.len()
        );

        Ok(Some(format!(
            "All {} commands executed in new terminal",
            cmds.len()
        )))
    }
}

// Add this helper method to Command if it doesn't exist
impl Command {
    fn substitute_params(&mut self, params: &HashMap<String, String>) {
        for (key, value) in params {
            let placeholder = format!("${{{key}}}");
            self.command = self.command.replace(&placeholder, value);
        }
    }
}
