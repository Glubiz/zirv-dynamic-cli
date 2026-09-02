//! Versioned workflow definitions and durable execution state.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::agents::AgentRegistry;
use super::classify::{self, Classification, Complexity, Intent, RiskBand, WorkDomain};
use super::deploy::DeployTier;
use super::skill::{SkillRegistry, WorkflowPhase};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

pub const WORKFLOW_SCHEMA_VERSION: u32 = 4;
const MAX_STEP_ATTEMPTS: u8 = 3;
const MAX_WORK_ARTIFACT_CONTEXT_BYTES: usize = 24 * 1024;

const INTENT_TEMPLATE: &str = r#"# Intent

## Problem

<!-- What problem are we solving, for whom, and why now? -->

## Desired outcome

<!-- Describe the observable end state. -->

## Constraints

<!-- Technical, product, policy, compatibility, time, or scope constraints. -->

## Open questions

<!-- Keep only questions that materially affect correctness. Use "None" when resolved. -->

## Acceptance criteria

- [ ] <!-- Observable outcome -->
"#;

const SPEC_TEMPLATE: &str = r#"# Specification

## Context

<!-- Existing behavior, architecture, and evidence that constrain the design. -->

## Goals

- <!-- Goal -->

## Non-goals

- <!-- Explicitly out of scope -->

## Design

<!-- Chosen approach, affected boundaries, data/control flow, compatibility, and tradeoffs. -->

## Testing strategy

<!-- Deterministic checks and evidence required before completion. -->

## Risks

<!-- Material risks and mitigations. -->
"#;

const PLAN_TEMPLATE: &str = r#"# Implementation plan

## Ordered tasks

- [ ] T1: <!-- concrete task -->
  - Files: <!-- exact paths or bounded areas -->
  - Verify: <!-- exact command/check -->

## Execution ledger

| Task | Started | Finished | Evidence |
| --- | --- | --- | --- |
| T1 |  |  |  |
"#;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactStage {
    Intent,
    Spec,
    Plan,
}

impl ArtifactStage {
    fn key(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Spec => "spec",
            Self::Plan => "plan",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Intent => "intent.md",
            Self::Spec => "spec.md",
            Self::Plan => "plan.md",
        }
    }

    fn template(self) -> &'static str {
        match self {
            Self::Intent => INTENT_TEMPLATE,
            Self::Spec => SPEC_TEMPLATE,
            Self::Plan => PLAN_TEMPLATE,
        }
    }
}

