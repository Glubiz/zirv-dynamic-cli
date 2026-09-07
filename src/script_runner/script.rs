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
    /// Whether any step of this script delegates to a supervised agent.
    ///
    /// Issue #330: an `agent:` step runs its supervisor IN this process, so a
    /// script that has one makes the whole script-runner process a worker
    /// supervisor -- and the harness it spawns, plus every cargo run under
    /// that harness, inherits this process's scheduling class. `main` asks
    /// this before it runs the script, which is the only seam on that path
    /// no unit test drives (`Script::run` and the step's own `execute` are
    /// both exercised in-process). Pure, so the decision itself stays
    /// testable without spawning anything.
    pub fn has_agent_step(&self) -> bool {
        self.commands
            .iter()
            .any(|step| matches!(step, CommandTypes::Agent(_)))
    }

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
                // A-2/D-4: a dry run that reports success for a script the
                // real run refuses is worse than no dry run at all -- the
                // same principle `AgentCommand::validate` applies at load
                // time, for the one check that needs the resolved context.
                if let Err(e) = step.check(context) {
                    crate::output::error(format!(
                        "step {}/{} in script '{}': {}",
                        index + 1,
                        total,
                        self.name,
                        e
                    ));
                    return Err(e);
                }
                // R7: only `execute` registers a `capture:` variable, so
                // without this every later step naming one was rejected as
                // unresolved -- a dry run refusing a script the real run
                // completes. The stand-in is deliberately visible in the
                // printed command line: a dry run must never look like it
                // knows a value it cannot have.
                if let Some(var) = step.captured_var() {
                    context.insert(var.to_string(), format!("<capture:{var}>"));
                }
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

    /// A-2/D-4: `--dry-run` printed each step and moved on without ever
    /// consulting `context`, so `echo ${missing}` dry-ran with exit 0 while
    /// the real run failed on the same script. `AgentCommand::validate` is
    /// already called at load time for exactly this reason -- the two modes
    /// must reject the same scripts.
    #[tokio::test]
    async fn a_dry_run_rejects_an_unresolved_placeholder_the_real_run_rejects() {
        let script = Script {
            name: "Unresolved".to_string(),
            description: None,
            params: None,
            secrets: None,
            commands: vec![CommandTypes::Command(Command {
                command: "echo ${missing}".to_string(),
                capture: None,
                description: None,
                options: None,
            })],
        };

        let dry = script.run(&mut HashMap::new(), true).await;
        let real = script.run(&mut HashMap::new(), false).await;

        assert!(real.is_err(), "the real run must reject it: {real:?}");
        let message = dry.expect_err("the dry run must reject it too");
        assert!(message.contains("missing"), "got: {message}");
    }

    /// Review round 1 (R7): the other half of the same rule. Only `execute`
    /// registers a `capture:` variable, so a dry run rejected every later step
    /// that referenced one -- a script the real run completes cleanly could
    /// not be dry-run at all.
    #[tokio::test]
    async fn a_dry_run_accepts_a_placeholder_captured_by_an_earlier_step() {
        let script = Script {
            name: "Captured".to_string(),
            description: None,
            params: None,
            secrets: None,
            commands: vec![
                CommandTypes::Command(Command {
                    command: "echo hello".to_string(),
                    capture: Some("greeting".to_string()),
                    description: None,
                    options: None,
                }),
                CommandTypes::Command(Command {
                    command: "echo ${greeting}".to_string(),
                    capture: None,
                    description: None,
                    options: None,
                }),
            ],
        };

        let mut context = HashMap::new();
        script
            .run(&mut context, true)
            .await
            .expect("a dry run resolves what an earlier step captures");
        assert_eq!(
            context.get("greeting").map(String::as_str),
            Some("<capture:greeting>"),
            "the placeholder is visibly a dry-run stand-in, not a real value"
        );
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

    /// Issue #330: `main` lowers the script-runner process's scheduling class
    /// for a script that delegates to an agent, and only for such a script --
    /// an ordinary shell script is the operator's own foreground work and
    /// must keep the class it was launched with.
    #[test]
    fn only_a_script_with_an_agent_step_reports_one() {
        let shell_only = Script {
            name: "Shell".to_string(),
            description: None,
            params: None,
            secrets: None,
            commands: vec![CommandTypes::Command(Command {
                command: "echo hello".to_string(),
                capture: None,
                description: None,
                options: None,
            })],
        };
        assert!(!shell_only.has_agent_step());

        let empty = Script {
            commands: vec![],
            ..shell_only.clone()
        };
        assert!(!empty.has_agent_step());

        let with_agent = Script {
            commands: vec![
                CommandTypes::Command(Command {
                    command: "echo hello".to_string(),
                    capture: None,
                    description: None,
                    options: None,
                }),
                CommandTypes::Agent(AgentCommand {
                    agent: "claude".to_string(),
                    prompt: "do the work".to_string(),
                    flags: None,
                    description: None,
                    options: None,
                    capture: None,
                }),
            ],
            ..shell_only.clone()
        };
        assert!(
            with_agent.has_agent_step(),
            "an agent step anywhere in the script counts, not just the first"
        );
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
