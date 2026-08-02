use std::path::{Path, PathBuf};

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use super::command::{check_unresolved, substitute};
use super::options::Options;

/// A script step that runs a supervised AI-agent task, in-process, through the
/// same machinery `zirv ctx exec` uses: pacing against usage windows, rot
/// detection and automatic restart with handoff injection.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentCommand {
    /// Adapter name, e.g. "claude". Passed straight to the ctx exec supervisor
    /// as `--agent`.
    pub agent: String,
    /// The task prompt. Supports the same `${var}` substitution as `command`,
    /// including the unresolved-placeholder hard error.
    pub prompt: String,
    /// Extra flags passed straight through to the agent CLI (e.g. `["--model",
    /// "sonnet"]`).
    #[serde(default)]
    pub flags: Option<Vec<String>>,
    /// An optional description of what the step does.
    pub description: Option<String>,
    /// Optional options that control the behavior of the step. `capture` and
    /// `interactive` are not supported for agent steps.
    pub options: Option<Options>,
    /// Not supported for agent steps. Declared here only so a script author
    /// gets a clear error instead of the field being silently ignored.
    pub capture: Option<String>,
}

impl AgentCommand {
    pub fn substituted_prompt(&self, context: &HashMap<String, String>) -> String {
        substitute(&self.prompt, context)
    }

    /// What the step would run, for `--dry-run` and step framing.
    pub fn display(&self, context: &HashMap<String, String>) -> String {
        format!(
            "[agent:{}] {}",
            self.agent,
            self.substituted_prompt(context)
        )
    }

    fn check_unsupported_options(&self) -> Result<(), String> {
        if self.capture.is_some() {
            return Err("agent steps do not support 'capture'".to_string());
        }
        if self.options.as_ref().is_some_and(|o| o.interactive) {
            return Err("agent steps do not support 'interactive'".to_string());
        }
        Ok(())
    }

    pub async fn execute(
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        if let Some(options) = &self.options
            && options.skip_for_os()
        {
            return Ok(Some("Command skipped due to OS filter".to_string()));
        }

        self.check_unsupported_options()?;

        let prompt = self.substituted_prompt(context);
        check_unresolved(&self.prompt, &prompt)?;

        let cwd = context.get("cwd").cloned();
        if let Err(e) = self.invoke(&prompt, cwd.as_deref()).await {
            if let Some(options) = &self.options {
                if let Some(commands) = &options.fallback {
                    for cmd in commands {
                        if let Err(fallback_error) = cmd.invoke().await {
                            return Err(format!(
                                "Agent '{}' failed and fallback '{}' also failed: {}",
                                self.agent, cmd.command, fallback_error
                            ));
                        }
                    }
                }

                if options.proceed_on_failure {
                    return Ok(Some(
                        "Agent step failed but proceeding due to options".to_string(),
                    ));
                }
            }
            return Err(format!("Agent '{}' failed: {}", self.agent, e));
        }

        if let Some(options) = &self.options
            && let Some(d) = options.delay_ms
        {
            tokio::time::sleep(tokio::time::Duration::from_millis(d)).await;
        }

        Ok(None)
    }

    /// Runs the supervised session on a blocking thread: `run_supervised`
    /// spawns child processes and sleeps synchronously, exactly like `zirv ctx
    /// exec` does on its own thread, so it must not run on the async executor.
    async fn invoke(&self, prompt: &str, cwd: Option<&str>) -> Result<(), String> {
        let agent = self.agent.clone();
        let prompt = prompt.to_string();
        let flags = self.flags.clone().unwrap_or_default();
        let repo = resolve_repo(cwd);

        let code =
            tokio::task::spawn_blocking(move || run_supervised(&agent, &prompt, &flags, &repo))
                .await
                .map_err(|e| format!("agent task panicked: {e}"))??;

        if code == 0 {
            return Ok(());
        }
        Err(format!("exited with code {code}"))
    }
}

