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

pub mod adoption;
pub mod agents;
pub mod artifact;
pub mod capability;
pub mod classify;
pub mod deploy;
pub mod engine;
pub mod frontend;
pub mod frontend_detector;
pub mod frontend_render;
pub mod maintain;
pub mod review;
pub mod skill;
pub mod telemetry;
pub mod verification;

/// A workflow's position, as much as issue #209/v3's dashboard footer
/// segment (§D) needs to show it: which methodology, which step, and
/// whether that step is gated on the operator's approval right now.
/// Deliberately smaller than `engine::WorkflowState` -- the dashboard reads
/// this on its own disk-facts throttle (`ctx::dash::mod::FactsCache::
/// refresh_if_due`) and has no use for the rest of a workflow's state
/// (artifacts, review findings, classification, ...), the same "small data
/// interface" this module's own doc comment above calls for rather than
/// exposing `ctx` to workflow internals wholesale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorkflowSummary {
    pub kind: &'static str,
    pub step: String,
    pub awaiting_approval: bool,
}

/// The same read `zirv workflow status` uses when no explicit `--id` is
/// given (`engine::load_active`): the repo's active-workflow pointer file,
/// then that workflow's own state file -- both plain file reads, no
/// subprocess, safe to call on a ~1s render-loop throttle.
///
/// `None` covers every reason there is nothing to show: no active workflow
/// for this repo, or one that failed to load (a torn write mid-save, an
/// unsupported schema version left behind by an older binary). The
/// dashboard's footer renders the same dim `▸ –` placeholder either way --
/// surfacing a parse error where an operator expects a status glyph would
/// be a worse failure mode than just not showing one.
pub fn active_workflow_summary(
    state: &crate::commands::ctx::state::StateDir,
    repo: &std::path::Path,
) -> Option<ActiveWorkflowSummary> {
    let wf = engine::load_active(state, repo).ok().flatten()?;
    let step = wf.current().map(|s| s.id.clone()).unwrap_or_default();
    Some(ActiveWorkflowSummary {
        kind: wf.kind.as_str(),
        step,
        awaiting_approval: wf.status == engine::WorkflowStatus::AwaitingApproval,
    })
}

/// Reserved top-level names handled by this command tree. Later workflow
/// layers add implementations for every name; reserving the complete surface
/// now prevents a repository script from taking one over between releases.
pub const TOP_LEVEL_COMMANDS: &[&str] = &[
    "skill", "workflow", "test", "verify", "artifact", "frontend",
];

