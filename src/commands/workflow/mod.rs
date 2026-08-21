//! Provider-neutral development workflows.
//!
//! This subsystem intentionally lives outside `ctx`: `ctx` supervises agent
//! processes, while this module owns reusable methodology and durable
//! workflow state. The two meet through small data interfaces instead of
//! provider-specific prompt/tool names.

use clap::{Parser, Subcommand};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use super::ctx::CtxResult;

pub mod artifact;
pub mod capability;
pub mod classify;
pub mod engine;
pub mod frontend;
pub mod review;
pub mod skill;
pub mod telemetry;
pub mod verification;

/// Reserved top-level names handled by this command tree. Later workflow
/// layers add implementations for every name; reserving the complete surface
/// now prevents a repository script from taking one over between releases.
pub const TOP_LEVEL_COMMANDS: &[&str] = &[
    "skill",
    "workflow",
    "test",
    "verify",
    "artifact",
    "frontend",
];

/// The two operator gates over repository-provided workflow input:
/// `workflow.repo_checks_enabled` (may `.zirv/verify.toml` and `package.json`
/// script commands run at all) and `workflow.repo_skills_enabled` (is
/// `.zirv/skills/` loaded at all).
pub(crate) struct RepoGates {
    pub checks: bool,
    pub skills: bool,
}

/// Resolves both gates, failing **closed** when the configuration cannot be
/// read.
///
/// This is the one answer both gates need, and it has to be the same answer.
/// Reading the config per-gate produced two different wrong behaviors on an
/// unparseable `.zirv/ctx.toml` -- which a repository checkout controls: the
/// skill gate defaulted to *enabled* (so a malformed repo config was a way to
/// force the untrusted skill layer back on) while verification hard-errored
/// (so the same file bricked `zirv test`/`zirv verify` in that checkout).
/// Neither is acceptable, and they disagree: an unreadable config means the
/// operator's intent is unknown, so the security decision goes to "no
/// repository-provided input" while everything zirv owns itself -- built-in
/// skills, discovered toolchain checks -- keeps working.
pub(crate) fn repo_gates(repo: &std::path::Path) -> RepoGates {
    match crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()) {
        Ok(cfg) => RepoGates {
            checks: cfg.workflow.repo_checks_enabled,
            skills: cfg.workflow.repo_skills_enabled,
        },
        Err(error) => {
            announce_unreadable_config(&error.to_string());
            RepoGates {
                checks: false,
                skills: false,
            }
        }
    }
}

/// The degradation notice for a configuration that would not load. `chrome.
/// events` is the switch this channel normally reads, and it lives in the very
/// file that just failed to parse, so the operator's own `--quiet`/
/// `ZIRV_CTX_QUIET` is consulted directly instead of assuming silence.
fn announce_unreadable_config(reason: &str) {
    let quiet = std::env::var("ZIRV_CTX_QUIET")
        .map(|value| matches!(value.trim(), "true" | "1"))
        .unwrap_or(false);
    crate::commands::ctx::announce::Announcer::new(!quiet, false).emit(
        &crate::commands::ctx::announce::Event::WorkflowGatesClosed {
            reason: reason.to_string(),
        },
    );
}

/// Put a shell-backed workflow command in its own process group on Unix so a
/// timeout can stop descendants as well as the shell. Windows uses
/// `taskkill /T` in [`terminate_process_tree`] instead.
pub(crate) fn isolate_process_tree(_command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        _command.process_group(0);
    }
}

/// Terminate a child and every process it spawned, then reap the direct child.
pub(crate) fn terminate_process_tree(child: &mut Child) -> CtxResult<()> {
    let direct_child_exited = child.try_wait()?.is_some();
    #[cfg(not(unix))]
    if direct_child_exited {
        return Ok(());
    }

    #[cfg(unix)]
    let process_group = child.id() as libc::pid_t;
    #[cfg(unix)]
    let tree_exited = unsafe { libc::kill(-process_group, 0) != 0 };
    #[cfg(unix)]
    if direct_child_exited && tree_exited {
        return Ok(());
    }
    #[cfg(unix)]
    unsafe {
        // The child was spawned with `process_group(0)`, so its pid is also
        // its process-group id. A negative pid addresses the whole group.
        libc::kill(-process_group, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    if !crate::commands::ctx::supervise::kill_tree(child.id()) {
        let _ = child.kill();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let direct_child_exited = child.try_wait()?.is_some();
        #[cfg(unix)]
        let tree_exited = unsafe { libc::kill(-process_group, 0) != 0 };
        #[cfg(not(unix))]
        let tree_exited = direct_child_exited;
        if direct_child_exited && tree_exited {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
    Ok(())
}

#[derive(Debug, Parser)]
#[command(name = "zirv", disable_help_subcommand = true)]
struct WorkflowCli {
    #[command(subcommand)]
    command: WorkflowCommand,
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Inspect model-agnostic engineering skills.
    Skill(skill::SkillArgs),
    /// Run and inspect durable development workflows.
    Workflow(engine::WorkflowArgs),
    /// Run repository-aware checks.
    Test(verification::TestArgs),
    /// Run final verification for the current change set.
    Verify(verification::VerifyArgs),
    /// Register and present workflow artifacts.
    Artifact(artifact::ArtifactArgs),
    /// Infer and inspect autonomous frontend quality state.
    Frontend(frontend::FrontendArgs),
}

fn run(cli: &WorkflowCli, writer: &mut impl std::io::Write) -> CtxResult<i32> {
    match &cli.command {
        WorkflowCommand::Skill(args) => skill::run(args, writer),
        WorkflowCommand::Workflow(args) => engine::run(args, writer),
        WorkflowCommand::Test(args) => verification::run_test(args, writer),
        WorkflowCommand::Verify(args) => verification::run_verify(args, writer),
        WorkflowCommand::Artifact(args) => artifact::run(args, writer),
        WorkflowCommand::Frontend(args) => frontend::run(args, writer),
    }
}

fn normalized_args(args: &[String]) -> Vec<String> {
    let mut args = args.to_vec();
    if let Some(command) = args.get_mut(1)
        && TOP_LEVEL_COMMANDS
            .iter()
            .any(|candidate| command.eq_ignore_ascii_case(candidate))
    {
        command.make_ascii_lowercase();
    }
    args
}

pub fn dispatch(args: &[String]) -> i32 {
    let cli = match WorkflowCli::try_parse_from(normalized_args(args)) {
        Ok(cli) => cli,
        Err(err) => {
            let code = if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                0
            } else {
                2
            };
            let _ = err.print();
            return code;
        }
    };

    match run(&cli, &mut std::io::stdout()) {
        Ok(code) => code,
        Err(err) => {
            crate::output::error(err);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_command_parses_in_its_own_tree() {
        let cli = WorkflowCli::try_parse_from(["zirv", "skill", "list"])
            .expect("skill list should parse");
        assert!(matches!(cli.command, WorkflowCommand::Skill(_)));
    }

    #[test]
    fn top_level_workflow_command_is_case_insensitive() {
        let args = ["zirv", "SKILL", "list"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let cli = WorkflowCli::try_parse_from(normalized_args(&args))
            .expect("uppercase reserved workflow command should parse");
        assert!(matches!(cli.command, WorkflowCommand::Skill(_)));
    }

    #[test]
    fn frontend_profile_command_parses_without_an_init_verb() {
        let cli = WorkflowCli::try_parse_from(["zirv", "frontend", "profile"])
            .expect("frontend profile should parse");
        assert!(matches!(cli.command, WorkflowCommand::Frontend(_)));
    }
}
