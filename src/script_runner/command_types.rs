use std::process::Command as StdCommand;

use super::agent_command::AgentCommand;
use super::command::Command;
use hashbrown::HashMap;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    Command(Command),
    Commands(Vec<Command>),
    Agent(AgentCommand),
}

/// Steps are dispatched on the key that names their kind, not by serde's
/// untagged fallback. Untagged reports only "data did not match any variant of
/// untagged enum CommandTypes", which names neither the key that was missing
/// nor the one that was misspelled, and silently picks the first variant that
/// happens to fit -- so a step carrying both `command` and `agent` ran as a
/// shell command and threw the agent half away.
impl CommandTypes {
    fn from_value(value: serde_yaml_ng::Value) -> Result<Self, String> {
        let describe = |e: serde_yaml_ng::Error| e.to_string();

        if value.is_sequence() {
            return serde_yaml_ng::from_value(value)
                .map(CommandTypes::Commands)
                .map_err(describe);
        }
        let Some(map) = value.as_mapping() else {
            return Err("expected a mapping with 'command' or 'agent', \
                        or a list of shell commands"
                .to_string());
        };

        let has = |key: &str| map.contains_key(serde_yaml_ng::Value::String(key.to_string()));
        match (has("command"), has("agent")) {
            (true, true) => Err("has both 'command' and 'agent'; a step is either a shell \
                                 command or an agent step, not both"
                .to_string()),
            (true, false) => serde_yaml_ng::from_value(value)
                .map(CommandTypes::Command)
                .map_err(describe),
            (false, true) => {
                let agent: AgentCommand = serde_yaml_ng::from_value(value).map_err(describe)?;
                // Checked here rather than at execution time so `--dry-run`
                // and a real run reject exactly the same scripts.
                agent.validate()?;
                Ok(CommandTypes::Agent(agent))
            }
            (false, false) => Err("needs either 'command' (a shell command) or 'agent' \
                                   together with 'prompt' (an agent step)"
                .to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for CommandTypes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(de::Error::custom)
    }
}

/// A step does not know its own position, so the list is what names it. Worth
/// the wrapper: "step 3" is the difference between a fixable error and a hunt.
pub fn deserialize_steps<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<CommandTypes>, D::Error> {
    let raw = Vec::<serde_yaml_ng::Value>::deserialize(deserializer)?;
    raw.into_iter()
        .enumerate()
        .map(|(index, value)| {
            CommandTypes::from_value(value)
                .map_err(|e| de::Error::custom(format!("step {}: {e}", index + 1)))
        })
        .collect()
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

/// Tries each `(binary, args)` candidate in order, returning `Ok(())` on the
/// first one whose process spawns successfully. `spawn()` only proves the OS
/// could exec the binary — it does not wait for (or know whether) a GUI
/// window actually appeared, since these windows are meant to stay open
/// after zirv returns. When every candidate fails to spawn, that is the
/// clearest signal available that there is no terminal emulator to open a
/// window with, which on Linux usually means a headless/SSH session.
fn spawn_first_available(candidates: &[(&str, &[&str])]) -> Result<(), String> {
    for (bin, args) in candidates {
        if StdCommand::new(bin).args(*args).spawn().is_ok() {
            return Ok(());
        }
    }

    let tried: Vec<&str> = candidates.iter().map(|(bin, _)| *bin).collect();
    Err(headless_error(&tried))
}

/// A clear, actionable error for when no terminal emulator could be spawned:
/// concurrent-command windows need a desktop/GUI session, which a headless
/// or SSH-only session does not have.
fn headless_error(tried: &[&str]) -> String {
    format!(
        "Could not open a terminal window for the concurrent-commands feature: \
         no terminal emulator could be launched (tried: {}). This feature \
         requires a desktop/GUI session and will not work over a headless or \
         SSH-only connection.",
        tried.join(", ")
    )
}

fn spawn_terminal_windows(command: &str, working_dir: &str) -> Result<(), String> {
    let args = ["/C", "start", "", "/D", working_dir, "cmd", "/K", command];
    spawn_first_available(&[("cmd", args.as_slice())])
}

fn spawn_terminal_macos(command: &str) -> Result<(), String> {
    let applescript_cmd = format!(
        r#"tell application "Terminal"
activate
do script "{}"
end tell"#,
        escape_for_applescript(command)
    );
    let args = ["-e", applescript_cmd.as_str()];
    spawn_first_available(&[("osascript", args.as_slice())])
}

fn spawn_terminal_linux(cwd: &str, joined: &str) -> Result<(), String> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(
            "Could not open a terminal window for the concurrent-commands feature: neither \
             DISPLAY nor WAYLAND_DISPLAY is set (would have tried: gnome-terminal, \
             x-terminal-emulator, xterm). This feature requires a desktop/GUI session and \
             will not work over a headless or SSH-only connection."
                .to_string(),
        );
    }

    let gnome_command = format!("{joined} ; exec bash");
    let gnome_args = [
        "--working-directory",
        cwd,
        "--",
        "bash",
        "-lc",
        gnome_command.as_str(),
    ];

    let fallback_cmd = format!(
        "cd '{}' ; {} ; exec bash",
        escape_single_quotes(cwd),
        joined
    );
    let xte_args = ["-e", "bash", "-lc", fallback_cmd.as_str()];
    let xterm_args = ["-hold", "-e", "bash", "-lc", fallback_cmd.as_str()];

    spawn_first_available(&[
        ("gnome-terminal", gnome_args.as_slice()),
        ("x-terminal-emulator", xte_args.as_slice()),
        ("xterm", xterm_args.as_slice()),
    ])
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

    fn parse_error(yaml: &str) -> String {
        serde_yaml_ng::from_str::<crate::script_runner::script::Script>(yaml)
            .expect_err("must not parse")
            .to_string()
    }

    /// The untagged enum picked `Command` first and threw the agent half away
    /// in silence, so converting a step and forgetting to delete `command:`
    /// ran the old shell command instead.
    #[test]
    fn a_step_that_is_both_a_command_and_an_agent_is_rejected_by_name() {
        let message = parse_error(
            "name: t\ncommands:\n  - command: echo hello\n    agent: claude\n    prompt: go\n",
        );
        assert!(message.contains("step 1"), "names the step: {message}");
        assert!(message.contains("'command'"), "{message}");
        assert!(message.contains("'agent'"), "{message}");
    }

    /// Untagged reported only "data did not match any variant", which named
    /// neither the key that was misspelled nor the one that was missing.
    #[test]
    fn a_step_naming_no_kind_at_all_says_what_it_needed() {
        let message = parse_error("name: t\ncommands:\n  - agnet: claude\n    prompt: go\n");
        assert!(message.contains("step 1"), "{message}");
        assert!(
            message.contains("'command'") && message.contains("'agent'"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_prompt_names_the_field_and_the_step() {
        let message = parse_error("name: t\ncommands:\n  - command: ok\n  - agent: claude\n");
        assert!(message.contains("step 2"), "names the step: {message}");
        assert!(message.contains("prompt"), "names the field: {message}");
    }

    /// Validation used to live in `execute`, so `--dry-run` reported success
    /// for scripts that could never run. Rejecting at load time makes the two
    /// agree.
    #[test]
    fn an_agent_step_that_can_never_run_is_rejected_at_load_time() {
        for (yaml, expected) in [
            (
                "name: t\ncommands:\n  - agent: claude\n    prompt: go\n    capture: out\n",
                "capture",
            ),
            (
                "name: t\ncommands:\n  - agent: claude\n    prompt: go\n    options:\n      interactive: true\n",
                "interactive",
            ),
            (
                "name: t\ncommands:\n  - agent: gemini\n    prompt: go\n",
                "unknown agent",
            ),
            (
                "name: t\ncommands:\n  - agent: claude\n    prompt: go\n    flags: [\"sonnet\"]\n",
                "must start with",
            ),
        ] {
            let message = parse_error(yaml);
            assert!(
                message.contains(expected),
                "expected {expected:?} in: {message}"
            );
        }
    }

    /// The dispatch reads a self-describing value, so the other supported
    /// script formats have to keep working.
    #[test]
    fn dispatch_still_reads_json_scripts() {
        let json = r#"{"name":"t","commands":[{"command":"echo hi"},
                       {"agent":"claude","prompt":"go"}]}"#;
        let script: crate::script_runner::script::Script =
            serde_json::from_str(json).expect("valid json script");
        assert!(matches!(script.commands[0], CommandTypes::Command(_)));
        assert!(matches!(script.commands[1], CommandTypes::Agent(_)));
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

    /// Overrides an env var for the duration of `test`, restoring whatever
    /// was there before (including "unset").
    fn with_env_var<F, R>(key: &str, value: Option<&str>, test: F) -> R
    where
        F: FnOnce() -> R,
    {
        let original = std::env::var(key).ok();

        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }

        let result = test();

        unsafe {
            match original {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }

        result
    }

    fn with_no_display<F, R>(test: F) -> R
    where
        F: FnOnce() -> R,
    {
        with_env_var("DISPLAY", None, || {
            with_env_var("WAYLAND_DISPLAY", None, test)
        })
    }

    #[test]
    fn test_spawn_first_available_succeeds_on_first_working_candidate() {
        // A binary guaranteed to exist and exit instantly, so the test never
        // opens anything visible: `true` on unix, `cmd /C exit 0` on Windows.
        let (bin, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C", "exit", "0"])
        } else {
            ("true", &[])
        };

        let result =
            spawn_first_available(&[("definitely-not-a-real-binary-xyz", &[]), (bin, args)]);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn test_spawn_first_available_reports_clear_error_when_all_candidates_missing() {
        let result = spawn_first_available(&[
            ("definitely-not-a-real-binary-xyz", &[]),
            ("also-not-a-real-binary-abc", &[]),
        ]);

        let err = result.expect_err("no fake binary should ever spawn successfully");
        assert!(err.contains("definitely-not-a-real-binary-xyz"), "{err}");
        assert!(err.contains("also-not-a-real-binary-abc"), "{err}");
        assert!(
            err.contains("desktop") || err.contains("GUI"),
            "expected the error to explain a desktop/GUI session is required, got: {err}"
        );
        assert!(
            err.contains("headless") || err.contains("SSH"),
            "expected the error to name the headless/SSH scenario, got: {err}"
        );
    }

    #[test]
    fn test_spawn_terminal_linux_headless_without_display_env() {
        with_no_display(|| {
            let result = spawn_terminal_linux("/tmp", "echo hi");
            let err = result.expect_err("no DISPLAY/WAYLAND_DISPLAY must be a clear error");
            assert!(err.contains("DISPLAY"), "{err}");
            assert!(
                err.contains("desktop") || err.contains("GUI"),
                "expected the error to explain a desktop/GUI session is required, got: {err}"
            );
        });
    }

    #[test]
    fn test_spawn_terminal_linux_with_display_but_no_emulators_installed() {
        // This dev/CI host has none of gnome-terminal, x-terminal-emulator or
        // xterm installed, so with DISPLAY set we still exhaust every
        // candidate and should get the clear "tried: ..." error rather than a
        // raw OS error.
        with_env_var("DISPLAY", Some(":0"), || {
            let result = spawn_terminal_linux("/tmp", "echo hi");
            let err = result.expect_err("no linux terminal emulator is installed on this host");
            assert!(err.contains("gnome-terminal"), "{err}");
            assert!(err.contains("xterm"), "{err}");
        });
    }
}