impl std::fmt::Display for ArtifactStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowArtifactRecord {
    pub stage: ArtifactStage,
    pub rel_path: String,
    pub accepted_hash: Option<String>,
    pub accepted_at: Option<String>,
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
    /// Provider-neutral workflow seat. This is an address, not authority:
    /// dispatch still resolves the seat through the trusted agent registry and
    /// narrows it through the effective policy.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub artifact: Option<ArtifactStage>,
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

fn seat_for_phase(phase: WorkflowPhase) -> Option<String> {
    match phase {
        WorkflowPhase::Implement | WorkflowPhase::Debug => Some("implementer".into()),
        WorkflowPhase::Review => Some("reviewer".into()),
        _ => None,
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
        agent: seat_for_phase(phase),
        artifact: None,
        condition,
        approval,
        max_attempts: MAX_STEP_ATTEMPTS,
    }
}

fn artifact_step(
    id: &str,
    phase: WorkflowPhase,
    skill: &str,
    stage: ArtifactStage,
    condition: StepCondition,
) -> WorkflowStep {
    WorkflowStep {
        id: id.to_string(),
        phase,
        skill: skill.to_string(),
        agent: None,
        artifact: Some(stage),
        condition,
        approval: true,
        max_attempts: MAX_STEP_ATTEMPTS,
    }
}

pub fn definitions() -> Vec<WorkflowDefinition> {
    use ArtifactStage as Artifact;
    use Complexity as C;
    use RiskBand as R;
    use StepCondition as When;
    use WorkflowPhase as Phase;
    vec![
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Feature,
            description: "Capture intent, design and plan proportionally, then implement, test and review.".into(),
            steps: vec![
                artifact_step(
                    "intent",
                    Phase::Intent,
                    "brainstorm",
                    Artifact::Intent,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::Medium,
                    },
                ),
                artifact_step(
                    "spec",
                    Phase::Design,
                    "design",
                    Artifact::Spec,
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                ),
                artifact_step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    Artifact::Plan,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::High,
                    },
                ),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
                step("deploy", Phase::Deploy, "finish-branch", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Bugfix,
            description: "Capture intent when warranted, reproduce, plan larger fixes, test and verify.".into(),
            steps: vec![
                artifact_step(
                    "intent",
                    Phase::Intent,
                    "brainstorm",
                    Artifact::Intent,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::Medium,
                    },
                ),
                step("debug", Phase::Debug, "systematic-debugging", When::Always, false),
                artifact_step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    Artifact::Plan,
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                ),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
                step("deploy", Phase::Deploy, "finish-branch", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Refactor,
            description: "Plan proportional behavior-preserving changes with intent capture for substantial or high-risk work.".into(),
            steps: vec![
                artifact_step(
                    "intent",
                    Phase::Intent,
                    "brainstorm",
                    Artifact::Intent,
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                ),
                artifact_step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    Artifact::Plan,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::Medium,
                    },
                ),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
                step("deploy", Phase::Deploy, "finish-branch", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Spike,
            description: "Capture intent, run time-bounded exploration and record explicit findings.".into(),
            steps: vec![
                artifact_step("intent", Phase::Intent, "brainstorm", Artifact::Intent, When::Always),
                step("design", Phase::Design, "design", When::Always, false),
                step("implement", Phase::Implement, "implement", When::Always, false),
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
    /// Explicitly closed via `zirv workflow close` -- typically a workflow
    /// whose review/fix loop hit `MAX_FIX_REVIEW_ROUNDS` (review.rs) and
    /// would otherwise stay `Running` forever, still reported as this
    /// repository's active workflow by `load_active`. Terminal, like
    /// `Failed`/`Completed`: `resume` refuses to re-activate it.
    Closed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
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

/// Whether a workflow's `profile` came from automatic classification or was
/// later forced by an operator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileSource {
    #[default]
    Classified,
    OperatorOverride,
}

/// Recorded once an operator accepts a workflow's pre-existing blocking
/// frontend findings with `--accept-preexisting-findings` (#251): pre-dating
/// findings the detector's full-surface scan turned up that were not
/// introduced by this change. `blocking`/`total` are the pre-existing counts
/// at the moment of acceptance, not a live re-count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedPreexistingFindings {
    pub step: String,
    pub at: String,
    pub blocking: usize,
    pub total: usize,
}

/// Selects each step's skill for `profile`, in both directions: applying
/// Frontend overlays the frontend-flavored skills, and applying Standard
/// (used by `WorkflowState::set_profile`/`workflow reclassify` reverting a
/// workflow away from Frontend) restores the same base names `materialize`
/// assigns by default. Idempotent either way, so calling it from ordinary
/// materialize with the freshly-derived profile is a no-op the first time.
///
/// `kind` is needed only to restore a Design step's approval requirement
/// (see below) when reverting away from Frontend; it does not otherwise
/// affect which skill each phase gets.
fn apply_profile(kind: WorkflowKind, profile: WorkflowProfile, steps: &mut [WorkflowStep]) {
    // The kind's own authored default approval, keyed by step id -- looked
    // up (not recomputed) so a future kind whose default ever changes still
    // round-trips correctly through Frontend and back.
    let defaults = definition(kind).steps;
    for step in steps {
        step.skill = match (profile, step.phase) {
            (_, WorkflowPhase::Intent) => continue,
            (WorkflowProfile::Frontend, WorkflowPhase::Design) => "frontend-design",
            (WorkflowProfile::Standard, WorkflowPhase::Design) => "design",
            (WorkflowProfile::Frontend, WorkflowPhase::Plan) => "frontend-plan",
            (WorkflowProfile::Standard, WorkflowPhase::Plan) => "plan",
            (WorkflowProfile::Frontend, WorkflowPhase::Implement) => "frontend-implement",
            (WorkflowProfile::Standard, WorkflowPhase::Implement) => "implement",
            (WorkflowProfile::Frontend, WorkflowPhase::Debug) => "frontend-debug",
            (WorkflowProfile::Standard, WorkflowPhase::Debug) => "systematic-debugging",
            (WorkflowProfile::Frontend, WorkflowPhase::Test) => "frontend-test",
            (WorkflowProfile::Standard, WorkflowPhase::Test) => "testing",
            (WorkflowProfile::Frontend, WorkflowPhase::Review) => "frontend-review",
            (WorkflowProfile::Standard, WorkflowPhase::Review) => "review",
            (WorkflowProfile::Frontend, WorkflowPhase::Verify) => "frontend-verify",
            (WorkflowProfile::Standard, WorkflowPhase::Verify) => "verify",
            (_, WorkflowPhase::Deploy | WorkflowPhase::Delegate | WorkflowPhase::Present) => {
                continue;
            }
        }
        .into();
        if step.phase == WorkflowPhase::Design && step.artifact.is_none() {
            if profile == WorkflowProfile::Frontend {
                // The agent owns routine visual decisions. The workflow
                // still enforces evidence gates; it never pauses for a
                // theme vote.
                step.approval = false;
            } else if let Some(default) = defaults.iter().find(|candidate| candidate.id == step.id)
            {
                // Reverting away from Frontend must not leave the kind's
                // own approval requirement permanently overridden.
                step.approval = default.approval;
            }
        }
    }
}

/// Default intent-step skill per kind, absent a `--brainstorm`/
/// `--no-brainstorm` override: on for exploratory Feature/Spike, off for
/// Bugfix/Refactor's autonomous default. `Review` has no intent step.
fn default_brainstorm_for_kind(kind: WorkflowKind) -> bool {
    matches!(kind, WorkflowKind::Feature | WorkflowKind::Spike)
}

/// Selects the intent step's skill, same shape as `apply_profile`.
fn apply_brainstorm_selection(brainstorm: bool, steps: &mut [WorkflowStep]) {
    for step in steps {
        if step.phase == WorkflowPhase::Intent {
            step.skill = if brainstorm {
                "brainstorm"
            } else {
                "write-intent"
            }
            .into();
        }
    }
}

fn apply_deploy_tier(tier: DeployTier, steps: &mut Vec<WorkflowStep>) {
    if tier == DeployTier::Production
        && !steps.iter().any(|step| step.phase == WorkflowPhase::Review)
        && let Some(verify_index) = steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
    {
        steps.insert(
            verify_index,
            step(
                "review",
                WorkflowPhase::Review,
                "review",
                StepCondition::Always,
                false,
            ),
        );
    }
    for step in steps {
        if step.phase == WorkflowPhase::Deploy {
            step.approval = tier >= DeployTier::Staging;
        }
    }
}

fn materialize(
    kind: WorkflowKind,
    classification: &Classification,
    profile: WorkflowProfile,
    deploy_tier: DeployTier,
    brainstorm: bool,
) -> Vec<WorkflowStep> {
    let mut steps = definition(kind).materialize(classification);
    apply_profile(kind, profile, &mut steps);
    apply_brainstorm_selection(brainstorm, &mut steps);
    apply_deploy_tier(deploy_tier, &mut steps);
    steps
}

/// Skill ids that compose one materialized step. The primary step skill stays
/// stable for state/back-compat; substantial implementation additionally
/// receives the resume-safe accepted-plan executor, whose own dependency stack
/// includes worktree isolation and the general implementation discipline.
fn step_skill_ids(step: &WorkflowStep, classification: &Classification) -> Vec<String> {
    let mut ids = Vec::new();
    if step.phase == WorkflowPhase::Implement
        && classification.complexity >= Complexity::Substantial
    {
        ids.push("execute-plan".to_string());
    }
    ids.push(step.skill.clone());
    ids
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
    #[serde(default)]
    pub deploy_tier: DeployTier,
    pub steps: Vec<WorkflowStep>,
    pub current_step: usize,
    pub completed_steps: Vec<String>,
    pub attempts: BTreeMap<String, u8>,
    /// Wall-clock milliseconds each completed step took, keyed by step id.
    /// Read by `zirv workflow status` to render `completed: intent (2m10s)`.
    #[serde(default)]
    pub step_durations_ms: BTreeMap<String, u64>,
    /// Version-controlled workflow work products. Acceptance authority remains
    /// in this private state: repository markdown is never trusted as config.
    #[serde(default)]
    pub artifacts: BTreeMap<String, WorkflowArtifactRecord>,
    #[serde(default)]
    pub review_findings: Vec<super::review::ReviewFinding>,
    #[serde(default)]
    pub review_evidence: Vec<super::review::ReviewRunEvidence>,
    #[serde(default)]
    pub usage_checkpoint: Option<UsageCheckpoint>,
    /// Repository whose frontend the detector/render evidence should scan
    /// instead of `repo`, for workflows tracked in one repository while the
    /// actual frontend under test lives in a sibling checkout. `None` keeps
    /// the historical single-repo behavior of scanning `repo` itself.
    #[serde(default)]
    pub frontend_target_root: Option<PathBuf>,
    /// Whether `profile` came from automatic classification or was later
    /// forced by an operator (`--profile` at start, or `workflow
    /// reclassify`). A state saved before this key existed defaults to
    /// `Classified`, its historical-only behavior.
    #[serde(default)]
    pub profile_source: ProfileSource,
    /// Set once an operator has accepted a workflow's pre-existing (not
    /// newly introduced) blocking frontend findings via
    /// `--accept-preexisting-findings`. Once present, pre-existing blocking
    /// findings stop failing the frontend gate for the rest of this
    /// workflow; newly introduced blocking findings always still fail.
    #[serde(default)]
    pub accepted_preexisting_findings: Option<AcceptedPreexistingFindings>,
    #[serde(default)]
    pub phase_started_at: u64,
    /// Whether the intent step (when present) uses `brainstorm` (interactive
    /// Q&A) or `write-intent` (autonomous). A state saved before this key
    /// existed defaults to interactive on load.
    #[serde(default = "default_true")]
    pub brainstorm: bool,
    pub status: WorkflowStatus,
    /// Operator-supplied reason recorded by `zirv workflow close --reason`.
    /// A state saved before `close` existed defaults to `None`.
    #[serde(default)]
    pub closed_reason: Option<String>,
    /// When this workflow was closed (`WorkflowStatus::Closed`), `now_secs()`
    /// at that moment. `None` for a workflow never closed.
    #[serde(default)]
    pub closed_at: Option<u64>,
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
        let deploy_tier = DeployTier::Development;
        let brainstorm = default_brainstorm_for_kind(kind);
        let steps = materialize(kind, &classification, profile, deploy_tier, brainstorm);
        let status = if steps.first().is_some_and(|step| step.approval) {
            WorkflowStatus::AwaitingApproval
        } else {
            WorkflowStatus::Running
        };
        let now = now_secs();
        let id = uuid::Uuid::new_v4().to_string();
        let artifacts = initial_artifact_records(&id, &steps);
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            id,
            repo,
            task,
            kind,
            profile,
            adapter,
            include_custom_skills,
            classification,
            deploy_tier,
            steps,
            current_step: 0,
            completed_steps: Vec::new(),
            attempts: BTreeMap::new(),
            step_durations_ms: BTreeMap::new(),
            artifacts,
            review_findings: Vec::new(),
            review_evidence: Vec::new(),
            usage_checkpoint: None,
            frontend_target_root: None,
            profile_source: ProfileSource::Classified,
            accepted_preexisting_findings: None,
            phase_started_at: now,
            brainstorm,
            status,
            closed_reason: None,
            closed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Forces this workflow's methodology overlay to `profile`, marks it an
    /// operator override, and re-runs `apply_profile` over the current step
    /// list so every not-yet-completed step picks up the new profile's
    /// skills. Used by `--profile` at `workflow start` (applied after
    /// classification has already materialized the default steps) and by
    /// `workflow reclassify`.
    pub(crate) fn set_profile(&mut self, profile: WorkflowProfile) {
        self.profile = profile;
        self.profile_source = ProfileSource::OperatorOverride;
        apply_profile(self.kind, profile, &mut self.steps);
    }
}

fn initial_artifact_records(
    workflow_id: &str,
    steps: &[WorkflowStep],
) -> BTreeMap<String, WorkflowArtifactRecord> {
    let mut records = BTreeMap::new();
    for step in steps {
        let Some(stage) = step.artifact else {
            continue;
        };
        records
            .entry(stage.key().to_string())
            .or_insert_with(|| WorkflowArtifactRecord {
                stage,
                rel_path: format!(".zirv/work/{workflow_id}/{}", stage.file_name()),
                accepted_hash: None,
                accepted_at: None,
            });
    }
    records
}

fn sync_artifact_records(state: &mut WorkflowState) {
    for step in &state.steps {
        let Some(stage) = step.artifact else {
            continue;
        };
        state
            .artifacts
            .entry(stage.key().to_string())
            .or_insert_with(|| WorkflowArtifactRecord {
                stage,
                rel_path: format!(".zirv/work/{}/{}", state.id, stage.file_name()),
                accepted_hash: None,
                accepted_at: None,
            });
    }
}

/// Refuses a repo-owned workflow artifact path routed through a symlinked
/// `.zirv/work` directory, workflow directory, or artifact file itself --
/// the same defense `agents::load_dir` and `artifact::register` apply to
/// their own repo-owned surfaces. Checked once at the single choke point
/// every reader/writer/hasher of a workflow artifact goes through
/// (`workflow_artifact_path`), before any create/read/hash touches disk.
///
/// A missing component is not a symlink, so `symlink_metadata` erroring with
/// `NotFound` is treated as "nothing to refuse yet" -- `ensure_current_
/// artifact_template` still needs to be able to create these paths fresh.
fn refuse_symlinked_artifact_path(repo: &Path, workflow_id: &str, path: &Path) -> CtxResult<()> {
    let work_root = repo.join(".zirv").join("work");
    let workflow_dir = work_root.join(workflow_id);
    for candidate in [work_root.as_path(), workflow_dir.as_path(), path] {
        if let Ok(metadata) = std::fs::symlink_metadata(candidate)
            && metadata.file_type().is_symlink()
        {
            return Err(format!(
                "refusing symlinked workflow artifact path '{}'",
                candidate.display()
            )
            .into());
        }
    }
    Ok(())
}

/// Design spec risk-section commitment: a repo that `.gitignore`s `.zirv/`
/// (or `.zirv/work/` specifically) silently loses every work-product
/// artifact a workflow produces -- nothing else in this crate would notice.
/// Best-effort only, mirroring `classify::git_change_input`'s own plain
/// `git -C <repo> ...` shell-out: a missing `git`, a non-repository `repo`,
/// or any other probe failure reads as "not ignored" rather than blocking
/// `workflow start` on an environment problem this warning is not
/// authoritative about anyway.
fn work_dir_is_gitignored(repo: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "--quiet", ".zirv/work"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn workflow_artifact_path(state: &WorkflowState, stage: ArtifactStage) -> CtxResult<PathBuf> {
    let record = state
        .artifacts
        .get(stage.key())
        .ok_or_else(|| format!("workflow '{}' has no {stage} artifact record", state.id))?;
    let expected = format!(".zirv/work/{}/{}", state.id, stage.file_name());
    if record.rel_path != expected {
        return Err(format!(
            "workflow '{}' has invalid {stage} artifact path '{}'",
            state.id, record.rel_path
        )
        .into());
    }
    let path = state.repo.join(&record.rel_path);
    refuse_symlinked_artifact_path(&state.repo, &state.id, &path)?;
    Ok(path)
}

fn ensure_current_artifact_template(state: &WorkflowState) -> CtxResult<()> {
    let Some(stage) = state.current().and_then(|step| step.artifact) else {
        return Ok(());
    };
    let path = workflow_artifact_path(state, stage)?;
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or("workflow artifact has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(path, stage.template())?;
    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(crate) fn artifact_hash(path: &Path) -> CtxResult<String> {
    Ok(hash_bytes(&std::fs::read(path)?))
}

fn rfc3339_now() -> String {
    // UTC conversion using Howard Hinnant's civil-from-days algorithm. Keeping
    // this tiny avoids a date/time dependency solely for an audit timestamp.
    let seconds = i64::try_from(now_secs()).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let sod = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe.div_euclid(1_460) + doe.div_euclid(36_524) - doe.div_euclid(146_096))
        .div_euclid(365);
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe.div_euclid(4) - yoe.div_euclid(100));
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    let hour = sod.div_euclid(3_600);
    let minute = sod.rem_euclid(3_600).div_euclid(60);
    let second = sod.rem_euclid(60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn pin_current_artifact(state: &mut WorkflowState) -> CtxResult<ArtifactStage> {
    let stage = state
        .current()
        .and_then(|step| step.artifact)
        .ok_or("current workflow step has no artifact to approve")?;
    ensure_current_artifact_template(state)?;
    let path = workflow_artifact_path(state, stage)?;
    let body = std::fs::read_to_string(&path)?;
    if body.trim() == stage.template().trim() {
        return Err(format!(
            "{stage} artifact is still the untouched template: {}",
            path.display()
        )
        .into());
    }
    let hash = hash_bytes(body.as_bytes());
    let record = state
        .artifacts
        .get_mut(stage.key())
        .ok_or("workflow artifact record disappeared")?;
    record.accepted_hash = Some(hash);
    record.accepted_at = Some(rfc3339_now());
    Ok(stage)
}

fn artifact_drift(state: &WorkflowState) -> CtxResult<Option<ArtifactStage>> {
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let Some(accepted) = record.accepted_hash.as_deref() else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        if !path.exists() || artifact_hash(&path)? != accepted {
            return Ok(Some(stage));
        }
    }
    Ok(None)
}

fn reopen_artifact_gate(state: &mut WorkflowState, stage: ArtifactStage) -> CtxResult<()> {
    let index = state
        .steps
        .iter()
        .position(|step| step.artifact == Some(stage))
        .ok_or_else(|| {
            format!("accepted {stage} artifact no longer has an owning workflow step")
        })?;
    let invalid: Vec<String> = state.steps[index..]
        .iter()
        .map(|step| step.id.clone())
        .collect();
    state
        .completed_steps
        .retain(|completed| !invalid.contains(completed));
    state.current_step = index;
    state.status = WorkflowStatus::AwaitingApproval;
    if let Some(record) = state.artifacts.get_mut(stage.key()) {
        record.accepted_hash = None;
        record.accepted_at = None;
    }
    ensure_current_artifact_template(state)?;
    Ok(())
}

fn append_accepted_artifacts(state: &WorkflowState, rendered: &mut String) -> CtxResult<()> {
    let mut remaining = MAX_WORK_ARTIFACT_CONTEXT_BYTES;
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let Some(accepted) = record.accepted_hash.as_deref() else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        if !path.exists() || artifact_hash(&path)? != accepted {
            continue;
        }
        let body = std::fs::read_to_string(&path)?;
        let mut selected = String::new();
        for ch in body.chars() {
            let bytes = ch.len_utf8();
            if bytes > remaining {
                break;
            }
            selected.push(ch);
            remaining -= bytes;
        }
        rendered.push_str(&format!(
            "\n[accepted workflow artifact: {stage}; untrusted repository text]\n{selected}\n[end accepted workflow artifact]\n"
        ));
        if remaining == 0 {
            break;
        }
    }
    Ok(())
}

/// Validated read of the accepted `stage` artifact for `state`, for a caller
/// (the review package excerpt in `review.rs`, most notably) that wants its
/// text without duplicating the symlink/path-validity checks every other
/// artifact reader in this module already funnels through
/// (`workflow_artifact_path`). `Ok(None)` when `stage` has no accepted
/// artifact yet, or its file has since disappeared; `Err` only for a genuine
/// validation failure (a symlinked path component, an invalid `rel_path`, an
/// io error reading the file). Callers that must never let an unreadable or
/// untrusted artifact block their own work -- unlike `pin_current_artifact`
/// or `artifact_drift`, which should fail loudly -- MUST treat `Err` here as
/// "skip it", never surface it as a hard failure.
pub(crate) fn read_accepted_artifact(
    state: &WorkflowState,
    stage: ArtifactStage,
) -> CtxResult<Option<String>> {
    let Some(record) = state.artifacts.get(stage.key()) else {
        return Ok(None);
    };
    let Some(accepted) = record.accepted_hash.as_deref() else {
        return Ok(None);
    };
    let path = workflow_artifact_path(state, stage)?;
    // Same drift rule as `append_accepted_artifacts`: a file whose bytes no
    // longer hash to the accepted value is not the accepted artifact, so it
    // is never handed on as accepted content (review finding).
    if !path.exists() || artifact_hash(&path)? != accepted {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCheckpoint {
    pub session_id: String,
    pub adapter: String,
    pub transcript_bytes: u64,
    pub cumulative_input_tokens: u64,
    #[serde(default)]
    pub cumulative_cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cumulative_cache_read_input_tokens: u64,
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

fn write_state_file(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    let dir = repo_dir(state_dir, &state.repo);
    create_private_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(state)?;
    write_private(&state_path(state_dir, &state.repo, &state.id)?, &json)?;
    Ok(())
}

pub(crate) fn save(state_dir: &StateDir, state: &WorkflowState, active: bool) -> CtxResult<()> {
    write_state_file(state_dir, state)?;
    if active {
        write_private(&active_path(state_dir, &state.repo), &state.id)?;
    } else if active_path(state_dir, &state.repo).exists() {
        std::fs::remove_file(active_path(state_dir, &state.repo))?;
    }
    Ok(())
}

/// Persists `state` and clears this repository's active pointer only when it
/// currently names `state.id` -- unlike `save(state_dir, state, false)`,
/// which clears the pointer unconditionally regardless of which workflow it
/// names. Used by [`close`] so closing an older, non-active workflow never
/// deactivates a different, currently-running workflow for the same repo.
fn save_inactive_if_active(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    write_state_file(state_dir, state)?;
    let pointer = active_path(state_dir, &state.repo);
    if pointer.exists() {
        let current = std::fs::read_to_string(&pointer)?;
        if current.trim() == state.id {
            std::fs::remove_file(&pointer)?;
        }
    }
    Ok(())
}

pub fn load(state: &StateDir, repo: &Path, id: &str) -> CtxResult<WorkflowState> {
    let path = state_path(state, repo, id)?;
    // Every verb that resolves a workflow by id (`status`, `resume`,
    // `context`, `artifacts`, `approve`, `advance`, ...) goes through this
    // one function, so checking here once is enough to keep a bogus id from
    // leaking a raw OS error ("The system cannot find the path specified.
    // (os error 3)") instead of a domain-shaped message.
    if !path.exists() {
        return Err(format!("unknown workflow '{id}'").into());
    }
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
    /// Issue #155, Phase 2: the raw cache classes behind `input_tokens`
    /// (which keeps its existing "combined context size" meaning), and the
    /// same four classes read separately over `isSidechain` rows -- subagent
    /// spend that used to be dropped entirely. `parent_session_id` and
    /// `work_group_id` are NOT here: they stay on `TelemetryEvent` only,
    /// populated by Phase 5, never invented here.
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub sidechain_input_tokens: Option<u64>,
    pub sidechain_cache_creation_input_tokens: Option<u64>,
    pub sidechain_cache_read_input_tokens: Option<u64>,
    pub sidechain_output_tokens: Option<u64>,
    pub session_id: Option<String>,
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
        cumulative_cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cumulative_cache_read_input_tokens: usage.cache_read_input_tokens,
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
            cache_creation_input_tokens: usage
                .cache_creation_input_tokens
                .saturating_sub(checkpoint.cumulative_cache_creation_input_tokens),
            cache_read_input_tokens: usage
                .cache_read_input_tokens
                .saturating_sub(checkpoint.cumulative_cache_read_input_tokens),
            output_tokens: usage
                .output_tokens
                .saturating_sub(checkpoint.cumulative_output_tokens),
        })
    } else {
        Some(usage)
    }
}

/// The sidechain-only counterpart to [`usage_since`]. Subagent (`isSidechain`)
/// spend is a Claude Code transcript concept, not a general adapter one, so
/// this reads the claude free function directly rather than growing the
/// `AgentAdapter` trait for a shape only one adapter has. Claude's
/// `transcript_usage` is not cumulative (`transcript_usage_is_cumulative` is
/// unset for it), so sidechain reads need none of `usage_since`'s
/// cumulative-checkpoint subtraction either -- summing the same byte range is
/// exact.
fn sidechain_usage_since(
    repo: &Path,
    checkpoint: &UsageCheckpoint,
) -> Option<crate::commands::ctx::event::TranscriptUsage> {
    if checkpoint.adapter != "claude" {
        return None;
    }
    let path = transcript_path(repo, &checkpoint.session_id, &checkpoint.adapter)?;
    let end = std::fs::metadata(&path).ok()?.len();
    if end < checkpoint.transcript_bytes {
        return None;
    }
    let body = read_transcript_range(&path, checkpoint.transcript_bytes, end)?;
    crate::commands::ctx::adapters::claude::sidechain_transcript_usage(&body)
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
    let mut sidechain_observed = previous
        .as_ref()
        .and_then(|checkpoint| sidechain_usage_since(&state.repo, checkpoint));
    if let (Some(previous), Some(current)) = (&previous, &current)
        && (previous.session_id != current.session_id || previous.adapter != current.adapter)
    {
        let beginning = UsageCheckpoint {
            transcript_bytes: 0,
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
            ..current.clone()
        };
        if let Some(next) = usage_since(&state.repo, &beginning) {
            let total = observed.get_or_insert_default();
            total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
            total.cache_creation_input_tokens = total
                .cache_creation_input_tokens
                .saturating_add(next.cache_creation_input_tokens);
            total.cache_read_input_tokens = total
                .cache_read_input_tokens
                .saturating_add(next.cache_read_input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
        }
        if let Some(next) = sidechain_usage_since(&state.repo, &beginning) {
            let total = sidechain_observed.get_or_insert_default();
            total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
            total.cache_creation_input_tokens = total
                .cache_creation_input_tokens
                .saturating_add(next.cache_creation_input_tokens);
            total.cache_read_input_tokens = total
                .cache_read_input_tokens
                .saturating_add(next.cache_read_input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
        }
    }
    if let Some(usage) = observed {
        if evidence.input_tokens.is_none() {
            // `context_total()`, not the raw `input_tokens` field: this is
            // the same combined "real context size" number this call site
            // always reported, back when `TranscriptUsage::input_tokens` was
            // the adapter's pre-summed figure. Existing telemetry consumers
            // must keep seeing that value unchanged (issue #155 Phase 2).
            evidence.input_tokens = Some(usage.context_total());
        }
        if evidence.output_tokens.is_none() {
            evidence.output_tokens = Some(usage.output_tokens);
        }
        if evidence.token_usage_source.is_none() {
            evidence.token_usage_source = Some("harness-transcript-delta".into());
        }
        if evidence.cache_creation_input_tokens.is_none() {
            evidence.cache_creation_input_tokens = Some(usage.cache_creation_input_tokens);
        }
        if evidence.cache_read_input_tokens.is_none() {
            evidence.cache_read_input_tokens = Some(usage.cache_read_input_tokens);
        }
    }
    if let Some(usage) = sidechain_observed {
        if evidence.sidechain_input_tokens.is_none() {
            evidence.sidechain_input_tokens = Some(usage.input_tokens);
        }
        if evidence.sidechain_cache_creation_input_tokens.is_none() {
            evidence.sidechain_cache_creation_input_tokens =
                Some(usage.cache_creation_input_tokens);
        }
        if evidence.sidechain_cache_read_input_tokens.is_none() {
            evidence.sidechain_cache_read_input_tokens = Some(usage.cache_read_input_tokens);
        }
        if evidence.sidechain_output_tokens.is_none() {
            evidence.sidechain_output_tokens = Some(usage.output_tokens);
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
    if evidence.session_id.is_none() {
        evidence.session_id = session_identity().map(|(session_id, _)| session_id);
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
        apply_profile(state.kind, state.profile, &mut state.steps);
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
    let desired = materialize(
        state.kind,
        &state.classification,
        state.profile,
        state.deploy_tier,
        state.brainstorm,
    );
    let known: Vec<String> = state.steps.iter().map(|step| step.id.clone()).collect();
    let earliest_new = desired.iter().position(|step| {
        !known.contains(&step.id)
            && (step.artifact.is_some()
                || matches!(step.phase, WorkflowPhase::Review | WorkflowPhase::Verify))
    });
    let Some(cutoff) = earliest_new else {
        return;
    };

    // A newly-required artifact can land before already-completed code work.
    // Preserve only evidence that precedes the new gate; later work must be
    // replayed against the newly accepted artifact instead of being blessed
    // retroactively.
    let safe_ids: Vec<String> = desired[..cutoff]
        .iter()
        .map(|step| step.id.clone())
        .collect();
    state
        .completed_steps
        .retain(|completed| safe_ids.contains(completed));
    state.steps = desired;
    sync_artifact_records(state);
    state.current_step = state
        .steps
        .iter()
        .position(|step| !state.completed_steps.contains(&step.id))
        .unwrap_or(state.steps.len());
}

fn apply_effective_deploy_tier(state: &mut WorkflowState, effective: DeployTier) {
    let target = state.deploy_tier.max(effective);
    let desired = materialize(
        state.kind,
        &state.classification,
        state.profile,
        target,
        state.brainstorm,
    );

    let known: Vec<String> = state.steps.iter().map(|step| step.id.clone()).collect();
    let earliest_new = desired.iter().position(|step| !known.contains(&step.id));

    if target > state.deploy_tier
        && let Some(cutoff) = earliest_new
    {
        let safe_ids: Vec<String> = desired[..cutoff]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state
            .completed_steps
            .retain(|completed| safe_ids.contains(completed));
    }

    state.deploy_tier = target;
    state.steps = desired;
    sync_artifact_records(state);
    state.current_step = state
        .steps
        .iter()
        .position(|step| !state.completed_steps.contains(&step.id))
        .unwrap_or(state.steps.len());
    state.status = match state.current() {
        None => WorkflowStatus::Completed,
        Some(step) if step.approval => WorkflowStatus::AwaitingApproval,
        Some(_) => WorkflowStatus::Running,
    };
}

fn refresh_deploy_tier(state: &mut WorkflowState) -> CtxResult<()> {
    let effective = super::deploy::effective_tier(&state.repo)?;
    apply_effective_deploy_tier(state, effective);
    Ok(())
}
pub fn advance_with_evidence(
    state_dir: &StateDir,
    mut state: WorkflowState,
    outcome: StepOutcome,
    evidence: Option<&TransitionEvidence>,
    accept_preexisting_findings: bool,
) -> CtxResult<WorkflowState> {
    refresh_deploy_tier(&mut state)?;
    if let Some(stage) = artifact_drift(&state)? {
        reopen_artifact_gate(&mut state, stage)?;
        state.updated_at = now_secs();
        save(state_dir, &state, true)?;
        return Err(format!(
            "accepted {stage} artifact changed after approval; review and run `zirv workflow approve {}` again",
            state.id
        )
        .into());
    }
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
            let frontend_root: PathBuf = state
                .frontend_target_root
                .clone()
                .unwrap_or_else(|| state.repo.clone());
            if state.profile == WorkflowProfile::Frontend
                && matches!(
                    current.phase,
                    WorkflowPhase::Test | WorkflowPhase::Review | WorkflowPhase::Verify
                )
                && !super::frontend_detector::latest_is_fresh_and_passing(
                    state_dir,
                    &frontend_root,
                )?
            {
                let report = super::frontend_detector::detect_for_workflow(
                    state_dir,
                    &frontend_root,
                    matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify),
                )?;
                let introduced = report.introduced_blocking_count();
                let preexisting = report.preexisting_blocking_count();
                let preexisting_already_accepted = state.accepted_preexisting_findings.is_some();
                let preexisting_accepted =
                    accept_preexisting_findings || preexisting_already_accepted;
                if report.truncated || introduced > 0 || (preexisting > 0 && !preexisting_accepted)
                {
                    let accept_hint = if preexisting > 0 && !preexisting_accepted {
                        format!(
                            "; pass --accept-preexisting-findings to accept {preexisting} pre-existing blocking finding(s) and proceed"
                        )
                    } else {
                        String::new()
                    };
                    return Err(format!(
                        "frontend step '{}' automatically ran the detector against '{}', but evidence did not pass ({} introduced blocking, {} pre-existing blocking, {} files, truncated={}); inspect with `zirv frontend check --all --repo {}`{}, or set `--frontend-root` if the frontend lives in a different repository",
                        current.id,
                        frontend_root.display(),
                        introduced,
                        preexisting,
                        report.analyzed_files.len(),
                        report.truncated,
                        frontend_root.display(),
                        accept_hint
                    )
                    .into());
                }
                if preexisting > 0 && accept_preexisting_findings && !preexisting_already_accepted {
                    state.accepted_preexisting_findings = Some(AcceptedPreexistingFindings {
                        step: current.id.clone(),
                        at: rfc3339_now(),
                        blocking: preexisting,
                        total: report.preexisting_total_count(),
                    });
                    // Persisted immediately, mirroring `--frontend-root`
                    // below: a later gate in this same advance (render/
                    // visual-review, or the general test-evidence gate)
                    // can still fail closed, and the operator should not
                    // have to pass the flag again on retry.
                    state.updated_at = now_secs();
                    save(state_dir, &state, true)?;
                }
            }
            if state.profile == WorkflowProfile::Frontend
                && matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify)
                && !super::frontend_render::latest_visual_is_fresh_and_passing(
                    state_dir,
                    &frontend_root,
                )?
            {
                let render = super::frontend_render::render(state_dir, &frontend_root)?;
                if !render.passed() {
                    return Err(format!(
                        "frontend step '{}' could not collect automatic rendered evidence against '{}': {}; inspect with `zirv frontend render --repo {}`",
                        current.id,
                        frontend_root.display(),
                        render.notes.join("; "),
                        frontend_root.display()
                    )
                    .into());
                }
                let review = super::frontend_render::review(
                    state_dir,
                    &frontend_root,
                    &super::frontend_render::VisualReviewArgs {
                        repo: Some(frontend_root.to_path_buf()),
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
            if current.phase == WorkflowPhase::Deploy {
                let gate = super::deploy::production_gate_satisfied(state_dir, &state);
                let mut event = super::telemetry::TelemetryEvent::new(
                    super::telemetry::TelemetryKind::DeployGateEvaluated,
                );
                event.workflow_id = Some(state.id.clone());
                event.phase = Some(current.phase);
                event.intent = Some(state.classification.intent);
                event.complexity = Some(state.classification.complexity);
                event.risk = Some(state.classification.risk);
                event.work_domain = Some(state.classification.work_domain.domain);
                event.deploy_tier = Some(state.deploy_tier.to_string());
                event.succeeded = Some(gate.is_ok());
                let _ = super::telemetry::record(
                    state_dir,
                    &state.repo,
                    &event,
                    &super::telemetry::TelemetryConfig::for_repo(&state.repo),
                );
                gate?;
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
                    let announcement =
                        super::verification::gate_announcement(state_dir, &state.repo, final_only);
                    return Err(format!(
                        "step '{}' requires fresh passing evidence for the current change set; run `{command}`\n{announcement}",
                        current.id
                    )
                    .into());
                }
            }
            record_step_duration_ms(&mut state, &current.id);
            state.completed_steps.push(current.id.clone());
            state.current_step += 1;
            reclassify_at_gate(&mut state);
            sync_artifact_records(&mut state);
            ensure_current_artifact_template(&state)?;
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
    event.cache_creation_input_tokens = evidence.cache_creation_input_tokens;
    event.cache_read_input_tokens = evidence.cache_read_input_tokens;
    event.sidechain_input_tokens = evidence.sidechain_input_tokens;
    event.sidechain_cache_creation_input_tokens = evidence.sidechain_cache_creation_input_tokens;
    event.sidechain_cache_read_input_tokens = evidence.sidechain_cache_read_input_tokens;
    event.sidechain_output_tokens = evidence.sidechain_output_tokens;
    event.session_id = evidence.session_id;
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
        completed.deploy_tier = Some(state.deploy_tier.to_string());
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
    if outcome == StepOutcome::Success {
        try_auto_spawn(state_dir, &state);
    }
    Ok(state)
}

pub fn approve(state_dir: &StateDir, mut state: WorkflowState) -> CtxResult<WorkflowState> {
    refresh_deploy_tier(&mut state)?;
    if state.status != WorkflowStatus::AwaitingApproval {
        return Err("workflow is not awaiting approval".into());
    }

    if let Some(stage) = state.current().and_then(|step| step.artifact) {
        // Accepted predecessor artifacts must still be the exact bytes that
        // were reviewed. The current stage itself is intentionally excluded
        // until pin_current_artifact replaces its acceptance record.
        if let Some(drifted) = artifact_drift(&state)?
            && drifted != stage
        {
            reopen_artifact_gate(&mut state, drifted)?;
            save(state_dir, &state, true)?;
            return Err(format!(
                "accepted {drifted} artifact changed after approval; re-approve it before {stage}"
            )
            .into());
        }
        let completed = state.current().expect("artifact step exists").clone();
        let accepted = pin_current_artifact(&mut state)?;
        if !state.completed_steps.contains(&completed.id) {
            record_step_duration_ms(&mut state, &completed.id);
            state.completed_steps.push(completed.id);
        }
        state.current_step += 1;
        reclassify_at_gate(&mut state);
        sync_artifact_records(&mut state);
        ensure_current_artifact_template(&state)?;
        state.status = match state.current() {
            None => WorkflowStatus::Completed,
            Some(step) if step.approval => WorkflowStatus::AwaitingApproval,
            Some(_) => WorkflowStatus::Running,
        };
        state.updated_at = now_secs();
        state.phase_started_at = state.updated_at;
        let active = matches!(
            state.status,
            WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
        );
        save(state_dir, &state, active)?;

        let mut event = super::telemetry::TelemetryEvent::new(
            super::telemetry::TelemetryKind::ArtifactAccepted,
        );
        event.workflow_id = Some(state.id.clone());
        event.phase = Some(completed.phase);
        event.intent = Some(state.classification.intent);
        event.complexity = Some(state.classification.complexity);
        event.risk = Some(state.classification.risk);
        event.work_domain = Some(state.classification.work_domain.domain);
        event.succeeded = Some(true);
        event.artifact_stage = Some(accepted.to_string());
        let _ = super::telemetry::record(
            state_dir,
            &state.repo,
            &event,
            &super::telemetry::TelemetryConfig::for_repo(&state.repo),
        );
        return Ok(state);
    }

    state.status = WorkflowStatus::Running;
    state.updated_at = now_secs();
    save(state_dir, &state, true)?;
    Ok(state)
}

/// Closes a workflow that will not reach `Completed` -- typically one whose
/// review/fix loop hit `MAX_FIX_REVIEW_ROUNDS` (review.rs) and would
/// otherwise stay `Running` forever, still reported as this repository's
/// active workflow by `load_active`. Refuses (fail closed) when the
/// workflow's status is already terminal (`Completed`/`Failed`/`Closed`) --
/// `close` only applies to a workflow still in flight -- while any review
/// finding is still `Open` -- residual dispositions must be recorded first,
/// via `workflow review dispose` -- or while the workflow is
/// `AwaitingApproval`, since approval is itself a pending decision on the
/// current step. Otherwise sets `status: Closed`, records `closed_reason`/
/// `closed_at`, and persists via `save_inactive_if_active`, which clears
/// this repository's active pointer only when it currently names THIS
/// workflow -- closing an older, non-active workflow must never deactivate a
/// different, currently-running one for the same repo.
pub fn close(
    state_dir: &StateDir,
    mut state: WorkflowState,
    reason: Option<String>,
) -> CtxResult<WorkflowState> {
    if matches!(
        state.status,
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Closed
    ) {
        return Err(format!(
            "cannot close workflow: already {:?}; close only applies to a workflow that will \
             not reach Completed on its own",
            state.status
        )
        .into());
    }
    let open_findings = state
        .review_findings
        .iter()
        .filter(|finding| finding.disposition == super::review::FindingDisposition::Open)
        .count();
    if open_findings > 0 {
        return Err(format!(
            "cannot close workflow: {open_findings} open review finding(s) remain; record \
             dispositions first (see `zirv workflow review dispose`)"
        )
        .into());
    }
    if state.status == WorkflowStatus::AwaitingApproval {
        return Err(
            "cannot close workflow while awaiting approval; approve or reject the current step \
             first"
                .into(),
        );
    }
    let now = now_secs();
    state.status = WorkflowStatus::Closed;
    state.closed_reason = reason;
    state.closed_at = Some(now);
    state.updated_at = now;
    save_inactive_if_active(state_dir, &state)?;

    let mut event = super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::Closed);
    event.workflow_id = Some(state.id.clone());
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    Ok(state)
}

/// What became of one open review finding when its own
/// `recommended_disposition` was applied in bulk -- see
/// [`apply_recommended_dispositions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedDisposition {
    pub finding_id: String,
    /// `Some` when the finding carried a recommendation and was moved to it;
    /// `None` when it had none and was left `Open`.
    pub applied: Option<super::review::FindingDisposition>,
}

/// Applies every *open* review finding's own `recommended_disposition` in one
/// call, collapsing the per-finding turn cost of `zirv workflow review
/// dispose` for the common case where a reviewer already recommended a
/// disposition (review.rs stores it as `ReviewFinding::recommended_
/// disposition`, read here through that existing field rather than any new
/// accessor). A finding that is not `Open` (already `Accepted`/`Dismissed`/
/// `Fixed`/`Residual`) is left untouched and not reported at all -- this is
/// additive over the single-finding dispose, never a way to revisit a
/// decision already made. An open finding with no recommendation is left
/// `Open` and still reported, so the caller can see it was considered and
/// skipped rather than silently missed.
///
/// Called directly by `zirv workflow review dispose --apply-recommended`
/// (`review::ReviewCommand::Dispose`'s handler): the flag lives on the same
/// `dispose` verb the single-finding form uses -- a standalone
/// `DisposeRecommended` subcommand was the stopgap spelling before that flag
/// was wired up. Mirrors `review.rs`'s own single-finding arm otherwise: the
/// same load/mutate/save/telemetry shape (its `record_finding_update` is
/// private to that module, so the telemetry recording is reimplemented here
/// rather than exposed solely for this one caller), just applied to every
/// eligible finding instead of one named by id.
pub fn apply_recommended_dispositions(
    state_dir: &StateDir,
    mut state: WorkflowState,
) -> CtxResult<(WorkflowState, Vec<AppliedDisposition>)> {
    let mut results = Vec::new();
    for finding in &mut state.review_findings {
        if finding.disposition != super::review::FindingDisposition::Open {
            continue;
        }
        results.push(AppliedDisposition {
            finding_id: finding.id.clone(),
            applied: finding.recommended_disposition,
        });
        if let Some(recommended) = finding.recommended_disposition {
            finding.disposition = recommended;
        }
    }
    state.updated_at = now_secs();
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    save(state_dir, &state, active)?;

    let (findings_total, findings_meaningful, findings_dismissed) =
        super::telemetry::finding_counts(&state.review_findings);
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::FindingUpdated);
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(WorkflowPhase::Review);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.findings_total = findings_total;
    event.findings_meaningful = findings_meaningful;
    event.findings_dismissed = findings_dismissed;
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    Ok((state, results))
}

/// Forces a persisted workflow's methodology overlay (#255 recovery path: a
/// misclassified profile previously could only be fixed by abandoning the
/// workflow). A profile change never adds, removes, or reorders steps --
/// `WorkflowState::set_profile` relabels skills on the existing step list in
/// place -- so completed steps and accepted artifacts are structurally
/// untouched; the state machine is never reset. (Unlike a risk increase,
/// which can genuinely require a new gate, `rematerialize_after_risk_
/// increase`'s known-step-id trimming would be a no-op here anyway, since
/// the same classification always produces the same step ids.)
pub fn reclassify(
    state_dir: &StateDir,
    mut state: WorkflowState,
    profile: WorkflowProfile,
) -> CtxResult<WorkflowState> {
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    ) {
        return Err(format!("cannot reclassify workflow in {:?} state", state.status).into());
    }
    state.set_profile(profile);
    sync_artifact_records(&mut state);
    state.status = match state.current() {
        None => WorkflowStatus::Completed,
        Some(step) if step.approval => WorkflowStatus::AwaitingApproval,
        Some(_) => WorkflowStatus::Running,
    };
    state.updated_at = now_secs();
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    save(state_dir, &state, active)?;
    Ok(state)
}

/// A headless worker must not answer `brainstorm`'s clarifying questions or
/// write the intent artifact on the operator's behalf.
const BRAINSTORM_HEADLESS_REFUSAL: &str = "This step needs an interactive operator. Do not answer the clarifying questions on their behalf or write the intent artifact; stop and report that the workflow is waiting for the operator.";

fn refusal_for(skill_id: &str, headless: bool) -> Option<&'static str> {
    (headless && skill_id == "brainstorm").then_some(BRAINSTORM_HEADLESS_REFUSAL)
}

/// Only the exact value `"1"` means headless -- `ZIRV_CTX_HEADLESS=0`, an
/// empty string, or any other value must not trip the refusal. Split out of
/// the `std::env::var` call site so the value comparison is testable without
/// a real (racy) environment variable.
fn is_headless_env(raw: Option<&str>) -> bool {
    raw == Some("1")
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
    if let Some(agent) = step.agent.as_deref() {
        rendered.push_str(&format!("agent-seat: {agent}\n"));
    }
    if let Some(stage) = step.artifact {
        ensure_current_artifact_template(state)?;
        let record = state
            .artifacts
            .get(stage.key())
            .ok_or("current workflow artifact record is missing")?;
        rendered.push_str(&format!(
            "artifact: {} ({stage}; fill this committed work product, then wait for acceptance)\n",
            record.rel_path
        ));
    }
    append_accepted_artifacts(state, &mut rendered)?;
    if state.profile == WorkflowProfile::Frontend {
        let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
        let profile = super::frontend::ensure_profile(&state_dir, repo)?;
        rendered.push('\n');
        rendered.push_str(&super::frontend::render_profile(&profile));
        rendered.push('\n');
    }
    let headless = is_headless_env(
        std::env::var(crate::commands::ctx::adapters::HEADLESS_ENV)
            .ok()
            .as_deref(),
    );
    let mut rendered_skill_ids = BTreeSet::new();
    for selected in step_skill_ids(step, &state.classification) {
        for skill in registry.resolve_stack(&selected)? {
            if !rendered_skill_ids.insert(skill.manifest.id.clone()) {
                continue;
            }
            let body = refusal_for(&skill.manifest.id, headless)
                .unwrap_or_else(|| skill.manifest.instructions.trim());
            rendered.push_str(&format!(
                "\n[skill {}@{}; source={}]\n{}\n",
                skill.manifest.id, skill.manifest.version, skill.source, body
            ));
        }
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
    /// Force a persisted workflow's methodology overlay, preserving
    /// completed steps and accepted artifacts (#255 recovery path).
    Reclassify(ReclassifyArgs),
    /// Print only the current step's resolved skill context.
    Context(StatusArgs),
    /// Inspect committed workflow work-product artifacts and acceptance state.
    Artifacts(ArtifactsArgs),
    /// Inspect provider-neutral workflow seats and their trust provenance.
    Agents(super::agents::AgentArgs),
    /// Approve the current gated step.
    Approve(StateIdArgs),
    /// Record a step result and transition the state machine.
    Advance(AdvanceArgs),
    /// Close a workflow that will not reach `Completed` (for example one
    /// whose review/fix loop hit `MAX_FIX_REVIEW_ROUNDS`), recording residual
    /// dispositions first. Refuses while any review finding is `Open` or the
    /// workflow is `AwaitingApproval`; clears it as this repository's active
    /// workflow.
    Close(CloseArgs),
    /// Build compact review packages and persist finding dispositions,
    /// including `review dispose --apply-recommended` -- see
    /// [`apply_recommended_dispositions`]'s own doc comment.
    Review(super::review::ReviewArgs),
    /// Run deterministic operator-configured maintenance detectors.
    Maintain(super::maintain::MaintainArgs),
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
    /// Harness adapter used for capability preflight (for example claude/codex).
    #[arg(long)]
    pub agent: Option<String>,
    /// Ignore operator-global and repository-provided skill/agent overrides.
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
    /// Repository whose frontend the auto-run detector/render evidence
    /// should scan for a Frontend-profile workflow, when it differs from
    /// `--repo` (for example a workflow tracked in this repo whose frontend
    /// lives in a sibling checkout).
    #[arg(long)]
    pub frontend_root: Option<PathBuf>,
    /// Force the interactive `brainstorm` skill at the intent step.
    #[arg(long, conflicts_with = "no_brainstorm")]
    pub brainstorm: bool,
    /// Force the autonomous `write-intent` skill at the intent step.
    #[arg(long)]
    pub no_brainstorm: bool,
    /// Force this workflow's methodology overlay instead of trusting
    /// automatic classification (#255 recovery path: a misclassified
    /// profile can otherwise only be fixed by abandoning the workflow).
    #[arg(long, value_enum)]
    pub profile: Option<WorkflowProfile>,
    #[arg(long)]
    pub json: bool,
}

impl StartArgs {
    pub(crate) fn brainstorm_override(&self) -> Option<bool> {
        if self.brainstorm {
            Some(true)
        } else if self.no_brainstorm {
            Some(false)
        } else {
            None
        }
    }
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
pub struct CloseArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Operator-supplied reason for closing without reaching `Completed`,
    /// recorded on the workflow state.
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ReclassifyArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub profile: WorkflowProfile,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactsArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowArtifactStatus {
    stage: ArtifactStage,
    rel_path: String,
    exists: bool,
    accepted: bool,
    drifted: bool,
    accepted_at: Option<String>,
}

fn workflow_artifact_statuses(state: &WorkflowState) -> CtxResult<Vec<WorkflowArtifactStatus>> {
    let mut statuses = Vec::new();
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        let exists = path.exists();
        let accepted = record.accepted_hash.is_some();
        let drifted = match record.accepted_hash.as_deref() {
            Some(hash) => !exists || artifact_hash(&path)? != hash,
            None => false,
        };
        statuses.push(WorkflowArtifactStatus {
            stage,
            rel_path: record.rel_path.clone(),
            exists,
            accepted,
            drifted,
            accepted_at: record.accepted_at.clone(),
        });
    }
    Ok(statuses)
}

#[derive(Debug, Args)]
pub struct AdvanceArgs {
    pub id: String,
    /// Required unless `--run-checks` is set, which determines the outcome
    /// itself from the evidence command's own result.
    #[arg(long, value_enum, required_unless_present = "run_checks")]
    pub outcome: Option<StepOutcome>,
    /// For a `Test`/`Verify` step, run the step's own required evidence
    /// command in-process (`zirv test changed` for `Test`, `zirv verify` for
    /// `Verify`) and advance on success, printing the evidence summary; on
    /// failure, print it and do not advance. Collapses "run the gate, then
    /// advance" into one call. Conflicts with `--outcome`, which the checks'
    /// own result determines instead.
    #[arg(long, conflicts_with = "outcome")]
    pub run_checks: bool,
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
    /// Set (or update) the sibling repository whose frontend the auto-run
    /// detector/render evidence should scan for this workflow, for example
    /// once it becomes clear the tracked repo isn't the one under test.
    #[arg(long)]
    pub frontend_root: Option<PathBuf>,
    /// Accept the frontend detector's pre-existing (not newly introduced)
    /// blocking findings so this advance can proceed; introduced blocking
    /// findings always still fail. Recorded on the workflow and applies for
    /// the rest of it once accepted.
    #[arg(long)]
    pub accept_preexisting_findings: bool,
}

fn resolve_repo(repo: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match repo {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

/// Resolves and validates `--frontend-root`: absolutized against the current
/// directory, then required to exist and be a directory so a typo fails
/// loudly at parse time instead of surfacing later as "0 files scanned".
fn resolve_frontend_root(path: &Path) -> CtxResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = absolute.canonicalize().map_err(|err| {
        format!(
            "frontend root '{}' does not exist: {err}",
            absolute.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!("frontend root '{}' is not a directory", canonical.display()).into());
    }
    Ok(canonical)
}

fn resolve_state() -> CtxResult<StateDir> {
    StateDir::resolve(&|key| std::env::var(key).ok())
}

/// Runs the evidence command a `Test`/`Verify` step itself requires --
/// `zirv test changed` for `Test`, `zirv verify` for `Verify` -- through the
/// same in-process function the CLI verb itself calls (never a subprocess),
/// printing its evidence summary to `writer`. Returns whether it passed.
/// Backs `zirv workflow advance --run-checks`, which collapses "run the
/// gate, then advance" into a single call. Any other phase is not something
/// `--run-checks` knows how to satisfy, so it errors rather than silently
/// treating the step as passed.
///
/// Pass/fail is decided by [`super::verification::latest_is_fresh_and_passing`]
/// against the report the run above just persisted -- the exact same
/// baseline-aware gate the plain `zirv workflow advance --outcome success`
/// path applies to a report from an out-of-process `zirv test changed`/`zirv
/// verify` run (see the `Test`/`Verify` arm of `advance_with_evidence`).
/// `run_test`/`run_verify`'s own raw exit code is deliberately not used here:
/// it reflects the run's unwaived pass/fail, so a report whose only failures
/// are covered by the operator's recorded baseline (`zirv test baseline`)
/// exits non-zero even though the same report satisfies the gate -- see the
/// dogfooding bug where `--run-checks` printed "checks failed" immediately
/// before a follow-up `--outcome success` against the identical report
/// advanced with the baseline warning.
///
/// Before running, and again after, this snapshots
/// [`super::verification::latest_report_id`] -- the persisted report's own
/// identity (a fresh UUID every run). `run_and_persist` swallows a `persist`
/// failure into a warning so the run's printed results survive a transient
/// IO error, which means the report `latest_is_fresh_and_passing` would read
/// afterwards can still be whatever older report preceded this run. If the
/// identity did not change, no fresh report exists to gate on at all -- so
/// this fails the step outright rather than falling back to evaluating that
/// stale report (which could easily still be fresh-and-passing against the
/// unchanged fingerprint, silently advancing a step whose check just
/// genuinely failed).
fn run_required_checks(
    state_dir: &StateDir,
    repo: &Path,
    phase: WorkflowPhase,
    step_id: &str,
    writer: &mut impl Write,
) -> CtxResult<bool> {
    let before = super::verification::latest_report_id(state_dir, repo)?;
    let run_args = super::verification::RunArgs {
        repo: Some(repo.to_path_buf()),
        checks: Vec::new(),
        dry_run: false,
        json: false,
    };
    let final_only = match phase {
        WorkflowPhase::Test => {
            super::verification::run_test(
                &super::verification::TestArgs {
                    command: super::verification::TestCommand::Changed(run_args),
                },
                writer,
            )?;
            false
        }
        WorkflowPhase::Verify => {
            super::verification::run_verify(
                &super::verification::VerifyArgs { run: run_args },
                writer,
            )?;
            true
        }
        other => {
            return Err(format!(
                "--run-checks only applies to Test/Verify steps; the current step is {other:?} -- \
                 pass --outcome instead"
            )
            .into());
        }
    };
    let after = super::verification::latest_report_id(state_dir, repo)?;
    if after == before {
        writeln!(
            writer,
            "checks ran but no fresh report was persisted; step '{step_id}' was not advanced"
        )?;
        return Ok(false);
    }
    super::verification::latest_is_fresh_and_passing(state_dir, repo, final_only)
}

/// Call before pushing `step_id` onto `completed_steps`, while
/// `phase_started_at` still names its own start.
fn record_step_duration_ms(state: &mut WorkflowState, step_id: &str) {
    let elapsed_ms = now_secs()
        .saturating_sub(state.phase_started_at)
        .saturating_mul(1000);
    state
        .step_durations_ms
        .insert(step_id.to_string(), elapsed_ms);
}

/// `<minutes>m<seconds>s`, e.g. `2m10s`.
fn format_wall_clock(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{}m{}s", total_secs / 60, total_secs % 60)
}

/// A bounded worker to auto-spawn after a gate transition.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AutoSpawn {
    pub phase: WorkflowPhase,
    pub argv: Vec<String>,
}

/// Why a gate transition that WOULD otherwise be eligible (right phase,
/// `Running`, enabled) did not produce an [`AutoSpawn`]. `Quiet` covers
/// every case that is not worth an operator's attention: the config key is
/// off, the phase is not Review/Test/Verify, or the workflow is
/// `AwaitingApproval` -- an operator who never opted in, or a transition
/// this feature was never meant to touch, must see nothing new. `NoPermit`/
/// `NoAgent` are the opposite: the operator explicitly enabled this, so a
/// skip is reported, not silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSpawnSkip {
    Quiet,
    NoPermit,
    NoAgent,
}

/// Pure: whether a gate transition should auto-spawn a worker, the argv for
/// it, or the reason it did not. `default_agent` is the operator's
/// configured chat harness (`adapters::resolve_default`, the same one `zirv
/// ctx chat` launches by default), consulted only when the workflow itself
/// has no recorded adapter -- `state.adapter` always wins when set.
pub(crate) fn auto_spawn_decision(
    state: &WorkflowState,
    enabled: bool,
    permit_available: bool,
    default_agent: Option<&str>,
) -> Result<AutoSpawn, AutoSpawnSkip> {
    if !enabled || state.status != WorkflowStatus::Running {
        return Err(AutoSpawnSkip::Quiet);
    }
    let phase = state.current().ok_or(AutoSpawnSkip::Quiet)?.phase;
    if !matches!(
        phase,
        WorkflowPhase::Review | WorkflowPhase::Test | WorkflowPhase::Verify
    ) {
        return Err(AutoSpawnSkip::Quiet);
    }
    if !permit_available {
        return Err(AutoSpawnSkip::NoPermit);
    }
    let repo = state.repo.display().to_string();
    let argv = match phase {
        WorkflowPhase::Review => {
            let agent = state
                .adapter
                .clone()
                .or_else(|| default_agent.map(str::to_string))
                .ok_or(AutoSpawnSkip::NoAgent)?;
            vec![
                "workflow".to_string(),
                "review".to_string(),
                "run".to_string(),
                state.id.clone(),
                "--agent".to_string(),
                agent,
                "--repo".to_string(),
                repo,
            ]
        }
        WorkflowPhase::Test => vec![
            "test".to_string(),
            "changed".to_string(),
            "--repo".to_string(),
            repo,
        ],
        WorkflowPhase::Verify => vec!["verify".to_string(), "--repo".to_string(), repo],
        _ => unreachable!("filtered above"),
    };
    Ok(AutoSpawn { phase, argv })
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

/// The operator explicitly enabled `auto_spawn_on_gate`, so a skip that
/// reaches this point (as opposed to `AutoSpawnSkip::Quiet`) is reported,
/// not silent.
fn announce_auto_spawn_skip(
    cfg: &crate::commands::ctx::config::CtxConfig,
    phase: WorkflowPhase,
    reason: &str,
) {
    crate::commands::ctx::announce::Announcer::new(cfg.chrome.events, false).emit(
        &crate::commands::ctx::announce::Event::AutoSpawnSkipped {
            phase: phase.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Issue #242: spawns `spawn.argv` detached and never fails `advance` --
/// `test changed`/`verify`/`review run` govern no heavy-operation permit of
/// their own, so this acquires one on their behalf and leaks it (the child
/// outlives this call): `HeavyPermit::set_child_pid` plus `permit::live_
/// records`' own dead-owner sweep is exactly the mechanism that frees the
/// slot once the detached child exits, the same as a parent that dies while
/// its child keeps running.
fn spawn_auto_worker(
    state_dir: &StateDir,
    state: &WorkflowState,
    cfg: &crate::commands::ctx::config::CtxConfig,
    spawn: AutoSpawn,
) {
    use crate::commands::ctx::permit;

    // A race against `try_auto_spawn`'s own peek: the peek said a slot was
    // free, but another caller took it before this real acquire ran.
    let Some(permit) = permit::acquire(
        state_dir,
        cfg.supervise.max_heavy_operations,
        &format!("auto-spawn: {}", spawn.argv.join(" ")),
    ) else {
        announce_auto_spawn_skip(cfg, spawn.phase, "no heavy-operation permit was free");
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        announce_auto_spawn_skip(
            cfg,
            spawn.phase,
            "could not resolve the zirv executable path",
        );
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(&spawn.argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    let Ok(child) = command.spawn() else {
        announce_auto_spawn_skip(cfg, spawn.phase, "failed to spawn the worker process");
        return;
    };
    permit.set_child_pid(child.id());
    std::mem::forget(permit);

    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::AgentDispatched);
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(spawn.phase);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.agent_id = Some(format!("auto-spawn:{}", spawn.phase));
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    crate::commands::ctx::announce::Announcer::new(cfg.chrome.events, false).emit(
        &crate::commands::ctx::announce::Event::AutoSpawned {
            phase: spawn.phase.to_string(),
            command: spawn.argv.join(" "),
        },
    );
}

/// Thin I/O wrapper around [`auto_spawn_decision`]: resolves config, a
/// permit peek, and (only when the workflow itself has no adapter) the
/// operator's default chat harness, then hands off to [`spawn_auto_worker`].
/// Any failure along the way (config, permit, spawn) is silently degraded --
/// never propagated to `advance`'s own result -- but a skip the operator's
/// own `auto_spawn_on_gate = true` made eligible is announced, not silent.
fn try_auto_spawn(state_dir: &StateDir, state: &WorkflowState) {
    let cfg = match crate::commands::ctx::config::CtxConfig::load(&state.repo, &|key| {
        std::env::var(key).ok()
    }) {
        Ok(cfg) => cfg,
        Err(_) => return,
    };
    if !cfg.workflow.auto_spawn_on_gate {
        return;
    }
    let permit_available =
        crate::commands::ctx::permit::live_count(state_dir) < cfg.supervise.max_heavy_operations;
    // `state.adapter` always wins; the operator's configured chat harness
    // (the same one `adapters::resolve_default` picks for `zirv ctx chat`)
    // is only worth resolving -- readiness probes and all -- when the
    // workflow itself named none.
    let default_agent = state
        .adapter
        .is_none()
        .then(|| {
            crate::commands::ctx::adapters::resolve_default(&cfg)
                .ok()
                .map(|(adapter, _)| adapter.name().to_string())
        })
        .flatten();
    match auto_spawn_decision(state, true, permit_available, default_agent.as_deref()) {
        Ok(spawn) => spawn_auto_worker(state_dir, state, &cfg, spawn),
        Err(skip) => {
            if let Some(reason) = auto_spawn_skip_reason(skip)
                && let Some(phase) = state.current().map(|step| step.phase)
            {
                announce_auto_spawn_skip(&cfg, phase, reason);
            }
        }
    }
}

/// The advisory text for a skip the operator's own `auto_spawn_on_gate =
/// true` made eligible, or `None` for `Quiet` -- the ordinary, never-
/// announced case (disabled, wrong phase, `AwaitingApproval`).
fn auto_spawn_skip_reason(skip: AutoSpawnSkip) -> Option<&'static str> {
    match skip {
        AutoSpawnSkip::Quiet => None,
        AutoSpawnSkip::NoPermit => Some("no heavy-operation permit was free"),
        AutoSpawnSkip::NoAgent => Some(
            "no adapter to run the reviewer as (the workflow has none, and no operator \
             default chat harness could be resolved)",
        ),
    }
}

fn write_state(writer: &mut impl Write, state: &WorkflowState, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, state)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "workflow: {}", state.id)?;
        writeln!(writer, "kind: {}", state.kind.as_str())?;
        writeln!(
            writer,
            "profile: {:?} ({})",
            state.profile,
            match state.profile_source {
                ProfileSource::Classified => "classified",
                ProfileSource::OperatorOverride => "operator override",
            }
        )?;
        if let Some(frontend_root) = &state.frontend_target_root {
            writeln!(writer, "frontend target root: {}", frontend_root.display())?;
        }
        if let Some(accepted) = &state.accepted_preexisting_findings {
            writeln!(
                writer,
                "accepted pre-existing frontend findings: {} blocking / {} total at {} ({})",
                accepted.blocking, accepted.total, accepted.step, accepted.at
            )?;
        }
        writeln!(writer, "deploy tier: {}", state.deploy_tier)?;
        writeln!(writer, "status: {:?}", state.status)?;
        if let Some(reason) = &state.closed_reason {
            writeln!(writer, "closed reason: {reason}")?;
        }
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
        // Issue #236: only meaningful when this workflow actually has an
        // intent step -- `Review` never does, and a Feature/Bugfix/Refactor
        // whose classification did not gate one in has nothing for the flag
        // to select between.
        if state
            .steps
            .iter()
            .any(|step| step.phase == WorkflowPhase::Intent)
        {
            writeln!(
                writer,
                "brainstorm: {}",
                if state.brainstorm { "on" } else { "off" }
            )?;
        }
        if let Some(step) = state.current() {
            writeln!(
                writer,
                "current: {} ({}, skill {}, agent {}{})",
                step.id,
                step.phase,
                step.skill,
                step.agent.as_deref().unwrap_or("-"),
                step.artifact
                    .map(|stage| format!(", artifact {stage}"))
                    .unwrap_or_default()
            )?;
        } else {
            writeln!(writer, "current: none")?;
        }
        let completed_rendered = state
            .completed_steps
            .iter()
            .map(|id| match state.step_durations_ms.get(id) {
                Some(&ms) => format!("{id} ({})", format_wall_clock(ms)),
                None => id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(writer, "completed: {completed_rendered}")?;
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
                        "  {}\t{}\tskill={}\tagent={}\tartifact={}\twhen={:?}\tapproval={}",
                        step.id,
                        step.phase,
                        step.skill,
                        step.agent.as_deref().unwrap_or("-"),
                        step.artifact
                            .map(|stage| stage.to_string())
                            .unwrap_or_else(|| "-".into()),
                        step.condition,
                        step.approval
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
            let deploy_tier = super::deploy::effective_tier(&repo)?;
            let brainstorm = args
                .brainstorm_override()
                .unwrap_or_else(|| default_brainstorm_for_kind(args.kind));
            if let Some(agent) = &selected_agent {
                let registry = SkillRegistry::load_for_repo(
                    &repo,
                    dirs::home_dir().as_deref(),
                    !args.built_in_only,
                )?;
                let agent_registry = AgentRegistry::load_for_repo(
                    &repo,
                    dirs::home_dir().as_deref(),
                    !args.built_in_only,
                )?;
                let report = super::capability::CapabilityReport::for_repo(agent, &repo)?;
                for step in materialize(
                    definition.kind,
                    &classification,
                    profile,
                    deploy_tier,
                    brainstorm,
                ) {
                    for skill in step_skill_ids(&step, &classification) {
                        registry.ensure_supported(&skill, &report)?;
                    }
                    if let Some(seat) = step.agent.as_deref() {
                        agent_registry.ensure_supported(seat, &report)?;
                    }
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
            if brainstorm != state.brainstorm {
                state.brainstorm = brainstorm;
                apply_brainstorm_selection(brainstorm, &mut state.steps);
            }
            if let Some(forced_profile) = args.profile {
                state.set_profile(forced_profile);
            }
            apply_effective_deploy_tier(&mut state, deploy_tier);
            state.usage_checkpoint = usage_checkpoint(&state.repo);
            if let Some(frontend_root) = &args.frontend_root {
                state.frontend_target_root = Some(resolve_frontend_root(frontend_root)?);
            }
            ensure_current_artifact_template(&state)?;
            if work_dir_is_gitignored(&state.repo) {
                writeln!(
                    writer,
                    "warning: .zirv/work is ignored by this repository's .gitignore -- workflow artifacts will not be tracked by git"
                )?;
            }
            save(&state_dir, &state, true)?;
            let mut event = super::telemetry::TelemetryEvent::new(
                super::telemetry::TelemetryKind::WorkflowStarted,
            );
            event.workflow_id = Some(state.id.clone());
            event.intent = Some(state.classification.intent);
            event.complexity = Some(state.classification.complexity);
            event.risk = Some(state.classification.risk);
            event.work_domain = Some(state.classification.work_domain.domain);
            event.deploy_tier = Some(state.deploy_tier.to_string());
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
            let mut state = load(&state_dir, &repo, &args.id)?;
            // Checked against the as-loaded status, before `refresh_deploy_tier`:
            // `apply_effective_deploy_tier` unconditionally recomputes `status`
            // from the current step's position, which would otherwise silently
            // revive a terminal `Failed`/`Completed`/`Closed` workflow back to
            // `Running`/`AwaitingApproval`.
            if !matches!(
                state.status,
                WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
            ) {
                return Err(format!("cannot resume workflow in {:?} state", state.status).into());
            }
            refresh_deploy_tier(&mut state)?;
            ensure_current_artifact_template(&state)?;
            save(&state_dir, &state, true)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Reclassify(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = reclassify(&state_dir, load(&state_dir, &repo, &args.id)?, args.profile)?;
            write_state(writer, &state, args.json)?;
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
        WorkflowSubcommand::Agents(args) => {
            return super::agents::run(args, writer);
        }
        WorkflowSubcommand::Artifacts(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = load(&state_dir, &repo, &args.id)?;
            let statuses = workflow_artifact_statuses(&state)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &statuses)?;
                writeln!(writer)?;
            } else if statuses.is_empty() {
                writeln!(writer, "workflow has no committed work-product artifacts")?;
            } else {
                writeln!(writer, "STAGE\tPATH\tSTATE")?;
                for status in statuses {
                    let state = if status.drifted {
                        "drifted"
                    } else if status.accepted {
                        "accepted"
                    } else if status.exists {
                        "pending"
                    } else {
                        "missing"
                    };
                    writeln!(
                        writer,
                        "{}\t{}\t{}{}",
                        status.stage,
                        status.rel_path,
                        state,
                        status
                            .accepted_at
                            .as_deref()
                            .map(|at| format!(" ({at})"))
                            .unwrap_or_default()
                    )?;
                }
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
            if let Some(frontend_root) = &args.frontend_root {
                state.frontend_target_root = Some(resolve_frontend_root(frontend_root)?);
                // Persisted before the gate runs: a fail-closed advance below
                // must not force the operator to pass `--frontend-root` again
                // on retry.
                let active = matches!(
                    state.status,
                    WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
                );
                save(&state_dir, &state, active)?;
            }
            let outcome = if args.run_checks {
                let current = state
                    .current()
                    .cloned()
                    .ok_or("workflow has no current step")?;
                if run_required_checks(&state_dir, &repo, current.phase, &current.id, writer)? {
                    StepOutcome::Success
                } else {
                    writeln!(
                        writer,
                        "checks failed; step '{}' was not advanced",
                        current.id
                    )?;
                    return Ok(1);
                }
            } else {
                args.outcome
                    .ok_or("--outcome is required unless --run-checks is set")?
            };
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
                    ..Default::default()
                },
            );
            let state = advance_with_evidence(
                &state_dir,
                state,
                outcome,
                Some(&evidence),
                args.accept_preexisting_findings,
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Close(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = close(
                &state_dir,
                load(&state_dir, &repo, &args.id)?,
                args.reason.clone(),
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Review(args) => {
            return super::review::run(args, writer);
        }
        WorkflowSubcommand::Maintain(args) => {
            return super::maintain::run(args, writer);
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

    /// Prepends a synthetic, always-present intent step, for engine-internals
    /// tests (resume, artifact status, deploy-tier tightening) that need a
    /// leading artifact step regardless of classification -- Feature's own
    /// intent step is conditional now (`ComplexityOrRisk{Bounded, Medium}`,
    /// same as Bugfix), and that threshold overlaps Feature's existing plan
    /// gate (`ComplexityOrRisk{Bounded, High}`) and review gate
    /// (`RiskAtLeast(Medium)`), so no classification gates intent in alone
    /// without also gating in plan or review, which these tests are not
    /// about. `current_step` is already `0` on a freshly started state, so
    /// inserting at the front needs no index adjustment.
    fn with_synthetic_intent_step(mut state: WorkflowState) -> WorkflowState {
        state.steps.insert(
            0,
            artifact_step(
                "intent",
                WorkflowPhase::Intent,
                "brainstorm",
                ArtifactStage::Intent,
                StepCondition::Always,
            ),
        );
        state
    }

    fn skip_leading_artifact_steps(mut state: WorkflowState) -> WorkflowState {
        while state.current().is_some_and(|step| step.artifact.is_some()) {
            let id = state.current().unwrap().id.clone();
            state.completed_steps.push(id);
            state.current_step += 1;
        }
        state.status = if state.current().is_some() {
            WorkflowStatus::Running
        } else {
            WorkflowStatus::Completed
        };
        state
    }

    #[test]
    fn trivial_feature_skips_intent_spec_plan_and_review() {
        // Feature's intent step now shares Bugfix's ComplexityOrRisk{Bounded,
        // Medium} condition instead of `Always`, so a trivial/low-risk
        // classification no longer stops at an approval-gated intent
        // artifact.
        let steps = definition(WorkflowKind::Feature).materialize(&low_classification());
        assert_eq!(
            steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["implement", "test", "verify", "deploy"]
        );
    }

    #[test]
    fn bounded_feature_keeps_the_intent_step() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let steps = definition(WorkflowKind::Feature).materialize(&classification);
        assert_eq!(steps.first().unwrap().id, "intent");
        assert_eq!(steps[0].artifact, Some(ArtifactStage::Intent));
        assert!(steps[0].approval);
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
        assert_eq!(
            steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            [
                "intent",
                "spec",
                "plan",
                "implement",
                "test",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert!(steps[1].approval);
        assert_eq!(steps[1].artifact, Some(ArtifactStage::Spec));
    }

    #[test]
    fn deploy_tier_matrix_adds_structural_gates() {
        let classification = low_classification();
        let profile = WorkflowProfile::Standard;

        let development = materialize(
            WorkflowKind::Feature,
            &classification,
            profile,
            DeployTier::Development,
            true,
        );
        let development_deploy = development
            .iter()
            .find(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        assert!(!development_deploy.approval);
        assert!(
            !development
                .iter()
                .any(|step| step.phase == WorkflowPhase::Review)
        );

        let staging = materialize(
            WorkflowKind::Feature,
            &classification,
            profile,
            DeployTier::Staging,
            true,
        );
        assert!(
            staging
                .iter()
                .find(|step| step.phase == WorkflowPhase::Deploy)
                .unwrap()
                .approval
        );
        assert!(
            !staging
                .iter()
                .any(|step| step.phase == WorkflowPhase::Review)
        );

        let production = materialize(
            WorkflowKind::Feature,
            &classification,
            profile,
            DeployTier::Production,
            true,
        );
        let review = production
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .unwrap();
        let verify = production
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();
        let deploy = production
            .iter()
            .position(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        assert!(review < verify && verify < deploy);
        assert!(production[review].agent.as_deref() == Some("reviewer"));
        assert!(production[deploy].approval);
    }

    fn at_phase(mut state: WorkflowState, phase: WorkflowPhase) -> WorkflowState {
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == phase)
            .expect("phase present in this workflow's steps");
        state.status = WorkflowStatus::Running;
        state
    }

    fn production_feature_state(repo: &Path) -> WorkflowState {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let mut state = WorkflowState::start(
            repo.to_path_buf(),
            "ship it".into(),
            WorkflowKind::Feature,
            Some("claude".to_string()),
            true,
            classification,
        );
        apply_effective_deploy_tier(&mut state, DeployTier::Production);
        state
    }

    #[test]
    fn auto_spawn_decision_truth_table() {
        let repo = tempdir().unwrap();
        let state = production_feature_state(repo.path());

        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Review),
                false,
                true,
                None
            ),
            Err(AutoSpawnSkip::Quiet),
            "disabled must never fire"
        );
        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Review),
                true,
                false,
                None
            ),
            Err(AutoSpawnSkip::NoPermit),
            "no permit must never fire"
        );
        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Implement),
                true,
                true,
                None
            ),
            Err(AutoSpawnSkip::Quiet),
            "Implement must never fire"
        );

        let mut awaiting = at_phase(state.clone(), WorkflowPhase::Review);
        awaiting.status = WorkflowStatus::AwaitingApproval;
        assert_eq!(
            auto_spawn_decision(&awaiting, true, true, None),
            Err(AutoSpawnSkip::Quiet),
            "AwaitingApproval must never fire"
        );

        let review = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Review),
            true,
            true,
            None,
        )
        .expect("Review with a workflow adapter fires");
        assert_eq!(review.phase, WorkflowPhase::Review);
        assert_eq!(
            review.argv,
            vec![
                "workflow",
                "review",
                "run",
                &state.id,
                "--agent",
                "claude",
                "--repo",
                &state.repo.display().to_string(),
            ]
        );

        // `state.adapter` always wins over a configured default, even when
        // both resolve.
        let with_both = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Review),
            true,
            true,
            Some("codex"),
        )
        .expect("Review fires");
        assert_eq!(
            with_both.argv[5], "claude",
            "the workflow's own adapter wins"
        );

        let mut no_adapter = at_phase(state.clone(), WorkflowPhase::Review);
        no_adapter.adapter = None;
        assert_eq!(
            auto_spawn_decision(&no_adapter, true, true, None),
            Err(AutoSpawnSkip::NoAgent),
            "Review with no workflow adapter and no default must not fire"
        );

        let with_default = auto_spawn_decision(&no_adapter, true, true, Some("codex"))
            .expect("Review with no workflow adapter falls back to the operator default");
        assert_eq!(with_default.argv[5], "codex");

        let test = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Test),
            true,
            true,
            None,
        )
        .expect("Test fires");
        assert_eq!(test.phase, WorkflowPhase::Test);
        assert_eq!(
            test.argv,
            vec![
                "test",
                "changed",
                "--repo",
                &state.repo.display().to_string()
            ]
        );

