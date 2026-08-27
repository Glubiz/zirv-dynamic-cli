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
/// directory cannot be resolved at all (a state dir this far off the rails
/// cannot record a permit either way, so there is nothing left to govern).
/// Best-effort, the same as every other piece of state-dir housekeeping in
/// this codebase: a script must never fail just because zirv's own
/// supervision state could not be found. `cwd` mirrors `agent_command::
/// resolve_repo`'s own preference for a script's own `cd`-tracked directory
/// over the process's real one, since `Command`'s `cd` handling only ever
/// updates `context["cwd"]` (see `execute`'s own handling), never
/// `std::env::set_current_dir`.
///
/// Finding B3: a `CtxConfig::load` error (e.g. a repo `ctx.toml` committing a
/// `REPO_FORBIDDEN` key, which is a hard error by design -- see that
/// constant's own doc comment) used to fall straight through to `None` here,
/// same as an unclassified command -- silently disabling the WHOLE permit
/// system for every heavy command in that repo, exactly the ungoverned-
/// concurrency incident (#133) this module exists to prevent. A config that
/// fails to load is not evidence heavy commands are safe to run unbounded;
/// it falls back to `SuperviseConfig::default()` (the built-in patterns,
/// `max_heavy_operations` at its safe default of 1) instead, so a broken or
/// hostile config narrows what this budget SEES, never whether it enforces
/// anything at all.
async fn heavy_permit_for(
    command: &str,
    cwd: Option<&str>,
) -> Option<crate::commands::ctx::permit::HeavyPermit> {
    use crate::commands::ctx::config::{CtxConfig, SuperviseConfig, env_from_process};
    use crate::commands::ctx::permit;
    use crate::commands::ctx::state::StateDir;

    let env = env_from_process();
    let state = StateDir::resolve(&env).ok()?;
    let repo = cwd
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;

    let supervise = match CtxConfig::load(&repo, &env) {
        Ok(cfg) => cfg.supervise,
        Err(err) => {
            warn_permit_config_load_failed_once(err.as_ref());
            SuperviseConfig::default()
        }
    };

    if !permit::is_heavy(command, &supervise.heavy_command_patterns) {
        return None;
    }

    let limit = supervise.max_heavy_operations;
    let label = permit_label(&env, command);
    // Refuse-not-queue is right for a *spawn* (issue #133's own gate, now
    // `dash::fulfill_spawn_request`) but wrong for a command the operator
    // already put in a script -- failing `zirv test` outright because a
    // background build is running would be worse than waiting for it. Polls
    // on a bounded interval up to a generous cap, then proceeds without a
    // permit rather than blocking a script forever -- see `wait_for_permit`'s
    // own doc comment for why that escape hatch stays loud rather than
    // simply vanishing back to the pre-#155 ungoverned behavior.
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const MAX_WAIT: Duration = Duration::from_secs(600);
    wait_for_permit(&state, limit, &label, command, POLL_INTERVAL, MAX_WAIT).await
}

/// Emits a warning that `CtxConfig::load` failed and the heavy-operation
/// budget is falling back to `SuperviseConfig::default()`, exactly once per
/// process -- the same one-shot discipline `config::announce_unparsable_
/// layers_once` and `poll.rs`'s `announce_keychain_prompt_once` already use
/// for their own process-wide degradation notices, applied here because
/// `heavy_permit_for` is called from every single heavy command a script
/// runs and a config error does not go away between calls.
fn warn_permit_config_load_failed_once(err: &dyn std::error::Error) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "zirv: WARNING: ctx config failed to load ({err}) -- the heavy-operation permit \
             budget is falling back to its built-in defaults (max_heavy_operations=1, no extra \
             heavy_command_patterns) rather than being disabled."
        );
    });
}

