//! Provider-neutral development workflows.
//!
//! This subsystem intentionally lives outside `ctx`: `ctx` supervises agent
//! processes, while this module owns reusable methodology and durable
//! workflow state. The two meet through small data interfaces instead of
//! provider-specific prompt/tool names.

use clap::{Parser, Subcommand};

use super::ctx::CtxResult;

pub mod capability;
pub mod artifact;
pub mod classify;
pub mod engine;
pub mod review;
pub mod skill;
pub mod telemetry;
pub mod verification;

/// Reserved top-level names handled by this command tree. Later workflow
/// layers add implementations for every name; reserving the complete surface
/// now prevents a repository script from taking one over between releases.
pub const TOP_LEVEL_COMMANDS: &[&str] = &["skill", "workflow", "test", "verify", "artifact"];

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
}

fn run(cli: &WorkflowCli, writer: &mut impl std::io::Write) -> CtxResult<i32> {
    match &cli.command {
        WorkflowCommand::Skill(args) => skill::run(args, writer),
        WorkflowCommand::Workflow(args) => engine::run(args, writer),
        WorkflowCommand::Test(args) => verification::run_test(args, writer),
        WorkflowCommand::Verify(args) => verification::run_verify(args, writer),
        WorkflowCommand::Artifact(args) => artifact::run(args, writer),
    }
}

pub fn dispatch(args: &[String]) -> i32 {
    let cli = match WorkflowCli::try_parse_from(args) {
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
}
