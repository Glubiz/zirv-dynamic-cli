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
        self.commands.iter().any(|step| match step {
            // Review round 2: a step `run` will skip for this platform
            // (`Options::skip_for_os`) never spawns a supervisor, so it must
            // not lower the process either.
            CommandTypes::Agent(agent) => !agent
                .options
                .as_ref()
                .is_some_and(super::options::Options::skip_for_os),
            _ => false,
        })
    }

    pub async fn run(
        &self,
        context: &mut HashMap<String, String>,
        dry_run: bool,
    ) -> Result<(), String> {
        let mut display_context = super::build_display_context(self, context);
        let total = self.commands.len();
        for (index, step) in self.commands.iter().enumerate() {
            let cmd_display = step.display(&display_context);
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
                    display_context.insert(var.to_string(), format!("<capture:{var}>"));
                }
                continue;
            }
            crate::output::step(index, total, &cmd_display);
            if let Some(desc) = step.description() {
                crate::output::step_description(&desc);
            }
            match step.execute(context, &mut display_context).await {
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

    fn script_with_secret(command: &str) -> Script {
        Script {
            name: "Secret Script".to_string(),
            description: None,
            params: None,
            secrets: Some(vec![Secret {
                name: "token".to_string(),
                env_var: "API_TOKEN".to_string(),
            }]),
            commands: vec![CommandTypes::Command(Command {
                command: command.to_string(),
                capture: Some("result".to_string()),
                description: None,
                options: None,
            })],
        }
    }

    #[tokio::test]
    async fn test_script_run_with_secrets() {
        let script = script_with_secret("echo '${token}'");
        let mut context = HashMap::from([("token".to_string(), "my_secret_password".to_string())]);

        script.run(&mut context, false).await.unwrap();

        assert_eq!(context["result"], "my_secret_password");
    }

    #[test]
    fn step_display_masks_secrets_and_preserves_params_and_captured_values() {
        let mut script = script_with_secret("echo '${token}' ${param} ${captured}");
        let context = HashMap::from([
            ("token".to_string(), "secret-${param}".to_string()),
            ("param".to_string(), "value-${token}".to_string()),
            ("captured".to_string(), "child-output".to_string()),
        ]);
        script.commands.push(CommandTypes::Agent(AgentCommand {
            agent: "claude".to_string(),
            prompt: "use ${token} with ${param} ${captured}".to_string(),
            flags: None,
            description: None,
            options: None,
            capture: None,
        }));
        let display_context = super::super::build_display_context(&script, &context);

        for step in &script.commands {
            let display = step.display(&display_context);
            assert!(display.contains("${token}"), "{display}");
            assert!(!display.contains(&context["token"]), "{display}");
            assert!(display.contains("value-${token}"), "{display}");
            assert!(display.contains("child-output"), "{display}");
        }
    }

    #[tokio::test]
    async fn dry_run_display_masks_secrets_and_keeps_capture_stand_ins() {
        let mut script = script_with_secret("echo '${token}'");
        script.commands.push(CommandTypes::Command(Command {
            command: "echo '${token}' ${result}".to_string(),
            capture: None,
            description: None,
            options: None,
        }));
        let mut context = HashMap::from([("token".to_string(), "my_secret_password".to_string())]);

        script.run(&mut context, true).await.unwrap();
        let display_context = super::super::build_display_context(&script, &context);
        let display = script.commands[1].display(&display_context);

        assert_eq!(display, "echo '${token}' <capture:result>");
        assert!(!display.contains(&context["token"]));
        assert_eq!(context["token"], "my_secret_password");
    }

    #[tokio::test]
    async fn command_failure_masks_secrets_with_and_without_capture() {
        for capture in [None, Some("result".to_string())] {
            let mut script = script_with_secret("echo '${token}'; exit 1");
            let CommandTypes::Command(command) = &mut script.commands[0] else {
                unreachable!()
            };
            command.capture = capture;
            let mut context =
                HashMap::from([("token".to_string(), "my_secret_password".to_string())]);

            let error = script.run(&mut context, false).await.unwrap_err();

            assert!(error.contains("echo '${token}'; exit 1"), "{error}");
            assert!(error.contains("exit code 1"), "{error}");
            assert!(!error.contains(&context["token"]), "{error}");
        }
    }

    #[tokio::test]
    async fn fallback_failure_masks_secrets_in_both_commands() {
        let mut script = script_with_secret("echo '${token}'; exit 1");
        let CommandTypes::Command(command) = &mut script.commands[0] else {
            unreachable!()
        };
        command.options = Some(super::super::options::Options {
            fallback: Some(vec![super::super::fallback_command::FallbackCommand {
                command: "echo '${token}'; exit 2".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let mut context = HashMap::from([("token".to_string(), "my_secret_password".to_string())]);

        let error = script.run(&mut context, false).await.unwrap_err();

        assert!(
            error.contains("fallback 'echo '${token}'; exit 2' also failed"),
            "{error}"
        );
        assert!(!error.contains(&context["token"]), "{error}");
    }

    #[tokio::test]
    async fn failed_directory_change_masks_the_secret_directory() {
        let script = script_with_secret("cd ${token}");
        let mut context = HashMap::from([(
            "token".to_string(),
            "nonexistent-secret-directory".to_string(),
        )]);

        let error = script.run(&mut context, false).await.unwrap_err();

        assert!(error.contains("${token}"), "{error}");
        assert!(!error.contains(&context["token"]), "{error}");
    }

    #[tokio::test]
    async fn a_secret_directory_stays_masked_when_referenced_as_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let secret_dir = tmp.path().join("secret-directory");
        std::fs::create_dir(&secret_dir).unwrap();
        let script = script_with_secret("cd ${token}");
        let mut context = HashMap::from([(
            "token".to_string(),
            secret_dir.to_string_lossy().to_string(),
        )]);
        let mut display_context = super::super::build_display_context(&script, &context);

        script.commands[0]
            .execute(&mut context, &mut display_context)
            .await
            .unwrap();
        let display = super::super::command::substitute("echo ${cwd}", &display_context);

        assert!(display.contains("${token}"), "{display}");
        assert!(!display.contains("secret-directory"), "{display}");
        assert_eq!(
            context["cwd"],
            secret_dir.canonicalize().unwrap().to_string_lossy()
        );
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

        let other_os = if cfg!(windows) {
            crate::script_runner::operating_system::OperatingSystem::Linux
        } else {
            crate::script_runner::operating_system::OperatingSystem::Windows
        };
        let filtered_out = Script {
            commands: vec![CommandTypes::Agent(AgentCommand {
                agent: "claude".to_string(),
                prompt: "do the work".to_string(),
                flags: None,
                description: None,
                options: Some(crate::script_runner::options::Options {
                    operating_system: Some(other_os),
                    ..Default::default()
                }),
                capture: None,
            })],
            ..shell_only.clone()
        };
        assert!(
            !filtered_out.has_agent_step(),
            "an agent step skipped for this platform never spawns a supervisor"
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