/// Operator gates over repository-provided workflow input. Each is resolved
/// from operator-controlled config and fails closed when that config cannot be
/// trusted.
pub(crate) struct RepoGates {
    pub checks: bool,
    pub skills: bool,
    pub agents: bool,
    /// Operator-owned `[workflow] check_env_passthrough` (REPO_FORBIDDEN,
    /// `~/.zirv/ctx.toml`/`ZIRV_CTX_*` only) -- extra environment variable
    /// names ADDED to `verification::DEFAULT_CHECK_ENV_PASSTHROUGH` when a
    /// check child is spawned. Empty (never widened) when the config could
    /// not even be read, same fail-closed posture as `checks`/`skills`/
    /// `agents` above.
    pub check_env_passthrough: Vec<String>,
    /// Operator-owned `[workflow] allow_empty_verify` (REPO_FORBIDDEN,
    /// `~/.zirv/ctx.toml`/`ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY`/flags only,
    /// issue #268) -- lets `verification::run_mode` report `Passed` instead
    /// of `Inconclusive` when zero checks are configured or discoverable.
    /// `false` (the stricter, fail-closed reading) when the config could
    /// not even be read, same posture as `checks`/`skills`/`agents` above.
    pub allow_empty_verify: bool,
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
            agents: cfg.workflow.repo_agents_enabled,
            check_env_passthrough: cfg.workflow.check_env_passthrough,
            allow_empty_verify: cfg.workflow.allow_empty_verify,
        },
        Err(error) => {
            announce_unreadable_config(&error.to_string());
            RepoGates {
                checks: false,
                skills: false,
                agents: false,
                check_env_passthrough: Vec::new(),
                allow_empty_verify: false,
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

    // Issue #209/v3 §D: `active_workflow_summary`, the dashboard footer's
    // own read of the same active-workflow state `zirv workflow status`
    // resolves.

    fn test_classification() -> classify::Classification {
        classify::Classification {
            intent: classify::Intent::Feature,
            complexity: classify::Complexity::Bounded,
            risk: classify::RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 10,
            declared_scope: true,
            work_domain: classify::DomainClassification::default(),
            risk_measurement: classify::RiskMeasurement::default(),
            reasons: Vec::new(),
        }
    }

    #[test]
    fn active_workflow_summary_is_none_with_nothing_active() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        assert_eq!(active_workflow_summary(&state, repo.path()), None);
    }

    #[test]
    fn active_workflow_summary_carries_the_kind_and_current_step() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        let summary = active_workflow_summary(&state_dir, repo.path())
            .expect("an active workflow was just saved");
        assert_eq!(summary.kind, "feature");
        assert_eq!(summary.step, wf.current().unwrap().id);
    }

    #[test]
    fn active_workflow_summary_reports_awaiting_approval() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let mut wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        wf.status = engine::WorkflowStatus::AwaitingApproval;
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        let summary = active_workflow_summary(&state_dir, repo.path()).expect("saved active");
        assert!(summary.awaiting_approval);
    }

    #[test]
    fn active_workflow_summary_is_none_for_a_different_repo() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let other_repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        assert_eq!(active_workflow_summary(&state_dir, other_repo.path()), None);
    }

    /// Issue #242 follow-up: `engine::auto_spawn_decision`'s argv is built by
    /// hand, so a renamed flag (`--repo`, `--agent`, the review-run
    /// positional id) would fail only at runtime, silently, unless something
    /// feeds it through the real parser. This does: every argv the pure
    /// decision produces for Review/Test/Verify must parse through the same
    /// `WorkflowCli` `main.rs` itself dispatches into, with the fields
    /// landing where the decision meant them to.
    #[test]
    fn auto_spawn_argv_parses_through_the_real_top_level_cli() {
        let repo = tempfile::tempdir().unwrap();
        let mut classification = test_classification();
        classification.complexity = classify::Complexity::Substantial;
        classification.risk = classify::RiskBand::High;
        let mut state = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "ship it".into(),
            engine::WorkflowKind::Feature,
            Some("claude".to_string()),
            true,
            classification,
        );
        state.status = engine::WorkflowStatus::Running;

        for phase in [
            skill::WorkflowPhase::Review,
            skill::WorkflowPhase::Test,
            skill::WorkflowPhase::Verify,
        ] {
            state.current_step = state
                .steps
                .iter()
                .position(|step| step.phase == phase)
                .unwrap_or_else(|| panic!("{phase:?} step must be present in this workflow"));
            let spawn = engine::auto_spawn_decision(&state, true, true, None)
                .unwrap_or_else(|skip| panic!("{phase:?} must be eligible to fire: {skip:?}"));

            let mut argv = vec!["zirv".to_string()];
            argv.extend(spawn.argv.clone());
            let cli = WorkflowCli::try_parse_from(&argv)
                .unwrap_or_else(|err| panic!("auto-spawn argv {argv:?} must parse: {err}"));

            match phase {
                skill::WorkflowPhase::Review => {
                    let WorkflowCommand::Workflow(wf) = &cli.command else {
                        panic!("expected a `workflow` subcommand, got {:?}", cli.command)
                    };
                    let engine::WorkflowSubcommand::Review(review_args) = &wf.command else {
                        panic!("expected `workflow review`, got {:?}", wf.command)
                    };
                    let review::ReviewCommand::Run(run_args) = &review_args.command else {
                        panic!("expected `review run`, got {:?}", review_args.command)
                    };
                    assert_eq!(run_args.id, state.id);
                    assert_eq!(run_args.agent, "claude");
                    assert_eq!(run_args.repo.as_deref(), Some(state.repo.as_path()));
                }
                skill::WorkflowPhase::Test => {
                    let WorkflowCommand::Test(test_args) = &cli.command else {
                        panic!("expected a `test` subcommand, got {:?}", cli.command)
                    };
                    let verification::TestCommand::Changed(run_args) = &test_args.command else {
                        panic!("expected `test changed`, got {:?}", test_args.command)
                    };
                    assert_eq!(run_args.repo.as_deref(), Some(state.repo.as_path()));
                }
                skill::WorkflowPhase::Verify => {
                    let WorkflowCommand::Verify(verify_args) = &cli.command else {
                        panic!("expected a `verify` subcommand, got {:?}", cli.command)
                    };
                    assert_eq!(verify_args.run.repo.as_deref(), Some(state.repo.as_path()));
                }
                _ => unreachable!("only the three eligible phases are iterated"),
            }
        }
    }
}