/// The actual wait loop `heavy_permit_for` drives with its own real
/// `POLL_INTERVAL`/`MAX_WAIT` -- split out so a test can drive it with
/// millisecond-scale durations instead of actually waiting out a 10-minute
/// cap.
///
/// Finding B2: this loop's liveness escape hatch (proceeding WITHOUT a
/// permit once `max_wait` elapses) is deliberately kept -- removing it could
/// deadlock a script behind a build that never finishes, and this codebase's
/// philosophy (`wrap.rs`'s own doc comment) is that supervision failure
/// degrades to passthrough rather than blocking forever. But a command that
/// proceeds ungoverned is exactly the failure mode the whole budget exists
/// to prevent, so it must never be silent: unlike the one-time "waiting"
/// notice below, the timeout itself always gets its own loud warning line,
/// naming both how long this command waited and that it is now running
/// without the budget's protection.
async fn wait_for_permit(
    state: &crate::commands::ctx::state::StateDir,
    limit: usize,
    label: &str,
    command: &str,
    poll_interval: Duration,
    max_wait: Duration,
) -> Option<crate::commands::ctx::permit::HeavyPermit> {
    use crate::commands::ctx::permit;

    let mut waited = Duration::ZERO;
    let mut announced = false;
    loop {
        if let Some(permit) = permit::acquire(state, limit, label) {
            return Some(permit);
        }
        if waited >= max_wait {
            eprintln!(
                "zirv: WARNING: waited {}s for a heavy-operation slot ({limit} in use) before \
                 running `{command}` -- proceeding WITHOUT a permit. This heavy operation is now \
                 running UNGOVERNED alongside whatever else is holding the budget.",
                max_wait.as_secs()
            );
            return None;
        }
        if !announced {
            // Issue #162: a wait that cannot say WHO holds the budget is
            // undiagnosable -- the same complaint that made the old
            // heavy-worker refusal's own advice ("see `zirv ctx status`")
            // point at a list that never named the actual holder. Read off
            // `permit::live_records`, the exact same source `zirv ctx
            // status`'s own occupancy line reads, so the two can never
            // disagree about who is holding a slot.
            let holders: Vec<String> = permit::live_records(state)
                .into_iter()
                .map(|record| record.label)
                .collect();
            eprintln!(
                "zirv: waiting for a heavy-operation slot ({limit} in use: {}) before running `{command}`",
                holders.join(", ")
            );
            announced = true;
        }
        sleep(poll_interval).await;
        waited += poll_interval;
    }
}

