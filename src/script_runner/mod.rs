use hashbrown::HashMap;
use script::Script;
use tokio::sync::mpsc::Sender;

mod command;
mod command_types;
mod fallback_command;
mod operating_system;
mod options;
pub mod script;
mod secret;

/// Events sent from the script runner to the UI layer.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A log line emitted from stdout or stderr of a command.
    Log { line: String, is_error: bool },
    /// Indicates a command has started executing.
    CommandStart { command: String },
    /// Indicates a command has finished executing with the given exit code.
    CommandEnd { status: i32 },
}

pub async fn execute(
    script: &Script,
    params: &[String],
    tx: Option<Sender<UiEvent>>,
) -> Result<(), String> {
    // Build the context from script parameters and secrets
    let mut context = build_context(script, params)?;

    // Execution loop
    script.run(&mut context, tx).await?;

    // Placeholder for the main execution logic
    // This function will orchestrate the execution of commands, handling files, etc.
    Ok(())
}

fn build_context(
    script: &Script,
    cli_params: &[String],
) -> Result<HashMap<String, String>, String> {
    // Build initial context from params + secrets
    let context: HashMap<String, String> = {
        // params
        let params = if let Some(names) = &script.params {
            if names.len() != cli_params.len() {
                return Err(format!(
                    "Expected {} parameters, got {}",
                    names.len(),
                    cli_params.len()
                ));
            }

            names
                .iter()
                .cloned()
                .zip(cli_params.iter().cloned())
                .collect()
        } else {
            HashMap::new()
        };

        // secrets
        let mut map = params;
        if let Some(secret_defs) = &script.secrets {
            for sd in secret_defs {
                let val = std::env::var(&sd.env_var).map_err(|_| {
                    format!("Secret '{}' not found in env '{}'", sd.name, sd.env_var)
                })?;
                map.insert(sd.name.clone(), val);
            }
        }
        map
    };

    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_runner::{command::Command, command_types::CommandTypes};

    #[tokio::test]
    async fn test_build_context() {
        let script = Script {
            name: "Test Script".to_string(),
            description: Some("A script for testing".to_string()),
            params: Some(vec!["param1".to_string(), "param2".to_string()]),
            secrets: Some(vec![secret::Secret {
                name: "commit_password".to_string(),
                env_var: "COMMIT_PASSWORD".to_string(),
            }]),
            commands: vec![CommandTypes::Command(Command {
                command: "echo 'Hello World'".to_string(),
                capture: None,
                description: Some("Prints Hello World".to_string()),
                options: None,
            })],
        };

        unsafe {
            std::env::set_var("COMMIT_PASSWORD", "secret123");
        }

        let context = build_context(&script, &["value1".to_string(), "value2".to_string()])
            .expect("Failed to build context");

        assert_eq!(context.get("param1"), Some(&"value1".to_string()));
        assert_eq!(context.get("param2"), Some(&"value2".to_string()));
        assert_eq!(
            context.get("commit_password"),
            Some(&"secret123".to_string())
        );
    }
}
