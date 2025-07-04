use hashbrown::HashMap;
use serde::Deserialize;
use std::process::Command as StdCommand;

use crate::script_runner::Shell;

use super::command::Command;

#[derive(Debug, Deserialize, Clone)]
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
            CommandTypes::Command(cmd) => {
                let shell = if cfg!(windows) {
                    let mut c = StdCommand::new("cmd");
                    c.arg("/C");
                    c
                } else {
                    let mut c = StdCommand::new("sh");
                    c.arg("-c");
                    c
                };

                let wrapped_shell = Shell::Parrent(shell);

                cmd.clone().execute(&mut wrapped_shell.into_child()
                    .map_err(|e| format!("Failed to spawn shell: {}", e))?, context).await
            },
            CommandTypes::Commands(cmds) => {
                // Determine shell command based on OS
                let (shell_cmd, shell_arg) = if cfg!(target_os = "windows") {
                    ("cmd", "/C")
                } else {
                    ("sh", "-c")
                };

                let mut shell = StdCommand::new(shell_cmd)
                    .arg(shell_arg)
                    .spawn()
                    .map_err(|e| format!("Failed to spawn shell: {}", e))?;

                for cmd in cmds {
                    // Execute each command in the context of the shell
                    let result = cmd.clone().execute(&mut shell, context).await;

                    if let Err(e) = result {
                        // If any command fails, return the error
                        return Err(format!("Command execution failed: {}", e));
                    }
                }
                
                Ok(None)
            }
        }
    }
}