fn resolve_repo(cwd: Option<&str>) -> PathBuf {
    cwd.map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Builds the same `ExecArgs` a `zirv ctx exec --agent <agent> --prompt
/// <prompt> -- <agent> -p <prompt> --session-id <id>` invocation would
/// receive, then drives it through the exact in-process entry point `zirv ctx
/// exec` uses, so a supervised agent step gets the same pacing, rot detection
/// and restart-with-handoff behavior as running it from the command line.
fn run_supervised(agent: &str, prompt: &str, flags: &[String], repo: &Path) -> Result<i32, String> {
    use crate::commands::ctx::adapters;
    use crate::commands::ctx::config::{CtxConfig, env_from_process};
    use crate::commands::ctx::event::SessionId;
    use crate::commands::ctx::exec::{self, ExecArgs};

    let env = env_from_process();
    let cfg = CtxConfig::load(repo, &env).map_err(|e| e.to_string())?;
    // Selecting the adapter here (rather than deferring entirely to `exec::run_with`)
    // is what surfaces an adapter's own "not ready" error (e.g. codex) before any
    // supervision starts, and lets the first spawn's argv be built from the exact
    // same `headless_cmd` restarts use.
    adapters::select(Some(agent), &[], cfg.agent_bin.as_deref()).map_err(|e| e.to_string())?;

    let session = SessionId::new_v4();
    // No argv: the prompt travels as data and `run_with` builds the launch
    // from the adapter, exactly as every relaunch does. Encoding the prompt
    // into argv here only to have `run_with` parse it back out again is what
    // let a prompt shaped like a flag be misread as one.
    let args = ExecArgs {
        agent: Some(agent.to_string()),
        session_id: Some(session.as_str().to_string()),
        transcript: None,
        prompt: Some(prompt.to_string()),
        max_restarts: None,
        timeout_secs: None,
        command: flags.to_vec(),
        simple: false,
    };

    let mut out = std::io::stdout();
    exec::run_with(&args, &mut out, repo, &env).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::STATE_ENV;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn agent_step(prompt: &str) -> AgentCommand {
        AgentCommand {
            agent: "claude".to_string(),
            prompt: prompt.to_string(),
            flags: None,
            description: None,
            options: None,
            capture: None,
        }
    }

    #[test]
    fn deserializes_from_yaml() {
        let yaml = "agent: claude\nprompt: \"fix ${thing}\"\nflags: [\"--model\", \"sonnet\"]\n";
        let cmd: AgentCommand = serde_yaml_ng::from_str(yaml).expect("valid yaml");
        assert_eq!(cmd.agent, "claude");
        assert_eq!(cmd.prompt, "fix ${thing}");
        assert_eq!(
            cmd.flags,
            Some(vec!["--model".to_string(), "sonnet".to_string()])
        );
    }

    #[test]
    fn flags_are_optional() {
        let yaml = "agent: claude\nprompt: go\n";
        let cmd: AgentCommand = serde_yaml_ng::from_str(yaml).expect("valid yaml");
        assert_eq!(cmd.flags, None);
    }

    #[tokio::test]
    async fn substitutes_prompt_placeholders() {
        let cmd = agent_step("Fix the failing tests in ${dir}");
        let mut context = HashMap::new();
        context.insert("dir".to_string(), "/repo".to_string());
        assert_eq!(
            cmd.substituted_prompt(&context),
            "Fix the failing tests in /repo"
        );
    }

    #[test]
    fn display_shows_agent_and_substituted_prompt() {
        let cmd = agent_step("Fix ${dir}");
        let mut context = HashMap::new();
        context.insert("dir".to_string(), "/repo".to_string());
        let text = cmd.display(&context);
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("/repo"), "got {text}");
    }

    #[tokio::test]
    async fn unresolved_prompt_placeholder_is_a_hard_error() {
        let cmd = agent_step("Fix ${missing}");
        let mut context = HashMap::new();
        let err = cmd
            .execute(&mut context)
            .await
            .expect_err("unresolved placeholder must error");
        assert!(err.contains("missing"), "got {err}");
    }

    #[tokio::test]
    async fn capture_is_rejected() {
        let mut cmd = agent_step("go");
        cmd.capture = Some("out".to_string());
        let mut context = HashMap::new();
        let err = cmd
            .execute(&mut context)
            .await
            .expect_err("capture is unsupported");
        assert!(err.contains("capture"), "got {err}");
    }

    #[tokio::test]
    async fn interactive_is_rejected() {
        let mut cmd = agent_step("go");
        cmd.options = Some(Options {
            interactive: true,
            ..Default::default()
        });
        let mut context = HashMap::new();
        let err = cmd
            .execute(&mut context)
            .await
            .expect_err("interactive is unsupported");
        assert!(err.contains("interactive"), "got {err}");
    }

    #[tokio::test]
    async fn os_filter_skips_without_running_the_agent() {
        let mut cmd = agent_step("go");
        cmd.agent = "codex".to_string(); // would error if it actually ran
        cmd.options = Some(Options {
            operating_system: Some(if cfg!(target_os = "linux") {
                crate::script_runner::operating_system::OperatingSystem::Windows
            } else {
                crate::script_runner::operating_system::OperatingSystem::Linux
            }),
            ..Default::default()
        });
        let mut context = HashMap::new();
        let result = cmd
            .execute(&mut context)
            .await
            .expect("skip is not an error");
        assert!(result.is_some(), "expected a skip message");
    }

    #[tokio::test]
    async fn an_unready_adapter_fails_with_its_own_error() {
        let mut cmd = agent_step("go");
        cmd.agent = "codex".to_string();
        let mut context = HashMap::new();
        let err = cmd
            .execute(&mut context)
            .await
            .expect_err("codex is not ready yet");
        assert!(err.contains("codex"), "got {err}");
    }

    #[tokio::test]
    async fn a_healthy_run_succeeds_through_ctx_exec_supervision() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var(STATE_ENV, &state);
            std::env::set_var("ZIRV_CTX_AGENT_BIN", fixture("fake-agent.sh"));
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let cmd = agent_step("do the work");
        let mut context = HashMap::new();
        context.insert("cwd".to_string(), tmp.path().display().to_string());

        let result = cmd.execute(&mut context).await;

        unsafe {
            std::env::remove_var(STATE_ENV);
            std::env::remove_var("ZIRV_CTX_AGENT_BIN");
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert!(result.is_ok(), "expected success, got {result:?}");
    }

    #[tokio::test]
    async fn a_failing_run_fails_the_step() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var(STATE_ENV, &state);
            std::env::set_var("ZIRV_CTX_AGENT_BIN", fixture("fake-agent.sh"));
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }

        let cmd = agent_step("do the work");
        let mut context = HashMap::new();
        context.insert("cwd".to_string(), tmp.path().display().to_string());

        let result = cmd.execute(&mut context).await;

        unsafe {
            std::env::remove_var(STATE_ENV);
            std::env::remove_var("ZIRV_CTX_AGENT_BIN");
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        let err = result.expect_err("a nonzero exit must fail the step");
        assert!(err.contains("claude"), "got {err}");
    }

    #[tokio::test]
    async fn proceed_on_failure_turns_a_failure_into_a_skip() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var(STATE_ENV, &state);
            std::env::set_var("ZIRV_CTX_AGENT_BIN", fixture("fake-agent.sh"));
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }

        let mut cmd = agent_step("do the work");
        cmd.options = Some(Options {
            proceed_on_failure: true,
            ..Default::default()
        });
        let mut context = HashMap::new();
        context.insert("cwd".to_string(), tmp.path().display().to_string());

        let result = cmd.execute(&mut context).await;

        unsafe {
            std::env::remove_var(STATE_ENV);
            std::env::remove_var("ZIRV_CTX_AGENT_BIN");
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert!(
            result.expect("proceed_on_failure must not error").is_some(),
            "expected a skip/failure message"
        );
    }
}
