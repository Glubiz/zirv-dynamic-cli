use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command as TokioCommand;
use tokio::time::{Duration, sleep};

use super::options::Options;

/// Represents a single command in the YAML script.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Command {
    /// The shell command to execute.
    pub command: String,
    /// Optional argument defines varable names to capture from the command output.
    pub capture: Option<String>,
    /// An optional description of what the command does.
    pub description: Option<String>,
    /// Optional options that control the behavior of the command.
    pub options: Option<Options>,
}

impl Command {
    pub async fn execute(
        &mut self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        // OS filter
        if let Some(options) = &self.options {
            if let Some(os) = &options.operating_system {
                if !os.is_current() {
                    return Ok(Some("Command skipped due to OS filter".to_string()));
                }
            }
        }

        // Substitute parameters in the command string
        self.substitute_params(context);

        if let Some(rest) = self.command.trim_start().strip_prefix("cd ") {
            let dir = rest.trim();

            let mut path = std::path::PathBuf::new();
            if let Some(cwd) = context.get("cwd") {
                path.push(cwd);
            } else if let Ok(cwd) = std::env::current_dir() {
                path.push(cwd);
            }

            if std::path::Path::new(dir).is_absolute() {
                path = std::path::PathBuf::from(dir);
            } else {
                path.push(dir);
            }

            if let Ok(p) = path.canonicalize() {
                context.insert("cwd".to_string(), p.to_string_lossy().to_string());
            } else {
                return Err(format!("Failed to change directory to {dir}"));
            }

            return Ok(None);
        }

        let invoke = self.invoke(&self.command, context).await;

        if let Err(e) = invoke {
            if let Some(options) = &self.options {
                if let Some(commands) = &options.fallback {
                    for cmd in commands {
                        if let Err(fallback_error) = cmd.invoke().await {
                            return Err(format!(
                                "Command '{}' failed and fallback '{}' also failed: {}",
                                self.command, cmd.command, fallback_error
                            ));
                        }
                    }
                }

                if options.proceed_on_failure {
                    return Ok(Some(
                        "Command failed but proceeding due to options".to_string(),
                    ));
                }
            }
            return Err(format!("Command '{}' failed: {}", self.command, e));
        }

        if let Some(options) = &self.options {
            if let Some(d) = options.delay_ms {
                sleep(Duration::from_millis(d)).await;
            }
        }

        Ok(None)
    }

    async fn invoke(
        &self,
        command: &str,
        context: &mut HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Pick shell based on the OS
        let mut shell = if cfg!(windows) {
            let mut c = TokioCommand::new("cmd");
            c.arg("/C").arg(command);
            c
        } else {
            let mut c = TokioCommand::new("sh");
            c.arg("-c").arg(command);
            c
        };

        if let Some(cwd) = context.get("cwd") {
            shell.current_dir(cwd);
        }

        println!("Executing command: {command}");
        if let Some(description) = &self.description {
            println!("Description: {description}");
        }

        if let Some(options) = &self.options {
            if options.interactive {
                shell
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
        }

        if let Some(var) = &self.capture {
            let out = shell.output().await?;
            if !out.status.success() {
                return Err(format!("`{command}` failed").into());
            }

            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();

            context.insert(var.clone(), val);

            Ok(())
        } else {
            let status = shell.status().await?;

            if !status.success() {
                return Err(format!("`{command}` failed").into());
            }

            Ok(())
        }
    }

    fn substitute_params(&mut self, params: &HashMap<String, String>) -> &Self {
        for (key, value) in params {
            let placeholder = format!("${{{key}}}");
            self.command = self.command.replace(&placeholder, value);
        }

        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashMap;

    #[tokio::test]
    async fn test_sustitute_params() {
        let mut command = Command {
            command: "echo ${name} is ${age} years old".to_string(),
            capture: None,
            description: None,
            options: None,
        };

        let mut params = HashMap::new();
        params.insert("name".to_string(), "Alice".to_string());
        params.insert("age".to_string(), "30".to_string());

        command.substitute_params(&params);

        assert_eq!(command.command, "echo Alice is 30 years old");
    }
}
