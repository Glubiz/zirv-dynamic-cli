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
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        if let Some(options) = &self.options
            && let Some(os) = &options.operating_system
            && !os.is_current()
        {
            return Ok(Some("Command skipped due to OS filter".to_string()));
        }

        let command = self.substituted_command(context);
        self.check_unresolved_placeholders(context)?;

        if let Some(rest) = command.trim_start().strip_prefix("cd ") {
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

        let invoke = self.invoke(&command, context).await;

        if let Err(e) = invoke {
            if let Some(options) = &self.options {
                if let Some(commands) = &options.fallback {
                    for cmd in commands {
                        if let Err(fallback_error) = cmd.invoke().await {
                            return Err(format!(
                                "Command '{}' failed and fallback '{}' also failed: {}",
                                command, cmd.command, fallback_error
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
            return Err(format!("Command '{}' failed: {}", command, e));
        }

        if let Some(options) = &self.options
            && let Some(d) = options.delay_ms
        {
            sleep(Duration::from_millis(d)).await;
        }

        Ok(None)
    }

    async fn invoke(
        &self,
        command: &str,
        context: &mut HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut shell = if cfg!(windows) {
            let mut c = TokioCommand::new("powershell");
            c.arg("-Command").arg(command);
            c
        } else {
            let mut c = TokioCommand::new("sh");
            c.arg("-c").arg(command);
            c
        };

        if let Some(cwd) = context.get("cwd") {
            shell.current_dir(cwd);
        }

        if let Some(options) = &self.options
            && options.interactive
        {
            shell
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }

        // Issue #155: hold a machine-wide permit for the duration of a
        // classified heavy command. Best effort by design -- a state
        // directory that cannot be resolved must never stop a script from
        // running -- and the guard's `Drop` releases the slot when the child
        // exits, however it exits.
        let _permit = heavy_permit_for(command, context.get("cwd").map(String::as_str)).await;

        if let Some(var) = &self.capture {
            let out = shell.output().await?;
            if !out.status.success() {
                let code = out.status.code().unwrap_or(1);
                return Err(format!("`{command}` failed with exit code {code}").into());
            }

            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();

            context.insert(var.clone(), val);

            Ok(())
        } else {
            let status = shell.status().await?;

            if !status.success() {
                let code = status.code().unwrap_or(1);
                return Err(format!("`{command}` failed with exit code {code}").into());
            }

            Ok(())
        }
    }

    pub fn substituted_command(&self, params: &HashMap<String, String>) -> String {
        substitute(&self.command, params)
    }

    pub fn check_unresolved_placeholders(
        &self,
        context: &HashMap<String, String>,
    ) -> Result<(), String> {
        let substituted = self.substituted_command(context);
        check_unresolved(&self.command, &substituted)
    }
}

/// Issue #155: resolves the state dir and config from the process
/// environment and classifies `command` against the heavy-operation pattern
/// set, returning `None` for anything that is not heavy -- or when the state
/// directory or config cannot be resolved at all. Best-effort, the same as
/// every other piece of state-dir housekeeping in this codebase: a script
/// must never fail just because zirv's own supervision state could not be
/// found. `cwd` mirrors `agent_command::resolve_repo`'s own preference for a
/// script's own `cd`-tracked directory over the process's real one, since
/// `Command`'s `cd` handling only ever updates `context["cwd"]` (see
/// `execute`'s own handling), never `std::env::set_current_dir`.
async fn heavy_permit_for(
    command: &str,
    cwd: Option<&str>,
) -> Option<crate::commands::ctx::permit::HeavyPermit> {
    use crate::commands::ctx::config::{CtxConfig, env_from_process};
    use crate::commands::ctx::permit;
    use crate::commands::ctx::state::StateDir;

    let env = env_from_process();
    let state = StateDir::resolve(&env).ok()?;
    let repo = cwd
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let cfg = CtxConfig::load(&repo, &env).ok()?;

    if !permit::is_heavy(command, &cfg.supervise.heavy_command_patterns) {
        return None;
    }

    let limit = cfg.supervise.max_heavy_operations;
    // Refuse-not-queue is right for a *spawn* (issue #133's own gate, now
    // `dash::fulfill_spawn_request`) but wrong for a command the operator
    // already put in a script -- failing `zirv test` outright because a
    // background build is running would be worse than waiting for it. Polls
    // on a bounded interval up to a generous cap, then proceeds without a
    // permit rather than blocking a script forever.
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const MAX_WAIT: Duration = Duration::from_secs(600);
    let mut waited = Duration::ZERO;
    let mut announced = false;
    loop {
        if let Some(permit) = permit::acquire(&state, limit, command) {
            return Some(permit);
        }
        if waited >= MAX_WAIT {
            return None;
        }
        if !announced {
            eprintln!(
                "zirv: waiting for a heavy-operation slot ({limit} in use) before running `{command}`"
            );
            announced = true;
        }
        sleep(POLL_INTERVAL).await;
        waited += POLL_INTERVAL;
    }
}

/// Replaces every `${key}` placeholder in `template` with its value from
/// `context`. Shared by `Command` and agent steps so both honor the same
/// substitution syntax.
pub(crate) fn substitute(template: &str, context: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in context {
        let placeholder = format!("${{{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

/// Hard error naming any `${...}` placeholder left in `substituted` after
/// substitution ran. `original` is the pre-substitution text, quoted in the
/// error so the message points at the offending line in the script.
pub(crate) fn check_unresolved(original: &str, substituted: &str) -> Result<(), String> {
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    let unresolved: Vec<&str> = re
        .captures_iter(substituted)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    if unresolved.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Unresolved placeholders in '{original}': {}",
        unresolved.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashMap;

    #[tokio::test]
    async fn test_unresolved_placeholder_detected() {
        let command = Command {
            command: "echo ${name} ${typo}".to_string(),
            capture: None,
            description: None,
            options: None,
        };

        let mut context = HashMap::new();
        context.insert("name".to_string(), "Alice".to_string());

        let result = command.check_unresolved_placeholders(&context);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("typo"));
    }

    #[tokio::test]
    async fn test_no_unresolved_placeholders() {
        let command = Command {
            command: "echo ${name}".to_string(),
            capture: None,
            description: None,
            options: None,
        };

        let mut context = HashMap::new();
        context.insert("name".to_string(), "Alice".to_string());

        let result = command.check_unresolved_placeholders(&context);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_substituted_command() {
        let command = Command {
            command: "echo ${name} is ${age} years old".to_string(),
            capture: None,
            description: None,
            options: None,
        };

        let mut params = HashMap::new();
        params.insert("name".to_string(), "Alice".to_string());
        params.insert("age".to_string(), "30".to_string());

        let result = command.substituted_command(&params);

        assert_eq!(result, "echo Alice is 30 years old");
    }

    /// Issue #155: a classified heavy command acquires a permit for as long
    /// as it is held, and releases it on drop -- the same lifecycle
    /// `permit::a_permit_is_bounded_and_released_on_drop` pins for
    /// `permit::acquire` itself, exercised here through the actual seam
    /// `Command::invoke` calls.
    #[tokio::test]
    async fn heavy_permit_for_acquires_and_releases_a_permit_for_a_heavy_command() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let _state_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            crate::commands::ctx::state::STATE_ENV,
            Some(state_dir.path().to_str().expect("utf8 path")),
        )]);

        let state = crate::commands::ctx::state::StateDir::resolve(
            &crate::commands::ctx::config::env_from_process(),
        )
        .expect("resolve state dir");

        let permit = heavy_permit_for(
            "cargo build",
            Some(repo.path().to_str().expect("utf8 path")),
        )
        .await
        .expect("a heavy command acquires a permit");
        assert_eq!(crate::commands::ctx::permit::live_count(&state), 1);

        drop(permit);
        assert_eq!(crate::commands::ctx::permit::live_count(&state), 0);
    }

    /// A command outside the heavy classification never touches the budget
    /// at all.
    #[tokio::test]
    async fn heavy_permit_for_is_none_for_a_light_command() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let _state_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            crate::commands::ctx::state::STATE_ENV,
            Some(state_dir.path().to_str().expect("utf8 path")),
        )]);

        let permit =
            heavy_permit_for("git status", Some(repo.path().to_str().expect("utf8 path"))).await;
        assert!(permit.is_none(), "a light command must not hold a permit");
    }
}
