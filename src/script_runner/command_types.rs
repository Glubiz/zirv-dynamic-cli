use hashbrown::HashMap;
use serde::Deserialize;
use std::process::Command as StdCommand;

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
            },
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
        // Execute command directly in current process context
        let mut command = cmd.clone();
        command.substitute_params(context);

        let (shell_cmd, shell_arg) = if cfg!(target_os = "windows") {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        let output = StdCommand::new(shell_cmd)
            .arg(shell_arg)
            .arg(&command.command)
            .output()
            .map_err(|e| format!("Failed to execute command: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Command '{}' failed: {}", command.command, stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        
        // Handle capture if specified
        if let Some(capture_var) = &command.capture {
            let captured_value = stdout.trim().to_string();
            context.insert(capture_var.clone(), captured_value);
        }

        if !stdout.is_empty() {
            println!("{stdout}");
        }

        Ok(Some(stdout.to_string()))
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
            content.join(" & ")  // Chain commands with &
        } else {
            // Unix shell commands
            let mut content = script_lines;
            content.push("echo".to_string());
            content.push("echo 'All commands completed.'".to_string());
            content.push("read -p 'Press enter to close...'".to_string()); // Keep terminal open
            content.join(" && ")  // Chain commands with &&
        };

        // Open new terminal and execute commands directly without temp files
        let _child = if cfg!(target_os = "windows") {
            // Windows: Execute commands directly in new command window
            StdCommand::new("cmd")
                .args(["/C", "start", "cmd", "/K", &script_content])
                .spawn()
                .map_err(|e| format!("Failed to open new command window: {}", e))?
        } else if cfg!(target_os = "macos") {
            // macOS: Execute commands directly in new Terminal
            let escaped_content = script_content.replace("\"", "\\\"");
            StdCommand::new("osascript")
                .args([
                    "-e",
                    &format!("tell application \"Terminal\" to do script \"{}\"", escaped_content)
                ])
                .spawn()
                .map_err(|e| format!("Failed to open new terminal: {}", e))?
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

        println!("All {} commands launched in ONE new terminal window (no temp files)", cmds.len());

        Ok(Some(format!("All {} commands executed in new terminal", cmds.len())))
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
