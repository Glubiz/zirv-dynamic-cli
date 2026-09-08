use std::process::Stdio;

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;

use super::command::{check_unresolved, substitute};
use crate::script_runner::options::Options;

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct FallbackCommand {
    pub command: String,
    pub description: Option<String>,
    pub options: Option<Options>,
}

impl FallbackCommand {
    /// G-2: this used to take no context at all, so a fallback's `${var}`
    /// placeholders were never substituted, its own `cd`-tracked `cwd` was
    /// ignored (it always ran relative to the process's real working
    /// directory), and only `options.interactive` was honored -- `options.
    /// operating_system` and `options.delay_ms` were silently dropped.
    /// Substitutes and checks placeholders the same way `Command` and
    /// `AgentCommand` do, skips when `skip_for_os()` says so, and sleeps for
    /// `delay_ms` after a successful run. A failure honors this fallback's
    /// own `proceed_on_failure` (converting it to success) so a single
    /// optional fallback in a chain can fail without aborting the rest.
    pub async fn invoke(
        &self,
        context: &mut HashMap<String, String>,
        display_context: &mut HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cmd_display = substitute(&self.command, display_context);
        if let Some(options) = &self.options
            && options.skip_for_os()
        {
            crate::output::note(format!(
                "fallback '{}' skipped due to OS filter",
                cmd_display
            ));
            return Ok(());
        }

        let command = substitute(&self.command, context);
        check_unresolved(&self.command, context)?;

        // Pick shell based on the OS
        let mut shell = if cfg!(windows) {
            let mut c = TokioCommand::new("powershell");
            c.arg("-Command").arg(&command);
            c
        } else {
            let mut c = TokioCommand::new("sh");
            c.arg("-c").arg(&command);
            c
        };

        if let Some(cwd) = context.get("cwd") {
            shell.current_dir(cwd);
        }

        crate::output::warn(format!("fallback: {cmd_display}"));
        if let Some(description) = &self.description {
            crate::output::step_description(description);
        }

        if let Some(options) = &self.options
            && options.interactive
        {
            shell
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }

        let status = shell.status().await?;

        if !status.success() {
            if self.options.as_ref().is_some_and(|o| o.proceed_on_failure) {
                return Ok(());
            }
            return Err(format!("`{cmd_display}` failed").into());
        }

        if let Some(options) = &self.options
            && let Some(d) = options.delay_ms
        {
            tokio::time::sleep(tokio::time::Duration::from_millis(d)).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_runner::operating_system::OperatingSystem;
    use hashbrown::HashMap;

    /// G-2: `invoke()` took no context, so a fallback's own `${var}`
    /// placeholders were never substituted. A resolved placeholder must
    /// reach the shell as the substituted value, not the literal `${...}`
    /// text.
    #[tokio::test]
    async fn fallback_substitutes_placeholders_before_running() {
        let cmd = FallbackCommand {
            command: if cfg!(windows) {
                "if (\"${name}\" -ne \"Alice\") { exit 1 }".to_string()
            } else {
                "[ \"${name}\" = \"Alice\" ]".to_string()
            },
            description: None,
            options: None,
        };
        let mut context = HashMap::new();
        context.insert("name".to_string(), "Alice".to_string());

        let mut display_context = context.clone();
        let result = cmd.invoke(&mut context, &mut display_context).await;
        assert!(
            result.is_ok(),
            "expected the substituted value to satisfy the check: {result:?}"
        );
    }

    /// A placeholder with no matching context entry must be a hard error,
    /// the same `check_unresolved` guarantee `Command` and `AgentCommand`
    /// already give.
    #[tokio::test]
    async fn an_unresolved_placeholder_in_a_fallback_command_is_a_hard_error() {
        let cmd = FallbackCommand {
            command: "echo ${missing}".to_string(),
            description: None,
            options: None,
        };
        let mut context = HashMap::new();

        let mut display_context = context.clone();
        let err = cmd
            .invoke(&mut context, &mut display_context)
            .await
            .expect_err("unresolved placeholder must error");
        assert!(err.to_string().contains("missing"), "got {err}");
    }

    /// G-2: `invoke()` ignored the script's own tracked `cwd`, always
    /// running relative to the process's real working directory.
    #[tokio::test]
    async fn fallback_honors_the_scripts_tracked_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = "fallback-marker.txt";
        let cmd = FallbackCommand {
            command: if cfg!(windows) {
                format!("New-Item -ItemType File -Name {marker} -Force | Out-Null")
            } else {
                format!("touch {marker}")
            },
            description: None,
            options: None,
        };
        let mut context = HashMap::new();
        context.insert("cwd".to_string(), tmp.path().to_string_lossy().to_string());

        let mut display_context = context.clone();
        let result = cmd.invoke(&mut context, &mut display_context).await;
        assert!(result.is_ok(), "got {result:?}");
        assert!(
            tmp.path().join(marker).exists(),
            "the fallback must run inside the tracked cwd, not the process cwd"
        );
    }

    /// G-2: `invoke()` never read `options.operating_system`, so a fallback
    /// filtered for the other platform still ran unconditionally.
    #[tokio::test]
    async fn a_fallback_filtered_for_the_other_os_is_skipped() {
        let cmd = FallbackCommand {
            command: "exit 1".to_string(),
            description: None,
            options: Some(Options {
                operating_system: Some(if cfg!(windows) {
                    OperatingSystem::Linux
                } else {
                    OperatingSystem::Windows
                }),
                ..Default::default()
            }),
        };
        let mut context = HashMap::new();

        let mut display_context = context.clone();
        let result = cmd.invoke(&mut context, &mut display_context).await;
        assert!(
            result.is_ok(),
            "a filtered fallback must be skipped rather than run: {result:?}"
        );
    }
}
