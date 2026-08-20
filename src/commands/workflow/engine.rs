//! Versioned workflow definitions and durable execution state.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::classify::{self, Classification, Complexity, Intent, RiskBand};
use super::skill::{SkillRegistry, WorkflowPhase};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, create_private_dir_all, now_secs, repo_slug, write_private};

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
const MAX_STEP_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowKind {
    Feature,
    Bugfix,
    Refactor,
    Spike,
    Review,
}

impl WorkflowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Bugfix => "bugfix",
            Self::Refactor => "refactor",
            Self::Spike => "spike",
            Self::Review => "review",
        }
    }

    fn intent(self) -> Intent {
        match self {
            Self::Feature => Intent::Feature,
            Self::Bugfix => Intent::Bugfix,
            Self::Refactor => Intent::Refactor,
            Self::Spike => Intent::Spike,
            Self::Review => Intent::Review,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepCondition {
    Always,
    ComplexityAtLeast(Complexity),
    RiskAtLeast(RiskBand),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub phase: WorkflowPhase,
    pub skill: String,
    pub condition: StepCondition,
    pub approval: bool,
    pub max_attempts: u8,
}

impl WorkflowStep {
    fn applies(&self, classification: &Classification) -> bool {
        match self.condition {
            StepCondition::Always => true,
            StepCondition::ComplexityAtLeast(minimum) => classification.complexity >= minimum,
            StepCondition::RiskAtLeast(minimum) => classification.risk >= minimum,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub schema_version: u32,
    pub kind: WorkflowKind,
    pub description: String,
    pub steps: Vec<WorkflowStep>,
}

impl WorkflowDefinition {
    pub fn materialize(&self, classification: &Classification) -> Vec<WorkflowStep> {
        self.steps
            .iter()
            .filter(|step| step.applies(classification))
            .cloned()
            .collect()
    }
}

fn step(
    id: &str,
    phase: WorkflowPhase,
    skill: &str,
    condition: StepCondition,
    approval: bool,
) -> WorkflowStep {
    WorkflowStep {
        id: id.to_string(),
        phase,
        skill: skill.to_string(),
        condition,
        approval,
        max_attempts: MAX_STEP_ATTEMPTS,
    }
}

pub fn definitions() -> Vec<WorkflowDefinition> {
    use Complexity as C;
    use RiskBand as R;
    use StepCondition as When;
    use WorkflowPhase as Phase;
    vec![
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Feature,
            description: "Design, implement, test and proportionally review a feature.".into(),
            steps: vec![
                step("design", Phase::Design, "design", When::ComplexityAtLeast(C::Substantial), true),
                step("plan", Phase::Plan, "plan", When::ComplexityAtLeast(C::Substantial), false),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step("review", Phase::Review, "review", When::RiskAtLeast(R::Medium), false),
                step("verify", Phase::Verify, "verify", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Bugfix,
            description: "Reproduce, fix, test and verify a defect.".into(),
            steps: vec![
                step("debug", Phase::Debug, "systematic-debugging", When::Always, false),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step("review", Phase::Review, "review", When::RiskAtLeast(R::Medium), false),
                step("verify", Phase::Verify, "verify", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Refactor,
            description: "Refactor with behavior-preserving tests and proportional review.".into(),
            steps: vec![
                step("design", Phase::Design, "design", When::ComplexityAtLeast(C::Substantial), true),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step("review", Phase::Review, "review", When::RiskAtLeast(R::Medium), false),
                step("verify", Phase::Verify, "verify", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Spike,
            description: "Time-bounded exploration with explicit findings.".into(),
            steps: vec![
                step("design", Phase::Design, "design", When::Always, false),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("verify", Phase::Verify, "verify", When::RiskAtLeast(R::Medium), false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Review,
            description: "Independent review with inspectable disposition.".into(),
            steps: vec![
                step("review", Phase::Review, "review", When::Always, false),
                step("verify", Phase::Verify, "verify", When::RiskAtLeast(R::High), false),
            ],
        },
    ]
}

pub fn definition(kind: WorkflowKind) -> WorkflowDefinition {
    definitions()
        .into_iter()
        .find(|definition| definition.kind == kind)
        .expect("every WorkflowKind has a built-in definition")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowStatus {
    Running,
    AwaitingApproval,
    Failed,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    pub task: String,
    pub kind: WorkflowKind,
    #[serde(default)]
    pub adapter: Option<String>,
    pub classification: Classification,
    pub steps: Vec<WorkflowStep>,
    pub current_step: usize,
    pub completed_steps: Vec<String>,
    pub attempts: BTreeMap<String, u8>,
    #[serde(default)]
    pub review_findings: Vec<super::review::ReviewFinding>,
    pub status: WorkflowStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

impl WorkflowState {
    pub fn current(&self) -> Option<&WorkflowStep> {
        self.steps.get(self.current_step)
    }

    fn start(
        repo: PathBuf,
        task: String,
        kind: WorkflowKind,
        adapter: Option<String>,
        classification: Classification,
    ) -> Self {
        let steps = definition(kind).materialize(&classification);
        let status = if steps.first().is_some_and(|step| step.approval) {
            WorkflowStatus::AwaitingApproval
        } else {
            WorkflowStatus::Running
        };
        let now = now_secs();
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo,
            task,
            kind,
            adapter,
            classification,
            steps,
            current_step: 0,
            completed_steps: Vec::new(),
            attempts: BTreeMap::new(),
            review_findings: Vec::new(),
            status,
            created_at: now,
            updated_at: now,
        }
    }
}

fn repo_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.workflows().join(repo_slug(repo))
}

fn state_path(state: &StateDir, repo: &Path, id: &str) -> CtxResult<PathBuf> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("invalid workflow id '{id}'").into());
    }
    Ok(repo_dir(state, repo).join(format!("{id}.json")))
}

fn active_path(state: &StateDir, repo: &Path) -> PathBuf {
    repo_dir(state, repo).join("active")
}

pub(crate) fn save(state_dir: &StateDir, state: &WorkflowState, active: bool) -> CtxResult<()> {
    let dir = repo_dir(state_dir, &state.repo);
    create_private_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(state)?;
    write_private(&state_path(state_dir, &state.repo, &state.id)?, &json)?;
    if active {
        write_private(&active_path(state_dir, &state.repo), &state.id)?;
    } else if active_path(state_dir, &state.repo).exists() {
        std::fs::remove_file(active_path(state_dir, &state.repo))?;
    }
    Ok(())
}

pub fn load(state: &StateDir, repo: &Path, id: &str) -> CtxResult<WorkflowState> {
    let path = state_path(state, repo, id)?;
    let value: WorkflowState = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    if value.schema_version != WORKFLOW_SCHEMA_VERSION {
        return Err(format!(
            "workflow '{}': unsupported state schema {}",
            value.id, value.schema_version
        )
        .into());
    }
    Ok(value)
}

pub fn load_active(state: &StateDir, repo: &Path) -> CtxResult<Option<WorkflowState>> {
    let path = active_path(state, repo);
    if !path.exists() {
        return Ok(None);
    }
    let id = std::fs::read_to_string(path)?;
    load(state, repo, id.trim()).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StepOutcome {
    Success,
    Failure,
}

pub fn advance(
    state_dir: &StateDir,
    state: WorkflowState,
    outcome: StepOutcome,
) -> CtxResult<WorkflowState> {
    advance_with_evidence(state_dir, state, outcome, None)
}

#[derive(Debug, Clone, Default)]
pub struct TransitionEvidence {
    pub duration_ms: Option<u64>,
    pub adapter: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub worker_count: u32,
}

pub fn advance_with_evidence(
    state_dir: &StateDir,
    mut state: WorkflowState,
    outcome: StepOutcome,
    evidence: Option<&TransitionEvidence>,
) -> CtxResult<WorkflowState> {
    if state.status == WorkflowStatus::AwaitingApproval {
        return Err("current workflow step is awaiting approval".into());
    }
    if state.status != WorkflowStatus::Running {
        return Err(format!("workflow is {:?}, not running", state.status).into());
    }
    let current = state.current().cloned().ok_or("workflow has no current step")?;
    match outcome {
        StepOutcome::Success => {
            if matches!(current.phase, WorkflowPhase::Test | WorkflowPhase::Verify) {
                let final_only = current.phase == WorkflowPhase::Verify;
                if !super::verification::latest_is_fresh_and_passing(
                    state_dir,
                    &state.repo,
                    final_only,
                )? {
                    let command = if final_only { "zirv verify" } else { "zirv test changed" };
                    return Err(format!(
                        "step '{}' requires fresh passing evidence for the current change set; run `{command}`",
                        current.id
                    )
                    .into());
                }
            }
            state.completed_steps.push(current.id.clone());
            state.current_step += 1;
            state.status = match state.current() {
                None => WorkflowStatus::Completed,
                Some(step) if step.approval => WorkflowStatus::AwaitingApproval,
                Some(_) => WorkflowStatus::Running,
            };
        }
        StepOutcome::Failure => {
            let attempts = state.attempts.entry(current.id.clone()).or_default();
            *attempts = attempts.saturating_add(1);
            if *attempts >= current.max_attempts {
                state.status = WorkflowStatus::Failed;
            }
        }
    }
    state.updated_at = now_secs();
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    save(state_dir, &state, active)?;
    let evidence = evidence.cloned().unwrap_or_default();
    let (findings_total, findings_meaningful, findings_dismissed) =
        super::telemetry::finding_counts(&state.review_findings);
    let mut event = super::telemetry::TelemetryEvent::new(match outcome {
        StepOutcome::Success => super::telemetry::TelemetryKind::PhaseCompleted,
        StepOutcome::Failure => super::telemetry::TelemetryKind::PhaseFailed,
    });
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(current.phase);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.duration_ms = evidence.duration_ms;
    event.adapter = evidence.adapter;
    event.model = evidence.model;
    event.role = evidence.role;
    event.input_tokens = evidence.input_tokens;
    event.output_tokens = evidence.output_tokens;
    event.succeeded = Some(outcome == StepOutcome::Success);
    event.findings_total = findings_total;
    event.findings_meaningful = findings_meaningful;
    event.findings_dismissed = findings_dismissed;
    event.fix_round = state.attempts.get(&current.id).copied().unwrap_or(0);
    event.worker_count = evidence.worker_count;
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::from_env(),
    );
    if state.status == WorkflowStatus::Completed {
        let mut completed = super::telemetry::TelemetryEvent::new(
            super::telemetry::TelemetryKind::WorkflowCompleted,
        );
        completed.workflow_id = Some(state.id.clone());
        completed.intent = Some(state.classification.intent);
        completed.complexity = Some(state.classification.complexity);
        completed.risk = Some(state.classification.risk);
        completed.succeeded = Some(true);
        let _ = super::telemetry::record(
            state_dir,
            &state.repo,
            &completed,
            &super::telemetry::TelemetryConfig::from_env(),
        );
    }
    Ok(state)
}

pub fn approve(state_dir: &StateDir, mut state: WorkflowState) -> CtxResult<WorkflowState> {
    if state.status != WorkflowStatus::AwaitingApproval {
        return Err("workflow is not awaiting approval".into());
    }
    state.status = WorkflowStatus::Running;
    state.updated_at = now_secs();
    save(state_dir, &state, true)?;
    Ok(state)
}

/// Current ephemeral skill context for the context compiler/session prompt.
/// Completed steps are intentionally absent; the durable state remains in
/// [`WorkflowState`] and is never accumulated into model context.
pub fn render_current_context(
    state: &WorkflowState,
    repo: &Path,
    home: Option<&Path>,
) -> CtxResult<Option<String>> {
    let Some(step) = state.current() else {
        return Ok(None);
    };
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    ) {
        return Ok(None);
    }
    let registry = SkillRegistry::load(repo, home, true)?;
    let stack = registry.resolve_stack(&step.skill)?;
    let mut rendered = format!(
        "zirv workflow step\nworkflow: {}\nstep: {}\nphase: {}\nstate: {:?}\n",
        state.kind.as_str(), step.id, step.phase, state.status
    );
    for skill in stack {
        rendered.push_str(&format!(
            "\n[skill {}@{}; source={}]\n{}\n",
            skill.manifest.id,
            skill.manifest.version,
            skill.source,
            skill.manifest.instructions.trim()
        ));
    }
    Ok(Some(rendered))
}

pub fn active_skill_context(repo: &Path) -> CtxResult<Option<String>> {
    let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let Some(state) = load_active(&state_dir, repo)? else {
        return Ok(None);
    };
    render_current_context(&state, repo, dirs::home_dir().as_deref())
}

#[derive(Debug, Args)]
pub struct WorkflowArgs {
    #[command(subcommand)]
    pub command: WorkflowSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkflowSubcommand {
    /// List built-in workflow definitions.
    List(OutputArgs),
    /// Show a workflow definition.
    Show(ShowArgs),
    /// Classify a task without starting a workflow.
    Classify(classify::ClassifyArgs),
    /// Start and persist a workflow.
    Start(StartArgs),
    /// Show one workflow instance, or the active one.
    Status(StatusArgs),
    /// Restore a running workflow as this repository's active workflow.
    Resume(StateIdArgs),
    /// Print only the current step's resolved skill context.
    Context(StatusArgs),
    /// Approve the current gated step.
    Approve(StateIdArgs),
    /// Record a step result and transition the state machine.
    Advance(AdvanceArgs),
    /// Build compact review packages and persist finding dispositions.
    Review(super::review::ReviewArgs),
    /// Aggregate privacy-conscious local workflow telemetry.
    Stats(super::telemetry::StatsArgs),
}

#[derive(Debug, Args)]
pub struct OutputArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    #[arg(value_enum)]
    pub kind: WorkflowKind,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    #[arg(value_enum)]
    pub kind: WorkflowKind,
    #[arg(long)]
    pub task: String,
    /// Validate every selected skill against this adapter before starting.
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    #[arg(long)]
    pub changed_lines: Option<usize>,
    #[arg(long)]
    pub tests_changed: bool,
    #[arg(long, value_enum)]
    pub complexity: Option<Complexity>,
    #[arg(long, value_enum)]
    pub risk: Option<RiskBand>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub id: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StateIdArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AdvanceArgs {
    pub id: String,
    #[arg(long, value_enum)]
    pub outcome: StepOutcome,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub duration_ms: Option<u64>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub input_tokens: Option<u64>,
    #[arg(long)]
    pub output_tokens: Option<u64>,
    #[arg(long, default_value_t = 0)]
    pub workers: u32,
}

fn resolve_repo(repo: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match repo {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

fn resolve_state() -> CtxResult<StateDir> {
    StateDir::resolve(&|key| std::env::var(key).ok())
}

fn write_state(writer: &mut impl Write, state: &WorkflowState, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, state)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "workflow: {}", state.id)?;
        writeln!(writer, "kind: {}", state.kind.as_str())?;
        writeln!(writer, "status: {:?}", state.status)?;
        writeln!(
            writer,
            "classification: {:?}/{:?} risk={} ({:?})",
            state.classification.intent,
            state.classification.complexity,
            state.classification.risk_score,
            state.classification.risk
        )?;
        if let Some(step) = state.current() {
            writeln!(writer, "current: {} ({}, skill {})", step.id, step.phase, step.skill)?;
        } else {
            writeln!(writer, "current: none")?;
        }
        writeln!(writer, "completed: {}", state.completed_steps.join(", "))?;
    }
    Ok(())
}

pub fn run(args: &WorkflowArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        WorkflowSubcommand::List(args) => {
            let definitions = definitions();
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &definitions)?;
                writeln!(writer)?;
            } else {
                for definition in definitions {
                    writeln!(writer, "{}\t{}", definition.kind.as_str(), definition.description)?;
                }
            }
        }
        WorkflowSubcommand::Show(args) => {
            let definition = definition(args.kind);
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &definition)?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "{}: {}", definition.kind.as_str(), definition.description)?;
                for step in definition.steps {
                    writeln!(
                        writer,
                        "  {}\t{}\tskill={}\twhen={:?}\tapproval={}",
                        step.id, step.phase, step.skill, step.condition, step.approval
                    )?;
                }
            }
        }
        WorkflowSubcommand::Classify(args) => {
            let value = classify::from_args(args)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &value)?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "intent={:?} complexity={:?} risk={:?} score={}",
                    value.intent, value.complexity, value.risk, value.risk_score
                )?;
                for reason in value.reasons {
                    writeln!(writer, "- {reason}")?;
                }
            }
        }
        WorkflowSubcommand::Start(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let classify_args = classify::ClassifyArgs {
                task: args.task.clone(),
                paths: args.paths.clone(),
                changed_lines: args.changed_lines,
                tests_changed: args.tests_changed,
                intent: Some(args.kind.intent()),
                complexity: args.complexity,
                risk: args.risk,
                repo: Some(repo.clone()),
                json: false,
            };
            let classification = classify::from_args(&classify_args)?;
            let definition = definition(args.kind);
            if let Some(agent) = &args.agent {
                let registry = SkillRegistry::load(&repo, dirs::home_dir().as_deref(), true)?;
                let report = super::capability::CapabilityReport::for_adapter(agent);
                for step in definition.materialize(&classification) {
                    registry.ensure_supported(&step.skill, &report)?;
                }
            }
            let state = WorkflowState::start(
                repo,
                args.task.clone(),
                args.kind,
                args.agent.clone(),
                classification,
            );
            let state_dir = resolve_state()?;
            save(&state_dir, &state, true)?;
            let mut event = super::telemetry::TelemetryEvent::new(
                super::telemetry::TelemetryKind::WorkflowStarted,
            );
            event.workflow_id = Some(state.id.clone());
            event.intent = Some(state.classification.intent);
            event.complexity = Some(state.classification.complexity);
            event.risk = Some(state.classification.risk);
            let _ = super::telemetry::record(
                &state_dir,
                &state.repo,
                &event,
                &super::telemetry::TelemetryConfig::from_env(),
            );
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Status(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = match &args.id {
                Some(id) => load(&state_dir, &repo, id)?,
                None => load_active(&state_dir, &repo)?.ok_or("no active workflow")?,
            };
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Resume(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = load(&state_dir, &repo, &args.id)?;
            if !matches!(
                state.status,
                WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
            ) {
                return Err(format!("cannot resume workflow in {:?} state", state.status).into());
            }
            save(&state_dir, &state, true)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Context(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = match &args.id {
                Some(id) => load(&state_dir, &repo, id)?,
                None => load_active(&state_dir, &repo)?.ok_or("no active workflow")?,
            };
            match render_current_context(&state, &repo, dirs::home_dir().as_deref())? {
                Some(context) => write!(writer, "{context}")?,
                None => writeln!(writer, "workflow has no active step context")?,
            }
        }
        WorkflowSubcommand::Approve(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = approve(&state_dir, load(&state_dir, &repo, &args.id)?)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Advance(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let evidence = TransitionEvidence {
                duration_ms: args.duration_ms,
                adapter: args.agent.clone(),
                model: args.model.clone(),
                role: args.role.clone(),
                input_tokens: args.input_tokens,
                output_tokens: args.output_tokens,
                worker_count: args.workers,
            };
            let state = advance_with_evidence(
                &state_dir,
                load(&state_dir, &repo, &args.id)?,
                args.outcome,
                Some(&evidence),
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Review(args) => {
            return super::review::run(args, writer);
        }
        WorkflowSubcommand::Stats(args) => {
            return super::telemetry::run_stats(args, writer);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn low_classification() -> Classification {
        Classification {
            intent: Intent::Feature,
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 5,
            reasons: vec!["small".into()],
        }
    }

    #[test]
    fn low_risk_feature_uses_fast_path_without_design_or_review() {
        let steps = definition(WorkflowKind::Feature).materialize(&low_classification());
        assert_eq!(
            steps.iter().map(|step| step.id.as_str()).collect::<Vec<_>>(),
            ["implement", "test", "verify"]
        );
    }

    #[test]
    fn high_risk_substantial_feature_keeps_design_gate_and_review() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let steps = definition(WorkflowKind::Feature).materialize(&classification);
        assert!(steps.first().unwrap().approval);
        assert!(steps.iter().any(|step| step.id == "review"));
    }

    #[test]
    fn resume_does_not_redispatch_completed_steps() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        state = advance(&state_dir, state, StepOutcome::Success).unwrap();
        let resumed = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(resumed.completed_steps, vec!["implement"]);
        assert_eq!(resumed.current().unwrap().id, "test");
    }

    #[test]
    fn failed_steps_have_a_hard_retry_limit() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        for _ in 0..MAX_STEP_ATTEMPTS {
            state = advance(&state_dir, state, StepOutcome::Failure).unwrap();
        }
        assert_eq!(state.status, WorkflowStatus::Failed);
        assert!(load_active(&state_dir, repo.path()).unwrap().is_none());
    }

    #[test]
    fn switching_steps_replaces_ephemeral_skill_context() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            low_classification(),
        );
        let implement = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        state.completed_steps.push("implement".into());
        state.current_step += 1;
        let testing = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(implement.contains("[skill implement@1"));
        assert!(!testing.contains("[skill implement@1"));
        assert!(testing.contains("[skill testing@1"));
    }
}
