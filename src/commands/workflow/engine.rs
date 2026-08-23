//! Versioned workflow definitions and durable execution state.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::classify::{self, Classification, Complexity, Intent, RiskBand, WorkDomain};
use super::skill::{SkillRegistry, WorkflowPhase};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
const MAX_STEP_ATTEMPTS: u8 = 3;
const MAX_PHASE_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;
const USAGE_SNAPSHOT_TAIL_BYTES: u64 = 256 * 1024;

fn default_true() -> bool {
    true
}

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
    ComplexityOrRisk {
        complexity: Complexity,
        risk: RiskBand,
    },
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
            StepCondition::ComplexityOrRisk { complexity, risk } => {
                classification.complexity >= complexity || classification.risk >= risk
            }
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
                step(
                    "design",
                    Phase::Design,
                    "design",
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                    true,
                ),
                step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    When::ComplexityAtLeast(C::Substantial),
                    false,
                ),
                step(
                    "implement",
                    Phase::Implement,
                    "implement",
                    When::Always,
                    false,
                ),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Bugfix,
            description: "Reproduce, fix, test and verify a defect.".into(),
            steps: vec![
                step(
                    "debug",
                    Phase::Debug,
                    "systematic-debugging",
                    When::Always,
                    false,
                ),
                step(
                    "implement",
                    Phase::Implement,
                    "implement",
                    When::Always,
                    false,
                ),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Refactor,
            description: "Refactor with behavior-preserving tests and proportional review.".into(),
            steps: vec![
                step(
                    "design",
                    Phase::Design,
                    "design",
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                    true,
                ),
                step(
                    "implement",
                    Phase::Implement,
                    "implement",
                    When::Always,
                    false,
                ),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Spike,
            description: "Time-bounded exploration with explicit findings.".into(),
            steps: vec![
                step("design", Phase::Design, "design", When::Always, false),
                step(
                    "implement",
                    Phase::Implement,
                    "implement",
                    When::Always,
                    false,
                ),
                step(
                    "verify",
                    Phase::Verify,
                    "verify",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Review,
            description: "Independent review with inspectable disposition.".into(),
            steps: vec![
                step("review", Phase::Review, "review", When::Always, false),
                step(
                    "verify",
                    Phase::Verify,
                    "verify",
                    When::RiskAtLeast(R::High),
                    false,
                ),
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowProfile {
    #[default]
    Standard,
    Frontend,
}

impl WorkflowProfile {
    fn for_classification(classification: &Classification) -> Self {
        match classification.work_domain.domain {
            WorkDomain::Frontend => Self::Frontend,
            WorkDomain::General => Self::Standard,
        }
    }
}

fn apply_profile(profile: WorkflowProfile, steps: &mut [WorkflowStep]) {
    if profile != WorkflowProfile::Frontend {
        return;
    }
    for step in steps {
        step.skill = match step.phase {
            WorkflowPhase::Design => "frontend-design",
            WorkflowPhase::Plan => "frontend-plan",
            WorkflowPhase::Implement => "frontend-implement",
            WorkflowPhase::Debug => "frontend-debug",
            WorkflowPhase::Test => "frontend-test",
            WorkflowPhase::Review => "frontend-review",
            WorkflowPhase::Verify => "frontend-verify",
            WorkflowPhase::Delegate | WorkflowPhase::Present => continue,
        }
        .into();
        // The agent owns routine visual decisions. The workflow still
        // enforces evidence gates; it never pauses for a theme vote.
        if step.phase == WorkflowPhase::Design {
            step.approval = false;
        }
    }
}

fn materialize(
    kind: WorkflowKind,
    classification: &Classification,
    profile: WorkflowProfile,
) -> Vec<WorkflowStep> {
    let mut steps = definition(kind).materialize(classification);
    apply_profile(profile, &mut steps);
    steps
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    pub task: String,
    pub kind: WorkflowKind,
    /// Automatically selected methodology overlay. This is derived from the
    /// task and change surface; there is deliberately no initialization flag.
    #[serde(default)]
    pub profile: WorkflowProfile,
    #[serde(default)]
    pub adapter: Option<String>,
    /// Whether operator-global and repository skills may override built-ins.
    /// Persisted so resume/prompt composition cannot silently change the
    /// trust mode selected at workflow start.
    #[serde(default = "default_true")]
    pub include_custom_skills: bool,
    pub classification: Classification,
    pub steps: Vec<WorkflowStep>,
    pub current_step: usize,
    pub completed_steps: Vec<String>,
    pub attempts: BTreeMap<String, u8>,
    #[serde(default)]
    pub review_findings: Vec<super::review::ReviewFinding>,
    #[serde(default)]
    pub review_evidence: Vec<super::review::ReviewRunEvidence>,
    #[serde(default)]
    pub usage_checkpoint: Option<UsageCheckpoint>,
    #[serde(default)]
    pub phase_started_at: u64,
    pub status: WorkflowStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

impl WorkflowState {
    pub fn current(&self) -> Option<&WorkflowStep> {
        self.steps.get(self.current_step)
    }

    pub(crate) fn start(
        repo: PathBuf,
        task: String,
        kind: WorkflowKind,
        adapter: Option<String>,
        include_custom_skills: bool,
        classification: Classification,
    ) -> Self {
        let profile = WorkflowProfile::for_classification(&classification);
        let steps = materialize(kind, &classification, profile);
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
            profile,
            adapter,
            include_custom_skills,
            classification,
            steps,
            current_step: 0,
            completed_steps: Vec::new(),
            attempts: BTreeMap::new(),
            review_findings: Vec::new(),
            review_evidence: Vec::new(),
            usage_checkpoint: None,
            phase_started_at: now,
            status,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCheckpoint {
    pub session_id: String,
    pub adapter: String,
    pub transcript_bytes: u64,
    pub cumulative_input_tokens: u64,
    pub cumulative_output_tokens: u64,
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

#[derive(Debug, Clone, Default)]
pub struct TransitionEvidence {
    pub duration_ms: Option<u64>,
    pub adapter: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub token_usage_source: Option<String>,
    pub worker_count: u32,
}

fn session_identity() -> Option<(String, String)> {
    let session_id = std::env::var(crate::commands::ctx::adapters::SESSION_ENV).ok()?;
    let adapter = std::env::var(crate::commands::ctx::adapters::AGENT_ENV).ok()?;
    let valid_session = !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    let valid_adapter = !adapter.is_empty()
        && adapter.len() <= 64
        && adapter.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        });
    (valid_session && valid_adapter).then_some((session_id, adapter))
}

fn adapter_by_name(name: &str) -> Option<Box<dyn crate::commands::ctx::adapters::AgentAdapter>> {
    crate::commands::ctx::adapters::ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .map(|(_, constructor)| constructor(None))
}

fn transcript_path(repo: &Path, session_id: &str, adapter: &str) -> Option<PathBuf> {
    let adapter = adapter_by_name(adapter)?;
    Some(
        adapter.transcript_path(&crate::commands::ctx::event::SessionRef {
            id: crate::commands::ctx::event::SessionId::parse(session_id),
            cwd: repo.to_path_buf(),
        }),
    )
}

fn read_transcript_range(path: &Path, start: u64, end: u64) -> Option<String> {
    let length = end.checked_sub(start)?;
    if length > MAX_PHASE_TRANSCRIPT_BYTES {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut body = Vec::with_capacity(usize::try_from(length).ok()?);
    file.take(length).read_to_end(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

fn cumulative_snapshot(
    path: &Path,
    transcript_bytes: u64,
    adapter: &dyn crate::commands::ctx::adapters::AgentAdapter,
) -> crate::commands::ctx::event::TranscriptUsage {
    if !adapter.transcript_usage_is_cumulative() || transcript_bytes == 0 {
        return Default::default();
    }
    let start = transcript_bytes.saturating_sub(USAGE_SNAPSHOT_TAIL_BYTES);
    read_transcript_range(path, start, transcript_bytes)
        .as_deref()
        .and_then(|body| adapter.transcript_usage(body))
        .unwrap_or_default()
}

fn usage_checkpoint(repo: &Path) -> Option<UsageCheckpoint> {
    let (session_id, adapter_name) = session_identity()?;
    let adapter = adapter_by_name(&adapter_name)?;
    let path = transcript_path(repo, &session_id, &adapter_name)?;
    let transcript_bytes = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let usage = cumulative_snapshot(&path, transcript_bytes, adapter.as_ref());
    Some(UsageCheckpoint {
        session_id,
        adapter: adapter_name,
        transcript_bytes,
        cumulative_input_tokens: usage.input_tokens,
        cumulative_output_tokens: usage.output_tokens,
    })
}

fn usage_since(
    repo: &Path,
    checkpoint: &UsageCheckpoint,
) -> Option<crate::commands::ctx::event::TranscriptUsage> {
    let adapter = adapter_by_name(&checkpoint.adapter)?;
    let path = transcript_path(repo, &checkpoint.session_id, &checkpoint.adapter)?;
    let end = std::fs::metadata(&path).ok()?.len();
    if end < checkpoint.transcript_bytes {
        return None;
    }
    let body = read_transcript_range(&path, checkpoint.transcript_bytes, end)?;
    let usage = adapter.transcript_usage(&body)?;
    if adapter.transcript_usage_is_cumulative() {
        Some(crate::commands::ctx::event::TranscriptUsage {
            input_tokens: usage
                .input_tokens
                .saturating_sub(checkpoint.cumulative_input_tokens),
            output_tokens: usage
                .output_tokens
                .saturating_sub(checkpoint.cumulative_output_tokens),
        })
    } else {
        Some(usage)
    }
}

fn enrich_transition_evidence(
    state: &mut WorkflowState,
    mut evidence: TransitionEvidence,
) -> TransitionEvidence {
    if evidence.duration_ms.is_none() {
        let started = if state.phase_started_at == 0 {
            state.updated_at
        } else {
            state.phase_started_at
        };
        evidence.duration_ms = Some(now_secs().saturating_sub(started).saturating_mul(1000));
    }
    let previous = state.usage_checkpoint.clone();
    let current = usage_checkpoint(&state.repo);
    let mut observed = previous
        .as_ref()
        .and_then(|checkpoint| usage_since(&state.repo, checkpoint));
    if let (Some(previous), Some(current)) = (&previous, &current)
        && (previous.session_id != current.session_id || previous.adapter != current.adapter)
    {
        let beginning = UsageCheckpoint {
            transcript_bytes: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            ..current.clone()
        };
        if let Some(next) = usage_since(&state.repo, &beginning) {
            let total = observed.get_or_insert_default();
            total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
        }
    }
    if let Some(usage) = observed {
        if evidence.input_tokens.is_none() {
            evidence.input_tokens = Some(usage.input_tokens);
        }
        if evidence.output_tokens.is_none() {
            evidence.output_tokens = Some(usage.output_tokens);
        }
        if evidence.token_usage_source.is_none() {
            evidence.token_usage_source = Some("harness-transcript-delta".into());
        }
    }
    if evidence.adapter.is_none() {
        evidence.adapter = current
            .as_ref()
            .map(|checkpoint| checkpoint.adapter.clone())
            .or_else(|| state.adapter.clone());
    }
    if evidence.model.is_none() {
        evidence.model = std::env::var(crate::commands::ctx::adapters::SEAT_MODEL_ENV).ok();
    }
    state.usage_checkpoint = current;
    evidence
}

/// Re-measure risk when a workflow reaches a gated step, and never lower it.
///
/// Classification used to be frozen at `workflow start`, which for the common
/// case (start the workflow, then do the work) measured an empty tree: the
/// review step was decided before a single line existed. Re-measuring here
/// means the tree that actually got written is what decides whether review is
/// required. Only Review/Verify steps are added by this path -- a design gate
/// appearing after the implementation is finished would be ceremony, not
/// safety -- and completed steps are never re-run.
///
/// Fails safe, not silently, when Git cannot be measured (not a repository,
/// no commits): the band is escalated one step (`classify::mark_unavailable`)
/// rather than left standing unchallenged -- see the Decision Log entry
/// "Unmeasurable risk fails safe, not open".
fn reclassify_at_gate(state: &mut WorkflowState) {
    let Some(step) = state.current().cloned() else {
        return;
    };
    if !matches!(step.phase, WorkflowPhase::Review | WorkflowPhase::Verify) {
        return;
    }
    let measured = classify::git_change_input(&state.repo, state.task.clone())
        .ok()
        .map(|mut input| {
            input.intent_override = Some(state.classification.intent);
            input
        })
        .and_then(|input| classify::classify(&input).ok());
    let Some(measured) = measured else {
        let raised = classify::mark_unavailable(
            &mut state.classification,
            format!(
                "git measurement unavailable at step '{}' (not a repository, or no commits)",
                step.id
            ),
        );
        if raised {
            rematerialize_after_risk_increase(state);
        }
        return;
    };
    if measured.work_domain.domain == WorkDomain::Frontend
        && state.profile == WorkflowProfile::Standard
    {
        state.profile = WorkflowProfile::Frontend;
        state.classification.work_domain = measured.work_domain.clone();
        apply_profile(state.profile, &mut state.steps);
        state.classification.reasons.push(format!(
            "frontend workflow profile selected at step '{}'",
            step.id
        ));
    }
    if measured.risk <= state.classification.risk {
        state.classification.reasons.sort();
        return;
    }
    state.classification.risk = measured.risk;
    state.classification.risk_score = state.classification.risk_score.max(measured.risk_score);
    state.classification.changed_files = measured.changed_files;
    state.classification.changed_lines = measured.changed_lines;
    state.classification.reasons.push(format!(
        "reclassified at step '{}': measured risk {:?}",
        step.id, measured.risk
    ));
    state.classification.reasons.sort();
    rematerialize_after_risk_increase(state);
}

/// Adds any Review/Verify step the just-raised risk band newly requires,
/// without re-running or reordering completed steps. Shared by the measured
/// re-classification above and by the fail-safe escalation applied when Git
/// measurement is unavailable at a gate.
fn rematerialize_after_risk_increase(state: &mut WorkflowState) {
    let completed = state.completed_steps.clone();
    let known: Vec<String> = state.steps.iter().map(|step| step.id.clone()).collect();
    let mut steps: Vec<WorkflowStep> = completed
        .iter()
        .filter_map(|id| state.steps.iter().find(|step| &step.id == id).cloned())
        .collect();
    steps.extend(
        materialize(state.kind, &state.classification, state.profile)
            .into_iter()
            .filter(|step| {
                !completed.contains(&step.id)
                    && (known.contains(&step.id)
                        || matches!(step.phase, WorkflowPhase::Review | WorkflowPhase::Verify))
            }),
    );
    state.current_step = steps
        .iter()
        .position(|step| !completed.contains(&step.id))
        .unwrap_or(steps.len());
    state.steps = steps;
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
    let current = state
        .current()
        .cloned()
        .ok_or("workflow has no current step")?;
    match outcome {
        StepOutcome::Success => {
            if state.profile == WorkflowProfile::Frontend
                && matches!(
                    current.phase,
                    WorkflowPhase::Test | WorkflowPhase::Review | WorkflowPhase::Verify
                )
                && !super::frontend_detector::latest_is_fresh_and_passing(state_dir, &state.repo)?
            {
                let report = super::frontend_detector::detect_for_workflow(
                    state_dir,
                    &state.repo,
                    matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify),
                )?;
                if !report.passed() || report.truncated || report.analyzed_files.is_empty() {
                    return Err(format!(
                        "frontend step '{}' automatically ran the detector, but evidence did not pass ({} blocking, {} files, truncated={}); inspect with `zirv frontend check --all`",
                        current.id,
                        report.blocking_count(),
                        report.analyzed_files.len(),
                        report.truncated
                    )
                    .into());
                }
            }
            if state.profile == WorkflowProfile::Frontend
                && matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify)
                && !super::frontend_render::latest_visual_is_fresh_and_passing(
                    state_dir,
                    &state.repo,
                )?
            {
                let render = super::frontend_render::render(state_dir, &state.repo)?;
                if !render.passed() {
                    return Err(format!(
                        "frontend step '{}' could not collect automatic rendered evidence: {}; inspect with `zirv frontend render`",
                        current.id,
                        render.notes.join("; ")
                    )
                    .into());
                }
                let review = super::frontend_render::review(
                    state_dir,
                    &state.repo,
                    &super::frontend_render::VisualReviewArgs {
                        repo: Some(state.repo.clone()),
                        agent: None,
                        model: None,
                        json: false,
                    },
                )?;
                if review.verdict != super::frontend_render::VisualVerdict::Pass {
                    return Err(format!(
                        "frontend step '{}' failed automatic visual review round {}: {}",
                        current.id,
                        review.review_round,
                        review.findings.join("; ")
                    )
                    .into());
                }
            }
            if current.phase == WorkflowPhase::Review {
                if state
                    .review_findings
                    .iter()
                    .any(|finding| finding.disposition == super::review::FindingDisposition::Open)
                {
                    return Err(
                        "review findings must have a final disposition before the review step can pass"
                            .into(),
                    );
                }
                let required = super::review::required_independent_reviews_for(&state);
                if required > 0 {
                    if state.review_evidence.is_empty() {
                        return Err(format!(
                            "review step requires {required} fresh independent review run(s); found 0"
                        )
                        .into());
                    }
                    let fingerprint = super::verification::change_fingerprint(&state.repo)?;
                    let completed = state
                        .review_evidence
                        .iter()
                        .filter(|evidence| evidence.change_fingerprint == fingerprint)
                        .count();
                    if completed < required {
                        return Err(format!(
                            "review step requires {required} fresh independent review run(s); found {completed}"
                        )
                        .into());
                    }
                }
            }
            if matches!(current.phase, WorkflowPhase::Test | WorkflowPhase::Verify) {
                let final_only = current.phase == WorkflowPhase::Verify;
                if !super::verification::latest_is_fresh_and_passing(
                    state_dir,
                    &state.repo,
                    final_only,
                )? {
                    let command = if final_only {
                        "zirv verify"
                    } else {
                        "zirv test changed"
                    };
                    return Err(format!(
                        "step '{}' requires fresh passing evidence for the current change set; run `{command}`",
                        current.id
                    )
                    .into());
                }
            }
            state.completed_steps.push(current.id.clone());
            state.current_step += 1;
            reclassify_at_gate(&mut state);
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
    state.phase_started_at = state.updated_at;
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
    event.work_domain = Some(state.classification.work_domain.domain);
    event.duration_ms = evidence.duration_ms;
    event.adapter = evidence.adapter;
    event.model = evidence.model;
    event.role = evidence.role;
    event.input_tokens = evidence.input_tokens;
    event.output_tokens = evidence.output_tokens;
    event.token_usage_source = evidence.token_usage_source;
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
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    if state.status == WorkflowStatus::Completed {
        let mut completed = super::telemetry::TelemetryEvent::new(
            super::telemetry::TelemetryKind::WorkflowCompleted,
        );
        completed.workflow_id = Some(state.id.clone());
        completed.intent = Some(state.classification.intent);
        completed.complexity = Some(state.classification.complexity);
        completed.risk = Some(state.classification.risk);
        completed.work_domain = Some(state.classification.work_domain.domain);
        completed.succeeded = Some(true);
        completed.findings_total = findings_total;
        completed.findings_meaningful = findings_meaningful;
        completed.findings_dismissed = findings_dismissed;
        let _ = super::telemetry::record(
            state_dir,
            &state.repo,
            &completed,
            &super::telemetry::TelemetryConfig::for_repo(&state.repo),
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
    let registry = SkillRegistry::load_for_repo(repo, home, state.include_custom_skills)?;
    let stack = registry.resolve_stack(&step.skill)?;
    let task = state
        .task
        .chars()
        .take(1_024)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut rendered = format!(
        "zirv workflow step\nworkflow: {}\nprofile: {:?}\ntask: {}\nstep: {}\nphase: {}\nstate: {:?}\n",
        state.kind.as_str(),
        state.profile,
        task,
        step.id,
        step.phase,
        state.status
    );
    if state.profile == WorkflowProfile::Frontend {
        let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
        let profile = super::frontend::ensure_profile(&state_dir, repo)?;
        rendered.push('\n');
        rendered.push_str(&super::frontend::render_profile(&profile));
        rendered.push('\n');
    }
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
    match render_current_context(&state, repo, dirs::home_dir().as_deref()) {
        Ok(context) => Ok(context),
        // The caller composes a prompt and cannot fail over this, but a
        // silently dropped workflow layer is a session running without the
        // methodology it thinks it has. Say so once, on the channel a repo
        // cannot silence (`chrome.events` is REPO_FORBIDDEN).
        Err(error) => {
            announce_degradation(repo, &error.to_string());
            Ok(None)
        }
    }
}

fn announce_degradation(repo: &Path, reason: &str) {
    let enabled =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())
            .map_or(true, |cfg| cfg.chrome.events);
    crate::commands::ctx::announce::Announcer::new(enabled, false).emit(
        &crate::commands::ctx::announce::Event::WorkflowLayerSkipped {
            reason: reason.to_string(),
        },
    );
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
    /// Ignore operator-global and repository-provided skill overrides.
    #[arg(long)]
    pub built_in_only: bool,
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
        writeln!(writer, "profile: {:?}", state.profile)?;
        writeln!(writer, "status: {:?}", state.status)?;
        writeln!(
            writer,
            "classification: {:?}/{:?} risk={} ({:?})",
            state.classification.intent,
            state.classification.complexity,
            state.classification.risk_score,
            state.classification.risk
        )?;
        if let classify::RiskMeasurement::Unavailable { reason } =
            &state.classification.risk_measurement
        {
            writeln!(writer, "risk measurement: unavailable ({reason})")?;
        }
        if let Some(step) = state.current() {
            writeln!(
                writer,
                "current: {} ({}, skill {})",
                step.id, step.phase, step.skill
            )?;
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
                    writeln!(
                        writer,
                        "{}\t{}",
                        definition.kind.as_str(),
                        definition.description
                    )?;
                }
            }
        }
        WorkflowSubcommand::Show(args) => {
            let definition = definition(args.kind);
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &definition)?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "{}: {}",
                    definition.kind.as_str(),
                    definition.description
                )?;
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
                    "intent={:?} domain={:?} complexity={:?} risk={:?} score={}",
                    value.intent,
                    value.work_domain.domain,
                    value.complexity,
                    value.risk,
                    value.risk_score
                )?;
                for reason in value.reasons {
                    writeln!(writer, "- {reason}")?;
                }
            }
        }
        WorkflowSubcommand::Start(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let inherited_agent = session_identity().map(|(_, adapter)| adapter);
            let selected_agent = args.agent.clone().or(inherited_agent);
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
            let state_dir = resolve_state()?;
            if classification.work_domain.domain == WorkDomain::Frontend {
                // Eager zero-touch bootstrap. Prompt rendering refreshes this
                // derived profile as repository evidence evolves.
                super::frontend::ensure_profile(&state_dir, &repo)?;
            }
            let definition = definition(args.kind);
            let profile = WorkflowProfile::for_classification(&classification);
            if let Some(agent) = &selected_agent {
                let registry = SkillRegistry::load_for_repo(
                    &repo,
                    dirs::home_dir().as_deref(),
                    !args.built_in_only,
                )?;
                let report = super::capability::CapabilityReport::for_repo(agent, &repo)?;
                for step in materialize(definition.kind, &classification, profile) {
                    registry.ensure_supported(&step.skill, &report)?;
                }
            }
            let mut state = WorkflowState::start(
                repo,
                args.task.clone(),
                args.kind,
                selected_agent,
                !args.built_in_only,
                classification,
            );
            state.usage_checkpoint = usage_checkpoint(&state.repo);
            save(&state_dir, &state, true)?;
            let mut event = super::telemetry::TelemetryEvent::new(
                super::telemetry::TelemetryKind::WorkflowStarted,
            );
            event.workflow_id = Some(state.id.clone());
            event.intent = Some(state.classification.intent);
            event.complexity = Some(state.classification.complexity);
            event.risk = Some(state.classification.risk);
            event.work_domain = Some(state.classification.work_domain.domain);
            let _ = super::telemetry::record(
                &state_dir,
                &state.repo,
                &event,
                &super::telemetry::TelemetryConfig::for_repo(&state.repo),
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
            let mut state = load(&state_dir, &repo, &args.id)?;
            let evidence = enrich_transition_evidence(
                &mut state,
                TransitionEvidence {
                    duration_ms: args.duration_ms,
                    adapter: args.agent.clone(),
                    model: args.model.clone(),
                    role: args.role.clone(),
                    input_tokens: args.input_tokens,
                    output_tokens: args.output_tokens,
                    token_usage_source: (args.input_tokens.is_some()
                        || args.output_tokens.is_some())
                    .then(|| "operator-reported".into()),
                    worker_count: args.workers,
                },
            );
            let state = advance_with_evidence(&state_dir, state, args.outcome, Some(&evidence))?;
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
            declared_scope: false,
            work_domain: Default::default(),
            risk_measurement: classify::RiskMeasurement::Measured,
            reasons: vec!["small".into()],
        }
    }

    #[test]
    fn low_risk_feature_uses_fast_path_without_design_or_review() {
        let steps = definition(WorkflowKind::Feature).materialize(&low_classification());
        assert_eq!(
            steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
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
    fn high_risk_bounded_feature_keeps_design_gate() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        classification.risk = RiskBand::High;
        let steps = definition(WorkflowKind::Feature).materialize(&classification);
        assert!(
            steps
                .first()
                .is_some_and(|step| step.id == "design" && step.approval)
        );
    }

    #[test]
    fn frontend_classification_selects_the_frontend_profile_automatically() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;

        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "build a responsive dashboard UI".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );

        assert_eq!(state.profile, WorkflowProfile::Frontend);
        assert_eq!(state.current().unwrap().skill, "frontend-implement");
    }

    #[test]
    fn frontend_design_is_autonomous_but_keeps_the_evidence_phases() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;

        let state = WorkflowState::start(
            PathBuf::from("repo"),
            "build a frontend design system".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );

        assert_eq!(state.status, WorkflowStatus::Running);
        assert_eq!(state.steps[0].skill, "frontend-design");
        assert!(!state.steps[0].approval);
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-review")
        );
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-verify")
        );
    }

    #[test]
    fn frontend_test_step_fails_closed_without_detector_evidence() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "build a frontend component".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("automatically ran the detector")
                || error.contains("cannot inspect changed paths"),
            "{error}"
        );
    }

    #[test]
    fn frontend_review_step_collects_visual_evidence_automatically_and_fails_closed() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::write(
            repo.path().join("App.tsx"),
            "export const App = () => <main />;\n",
        )
        .unwrap();
        git(&["add", "App.tsx"]);
        git(&["commit", "-q", "-m", "base"]);
        std::fs::write(
            repo.path().join("App.tsx"),
            "export const App = () => <main><h1>Settings</h1></main>;\n",
        )
        .unwrap();
        let profile = super::super::frontend::ensure_profile(&state_dir, repo.path()).unwrap();
        let detector = super::super::frontend_detector::DetectorReport {
            schema_version: super::super::frontend_detector::DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: repo.path().canonicalize().unwrap(),
            change_fingerprint: super::super::verification::change_fingerprint(repo.path())
                .unwrap(),
            profile_fingerprint: profile.source_fingerprint,
            scope: super::super::frontend_detector::DetectorScope::Changed,
            generated_at: now_secs(),
            analyzed_files: vec![PathBuf::from("App.tsx")],
            analyzed_bytes: 64,
            truncated: false,
            findings: Vec::new(),
            waivers_loaded: 0,
            waivers_rejected: 0,
        };
        super::super::frontend_detector::save_report(&state_dir, &detector).unwrap();
        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review a frontend component".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .unwrap();

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("automatic rendered evidence"), "{error}");
        assert!(error.contains("zirv frontend render"), "{error}");
        assert!(!error.contains("frontend review --help"), "{error}");
    }

    #[test]
    fn approval_gate_must_be_explicitly_released() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert!(
            advance_with_evidence(&state_dir, state.clone(), StepOutcome::Success, None).is_err()
        );
        let approved = approve(&state_dir, state).unwrap();
        assert_eq!(approved.status, WorkflowStatus::Running);
        assert_eq!(approved.current().unwrap().id, "design");
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
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None).unwrap();
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
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        for _ in 0..MAX_STEP_ATTEMPTS {
            state = advance_with_evidence(&state_dir, state, StepOutcome::Failure, None).unwrap();
        }
        assert_eq!(state.status, WorkflowStatus::Failed);
        assert!(load_active(&state_dir, repo.path()).unwrap().is_none());
    }

    /// #88: outside a git repository, `reclassify_at_gate` used to silently
    /// leave the risk band exactly as declared/measured at `workflow start`
    /// -- the safety net that exists specifically to catch a mismatch was
    /// inert exactly where it mattered most. It must now report the
    /// unmeasured state and escalate the band one step, adding whatever
    /// Review/Verify step the escalated band newly requires.
    #[test]
    fn reclassify_at_gate_fails_safe_when_git_is_unavailable_outside_a_repository() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        assert!(
            !state.steps.iter().any(|step| step.id == "review"),
            "the Low-risk fast path starts with no review step: {:?}",
            state.steps
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();

        reclassify_at_gate(&mut state);

        assert!(
            matches!(
                state.classification.risk_measurement,
                classify::RiskMeasurement::Unavailable { .. }
            ),
            "{:?}",
            state.classification
        );
        assert_eq!(state.classification.risk, RiskBand::Medium);
        assert!(
            state
                .classification
                .reasons
                .iter()
                .any(|reason| reason.contains("risk escalated"))
        );
        assert!(
            state.steps.iter().any(|step| step.id == "review"),
            "the escalated band newly requires review: {:?}",
            state.steps
        );
    }

    /// #88: a repository that exists but has no commits fails the same Git
    /// calls a non-repository does, and must fail the same safe way.
    #[test]
    fn reclassify_at_gate_fails_safe_when_the_repository_has_no_commits() {
        let repo = tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();

        reclassify_at_gate(&mut state);

        assert!(
            matches!(
                state.classification.risk_measurement,
                classify::RiskMeasurement::Unavailable { .. }
            ),
            "{:?}",
            state.classification
        );
        assert_eq!(state.classification.risk, RiskBand::Medium);
    }

    /// No change to behavior when measurement succeeds: reclassification
    /// with a real Git history still reports `Measured`.
    #[test]
    fn reclassify_at_gate_stays_measured_when_git_succeeds() {
        let repo = tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();

        reclassify_at_gate(&mut state);

        assert_eq!(
            state.classification.risk_measurement,
            classify::RiskMeasurement::Measured
        );
    }

    #[test]
    fn phase_usage_reads_only_the_appended_claude_transcript() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555555";
        let path = transcript_path(repo.path(), session_id, "claude").expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":10}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let checkpoint = UsageCheckpoint {
            session_id: session_id.into(),
            adapter: "claude".into(),
            transcript_bytes: std::fs::metadata(&path).unwrap().len(),
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":7,"cache_read_input_tokens":5,"output_tokens":3}}}}}}"#
        )
        .unwrap();

        assert_eq!(
            usage_since(repo.path(), &checkpoint),
            Some(crate::commands::ctx::event::TranscriptUsage {
                input_tokens: 12,
                output_tokens: 3,
            })
        );
    }

    #[test]
    fn switching_steps_replaces_ephemeral_skill_context() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let implement = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(implement.contains("task: small feature"));
        state.completed_steps.push("implement".into());
        state.current_step += 1;
        let testing = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(implement.contains("[skill implement@1"));
        assert!(!testing.contains("[skill implement@1"));
        assert!(testing.contains("[skill testing@1"));
    }

    #[test]
    fn built_in_only_state_survives_prompt_rendering() {
        let repo = tempdir().unwrap();
        let skills = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("implement.yaml"),
            "schema_version: 1\nid: implement\nversion: 2\nname: Override\ndescription: untrusted override\ncontext_budget_bytes: 64\nphases: [implement]\ninstructions: repository override\n",
        )
        .unwrap();
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            false,
            low_classification(),
        );
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill implement@1; source=built-in]"));
        assert!(!context.contains("repository override"));
    }

    #[test]
    fn review_step_cannot_pass_with_open_findings() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review change".into(),
            WorkflowKind::Review,
            None,
            true,
            low_classification(),
        );
        state
            .review_findings
            .push(super::super::review::ReviewFinding {
                id: "finding-1".into(),
                severity: super::super::review::FindingSeverity::Major,
                summary: "concrete defect".into(),
                path: None,
                line: None,
                disposition: super::super::review::FindingDisposition::Open,
                recommended_disposition: None,
                created_at: now_secs(),
            });
        let error =
            advance_with_evidence(&state_dir, state, StepOutcome::Success, None).unwrap_err();
        assert!(error.to_string().contains("final disposition"));
    }

    #[test]
    fn medium_risk_review_requires_independent_evidence() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review change".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        let error =
            advance_with_evidence(&state_dir, state, StepOutcome::Success, None).unwrap_err();
        assert!(error.to_string().contains("independent review"));
    }
}
