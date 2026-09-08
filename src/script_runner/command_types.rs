use std::process::Command as StdCommand;

use super::agent_command::AgentCommand;
use super::command::{self, Command};
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
            let commands: Vec<Command> = serde_yaml_ng::from_value(value).map_err(describe)?;
            validate_concurrent_block(&commands)?;
            return Ok(CommandTypes::Commands(commands));
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

/// G-8: a concurrent block runs each entry inside a detached terminal window
/// it never waits on, so `capture` (nothing in-process to read stdout from),
/// `fallback` (no in-process failure handling once the window owns the
/// command), and `interactive` (the window owns the terminal, not the
/// script's own stdio) can never be honored -- checked at load time, the
/// same as `AgentCommand::validate`, so a script naming one of these fails
/// the same way on `--dry-run` and a real run.
fn validate_concurrent_block(commands: &[Command]) -> Result<(), String> {
    for cmd in commands {
        if cmd.capture.is_some() {
            return Err(format!(
                "a concurrent-commands block cannot honor 'capture' (on '{}'); \
                 move this command out of the block",
                cmd.command
            ));
        }
        let Some(options) = &cmd.options else {
            continue;
        };
        if options.fallback.is_some() {
            return Err(format!(
                "a concurrent-commands block cannot honor 'fallback' (on '{}'); \
                 move this command out of the block",
                cmd.command
            ));
        }
        if options.interactive {
            return Err(format!(
                "a concurrent-commands block cannot honor 'interactive' (on '{}'); \
                 move this command out of the block",
                cmd.command
            ));
        }
    }
    Ok(())
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

    /// Everything wrong with the step that `context` can settle without
    /// running it -- today, a `${var}` the context has no value for.
    ///
    /// A-2/D-4: `--dry-run` printed each step and moved on, so `echo
    /// ${missing}` dry-ran with exit 0 while the real run failed on the
    /// same script. That is the same reasoning `AgentCommand::validate`
    /// already applies at load time; unlike `validate` this one needs the
    /// resolved context, so it runs per step at dry-run time instead.
    ///
    /// Review round 1 (R8): a step `execute` would skip for its
    /// `operating_system` filter resolves nothing, so there is nothing here
    /// to reject either -- a Linux-only step naming a Linux-only variable
    /// failed every dry run on Windows. The same `Command::skipped_for_os`
    /// predicate `execute` and `build_concurrent_command` apply, so the three
    /// can never disagree about which steps this platform runs.
    pub fn check(&self, context: &HashMap<String, String>) -> Result<(), String> {
        match self {
            CommandTypes::Command(cmd) if cmd.skipped_for_os() => Ok(()),
            CommandTypes::Command(cmd) => cmd.check_unresolved_placeholders(context),
            CommandTypes::Commands(cmds) => cmds
                .iter()
                .filter(|cmd| !cmd.skipped_for_os())
                .try_for_each(|cmd| cmd.check_unresolved_placeholders(context)),
            CommandTypes::Agent(agent) => {
                if agent
                    .options
                    .as_ref()
                    .is_some_and(super::options::Options::skip_for_os)
                {
                    return Ok(());
                }
                command::check_unresolved(&agent.prompt, context)
            }
        }
    }

    /// The variable a real run of this step would define through `capture:`,
    /// so `--dry-run` can stand a placeholder in for it and let later steps
    /// resolve (review round 1, R7). `None` for the two step kinds that never
    /// capture: an agent step rejects `capture` at load time, and a
    /// concurrent block spawns a terminal window it never reads back.
    pub fn captured_var(&self) -> Option<&str> {
        match self {
            CommandTypes::Command(cmd) if !cmd.skipped_for_os() => cmd.capture.as_deref(),
            CommandTypes::Command(_) | CommandTypes::Commands(_) | CommandTypes::Agent(_) => None,
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
        display_context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => cmd.execute(context, display_context).await,
            CommandTypes::Agent(agent) => agent.execute(context, display_context).await,
            CommandTypes::Commands(cmds) => {
                if cmds.is_empty() {
                    return Ok(None);
                }

                // G-8: `operating_system` (the one per-command option a
                // concurrent block CAN honor, since it decides membership
                // rather than requiring in-process control once the block's
                // terminal window is open) is applied here, before the join
                // -- a filtered entry must be dropped from the line
                // entirely, not merely left to run inside the shared window.
                let Some(joined) = build_concurrent_command(cmds, context)? else {
                    return Ok(Some("Command skipped due to OS filter".to_string()));
                };

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

/// Builds the `&&`-joined command line a concurrent block will run in its
/// new terminal window: drops any entry whose `operating_system` filter
/// excludes the current platform, substitutes `${var}` from `context` in
/// the rest, and hard-errors on any placeholder the template names that
/// `context` has no value for. `None` means every entry was filtered out, so
/// the caller must not open an empty terminal window at all.
///
/// G-8: a concurrent block used to discard every per-command `options` --
/// only `cmd.command` was ever joined -- so an entry filtered for the other
/// platform still ran inside the block's shared window regardless.
///
/// A-2/D-1: substitution and the unresolved check go through the same
/// `command::substitute`/`command::check_unresolved` a single `Command` step
/// uses (G-1/G-10), rather than this block's own hash-ordered replace loop
/// plus a re-scan of the already-substituted text.
fn build_concurrent_command(
    cmds: &[Command],
    context: &HashMap<String, String>,
) -> Result<Option<String>, String> {
    let mut substituted: Vec<Command> = cmds
        .iter()
        .filter(|cmd| !cmd.options.as_ref().is_some_and(|o| o.skip_for_os()))
        .cloned()
        .collect();

    if substituted.is_empty() {
        return Ok(None);
    }

    for cmd in &mut substituted {
        command::check_unresolved(&cmd.command, context)?;
        cmd.command = command::substitute(&cmd.command, context);
    }

    Ok(Some(
        substituted
            .into_iter()
            .map(|c| c.command)
            .collect::<Vec<_>>()
            .join(" && "),
    ))
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
                // Not `gemini`: that name is a registered adapter now
                // (issue #384), so it would no longer trip this branch.
                // `mystery` reads the same validation dynamically off
                // `adapters::all()` (`agent_command.rs::AgentCommand::
                // validate`), so any name absent from that registry works.
                "name: t\ncommands:\n  - agent: mystery\n    prompt: go\n",
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

    /// Review round 1 (R8): `execute` skips a step whose `operating_system`
    /// filter excludes this platform without resolving anything, so `check`
    /// must not reject its `${var}` either -- a Linux-only step naming a
    /// Linux-only variable made every dry run on Windows fail on a step that
    /// platform never runs. Both step shapes that carry the filter are
    /// covered: a plain `Command`, and one entry of a concurrent block (which
    /// `build_concurrent_command` drops from the joined line).
    #[test]
    fn a_dry_run_skips_the_check_for_a_step_filtered_out_on_this_os() {
        let other_os = if cfg!(windows) {
            crate::script_runner::operating_system::OperatingSystem::Linux
        } else {
            crate::script_runner::operating_system::OperatingSystem::Windows
        };
        let filtered = |command: &str| Command {
            command: command.to_string(),
            capture: None,
            description: None,
            options: Some(crate::script_runner::options::Options {
                operating_system: Some(other_os.clone()),
                ..Default::default()
            }),
        };
        let context = HashMap::new();

        assert!(
            CommandTypes::Command(filtered("echo ${linux_only_var}"))
                .check(&context)
                .is_ok(),
            "a step this platform never runs resolves nothing to check"
        );
        assert!(
            CommandTypes::Commands(vec![
                Command {
                    command: "echo keep".to_string(),
                    capture: None,
                    description: None,
                    options: None,
                },
                filtered("echo ${linux_only_var}"),
            ])
            .check(&context)
            .is_ok(),
            "a filtered block entry is dropped from the joined line, so it checks nothing"
        );
        assert!(
            CommandTypes::Command(Command {
                command: "echo ${missing}".to_string(),
                capture: None,
                description: None,
                options: None,
            })
            .check(&context)
            .is_err(),
            "an unfiltered step is still checked"
        );
    }

    /// G-8: a concurrent-commands block discarded every per-command
    /// `options` -- only `cmd.command` was ever joined -- so an entry
    /// filtered for the other platform still ran inside the block's shared
    /// terminal window. `operating_system` must be honored the same way a
    /// plain `Command` step honors it: dropped from the joined line
    /// entirely.
    #[test]
    fn build_concurrent_command_drops_an_entry_filtered_for_the_other_os() {
        let other_os = if cfg!(windows) {
            crate::script_runner::operating_system::OperatingSystem::Linux
        } else {
            crate::script_runner::operating_system::OperatingSystem::Windows
        };
        let cmds = vec![
            Command {
                command: "echo keep".to_string(),
                capture: None,
                description: None,
                options: None,
            },
            Command {
                command: "echo filtered".to_string(),
                capture: None,
                description: None,
                options: Some(crate::script_runner::options::Options {
                    operating_system: Some(other_os),
                    ..Default::default()
                }),
            },
        ];
        let context = HashMap::new();

        let joined = build_concurrent_command(&cmds, &context)
            .expect("no unresolved placeholders")
            .expect("at least one entry survives the filter");
        assert!(joined.contains("echo keep"), "got {joined}");
        assert!(!joined.contains("echo filtered"), "got {joined}");
    }

    /// When every entry is filtered out, the block must not open an empty
    /// terminal window at all.
    #[test]
    fn build_concurrent_command_is_none_when_every_entry_is_filtered_out() {
        let other_os = if cfg!(windows) {
            crate::script_runner::operating_system::OperatingSystem::Linux
        } else {
            crate::script_runner::operating_system::OperatingSystem::Windows
        };
        let cmds = vec![Command {
            command: "echo filtered".to_string(),
            capture: None,
            description: None,
            options: Some(crate::script_runner::options::Options {
                operating_system: Some(other_os),
                ..Default::default()
            }),
        }];
        let context = HashMap::new();

        let result = build_concurrent_command(&cmds, &context).expect("no unresolved placeholders");
        assert!(result.is_none(), "got {result:?}");
    }

    /// A-2/D-1: a concurrent block kept the pre-G-1 substitution -- a
    /// hash-ordered `String::replace` per context entry, then a regex
    /// re-scan of the *substituted* text -- so a value whose own text
    /// contains `${...}` was either re-expanded (splicing a secret into the
    /// spawned window's command line) or falsely reported as unresolved,
    /// depending on `HashMap` iteration order. It must resolve exactly the
    /// way a single `Command` step does.
    #[test]
    fn a_concurrent_block_substitutes_like_a_single_command() {
        let cmds = vec![Command {
            command: "echo ${a}".to_string(),
            capture: None,
            description: None,
            options: None,
        }];

        // A fresh map per iteration: iteration order is a property of the
        // map instance, so reusing one would only ever exercise a single
        // order and let the old hash-ordered loop pass by luck.
        for _ in 0..200 {
            let mut context = HashMap::new();
            context.insert("a".to_string(), "${secret}".to_string());
            context.insert("secret".to_string(), "hunter2".to_string());
            for i in 0..14 {
                context.insert(format!("filler{i}"), format!("v{i}"));
            }
            let joined = build_concurrent_command(&cmds, &context);
            assert_eq!(joined, Ok(Some("echo ${secret}".to_string())));
        }
    }

    /// G-8: a concurrent block cannot honor `capture` (there is no in-process
    /// stdout to read from a detached terminal window) -- it must be a
    /// load-time error naming the offending command, not a silently dropped
    /// option.
    #[test]
    fn a_concurrent_block_entry_with_capture_is_rejected_at_load_time() {
        let message =
            parse_error("name: t\ncommands:\n  - - command: echo hi\n      capture: out\n");
        assert!(message.contains("capture"), "{message}");
    }

    /// Same reasoning for `fallback` (no in-process failure handling for a
    /// command running inside a detached terminal window).
    #[test]
    fn a_concurrent_block_entry_with_fallback_is_rejected_at_load_time() {
        let message = parse_error(
            "name: t\ncommands:\n  - - command: echo hi\n      options:\n          fallback:\n            - command: echo fallback\n",
        );
        assert!(message.contains("fallback"), "{message}");
    }

    /// Same reasoning for `interactive` (the block's window, not the
    /// script's own stdio, owns the terminal).
    #[test]
    fn a_concurrent_block_entry_with_interactive_is_rejected_at_load_time() {
        let message = parse_error(
            "name: t\ncommands:\n  - - command: echo hi\n      options:\n          interactive: true\n",
        );
        assert!(message.contains("interactive"), "{message}");
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