        let verify = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Verify),
            true,
            true,
            None,
        )
        .expect("Verify fires");
        assert_eq!(verify.phase, WorkflowPhase::Verify);
        assert_eq!(
            verify.argv,
            vec!["verify", "--repo", &state.repo.display().to_string()]
        );
    }

    /// `Quiet` is the only skip that stays silent -- the config-disabled
    /// path, or a phase auto-spawn was never meant to touch. `NoPermit`/
    /// `NoAgent` are eligible-but-skipped, so an operator who turned the
    /// key on must see why.
    #[test]
    fn auto_spawn_skip_reason_is_silent_only_for_quiet() {
        assert_eq!(auto_spawn_skip_reason(AutoSpawnSkip::Quiet), None);
        assert!(auto_spawn_skip_reason(AutoSpawnSkip::NoPermit).is_some());
        assert!(auto_spawn_skip_reason(AutoSpawnSkip::NoAgent).is_some());
    }

    #[test]
    fn advance_with_evidence_records_no_agent_dispatched_event_when_auto_spawn_is_disabled() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        save(&state_dir, &state, true).unwrap();

        let advanced =
            advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false).unwrap();
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Test);

        let events = crate::commands::workflow::telemetry::list(&state_dir, &advanced.repo)
            .unwrap_or_default();
        assert!(
            !events.iter().any(|event| {
                event.kind == crate::commands::workflow::telemetry::TelemetryKind::AgentDispatched
            }),
            "auto_spawn_on_gate defaults to false; advance must never record a dispatch"
        );
    }

    #[test]
    fn tightening_to_production_rewinds_later_completed_evidence() {
        // `apply_effective_deploy_tier` re-materializes `steps` straight from
        // `state.classification`, so the leading artifact step below must
        // survive that regeneration rather than being injected by hand.
        // Bounded complexity gates Feature's own intent step in
        // (`ComplexityOrRisk{Bounded, Medium}`) but also gates its plan step
        // in (`ComplexityOrRisk{Bounded, High}`, the same `Bounded` bar), so
        // both now precede implement.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let mut state = WorkflowState::start(
            PathBuf::from("repo"),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        state.completed_steps = vec!["intent", "plan", "implement", "test", "verify"]
            .into_iter()
            .map(str::to_string)
            .collect();
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        state.status = WorkflowStatus::Running;

        apply_effective_deploy_tier(&mut state, DeployTier::Production);

        assert_eq!(state.deploy_tier, DeployTier::Production);
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Review);
        assert_eq!(
            state.completed_steps,
            ["intent", "plan", "implement", "test"],
            "verify evidence after the inserted production review must be replayed"
        );
    }

    #[test]
    fn frontend_classification_selects_the_frontend_profile_automatically() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        classification.complexity = Complexity::Bounded;
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
        assert_eq!(state.current().unwrap().skill, "brainstorm");
        assert_eq!(
            state.current().unwrap().artifact,
            Some(ArtifactStage::Intent)
        );
        assert!(
            state
                .steps
                .iter()
                .find(|step| step.phase == WorkflowPhase::Implement)
                .is_some_and(|step| step.skill == "frontend-implement")
        );
    }

    #[test]
    fn brainstorm_defaults_per_kind() {
        assert!(default_brainstorm_for_kind(WorkflowKind::Feature));
        assert!(default_brainstorm_for_kind(WorkflowKind::Spike));
        assert!(!default_brainstorm_for_kind(WorkflowKind::Bugfix));
        assert!(!default_brainstorm_for_kind(WorkflowKind::Refactor));
        assert!(!default_brainstorm_for_kind(WorkflowKind::Review));
    }

    #[test]
    fn brainstorm_flags_are_mutually_exclusive_and_resolve_to_an_override() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: StartArgs,
        }
        let plain =
            Cli::try_parse_from(["zirv", "feature", "--task", "x"]).expect("no flags parses");
        assert_eq!(plain.args.brainstorm_override(), None);

        let on = Cli::try_parse_from(["zirv", "feature", "--task", "x", "--brainstorm"])
            .expect("--brainstorm parses");
        assert_eq!(on.args.brainstorm_override(), Some(true));

        let off = Cli::try_parse_from(["zirv", "feature", "--task", "x", "--no-brainstorm"])
            .expect("--no-brainstorm parses");
        assert_eq!(off.args.brainstorm_override(), Some(false));

        assert!(
            Cli::try_parse_from([
                "zirv",
                "feature",
                "--task",
                "x",
                "--brainstorm",
                "--no-brainstorm",
            ])
            .is_err(),
            "both flags together must be refused"
        );
    }

    #[test]
    fn brainstorm_selects_the_intent_step_skill_and_survives_an_explicit_override() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        let mut feature_classification = low_classification();
        feature_classification.complexity = Complexity::Bounded;
        let feature = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            feature_classification,
        );
        assert_eq!(feature.current().unwrap().skill, "brainstorm");
        assert!(feature.brainstorm);

        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        classification.risk = RiskBand::Medium;
        let bugfix = WorkflowState::start(
            repo.path().to_path_buf(),
            "small bugfix".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            classification,
        );
        assert_eq!(bugfix.current().unwrap().skill, "write-intent");
        assert!(!bugfix.brainstorm);

        let mut overridden = bugfix;
        apply_brainstorm_selection(true, &mut overridden.steps);
        assert_eq!(overridden.current().unwrap().skill, "brainstorm");
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

        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert_eq!(state.steps[0].skill, "brainstorm");
        let design = state
            .steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Design)
            .expect("substantial frontend has spec/design");
        assert_eq!(design.skill, "frontend-design");
        assert!(
            design.approval,
            "spec acceptance remains a hard artifact gate"
        );
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

    /// Reviewer finding: `apply_profile` forced a Design step's approval off
    /// going *to* Frontend but never restored it going back to Standard, so
    /// a `reclassify`/`set_profile` revert could leave the workflow with
    /// Frontend's autonomous-design approval semantics while reporting
    /// Standard. The restore must come from the kind's own authored
    /// default, not merely "leave whatever value is currently set".
    #[test]
    fn apply_profile_restores_the_kind_default_design_approval_when_leaving_frontend() {
        let mut steps = definition(WorkflowKind::Spike).materialize(&low_classification());
        apply_profile(WorkflowKind::Spike, WorkflowProfile::Frontend, &mut steps);
        let design = steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Design)
            .expect("spike has a design step");
        assert!(!design.approval, "Frontend forces design approval off");

        // Simulate approval having drifted from the kind's own default for
        // any reason, so the assertion below proves the Standard branch
        // actively restores it rather than coincidentally leaving it alone.
        for step in &mut steps {
            if step.phase == WorkflowPhase::Design {
                step.approval = true;
            }
        }
        apply_profile(WorkflowKind::Spike, WorkflowProfile::Standard, &mut steps);
        let design = steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Design)
            .expect("spike has a design step");
        assert!(
            !design.approval,
            "leaving Frontend must restore the kind's own authored approval default"
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
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("automatically ran the detector")
                || error.contains("cannot inspect changed paths"),
            "{error}"
        );
    }

    #[test]
    fn frontend_gate_uses_frontend_target_root_when_set() {
        // #214: the workflow is tracked in `workflow_repo`, but the real
        // frontend under test lives in a sibling `target_repo` -- the
        // detector must scan `frontend_target_root`, not `state.repo`, once
        // it is set.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        // Both repos are real git checkouts, so `changed_paths_since_base`
        // resolves cleanly instead of surfacing an unrelated "not a git
        // repository" error that would mask what this test is about.
        git(workflow_repo.path(), &["init", "-q"]);
        std::fs::write(workflow_repo.path().join("README.md"), "readme\n").unwrap();
        git(workflow_repo.path(), &["add", "."]);
        git(workflow_repo.path(), &["commit", "-q", "-m", "base"]);
        // #255: an empty scan is now a pass ("not applicable"), so this repo
        // needs a real, introduced blocking finding in scope -- not just an
        // absence of frontend files -- to still demonstrate the wrong repo
        // being scanned when `--frontend-root` is missing.
        std::fs::write(
            workflow_repo.path().join("Bad.tsx"),
            "export const Bad = () => <img src={avatar} />;\n",
        )
        .unwrap();

        git(target_repo.path(), &["init", "-q"]);
        std::fs::write(target_repo.path().join("README.md"), "readme\n").unwrap();
        git(target_repo.path(), &["add", "."]);
        git(target_repo.path(), &["commit", "-q", "-m", "base"]);
        // A minimal, clean stylesheet: no images, semantic-action targets,
        // gradients, motion, or viewport hazards, so the detector should
        // report zero blocking findings for it.
        std::fs::write(
            target_repo.path().join("style.css"),
            ".card { color: rebeccapurple; }\n",
        )
        .unwrap();

        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;

        let build_state = |classification: Classification| {
            let mut state = WorkflowState::start(
                workflow_repo.path().to_path_buf(),
                "build a frontend component".into(),
                WorkflowKind::Feature,
                None,
                true,
                classification,
            );
            let test_index = state
                .steps
                .iter()
                .position(|step| step.phase == WorkflowPhase::Test)
                .unwrap();
            state.completed_steps = state.steps[..test_index]
                .iter()
                .map(|step| step.id.clone())
                .collect();
            state.current_step = test_index;
            state.status = WorkflowStatus::Running;
            state
        };

        // Without a frontend target root, the gate fails closed scanning the
        // frontend-less workflow repo -- same failure mode as #214.
        let without_root = advance_with_evidence(
            &state_dir,
            build_state(classification.clone()),
            StepOutcome::Success,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            without_root.contains("automatically ran the detector"),
            "{without_root}"
        );

        // With the frontend target root pointed at the sibling repo, the
        // detector gate must pass -- execution proceeds past it to the
        // unrelated general test-evidence gate, which `workflow_repo` has no
        // recorded evidence for.
        let mut state = build_state(classification);
        state.frontend_target_root = Some(target_repo.path().canonicalize().unwrap());
        let with_root = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();
        assert!(
            !with_root.contains("automatically ran the detector"),
            "{with_root}"
        );
        assert!(
            with_root.contains("requires fresh passing evidence"),
            "{with_root}"
        );
    }

    /// #255: a repository with zero frontend-extension files in scope is
    /// "frontend gate not applicable", not missing evidence to fail closed
    /// on -- the old `report.analyzed_files.is_empty()` check made a
    /// Frontend-profile workflow over a backend-only change unable to ever
    /// pass its Test step.
    #[test]
    fn frontend_test_step_passes_with_zero_frontend_files_in_the_change_surface() {
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

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
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;

        // Seed passing general test evidence so the ONLY thing under test
        // here is the frontend gate's handling of an empty (0 frontend
        // files) scope, not the unrelated `zirv test changed` gate.
        let fingerprint = super::super::verification::change_fingerprint(repo.path()).unwrap();
        let evidence_report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: super::super::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(&state_dir, &evidence_report).unwrap();

        let advanced = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("zero frontend files in scope must not fail the frontend gate");
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Verify);
    }

    /// #260-adjacent: `zirv workflow advance --run-checks` collapses "run
    /// the gate, then advance" into one call -- a passing check must both
    /// print the evidence summary and advance past the `Test` step.
    #[test]
    fn advance_run_checks_runs_the_test_gate_and_advances_on_success() {
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let passing = if cfg!(windows) { "exit /b 0" } else { "exit 0" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{passing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 0, "a passing check must advance");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("verification"),
            "expected the evidence summary to be printed, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Verify,
            "the test step must have advanced"
        );
    }

    /// The mirror of the above: a failing check must print the failure and
    /// leave the workflow exactly where it was, rather than advancing on
    /// bad evidence.
    #[test]
    fn advance_run_checks_does_not_advance_on_failure() {
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 1, "a failing check must not advance");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("was not advanced"),
            "expected the failure to be reported, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Test,
            "a failing check must leave the workflow on the test step"
        );
    }

    /// Dogfooding regression: on a repository with a recorded test baseline
    /// (`zirv test baseline`), `--run-checks` used to decide pass/fail from
    /// `run_test`/`run_verify`'s own raw exit code, which reflects the
    /// unwaived result -- non-zero even when the only failure is already
    /// covered by the baseline. The very next `--outcome success` against the
    /// identical persisted report advanced fine, because that path (and the
    /// gate `--run-checks` now shares) reads the report back through
    /// `latest_is_fresh_and_passing`, which is baseline-aware. This proves
    /// `--run-checks` advances in that exact situation instead of reporting
    /// "checks failed".
    #[test]
    fn advance_run_checks_advances_when_the_only_failure_is_baselined() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        super::super::verification::save_baseline(
            repo.path(),
            BTreeSet::from(["wrap::tests::a".to_string()]),
        )
        .unwrap();

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        // A `unit`-kind check whose output has the exact shape
        // `parse_cargo_test_failure_names`/`FailureNameScanner` recognize --
        // a `failures:` header naming `wrap::tests::a`, immediately followed
        // by a `test result: FAILED` line -- and a non-zero exit, so the
        // check itself is genuinely `Failed`; only the recorded baseline
        // makes the gate pass.
        let baselined_failure = if cfg!(windows) {
            "echo failures: & echo wrap::tests::a & echo test result: FAILED. 0 passed; 1 failed; \
             0 ignored; 0 measured; 0 filtered out; finished in 0.00s & exit /b 101"
        } else {
            "printf \"failures:\\nwrap::tests::a\\ntest result: FAILED. 0 passed; 1 failed; 0 \
             ignored; 0 measured; 0 filtered out; finished in 0.00s\\n\"; exit 101"
        };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{baselined_failure}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(
            code, 0,
            "a failure fully covered by the recorded baseline must still advance"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Verify,
            "the test step must have advanced on the baselined report"
        );
    }

    /// Fail-open regression: a stale, still-fingerprint-fresh PASSING report
    /// already sits at `latest` (fingerprint unchanged since -- nothing in
    /// the tree moved). The run this `--run-checks` call actually performs
    /// FAILS, but persisting its report is made to fail too (`latest`'s
    /// pointer file is read-only, so `run_and_persist`'s `persist` call hits
    /// a genuine IO error -- swallowed into a warning, never an error, so
    /// the run's own printed results survive). Before the identity check,
    /// `latest_is_fresh_and_passing` would still read the untouched stale
    /// PASSING report and the gate would incorrectly advance. It must not:
    /// the identity of `latest` is unchanged, so no fresh report exists to
    /// gate on, and the step must stay put.
    #[test]
    fn advance_run_checks_does_not_advance_when_persistence_silently_fails() {
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        // Seed a stale but still fingerprint-fresh PASSING report at
        // `latest`, matching the exact tree state above.
        let fingerprint = super::super::verification::change_fingerprint(repo.path()).unwrap();
        let stale_passing_report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "stale-pass".into(),
            mode: super::super::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(&state_dir, &stale_passing_report).unwrap();

        // Make the next persist's `latest`-pointer write fail: `write_private`
        // writes a temp sibling then `rename`s it over `latest`.
        let latest_pointer = state_dir
            .verification()
            .join(repo_slug(repo.path()))
            .join("latest");
        // On Unix a rename over a read-only FILE succeeds (the directory's
        // write bit governs), so there the directory holding `latest` is
        // what gets locked; on Windows the read-only destination file itself
        // makes the rename fail with ERROR_ACCESS_DENIED.
        let lock_target = if cfg!(unix) {
            latest_pointer.parent().unwrap().to_path_buf()
        } else {
            latest_pointer.clone()
        };
        let original_perms = std::fs::metadata(&lock_target).unwrap().permissions();
        let mut readonly_perms = original_perms.clone();
        readonly_perms.set_readonly(true);
        std::fs::set_permissions(&lock_target, readonly_perms).unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let result = run(&args, &mut out);

        // Cleanup before any assertion panics, so the tempdir can still be
        // removed on drop even if an assertion below fails. Restores the
        // exact original permissions rather than `set_readonly(false)`,
        // which clippy flags as leaving the file world-writable on Unix.
        std::fs::set_permissions(&lock_target, original_perms).unwrap();

        let code = result.unwrap();
        assert_eq!(
            code, 1,
            "a genuinely failing run whose report could not be persisted must not advance on a \
             stale prior report"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("no fresh report was persisted"),
            "expected the persistence-failure reason to be reported, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Test,
            "the step must not advance on stale evidence when the fresh report never persisted"
        );
    }

    /// `--run-checks` only knows how to satisfy a `Test`/`Verify` step's own
    /// evidence gate; any other current step must fail loudly rather than
    /// silently treating itself as satisfied.
    #[test]
    fn advance_run_checks_rejects_a_non_test_verify_step() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Implement);
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(
            error.contains("--run-checks") && error.contains("--outcome instead"),
            "{error}"
        );
    }

    fn review_finding(
        id: &str,
        disposition: super::super::review::FindingDisposition,
        recommended: Option<super::super::review::FindingDisposition>,
    ) -> super::super::review::ReviewFinding {
        super::super::review::ReviewFinding {
            id: id.into(),
            severity: super::super::review::FindingSeverity::Major,
            summary: "summary".into(),
            path: None,
            line: None,
            disposition,
            recommended_disposition: recommended,
            created_at: 0,
        }
    }

    /// #260-adjacent (T3, bulk dispose): three open findings with mixed
    /// recommendations must each land on their own recommended disposition
    /// in one call; a finding with no recommendation stays `Open` and is
    /// still reported (not silently dropped); an already-resolved finding is
    /// left alone and not reported at all.
    #[test]
    fn apply_recommended_dispositions_applies_each_open_findings_own_recommendation() {
        use super::super::review::FindingDisposition as Disposition;
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
        state.review_findings = vec![
            review_finding("a", Disposition::Open, Some(Disposition::Fixed)),
            review_finding("b", Disposition::Open, Some(Disposition::Dismissed)),
            review_finding("c", Disposition::Open, None),
            review_finding("d", Disposition::Accepted, Some(Disposition::Fixed)),
        ];
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let (state, results) = apply_recommended_dispositions(&state_dir, state).unwrap();

        assert_eq!(
            results.len(),
            3,
            "only the open findings are considered: {results:?}"
        );
        let by_id: std::collections::BTreeMap<_, _> = results
            .iter()
            .map(|result| (result.finding_id.clone(), result.applied))
            .collect();
        assert_eq!(by_id["a"], Some(Disposition::Fixed));
        assert_eq!(by_id["b"], Some(Disposition::Dismissed));
        assert_eq!(
            by_id["c"], None,
            "no recommendation must be reported as such"
        );

        let finding = |needle: &str| {
            state
                .review_findings
                .iter()
                .find(|finding| finding.id == needle)
                .unwrap()
        };
        assert_eq!(finding("a").disposition, Disposition::Fixed);
        assert_eq!(finding("b").disposition, Disposition::Dismissed);
        assert_eq!(
            finding("c").disposition,
            Disposition::Open,
            "no recommendation must leave the finding open"
        );
        assert_eq!(
            finding("d").disposition,
            Disposition::Accepted,
            "an already-resolved finding must never be revisited"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded
                .review_findings
                .iter()
                .find(|finding| finding.id == "a")
                .unwrap()
                .disposition,
            Disposition::Fixed,
            "the applied disposition must persist"
        );
    }

    #[test]
    fn advance_frontend_root_flag_persists_into_state() {
        let repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: Some(StepOutcome::Failure),
                run_checks: false,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: Some(target_repo.path().to_path_buf()),
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.frontend_target_root,
            Some(target_repo.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn advance_persists_frontend_root_before_the_gate_even_when_it_still_fails_closed() {
        // #214 follow-up: `--frontend-root` must be saved before the gate
        // runs, so a fail-closed advance (the target root has no fresh
        // evidence yet) still records the root -- the operator should not
        // have to pass the flag again on retry.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            workflow_repo.path().to_path_buf(),
            "build a frontend component".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: Some(StepOutcome::Success),
                run_checks: false,
                repo: Some(workflow_repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                // `target_repo` is empty and has no detector evidence of its
                // own, so the gate must still fail closed against it.
                frontend_root: Some(target_repo.path().to_path_buf()),
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let result = run(&args, &mut out);
        assert!(
            result.is_err(),
            "expected the gate to still fail closed against an empty target root"
        );

        let reloaded = load(&state_dir, workflow_repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.frontend_target_root,
            Some(target_repo.path().canonicalize().unwrap())
        );
    }

    /// #255 recovery path (i): a task classified General/Standard (no
    /// frontend text or path signal) can still be forced onto the Frontend
    /// methodology overlay with `--profile`, applied after classification
    /// materializes the default steps.
    #[test]
    fn start_profile_flag_overrides_automatic_classification() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                kind: WorkflowKind::Bugfix,
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                // Declared, so classification never needs a real git
                // repository: this test is about `--profile`, not about
                // git-measured risk.
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: Some(WorkflowProfile::Frontend),
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(
            state.classification.work_domain.domain,
            WorkDomain::General,
            "the task/paths alone must not have classified this as Frontend"
        );
        assert_eq!(state.profile, WorkflowProfile::Frontend);
        assert_eq!(state.profile_source, ProfileSource::OperatorOverride);
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-implement")
        );
    }

    /// #255 recovery path (ii): `workflow reclassify` forces a persisted
    /// workflow's profile without resetting the state machine -- completed
    /// steps and already-accepted artifacts survive the change.
    #[test]
    fn reclassify_preserves_completed_steps_and_accepted_artifacts() {
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
        assert_eq!(state.current().unwrap().id, "intent");
        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Intent).unwrap(),
            "# Intent\n\n## Problem\nConcrete problem\n\n## Desired outcome\nConcrete result\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).unwrap();
        assert_eq!(state.current().unwrap().id, "spec");
        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Spec).unwrap(),
            "# Specification\n\n## Context\nReal context\n\n## Goals\n- ship it\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).unwrap();
        assert_eq!(state.current().unwrap().id, "plan");
        assert_eq!(state.profile, WorkflowProfile::Standard);
        let intent_hash_before = state.artifacts.get("intent").unwrap().accepted_hash.clone();
        let spec_hash_before = state.artifacts.get("spec").unwrap().accepted_hash.clone();
        assert!(intent_hash_before.is_some());
        assert!(spec_hash_before.is_some());

        let reclassified = reclassify(&state_dir, state, WorkflowProfile::Frontend).unwrap();

        assert_eq!(reclassified.profile, WorkflowProfile::Frontend);
        assert_eq!(reclassified.profile_source, ProfileSource::OperatorOverride);
        assert_eq!(
            reclassified.completed_steps,
            vec!["intent".to_string(), "spec".to_string()],
            "completed steps must survive reclassification"
        );
        assert_eq!(
            reclassified.artifacts.get("intent").unwrap().accepted_hash,
            intent_hash_before,
            "the accepted intent artifact must survive reclassification"
        );
        assert_eq!(
            reclassified.artifacts.get("spec").unwrap().accepted_hash,
            spec_hash_before,
            "the accepted spec artifact must survive reclassification"
        );
        assert_eq!(reclassified.current().unwrap().id, "plan");
        assert_eq!(reclassified.current().unwrap().skill, "frontend-plan");

        let reloaded = load(&state_dir, repo.path(), &reclassified.id).unwrap();
        assert_eq!(reloaded.profile, WorkflowProfile::Frontend);
    }

    /// #251: a full-surface (Review/Verify) detector scan tags a finding
    /// `preexisting` when its path was not part of the since-base change
    /// set. Without `--accept-preexisting-findings` such a finding still
    /// fails the gate and the error names the flag and the count; with the
    /// flag, the gate accepts it, records the acceptance, and execution
    /// reaches the next (render) gate instead.
    #[test]
    fn accept_preexisting_findings_flag_lets_the_review_gate_pass_pre_existing_blocking_findings() {
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.path().join("Old.tsx"),
            "export const Old = () => <img src={avatar} />;\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            repo.path().join("style.css"),
            ".card { color: rebeccapurple; }\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add clean style"]);

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
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Review);
        state.status = WorkflowStatus::Running;

        let without_flag =
            advance_with_evidence(&state_dir, state.clone(), StepOutcome::Success, None, false)
                .unwrap_err()
                .to_string();
        assert!(
            without_flag.contains("--accept-preexisting-findings"),
            "{without_flag}"
        );
        assert!(
            without_flag.contains("1 pre-existing blocking"),
            "{without_flag}"
        );
        assert!(
            without_flag.contains("0 introduced blocking"),
            "{without_flag}"
        );

        let with_flag = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, true)
            .unwrap_err()
            .to_string();
        assert!(
            !with_flag.contains("automatically ran the detector"),
            "the flag must let the detector gate pass its pre-existing findings: {with_flag}"
        );
    }

    /// Reviewer finding: the acceptance was only mutated on the in-memory
    /// `WorkflowState`, so if a LATER gate in this same `advance` call (the
    /// render/visual-review gate, right after the detector gate) still
    /// fails closed, the acceptance was lost -- the operator would have to
    /// pass `--accept-preexisting-findings` again on the very next retry.
    #[test]
    fn accept_preexisting_findings_persists_even_when_a_later_gate_fails() {
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.path().join("Old.tsx"),
            "export const Old = () => <img src={avatar} />;\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            repo.path().join("style.css"),
            ".card { color: rebeccapurple; }\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add clean style"]);

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
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Review);
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, true)
            .unwrap_err()
            .to_string();
        // The render gate fails closed in this test environment (no dev
        // server/browser); confirm we actually got past the detector gate
        // so this test is exercising the scenario it claims to.
        assert!(!error.contains("automatically ran the detector"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert!(
            reloaded.accepted_preexisting_findings.is_some(),
            "the acceptance must survive a later gate failing closed in the same advance"
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
            not_applicable: false,
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

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("automatic rendered evidence"), "{error}");
        assert!(error.contains("zirv frontend render"), "{error}");
        assert!(!error.contains("frontend review --help"), "{error}");
    }

    #[test]
    fn frontend_render_gate_uses_frontend_target_root_when_set() {
        // #214 follow-up: once `frontend_target_root` is set, the render/
        // visual-review gate must scan it instead of `state.repo`, mirroring
        // `frontend_gate_uses_frontend_target_root_when_set` for the
        // detector gate.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        git(workflow_repo.path(), &["init", "-q"]);
        std::fs::write(workflow_repo.path().join("README.md"), "readme\n").unwrap();
        git(workflow_repo.path(), &["add", "."]);
        git(workflow_repo.path(), &["commit", "-q", "-m", "base"]);

        git(target_repo.path(), &["init", "-q"]);
        std::fs::write(
            target_repo.path().join("App.tsx"),
            "export const App = () => <main />;\n",
        )
        .unwrap();
        git(target_repo.path(), &["add", "App.tsx"]);
        git(target_repo.path(), &["commit", "-q", "-m", "base"]);

        // Pre-seed a fresh, passing detector report for `target_repo` so the
        // detector gate above the render step is already satisfied and
        // execution reaches the render/visual-review gate under test here.
        let profile =
            super::super::frontend::ensure_profile(&state_dir, target_repo.path()).unwrap();
        let detector = super::super::frontend_detector::DetectorReport {
            schema_version: super::super::frontend_detector::DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: target_repo.path().canonicalize().unwrap(),
            change_fingerprint: super::super::verification::change_fingerprint(target_repo.path())
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
            not_applicable: false,
        };
        super::super::frontend_detector::save_report(&state_dir, &detector).unwrap();

        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            workflow_repo.path().to_path_buf(),
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
        state.frontend_target_root = Some(target_repo.path().canonicalize().unwrap());

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("automatic rendered evidence"), "{error}");
        let target_display = target_repo
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string();
        let workflow_display = workflow_repo
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string();
        assert!(
            error.contains(&target_display),
            "expected the render gate error to name the target root: {error}"
        );
        assert!(
            !error.contains(&workflow_display),
            "render gate must not scan the workflow repo once frontend_target_root is set: {error}"
        );
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
        ensure_current_artifact_template(&state).unwrap();
        assert!(
            advance_with_evidence(&state_dir, state.clone(), StepOutcome::Success, None, false)
                .is_err()
        );
        assert!(
            approve(&state_dir, state.clone()).is_err(),
            "an untouched template cannot be accepted"
        );
        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            intent,
            "# Intent\n\n## Problem\nConcrete problem\n\n## Desired outcome\nConcrete result\n",
        )
        .unwrap();
        let approved = approve(&state_dir, state).unwrap();
        assert_eq!(approved.current().unwrap().id, "spec");
        assert_eq!(approved.status, WorkflowStatus::AwaitingApproval);
        assert!(
            approved
                .artifacts
                .get("intent")
                .and_then(|record| record.accepted_hash.as_ref())
                .is_some()
        );
    }

    /// Mirrors `skill::symlinked_manifests_are_refused` / `agents::load_dir`'s
    /// own symlink defense: a symlinked `.zirv/work/<id>` workflow directory
    /// must be refused before `ensure_current_artifact_template` ever creates
    /// or writes through it. `#[cfg(unix)]` for the same reason those two
    /// tests are: creating a real symlink needs elevated privileges on
    /// Windows, so this is verified on Linux/Docker instead (see the crate's
    /// own working instructions on cross-platform symlink tests).
    #[cfg(unix)]
    #[test]
    fn ensure_current_artifact_template_refuses_a_symlinked_workflow_directory() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".zirv/work")).unwrap();
        // Bounded complexity gates Feature's (now conditional) intent step in,
        // so `start` materializes the artifact record the template path needs.
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );
        let workflow_dir = repo.path().join(".zirv/work").join(&state.id);
        symlink(outside.path(), &workflow_dir).unwrap();

        let error = ensure_current_artifact_template(&state)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinked"), "{error}");
        assert!(
            !outside
                .path()
                .join(ArtifactStage::Intent.file_name())
                .exists(),
            "must refuse before ever writing through the symlink"
        );
    }

    /// Same defense, but the `.zirv/work` root itself is symlinked rather
    /// than the per-workflow directory beneath it.
    #[cfg(unix)]
    #[test]
    fn ensure_current_artifact_template_refuses_a_symlinked_work_root() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        symlink(outside.path(), repo.path().join(".zirv/work")).unwrap();
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );

        let error = ensure_current_artifact_template(&state)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinked"), "{error}");
    }

    /// `read_accepted_artifact` is the validated read `review.rs`'s
    /// `accepted_artifact_excerpt` funnels through instead of opening a
    /// `WorkflowArtifactRecord.rel_path` directly -- after acceptance, a
    /// repository writer replacing the accepted artifact file with a symlink
    /// to an arbitrary local file must be refused, not have its target's
    /// contents read and handed to an external review worker. Same
    /// `#[cfg(unix)]` rationale as the two tests above: a real symlink needs
    /// elevated privileges on Windows.
    #[cfg(unix)]
    #[test]
    fn read_accepted_artifact_refuses_a_symlinked_artifact_file() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret, not for the review worker").unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );
        ensure_current_artifact_template(&state).unwrap();
        let intent_path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::remove_file(&intent_path).unwrap();
        symlink(&secret, &intent_path).unwrap();
        state
            .artifacts
            .get_mut(ArtifactStage::Intent.key())
            .unwrap()
            .accepted_hash = Some("deadbeef".to_string());

        let error = read_accepted_artifact(&state, ArtifactStage::Intent)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinked"), "{error}");
    }

    /// Review finding: an accepted artifact whose bytes no longer hash to the
    /// accepted value is not the accepted artifact, so the validated read
    /// yields `None` for it -- the same drift rule `append_accepted_artifacts`
    /// applies -- while an unmodified one still reads back.
    #[test]
    fn read_accepted_artifact_rejects_hash_drift_and_accepts_the_pinned_bytes() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );
        ensure_current_artifact_template(&state).unwrap();
        let intent_path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(&intent_path, "# Intent\n\naccepted body\n").unwrap();
        let pinned = artifact_hash(&intent_path).unwrap();
        state
            .artifacts
            .get_mut(ArtifactStage::Intent.key())
            .unwrap()
            .accepted_hash = Some(pinned);

        let body = read_accepted_artifact(&state, ArtifactStage::Intent).unwrap();
        assert_eq!(body.as_deref(), Some("# Intent\n\naccepted body\n"));

        std::fs::write(&intent_path, "# Intent\n\nedited after acceptance\n").unwrap();
        assert_eq!(
            read_accepted_artifact(&state, ArtifactStage::Intent).unwrap(),
            None,
            "drifted bytes must never be handed on as the accepted artifact"
        );
    }

    /// Design spec risk-section commitment: `workflow start` warns (does not
    /// block) when `.zirv/work` would be gitignored, since a repo that
    /// ignores it silently loses every work-product artifact a workflow
    /// produces. Uses a real `git` shell-out, same as the frontend tests
    /// above (`git` is on PATH in this crate's own test environment).
    #[test]
    fn work_dir_is_gitignored_detects_a_zirv_work_gitignore_rule() {
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
        assert!(
            !work_dir_is_gitignored(repo.path()),
            "no .gitignore rule yet"
        );

        std::fs::write(repo.path().join(".gitignore"), ".zirv/\n").unwrap();
        assert!(
            work_dir_is_gitignored(repo.path()),
            "a .zirv/ gitignore rule covers .zirv/work"
        );
    }

    /// The probe is best-effort, never authoritative: a path with no git
    /// repository at all must read as "not ignored" rather than erroring
    /// `workflow start` on an environment `git` cannot make sense of.
    #[test]
    fn work_dir_is_gitignored_fails_open_outside_a_git_repository() {
        let not_a_repo = tempdir().unwrap();
        assert!(!work_dir_is_gitignored(not_a_repo.path()));
    }

    #[test]
    fn artifact_drift_reopens_the_owning_gate_and_invalidates_later_work() {
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
        ensure_current_artifact_template(&state).unwrap();
        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nA\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).unwrap();
        assert_eq!(state.current().unwrap().id, "spec");

        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nChanged after acceptance\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("intent artifact changed"));

        let reopened = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(reopened.current().unwrap().id, "intent");
        assert_eq!(reopened.status, WorkflowStatus::AwaitingApproval);
        assert!(
            reopened
                .artifacts
                .get("intent")
                .unwrap()
                .accepted_hash
                .is_none()
        );
    }

    #[test]
    fn workflow_artifact_status_reports_pending_accepted_and_drifted() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        // `approve` refreshes the deploy tier, which re-materializes `steps`
        // straight from `state.classification` -- so this needs a
        // classification that naturally keeps exactly one artifact step
        // (intent) through that regeneration. Bugfix's plan gate is
        // `ComplexityOrRisk{Substantial, High}` (unlike Feature's, which
        // shares intent's own `Bounded` threshold), so Bounded complexity
        // here gates intent in without also gating plan in.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small bugfix".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            classification,
        );
        ensure_current_artifact_template(&state).unwrap();
        let pending = workflow_artifact_statuses(&state).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].exists);
        assert!(!pending[0].accepted);

        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nA\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let accepted = approve(&state_dir, state).unwrap();
        let statuses = workflow_artifact_statuses(&accepted).unwrap();
        assert!(statuses[0].accepted);
        assert!(!statuses[0].drifted);

        std::fs::write(&intent, "# Intent\nchanged\n").unwrap();
        let statuses = workflow_artifact_statuses(&accepted).unwrap();
        assert!(statuses[0].drifted);
    }

    /// A step with no recorded duration (an older saved state) renders its
    /// bare id, never a bogus "0m0s".
    #[test]
    fn write_state_renders_completed_step_wall_clock_only_when_known() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.completed_steps = vec!["intent".to_string(), "spec".to_string()];
        state
            .step_durations_ms
            .insert("intent".to_string(), 130_000);
        state.step_durations_ms.insert("spec".to_string(), 40_000);
        let mut out = Vec::new();
        write_state(&mut out, &state, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("completed: intent (2m10s), spec (0m40s)"),
            "got: {text}"
        );

        // No recorded duration for a step (an older schema, or a test
        // fixture that only sets `completed_steps` directly): the bare id,
        // not a fabricated duration.
        let mut legacy = state.clone();
        legacy.step_durations_ms.clear();
        let mut out = Vec::new();
        write_state(&mut out, &legacy, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed: intent, spec"), "got: {text}");
    }

    #[test]
    fn write_state_renders_brainstorm_only_when_the_workflow_has_an_intent_step() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let feature = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let mut out = Vec::new();
        write_state(&mut out, &feature, false).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("brainstorm: on"));

        let review = WorkflowState::start(
            repo.path().to_path_buf(),
            "independent review".into(),
            WorkflowKind::Review,
            None,
            true,
            low_classification(),
        );
        let mut out = Vec::new();
        write_state(&mut out, &review, false).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("brainstorm:"));
    }

    /// `load` is the single choke point every id-resolving verb (`status`,
    /// `resume`, `context`, `artifacts`, `approve`, `advance`) goes through --
    /// pinned here so a bogus id gets the domain-shaped "unknown workflow"
    /// message rather than `std::fs::read_to_string`'s raw OS error ("The
    /// system cannot find the path specified. (os error 3)" on Windows).
    #[test]
    fn load_reports_a_domain_error_for_an_unknown_workflow_id() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let error = load(&state_dir, repo.path(), "does-not-exist")
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown workflow"), "{error}");
        assert!(error.contains("does-not-exist"), "{error}");
        assert!(
            !error.to_ascii_lowercase().contains("os error"),
            "must not leak a raw OS error: {error}"
        );
    }

    #[test]
    fn resume_does_not_redispatch_completed_steps() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state =
            skip_leading_artifact_steps(with_synthetic_intent_step(WorkflowState::start(
                repo.path().to_path_buf(),
                "small feature".into(),
                WorkflowKind::Feature,
                None,
                true,
                low_classification(),
            )));
        save(&state_dir, &state, true).unwrap();
        state =
            advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false).unwrap();
        let resumed = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(resumed.completed_steps, vec!["intent", "implement"]);
        assert_eq!(resumed.current().unwrap().id, "test");
    }

    #[test]
    fn failed_steps_have_a_hard_retry_limit() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        save(&state_dir, &state, true).unwrap();
        for _ in 0..MAX_STEP_ATTEMPTS {
            state = advance_with_evidence(&state_dir, state, StepOutcome::Failure, None, false)
                .unwrap();
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
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
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

        let usage = usage_since(repo.path(), &checkpoint).expect("usage");
        assert_eq!(
            usage,
            crate::commands::ctx::event::TranscriptUsage {
                input_tokens: 7,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 5,
                output_tokens: 3,
            }
        );
        assert_eq!(usage.context_total(), 12, "the pre-2.34.0 combined number");
    }

    /// The sidechain counterpart of `phase_usage_reads_only_the_appended_
    /// claude_transcript`: a subagent turn appended in the same byte range
    /// must be counted, and only that range -- not the whole transcript.
    #[test]
    fn sidechain_usage_since_reads_only_the_appended_sidechain_rows() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555555";
        let path = transcript_path(repo.path(), session_id, "claude").expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":1000,"output_tokens":1000}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let checkpoint = UsageCheckpoint {
            session_id: session_id.into(),
            adapter: "claude".into(),
            transcript_bytes: std::fs::metadata(&path).unwrap().len(),
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"usage":{{"input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":90}}}}}}"#
        )
        .unwrap();

        let usage = sidechain_usage_since(repo.path(), &checkpoint).expect("sidechain usage");
        assert_eq!(
            usage,
            crate::commands::ctx::event::TranscriptUsage {
                input_tokens: 40,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 12_000,
                output_tokens: 90,
            }
        );

        let codex_checkpoint = UsageCheckpoint {
            adapter: "codex".into(),
            ..checkpoint
        };
        assert_eq!(
            sidechain_usage_since(repo.path(), &codex_checkpoint),
            None,
            "sidechain rows are a claude transcript concept, not a general adapter one"
        );
    }

    /// Wiring test for issue #155 Phase 2: a completed phase must attribute
    /// spend to the session that produced it, and bucket subagent spend
    /// separately from the main session's own numbers instead of dropping it.
    #[test]
    fn enrich_transition_evidence_buckets_sidechain_spend_and_records_session_lineage() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555555";
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                crate::commands::ctx::adapters::SESSION_ENV,
                Some(session_id),
            ),
            (crate::commands::ctx::adapters::AGENT_ENV, Some("claude")),
        ]);
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
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
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
        writeln!(
            file,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"usage":{{"input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":90}}}}}}"#
        )
        .unwrap();
        drop(file);

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.usage_checkpoint = Some(checkpoint);

        let evidence = enrich_transition_evidence(&mut state, TransitionEvidence::default());

        assert_eq!(evidence.cache_creation_input_tokens, Some(0));
        assert_eq!(evidence.cache_read_input_tokens, Some(5));
        assert_eq!(
            evidence.sidechain_input_tokens,
            Some(40),
            "subagent spend must be counted rather than dropped"
        );
        assert_eq!(evidence.sidechain_cache_creation_input_tokens, Some(0));
        assert_eq!(evidence.sidechain_cache_read_input_tokens, Some(12_000));
        assert_eq!(evidence.sidechain_output_tokens, Some(90));
        assert_eq!(evidence.session_id.as_deref(), Some(session_id));
    }

    #[test]
    fn switching_steps_replaces_ephemeral_skill_context() {
        let repo = tempdir().unwrap();
        let mut state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
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
    fn substantial_implementation_composes_execute_plan_and_worktree() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill worktree@1"));
        assert!(context.contains("[skill implement@1"));
        assert!(context.contains("[skill execute-plan@1"));
    }

    #[test]
    fn trivial_implementation_does_not_pay_execute_plan_or_worktree_context() {
        let repo = tempdir().unwrap();
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill implement@1"));
        assert!(!context.contains("[skill execute-plan@1"));
        assert!(!context.contains("[skill worktree@1"));
    }

    #[test]
    fn refusal_for_only_fires_for_brainstorm_when_headless() {
        assert_eq!(
            refusal_for("brainstorm", true),
            Some(BRAINSTORM_HEADLESS_REFUSAL)
        );
        assert_eq!(refusal_for("brainstorm", false), None);
        assert_eq!(refusal_for("write-intent", true), None);
    }

    /// Only the exact value `"1"` means headless -- an interactive launch
    /// that inherited the variable set to `"0"`, empty, or anything else
    /// from its own parent process must not be refused.
    #[test]
    fn is_headless_env_requires_the_exact_value_1() {
        assert!(is_headless_env(Some("1")));
        assert!(!is_headless_env(Some("0")));
        assert!(!is_headless_env(Some("")));
        assert!(!is_headless_env(Some("true")));
        assert!(!is_headless_env(None));
    }

    #[test]
    fn a_headless_worker_refuses_the_brainstorm_step() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in to exercise the headless refusal at that step.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        assert_eq!(state.current().unwrap().skill, "brainstorm");
        // SAFETY: nextest runs one test per process.
        unsafe {
            std::env::set_var(crate::commands::ctx::adapters::HEADLESS_ENV, "1");
        }
        let context = render_current_context(&state, repo.path(), None).unwrap();
        unsafe {
            std::env::remove_var(crate::commands::ctx::adapters::HEADLESS_ENV);
        }
        let context = context.unwrap();
        assert!(context.contains(BRAINSTORM_HEADLESS_REFUSAL));
        assert!(!context.contains("Explore the repository"));
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
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            false,
            low_classification(),
        ));
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
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("final disposition"));
    }

    #[test]
    fn close_refuses_with_an_open_review_finding() {
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
        state.review_findings.push(review_finding(
            "finding-1",
            super::super::review::FindingDisposition::Open,
            None,
        ));
        save(&state_dir, &state, true).unwrap();

        let error = close(&state_dir, state, None).unwrap_err();
        assert!(error.to_string().contains("open review finding"), "{error}");
    }

    #[test]
    fn close_refuses_while_awaiting_approval() {
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
        state.status = WorkflowStatus::AwaitingApproval;
        save(&state_dir, &state, true).unwrap();

        let error = close(&state_dir, state, None).unwrap_err();
        assert!(error.to_string().contains("awaiting approval"), "{error}");
    }

    #[test]
    fn close_succeeds_after_residual_dispositions_and_clears_active() {
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
        // Reached MAX_FIX_REVIEW_ROUNDS: every finding has a residual (or
        // otherwise resolved) disposition, none left Open.
        state.review_findings.push(review_finding(
            "finding-1",
            super::super::review::FindingDisposition::Residual,
            None,
        ));
        save(&state_dir, &state, true).unwrap();
        assert!(
            load_active(&state_dir, repo.path())
                .unwrap()
                .is_some_and(|active| active.id == state.id)
        );

        let closed = close(&state_dir, state, Some("hit MAX_FIX_REVIEW_ROUNDS".into())).unwrap();
        assert_eq!(closed.status, WorkflowStatus::Closed);
        assert_eq!(
            closed.closed_reason.as_deref(),
            Some("hit MAX_FIX_REVIEW_ROUNDS")
        );
        assert!(closed.closed_at.is_some());

        assert!(load_active(&state_dir, repo.path()).unwrap().is_none());
        // The state itself is still readable by id, just no longer active.
        let reloaded = load(&state_dir, repo.path(), &closed.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Closed);
    }

    #[test]
    fn close_records_a_telemetry_event() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let workflow_id = state.id.clone();

        close(&state_dir, state, None).unwrap();

        let events = super::super::telemetry::list(&state_dir, repo.path()).unwrap();
        assert!(
            events.iter().any(
                |event| event.kind == super::super::telemetry::TelemetryKind::Closed
                    && event.workflow_id.as_deref() == Some(workflow_id.as_str())
            ),
            "{events:?}"
        );
    }

    #[test]
    fn close_leaves_a_different_active_workflows_pointer_intact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let active_state = WorkflowState::start(
            repo.path().to_path_buf(),
            "active feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let other_state = WorkflowState::start(
            repo.path().to_path_buf(),
            "other feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        // Persist both workflow files, then make sure the active pointer
        // ends up naming `active_state`: write `other_state` first (so its
        // own file exists on disk), then re-save `active_state` as active,
        // which repoints the pointer without touching `other_state`'s file.
        save(&state_dir, &other_state, true).unwrap();
        save(&state_dir, &active_state, true).unwrap();
        assert!(
            load_active(&state_dir, repo.path())
                .unwrap()
                .is_some_and(|state| state.id == active_state.id)
        );

        // `other_state` is Running but is NOT this repo's active workflow.
        let closed = close(&state_dir, other_state.clone(), None).unwrap();
        assert_eq!(closed.status, WorkflowStatus::Closed);

        let active = load_active(&state_dir, repo.path()).unwrap();
        assert!(
            active.is_some_and(|state| state.id == active_state.id),
            "closing a non-active workflow cleared a different workflow's active pointer"
        );
    }

    #[test]
    fn close_refuses_an_already_completed_workflow() {
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
        state.status = WorkflowStatus::Completed;
        save(&state_dir, &state, false).unwrap();

        let error = close(&state_dir, state.clone(), None).unwrap_err();
        assert!(error.to_string().contains("Completed"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Completed);
    }

    #[test]
    fn close_refuses_an_already_failed_workflow() {
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
        state.status = WorkflowStatus::Failed;
        save(&state_dir, &state, false).unwrap();

        let error = close(&state_dir, state.clone(), None).unwrap_err();
        assert!(error.to_string().contains("Failed"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Failed);
    }

    #[test]
    fn close_refuses_an_already_closed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let closed = close(&state_dir, state, None).unwrap();

        let error = close(&state_dir, closed.clone(), None).unwrap_err();
        assert!(error.to_string().contains("Closed"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &closed.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Closed);
    }

    #[test]
    fn resume_refuses_a_closed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let closed = close(&state_dir, state, None).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Resume(StateIdArgs {
                id: closed.id.clone(),
                repo: Some(repo.path().to_path_buf()),
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(
            error.contains("cannot resume") && error.contains("Closed"),
            "{error}"
        );
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
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("independent review"));
    }
}