/// Issue #162: names WHO holds a permit, not just what it is running --
/// `session <short>: <command>` when this process is itself supervised (the
/// common case: a `zirv test`/`zirv build` invoked from inside an agent's
/// own shell, which inherits `ZIRV_CTX_SESSION` from its supervisor), or the
/// bare command when it is not (an operator's own unsupervised terminal).
/// Whatever this returns is what both `zirv ctx status`'s occupancy line and
/// this module's own wait message name -- they read the same on-disk record
/// back through `permit::live_records`, so they cannot drift apart.
fn permit_label(env: &impl Fn(&str) -> Option<String>, command: &str) -> String {
    match env(crate::commands::ctx::adapters::SESSION_ENV) {
        Some(session) => format!(
            "session {}: {command}",
            crate::commands::ctx::sessions::short_id(&session)
        ),
        None => command.to_string(),
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

    /// Finding B3: a `CtxConfig::load` error used to fall straight through to
    /// `None`, the same answer an unclassified command gets -- silently
    /// disabling the whole permit system for a repo whose `ctx.toml` commits
    /// a `REPO_FORBIDDEN` key (a hard error by design). Writes exactly that
    /// (a repo layer setting `supervise.max_heavy_operations`, which only the
    /// operator's home layer or the matching env var may set) and asserts
    /// `heavy_permit_for` still classifies and gates `cargo build` at the
    /// default policy rather than letting it run ungoverned.
    #[tokio::test]
    async fn heavy_permit_for_falls_back_to_default_policy_when_config_load_fails() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join("ctx.toml"),
            "[supervise]\nmax_heavy_operations = 8\n",
        )
        .expect("write a REPO_FORBIDDEN key");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let _state_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            crate::commands::ctx::state::STATE_ENV,
            Some(state_dir.path().to_str().expect("utf8 path")),
        )]);

        // Sanity: this really does make `CtxConfig::load` fail -- otherwise
        // this test would pass for the wrong reason.
        let env = crate::commands::ctx::config::env_from_process();
        assert!(
            crate::commands::ctx::config::CtxConfig::load(repo.path(), &env).is_err(),
            "a repo layer setting a REPO_FORBIDDEN key must make load fail"
        );

        let permit = heavy_permit_for(
            "cargo build",
            Some(repo.path().to_str().expect("utf8 path")),
        )
        .await;
        assert!(
            permit.is_some(),
            "a heavy command must still be gated at the default policy even when \
             the repo's own config fails to load, never left ungoverned"
        );
    }

    /// Issue #162(a): the acceptance criterion is that a delegation dying
    /// during startup must leave the budget exactly as it found it. In this
    /// codebase's old design a session registered against the budget before
    /// it could fail; in this one only an actual classified command holds a
    /// permit, for exactly the duration of its own child process -- so a
    /// command that fails still releases on the way out through ordinary
    /// `?`/`Drop`, proven here through the real seam (`Command::execute`)
    /// rather than only `heavy_permit_for` in isolation.
    #[tokio::test]
    async fn a_failing_heavy_command_still_releases_its_permit() {
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

        // Classifies as heavy (`cargo build*`) and fails immediately on its
        // own bad flag -- no real build ever starts, matching #162's own
        // "dies during startup" shape.
        let command = Command {
            command: "cargo build --this-flag-does-not-exist".to_string(),
            capture: None,
            description: None,
            options: None,
        };
        let mut context = HashMap::new();
        context.insert("cwd".to_string(), repo.path().to_string_lossy().to_string());

        let result = command.execute(&mut context).await;
        assert!(
            result.is_err(),
            "the classified-heavy command must fail as invoked"
        );
        assert_eq!(
            crate::commands::ctx::permit::live_count(&state),
            0,
            "a permit acquired by a command that then fails must not survive it"
        );
    }

    /// Issue #162(b): a wait/occupancy message that cannot say WHO holds the
    /// budget is undiagnosable. Inside a supervised session (`ZIRV_CTX_
    /// SESSION` set, the common case for a `zirv test`/`zirv build` step run
    /// from an agent's own shell) the label names the session.
    #[test]
    fn permit_label_names_the_supervising_session_when_present() {
        let env = |key: &str| {
            (key == crate::commands::ctx::adapters::SESSION_ENV)
                .then(|| "11111111-2222-4333-8444-555555555555".to_string())
        };
        assert_eq!(
            permit_label(&env, "cargo nextest run"),
            "session 11111111: cargo nextest run"
        );
    }

    /// Outside any supervised session (an operator's own unsupervised
    /// terminal), the label degrades to the bare command -- there is no
    /// session identity to name.
    #[test]
    fn permit_label_falls_back_to_the_bare_command_when_unsupervised() {
        let env = |_: &str| None;
        assert_eq!(permit_label(&env, "cargo build"), "cargo build");
    }

    /// Finding B2: once the wait exceeds its cap, the loop must still
    /// proceed WITHOUT a permit (the liveness escape hatch stays -- removing
    /// it could deadlock a script behind a build that never finishes) but
    /// must not do so silently. Drives the real loop with millisecond-scale
    /// durations instead of the real 600s `MAX_WAIT`, so this stays fast and
    /// deterministic: the only live permit is held for the whole test, so
    /// every poll fails until `max_wait` elapses.
    #[tokio::test]
    async fn wait_for_permit_proceeds_ungoverned_once_the_wait_times_out() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        let _held = crate::commands::ctx::permit::acquire(&state, 1, "cargo build")
            .expect("the only slot is held for the whole test");

        let result = wait_for_permit(
            &state,
            1,
            "cargo test",
            "cargo test",
            Duration::from_millis(5),
            Duration::from_millis(30),
        )
        .await;
        assert!(
            result.is_none(),
            "the wait must give up and proceed without a permit once max_wait elapses"
        );
    }
}
