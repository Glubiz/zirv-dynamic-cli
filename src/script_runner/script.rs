use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use super::{command_types::CommandTypes, secret::Secret};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Script {
    /// A descriptive name for the script.
    pub name: String,
    // A description of what the script does.
    pub description: Option<String>,
    /// Optional list of expected parameter names (in order).
    pub params: Option<Vec<String>>,
    /// Optional list of secret definitions.
    pub secrets: Option<Vec<Secret>>,
    /// A list of commands to execute.
    pub commands: Vec<CommandTypes>,
}

impl Script {
    pub async fn run(&self, context: &mut HashMap<String, String>) -> Result<(), String> {
        // Spawned task handles for concurrent command groups
        let mut handles = Vec::new();

        for step in &self.commands {
            match step {
                CommandTypes::Command(cmd) => {
                    // Run sequential command and propagate errors immediately
                    if let Err(e) = cmd.clone().execute(context).await {
                        return Err(format!(
                            "Error executing command in script '{}': {}",
                            self.name, e
                        ));
                    }
                }
                CommandTypes::Commands(cmds) => {
                    // Each command group runs in its own context and task
                    let cmds_clone = cmds.clone();
                    let mut ctx_clone = context.clone();
                    handles.push(tokio::spawn(async move {
                        for mut c in cmds_clone {
                            c.execute(&mut ctx_clone).await?;
                        }
                        Ok::<(), String>(())
                    }));
                }
            }
        }

        // Await all concurrent groups and surface any errors
        for handle in handles {
            handle.await.map_err(|e| e.to_string())??;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::script_runner::command::Command;

    use super::*;

    #[tokio::test]
    async fn test_script_run() {
        let script = Script {
            name: "Test Script".to_string(),
            description: Some("A script for testing".to_string()),
            params: None,
            secrets: None,
            commands: vec![CommandTypes::Command(Command {
                command: "echo 'Hello World'".to_string(),
                capture: None,
                description: Some("Prints Hello World".to_string()),
                options: None,
            })],
        };

        let mut context = HashMap::new();

        let result = script.run(&mut context).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_script_run_with_multiple_commands() {
        let script = Script {
            name: "Multi Command Script".to_string(),
            description: Some("A script with multiple commands".to_string()),
            params: None,
            secrets: None,
            commands: vec![
                CommandTypes::Command(Command {
                    command: "echo 'First Command'".to_string(),
                    capture: None,
                    description: Some("Prints First Command".to_string()),
                    options: None,
                }),
                CommandTypes::Command(Command {
                    command: "echo 'Second Command'".to_string(),
                    capture: None,
                    description: Some("Prints Second Command".to_string()),
                    options: None,
                }),
            ],
        };

        let mut context = HashMap::new();

        let result = script.run(&mut context).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_script_run_with_secrets() {
        let script = Script {
            name: "Secret Script".to_string(),
            description: Some("A script that uses secrets".to_string()),
            params: None,
            secrets: Some(vec![Secret {
                name: "commit_password".to_string(),
                env_var: "COMMIT_PASSWORD".to_string(),
            }]),
            commands: vec![CommandTypes::Command(Command {
                command: "echo $COMMIT_PASSWORD".to_string(),
                capture: None,
                description: Some("Prints the commit password".to_string()),
                options: None,
            })],
        };

        let mut context = HashMap::new();
        context.insert(
            "COMMIT_PASSWORD".to_string(),
            "my_secret_password".to_string(),
        );

        let result = script.run(&mut context).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_script_run_with_params() {
        let script = Script {
            name: "Param Script".to_string(),
            description: Some("A script that uses parameters".to_string()),
            params: Some(vec!["param1".to_string(), "param2".to_string()]),
            secrets: None,
            commands: vec![CommandTypes::Command(Command {
                command: "echo $param1 $param2".to_string(),
                capture: None,
                description: Some("Prints parameters".to_string()),
                options: None,
            })],
        };

        let mut context = HashMap::new();
        context.insert("param1".to_string(), "value1".to_string());
        context.insert("param2".to_string(), "value2".to_string());

        let result = script.run(&mut context).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_script_run_with_empty_commands() {
        let script = Script {
            name: "Empty Commands Script".to_string(),
            description: Some("A script with no commands".to_string()),
            params: None,
            secrets: None,
            commands: vec![],
        };

        let mut context = HashMap::new();

        let result = script.run(&mut context).await;
        assert!(result.is_ok());
    }
}
