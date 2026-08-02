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
    #[serde(deserialize_with = "super::command_types::deserialize_steps")]
    pub commands: Vec<CommandTypes>,
}

impl Script {
    pub async fn run(
        &self,
        context: &mut HashMap<String, String>,
        dry_run: bool,
    ) -> Result<(), String> {
        let total = self.commands.len();
        for (index, step) in self.commands.iter().enumerate() {
            let cmd_display = step.display(context);
            if dry_run {
                crate::output::dry_run(index, total, &cmd_display);
                continue;
            }
            crate::output::step(index, total, &cmd_display);
            if let Some(desc) = step.description() {
                crate::output::step_description(&desc);
            }
            match step.execute(context).await {
                Ok(Some(output)) => {
                    crate::output::skipped(&output);
                }
                Ok(None) => {}
                Err(e) => {
                    crate::output::error(format!(
                        "step {}/{} in script '{}': {}",
                        index + 1,
                        total,
                        self.name,
                        e
                    ));
                    return Err(e);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::script_runner::agent_command::AgentCommand;
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

        let result = script.run(&mut context, false).await;
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

        let result = script.run(&mut context, false).await;
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

        let result = script.run(&mut context, false).await;
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

        let result = script.run(&mut context, false).await;
        assert!(result.is_ok());
    }

    /// Dry-run must never execute the agent step: `codex` has no ready
    /// adapter, so a real invocation would error, but a dry run only prints
    /// the step and must still succeed.
    #[tokio::test]
    async fn test_script_dry_run_never_executes_an_agent_step() {
        let script = Script {
            name: "Agent Dry Run".to_string(),
            description: None,
            params: None,
            secrets: None,
            commands: vec![CommandTypes::Agent(AgentCommand {
                agent: "codex".to_string(),
                prompt: "do the work".to_string(),
                flags: None,
                description: None,
                options: None,
                capture: None,
            })],
        };

        let mut context = HashMap::new();
        let result = script.run(&mut context, true).await;
        assert!(result.is_ok(), "dry run must not execute the agent step");
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

        let result = script.run(&mut context, false).await;
        assert!(result.is_ok());
    }
}
