use std::process::Command as StdCommand;

use super::agent_command::AgentCommand;
use super::command::Command;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    Command(Command),
    Commands(Vec<Command>),
    Agent(AgentCommand),
}

impl CommandTypes {
    pub fn display(&self, context: &HashMap<String, String>) -> String {
        match self {
            CommandTypes::Command(cmd) => cmd.substituted_command(context),
            CommandTypes::Commands(cmds) => {
                let joined = cmds
                    .iter()
                    .map(|c| c.command.as_str())
                    .collect::<Vec<_>>()
                    .join(" && ");
                format!("[multi-shell] {joined}")
            }
            CommandTypes::Agent(agent) => agent.display(context),
        }
    }

    pub fn description(&self) -> Option<String> {
        match self {
            CommandTypes::Command(cmd) => cmd.description.clone(),
            CommandTypes::Commands(_) => None,
            CommandTypes::Agent(agent) => agent.description.clone(),
        }
    }

    pub async fn execute(
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => cmd.execute(context).await,
            CommandTypes::Agent(agent) => agent.execute(context).await,
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

                let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
                for cmd in &substituted {
                    let unresolved: Vec<&str> = re
                        .captures_iter(&cmd.command)
                        .map(|c| c.get(1).unwrap().as_str())
                        .collect();
                    if !unresolved.is_empty() {
                        return Err(format!(
                            "Unresolved placeholders in '{}': {}",
                            cmd.command,
                            unresolved.join(", ")
                        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_step_parses_from_yaml() {
        let yaml = r#"
name: test
commands:
  - command: cargo test
  - agent: claude
    prompt: "Fix the failing tests in ${dir}"
    flags: ["--model", "sonnet"]
  - command: cargo test
"#;
        let script: crate::script_runner::script::Script =
            serde_yaml_ng::from_str(yaml).expect("valid script");
        assert_eq!(script.commands.len(), 3);
        assert!(matches!(script.commands[0], CommandTypes::Command(_)));
        assert!(matches!(script.commands[2], CommandTypes::Command(_)));
        match &script.commands[1] {
            CommandTypes::Agent(agent) => {
                assert_eq!(agent.agent, "claude");
                assert_eq!(agent.prompt, "Fix the failing tests in ${dir}");
                assert_eq!(
                    agent.flags,
                    Some(vec!["--model".to_string(), "sonnet".to_string()])
                );
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn agent_step_display_substitutes_the_prompt() {
        let step = CommandTypes::Agent(AgentCommand {
            agent: "claude".to_string(),
            prompt: "Fix ${dir}".to_string(),
            flags: None,
            description: None,
            options: None,
            capture: None,
        });
        let mut context = HashMap::new();
        context.insert("dir".to_string(), "/repo".to_string());
        let text = step.display(&context);
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("/repo"), "got {text}");
    }

    #[test]
    fn agent_step_description_is_read_from_the_step() {
        let step = CommandTypes::Agent(AgentCommand {
            agent: "claude".to_string(),
            prompt: "go".to_string(),
            flags: None,
            description: Some("fixes tests".to_string()),
            options: None,
            capture: None,
        });
        assert_eq!(step.description(), Some("fixes tests".to_string()));
    }
}
