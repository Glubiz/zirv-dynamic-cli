//! Privacy-conscious workflow telemetry and aggregate statistics.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;
use serde::{Deserialize, Serialize};

use super::classify::{Complexity, Intent, RiskBand, WorkDomain};
use super::review::FindingDisposition;
use super::skill::WorkflowPhase;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, prune_to_newest, repo_slug, write_private,
};

const TELEMETRY_SCHEMA_VERSION: u32 = 3;
const DEFAULT_MAX_EVENTS: usize = 1000;
const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_CONFIGURED_EVENTS: usize = 100_000;
/// Also reused by `verification.rs`'s report retention, which follows this
/// same config shape and clamp (see the Known Issues entry it closes).
pub(crate) const MAX_CONFIGURED_RETENTION_DAYS: u64 = 3650;
const MAX_LABEL_BYTES: usize = 256;
const MAX_EVENT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub max_events: usize,
    pub retention_days: u64,
}

impl TelemetryConfig {
    /// The operator's `[workflow]` config, with this module's hard caps still
    /// applied on top. Retention/enablement used to come straight from the
    /// process environment, which any repository script could set for itself;
    /// the keys now live in `ctx.toml` and are `REPO_FORBIDDEN`.
    pub fn from_config(cfg: &crate::commands::ctx::config::WorkflowConfig) -> Self {
        Self {
            enabled: cfg.telemetry_enabled,
            max_events: match cfg.telemetry_max_events {
                0 => DEFAULT_MAX_EVENTS,
                value => value.min(MAX_CONFIGURED_EVENTS),
            },
            retention_days: cfg
                .telemetry_retention_days
                .min(MAX_CONFIGURED_RETENTION_DAYS),
        }
    }

    /// Resolved for one repository. A configuration that will not load at all
    /// (a repo setting a forbidden key, say) disables telemetry rather than
    /// falling back to defaults: recording is the optional half here, and the
    /// operator's real intent is unknown at that point.
    pub fn for_repo(repo: &Path) -> Self {
        match crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()) {
            Ok(cfg) => Self::from_config(&cfg.workflow),
            Err(_) => Self {
                enabled: false,
                max_events: DEFAULT_MAX_EVENTS,
                retention_days: DEFAULT_RETENTION_DAYS,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryKind {
    WorkflowStarted,
    PhaseCompleted,
    PhaseFailed,
    VerificationRun,
    ReviewRun,
    ArtifactProduced,
    ArtifactAccepted,
    AgentDispatched,
    WorkflowCompleted,
    FindingUpdated,
    FrontendDetectorRun,
    FrontendRenderRun,
    FrontendVisualReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryEvent {
    pub schema_version: u32,
    pub id: String,
    pub timestamp: u64,
    pub workflow_id: Option<String>,
    pub kind: TelemetryKind,
    pub phase: Option<WorkflowPhase>,
    pub intent: Option<Intent>,
    pub complexity: Option<Complexity>,
    pub risk: Option<RiskBand>,
    #[serde(default)]
    pub work_domain: Option<WorkDomain>,
    pub duration_ms: Option<u64>,
    pub adapter: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub token_usage_source: Option<String>,
    pub succeeded: Option<bool>,
    pub findings_total: u32,
    pub findings_meaningful: u32,
    pub findings_dismissed: u32,
    pub fix_round: u8,
    pub artifact_count: u32,
    pub worker_count: u32,
    /// Issue #155: the raw cache classes alongside `input_tokens`.
    /// `input_tokens` keeps its pre-2.34.0 meaning for events produced by the
    /// workflow engine: the COMBINED context total (raw input plus both cache
    /// classes -- see `engine.rs`'s `usage.context_total()`), not the raw
    /// uncached class alone. `cache_read_input_tokens` is therefore a SUBSET
    /// of `input_tokens`, not a third figure to add to it: a cache-hit ratio
    /// is `cache_read_input_tokens / input_tokens` directly (see
    /// `cache_hit_ratio()` below) -- summing `input_tokens` with the cache
    /// classes double-counts them.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Subagent (`isSidechain`) spend for the same phase, in the same four
    /// classes. Its own bucket rather than folded into the main numbers: the
    /// main numbers mean "this session's own context", and a subagent's
    /// tokens are not part of it -- but they ARE charged to the account, so
    /// dropping them (the pre-2.34.0 behaviour) made a phase look cheaper
    /// than it was.
    #[serde(default)]
    pub sidechain_input_tokens: Option<u64>,
    #[serde(default)]
    pub sidechain_cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub sidechain_cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub sidechain_output_tokens: Option<u64>,
    /// The harness session this event was produced by, its parent (the
    /// session that delegated the work), and the work group both belong to.
    /// `role`/`worker_count` said what KIND of thing ran and how many; these
    /// say WHICH, which is what makes a delegation tree's cost attributable.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub work_group_id: Option<String>,
    /// Accepted workflow work-product stage (intent/spec/plan), when relevant.
    #[serde(default)]
    pub artifact_stage: Option<String>,
    /// Effective deploy tier, populated by deploy-gate events in phase 4.
    #[serde(default)]
    pub deploy_tier: Option<String>,
    /// Provider-neutral agent manifest id, populated by agent dispatch in phase 3.
    #[serde(default)]
    pub agent_id: Option<String>,
}

impl TelemetryEvent {
    pub fn new(kind: TelemetryKind) -> Self {
        Self {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_secs(),
            workflow_id: None,
            kind,
            phase: None,
            intent: None,
            complexity: None,
            risk: None,
            work_domain: None,
            duration_ms: None,
            adapter: None,
            model: None,
            role: None,
            input_tokens: None,
            output_tokens: None,
            token_usage_source: None,
            succeeded: None,
            findings_total: 0,
            findings_meaningful: 0,
            findings_dismissed: 0,
            fix_round: 0,
            artifact_count: 0,
            worker_count: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            sidechain_input_tokens: None,
            sidechain_cache_creation_input_tokens: None,
            sidechain_cache_read_input_tokens: None,
            sidechain_output_tokens: None,
            session_id: None,
            parent_session_id: None,
            work_group_id: None,
            artifact_stage: None,
            deploy_tier: None,
            agent_id: None,
        }
    }

    /// The fraction of `input_tokens` (the combined context total) served
    /// from cache. `cache_read_input_tokens` is a SUBSET of `input_tokens`,
    /// not a separate figure to add to it -- see the doc comment on that
    /// field. `None` when either value is missing, or `input_tokens` is `0`
    /// (no ratio to report, never a manufactured 0%).
    ///
    /// No CLI surface reads a `TelemetryEvent`'s ratio back yet (`usage.rs`'s
    /// own `--sessions` cache-hit line works off `window::SessionSpend`, a
    /// different type) -- this is the one correct formula for whichever
    /// future reporting surface needs it, landed now so it is not
    /// re-derived incorrectly a second time, the same "accessor lands ahead
    /// of its production caller" pattern `log::tail_delegations` used.
    #[allow(dead_code)]
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let read = self.cache_read_input_tokens?;
        let total = self.input_tokens?;
        (total > 0).then(|| read as f64 / total as f64)
    }
}

fn event_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.workflow_telemetry().join(repo_slug(repo))
}

pub fn record(
    state: &StateDir,
    repo: &Path,
    event: &TelemetryEvent,
    config: &TelemetryConfig,
) -> CtxResult<()> {
    if !config.enabled {
        return Ok(());
    }
    let mut event = event.clone();
    for value in [
        &mut event.workflow_id,
        &mut event.adapter,
        &mut event.model,
        &mut event.role,
        &mut event.token_usage_source,
        &mut event.session_id,
        &mut event.parent_session_id,
        &mut event.work_group_id,
        &mut event.artifact_stage,
        &mut event.deploy_tier,
        &mut event.agent_id,
    ]
    .into_iter()
    .flatten()
    {
        *value = crate::utils::truncate_bytes(value.clone(), Some(MAX_LABEL_BYTES));
    }
    let body = serde_json::to_string(&event)?;
    if body.len() > MAX_EVENT_BYTES {
        return Err(format!("workflow telemetry event exceeds {MAX_EVENT_BYTES} bytes").into());
    }
    let dir = event_dir(state, repo);
    create_private_dir_all(&dir)?;
    let path = dir.join(format!("{:020}-{}.json", event.timestamp, event.id));
    write_private(&path, &body)?;
    prune_expired_except(&dir, event.timestamp, config.retention_days, &[]);
    prune_to_newest(&dir, config.max_events);
    Ok(())
}

/// Removes every entry in `dir` whose filename starts with a
/// `{timestamp}-...` prefix older than `now - days`, except any name listed
/// in `keep` (used by `verification.rs` to protect its `latest` report even
/// when that report's own age would otherwise make it eligible). `days == 0`
/// means "keep forever" and is a no-op, matching `telemetry_retention_days`'s
/// own zero-means-unbounded convention.
///
/// Reused as-is by verification report retention (`verification.rs`'s
/// `save_report`) rather than adding a second pruner: both this module's
/// events and verification's reports are one file per record under a
/// per-repository directory, named with a leading zero-padded timestamp, so
/// the same age rule and cutoff math apply unchanged.
pub(crate) fn prune_expired_except(dir: &Path, now: u64, days: u64, keep: &[&str]) {
    if days == 0 {
        return;
    }
    let cutoff = now.saturating_sub(days.saturating_mul(86_400));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if keep.contains(&name) {
            continue;
        }
        let timestamp = name
            .split('-')
            .next()
            .and_then(|value| value.parse::<u64>().ok());
        if timestamp.is_some_and(|timestamp| timestamp < cutoff) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn list(state: &StateDir, repo: &Path) -> CtxResult<Vec<TelemetryEvent>> {
    let dir = event_dir(state, repo);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        // One unreadable event file used to fail the whole `workflow stats`
        // command. Statistics over almost every event beat no statistics.
        let body = match std::fs::read_to_string(entry.path()) {
            Ok(body) => body,
            Err(error) => {
                crate::output::warn(format!(
                    "skipping telemetry event {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        };
        if let Ok(event) = serde_json::from_str::<TelemetryEvent>(&body)
            && event.schema_version == TELEMETRY_SCHEMA_VERSION
        {
            events.push(event);
        }
    }
    events.sort_by_key(|event: &TelemetryEvent| (event.timestamp, event.id.clone()));
    Ok(events)
}

pub fn clear(state: &StateDir, repo: &Path) -> CtxResult<usize> {
    let dir = event_dir(state, repo);
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.parent() == Some(dir.as_path())
            && path.extension().and_then(|value| value.to_str()) == Some("json")
        {
            std::fs::remove_file(path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PhaseStats {
    pub events: usize,
    pub token_events: usize,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failures: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AdapterStats {
    pub events: usize,
    pub token_events: usize,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failures: usize,
}

/// Per-`workflow_id` breakdown (issue #155's "each shipped item showing its
/// measured reduction" acceptance hook): how many independent `ReviewRun`
/// rounds a workflow went through, its findings totals -- `findings_total`
/// is every finding *reported* by a reviewer, `findings_meaningful` is the
/// subset that is Major/Critical and not dismissed, i.e. *confirmed* -- and
/// the token totals attributed to it. Findings come from the same
/// latest-snapshot-per-workflow logic `aggregate`'s overall counters already
/// used, so this never double-counts across phases the way summing every
/// event's findings fields would.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkflowStats {
    pub review_runs: usize,
    pub findings_total: u32,
    pub findings_meaningful: u32,
    pub findings_dismissed: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub token_events: usize,
    /// Confirmed (`findings_meaningful`) findings per `ReviewRun` round --
    /// this workflow's own defect-rate accounting hook. `None` when this
    /// workflow has recorded no `ReviewRun` event, never a manufactured 0.
    pub confirmed_findings_per_review: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsReport {
    pub events: usize,
    pub phases: BTreeMap<String, PhaseStats>,
    pub adapters: BTreeMap<String, AdapterStats>,
    pub token_sources: BTreeMap<String, usize>,
    pub slowest_phase: Option<String>,
    pub most_token_expensive_phase: Option<String>,
    pub verification_runs: usize,
    pub verification_failures: usize,
    pub findings_total: u64,
    pub findings_meaningful: u64,
    pub findings_dismissed: u64,
    pub frontend_detector_runs: usize,
    pub frontend_detector_failures: usize,
    pub frontend_render_runs: usize,
    pub frontend_render_failures: usize,
    pub frontend_visual_reviews: usize,
    pub frontend_visual_review_failures: usize,
    /// Every workflow id seen in this repository's telemetry, keyed to its
    /// own `WorkflowStats` -- the per-workflow / per-`ReviewRun` breakdown
    /// `docs/benchmarks/token-cost.md` §1.3 previously had no CLI for.
    pub workflows: BTreeMap<String, WorkflowStats>,
    pub review_runs: usize,
    /// Confirmed findings per review round, across every workflow combined
    /// (`findings_meaningful` / `review_runs`). `None` when no `ReviewRun`
    /// event has been recorded at all -- the "no regression in
    /// review-confirmed defect rates" accounting hook issue #155 asks for.
    pub review_defect_rate: Option<f64>,
}

pub fn aggregate(events: &[TelemetryEvent]) -> StatsReport {
    let mut phases: BTreeMap<String, PhaseStats> = BTreeMap::new();
    let mut adapters: BTreeMap<String, AdapterStats> = BTreeMap::new();
    let mut token_sources: BTreeMap<String, usize> = BTreeMap::new();
    let mut verification_runs = 0usize;
    let mut verification_failures = 0usize;
    let mut frontend_detector_runs = 0usize;
    let mut frontend_detector_failures = 0usize;
    let mut frontend_render_runs = 0usize;
    let mut frontend_render_failures = 0usize;
    let mut frontend_visual_reviews = 0usize;
    let mut frontend_visual_review_failures = 0usize;
    let mut finding_snapshots: BTreeMap<String, (u64, String, u32, u32, u32)> = BTreeMap::new();
    let mut workflows: BTreeMap<String, WorkflowStats> = BTreeMap::new();
    let mut review_runs = 0usize;
    for event in events {
        if let Some(phase) = event.phase {
            let entry = phases.entry(phase.to_string()).or_default();
            entry.events += 1;
            entry.duration_ms = entry
                .duration_ms
                .saturating_add(event.duration_ms.unwrap_or(0));
            entry.input_tokens = entry
                .input_tokens
                .saturating_add(event.input_tokens.unwrap_or(0));
            entry.output_tokens = entry
                .output_tokens
                .saturating_add(event.output_tokens.unwrap_or(0));
            if event.input_tokens.is_some() || event.output_tokens.is_some() {
                entry.token_events += 1;
            }
            if event.succeeded == Some(false) {
                entry.failures += 1;
            }
        }
        if let Some(adapter) = &event.adapter {
            let entry = adapters.entry(adapter.clone()).or_default();
            entry.events += 1;
            entry.duration_ms = entry
                .duration_ms
                .saturating_add(event.duration_ms.unwrap_or(0));
            entry.input_tokens = entry
                .input_tokens
                .saturating_add(event.input_tokens.unwrap_or(0));
            entry.output_tokens = entry
                .output_tokens
                .saturating_add(event.output_tokens.unwrap_or(0));
            if event.input_tokens.is_some() || event.output_tokens.is_some() {
                entry.token_events += 1;
            }
            if event.succeeded == Some(false) {
                entry.failures += 1;
            }
        }
        if let Some(source) = &event.token_usage_source {
            *token_sources.entry(source.clone()).or_default() += 1;
        }
        if event.kind == TelemetryKind::VerificationRun {
            verification_runs += 1;
            if event.succeeded == Some(false) {
                verification_failures += 1;
            }
        }
        if event.kind == TelemetryKind::ReviewRun {
            review_runs += 1;
        }
        if let Some(workflow_id) = &event.workflow_id {
            let entry = workflows.entry(workflow_id.clone()).or_default();
            if event.kind == TelemetryKind::ReviewRun {
                entry.review_runs += 1;
            }
            entry.input_tokens = entry
                .input_tokens
                .saturating_add(event.input_tokens.unwrap_or(0));
            entry.output_tokens = entry
                .output_tokens
                .saturating_add(event.output_tokens.unwrap_or(0));
            if event.input_tokens.is_some() || event.output_tokens.is_some() {
                entry.token_events += 1;
            }
        }
        match event.kind {
            TelemetryKind::FrontendDetectorRun => {
                frontend_detector_runs += 1;
                if event.succeeded == Some(false) {
                    frontend_detector_failures += 1;
                }
            }
            TelemetryKind::FrontendRenderRun => {
                frontend_render_runs += 1;
                if event.succeeded == Some(false) {
                    frontend_render_failures += 1;
                }
            }
            TelemetryKind::FrontendVisualReview => {
                frontend_visual_reviews += 1;
                if event.succeeded == Some(false) {
                    frontend_visual_review_failures += 1;
                }
            }
            _ => {}
        }
        if let Some(workflow_id) = &event.workflow_id
            && matches!(
                event.kind,
                TelemetryKind::PhaseCompleted
                    | TelemetryKind::PhaseFailed
                    | TelemetryKind::ReviewRun
                    | TelemetryKind::WorkflowCompleted
                    | TelemetryKind::FindingUpdated
            )
        {
            let replacement = (
                event.timestamp,
                event.id.clone(),
                event.findings_total,
                event.findings_meaningful,
                event.findings_dismissed,
            );
            let replace = finding_snapshots
                .get(workflow_id)
                .is_none_or(|current| (replacement.0, &replacement.1) > (current.0, &current.1));
            if replace {
                finding_snapshots.insert(workflow_id.clone(), replacement);
            }
        }
    }
    let findings_total = finding_snapshots
        .values()
        .map(|snapshot| u64::from(snapshot.2))
        .sum();
    let findings_meaningful = finding_snapshots
        .values()
        .map(|snapshot| u64::from(snapshot.3))
        .sum();
    let findings_dismissed = finding_snapshots
        .values()
        .map(|snapshot| u64::from(snapshot.4))
        .sum();
    for (workflow_id, snapshot) in &finding_snapshots {
        let entry = workflows.entry(workflow_id.clone()).or_default();
        entry.findings_total = snapshot.2;
        entry.findings_meaningful = snapshot.3;
        entry.findings_dismissed = snapshot.4;
    }
    for stats in workflows.values_mut() {
        stats.confirmed_findings_per_review = (stats.review_runs > 0)
            .then(|| f64::from(stats.findings_meaningful) / stats.review_runs as f64);
    }
    let review_defect_rate =
        (review_runs > 0).then(|| findings_meaningful as f64 / review_runs as f64);
    let slowest_phase = phases
        .iter()
        .max_by_key(|(_, stats)| stats.duration_ms)
        .map(|(phase, _)| phase.clone());
    let most_token_expensive_phase = phases
        .iter()
        .max_by_key(|(_, stats)| stats.input_tokens.saturating_add(stats.output_tokens))
        .filter(|(_, stats)| stats.input_tokens > 0 || stats.output_tokens > 0)
        .map(|(phase, _)| phase.clone());
    StatsReport {
        events: events.len(),
        phases,
        adapters,
        token_sources,
        slowest_phase,
        most_token_expensive_phase,
        verification_runs,
        verification_failures,
        findings_total,
        findings_meaningful,
        findings_dismissed,
        frontend_detector_runs,
        frontend_detector_failures,
        frontend_render_runs,
        frontend_render_failures,
        frontend_visual_reviews,
        frontend_visual_review_failures,
        workflows,
        review_runs,
        review_defect_rate,
    }
}

pub fn finding_counts(findings: &[super::review::ReviewFinding]) -> (u32, u32, u32) {
    let total = u32::try_from(findings.len()).unwrap_or(u32::MAX);
    let meaningful = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.severity,
                super::review::FindingSeverity::Major | super::review::FindingSeverity::Critical
            ) && finding.disposition != FindingDisposition::Dismissed
        })
        .count();
    let dismissed = findings
        .iter()
        .filter(|finding| finding.disposition == FindingDisposition::Dismissed)
        .count();
    (
        total,
        u32::try_from(meaningful).unwrap_or(u32::MAX),
        u32::try_from(dismissed).unwrap_or(u32::MAX),
    )
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
    /// Remove all local telemetry events for this repository.
    #[arg(long)]
    pub clear: bool,
}

pub fn run_stats(args: &StatsArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = match args.repo.as_deref() {
        Some(repo) => repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()),
        None => std::env::current_dir()?,
    };
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    if args.clear {
        let removed = clear(&state, &repo)?;
        writeln!(writer, "cleared {removed} workflow telemetry events")?;
        return Ok(0);
    }
    let report = aggregate(&list(&state, &repo)?);
    if args.json {
        serde_json::to_writer_pretty(&mut *writer, &report)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "events: {}", report.events)?;
        for (phase, stats) in &report.phases {
            writeln!(
                writer,
                "{phase}: {} events, {} ms, {} tokens ({} measured events), {} failures",
                stats.events,
                stats.duration_ms,
                stats.input_tokens.saturating_add(stats.output_tokens),
                stats.token_events,
                stats.failures
            )?;
        }
        for (adapter, stats) in &report.adapters {
            writeln!(
                writer,
                "adapter {adapter}: {} events, {} ms, {} tokens ({} measured events), {} failures",
                stats.events,
                stats.duration_ms,
                stats.input_tokens.saturating_add(stats.output_tokens),
                stats.token_events,
                stats.failures
            )?;
        }
        if !report.token_sources.is_empty() {
            writeln!(
                writer,
                "token sources: {}",
                report
                    .token_sources
                    .iter()
                    .map(|(source, events)| format!("{source}={events}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
        writeln!(
            writer,
            "slowest phase: {}",
            report.slowest_phase.as_deref().unwrap_or("unknown")
        )?;
        writeln!(
            writer,
            "most token-expensive phase: {}",
            report
                .most_token_expensive_phase
                .as_deref()
                .unwrap_or("unknown")
        )?;
        writeln!(
            writer,
            "verification: {} runs, {} failures",
            report.verification_runs, report.verification_failures
        )?;
        writeln!(
            writer,
            "findings: {} total, {} meaningful, {} dismissed",
            report.findings_total, report.findings_meaningful, report.findings_dismissed
        )?;
        writeln!(
            writer,
            "review-confirmed defect rate: {}",
            report
                .review_defect_rate
                .map(|rate| format!(
                    "{rate:.2} confirmed findings/review round ({} review runs)",
                    report.review_runs
                ))
                .unwrap_or_else(|| "no ReviewRun events recorded yet".to_string())
        )?;
        if !report.workflows.is_empty() {
            writeln!(writer, "workflows:")?;
            for (workflow_id, stats) in &report.workflows {
                writeln!(
                    writer,
                    "  {workflow_id}: {} review runs, findings {} total/{} confirmed/{} dismissed, \
                     {} tokens ({} measured events), {}",
                    stats.review_runs,
                    stats.findings_total,
                    stats.findings_meaningful,
                    stats.findings_dismissed,
                    stats.input_tokens.saturating_add(stats.output_tokens),
                    stats.token_events,
                    stats
                        .confirmed_findings_per_review
                        .map(|rate| format!("{rate:.2} confirmed/review"))
                        .unwrap_or_else(|| "no review runs".to_string())
                )?;
            }
        }
        writeln!(
            writer,
            "frontend: detector {} runs/{} failures, render {} runs/{} failures, visual review {} runs/{} failures",
            report.frontend_detector_runs,
            report.frontend_detector_failures,
            report.frontend_render_runs,
            report.frontend_render_failures,
            report.frontend_visual_reviews,
            report.frontend_visual_review_failures
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn telemetry_schema_has_no_prompt_source_or_response_fields() {
        let json =
            serde_json::to_value(TelemetryEvent::new(TelemetryKind::PhaseCompleted)).unwrap();
        let object = json.as_object().unwrap();
        for forbidden in ["prompt", "source_code", "response", "diff", "output"] {
            assert!(!object.contains_key(forbidden));
        }
    }

    #[test]
    fn disabled_telemetry_writes_nothing() {
        let root = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        record(
            &state,
            repo.path(),
            &TelemetryEvent::new(TelemetryKind::WorkflowStarted),
            &TelemetryConfig {
                enabled: false,
                max_events: 10,
                retention_days: 1,
            },
        )
        .unwrap();
        assert!(list(&state, repo.path()).unwrap().is_empty());
    }

    /// The old reader was `std::env::var(..).and_then(|v| v.parse().ok())
    /// .unwrap_or(true)`, so `0` -- a perfectly ordinary way to write "off" --
    /// failed to parse and *enabled* telemetry. A privacy opt-out has to fail
    /// closed in both spellings.
    #[test]
    fn a_zero_or_false_opt_out_actually_disables_telemetry() {
        use crate::commands::ctx::config::CtxConfig;
        // Hermetic: an empty temp repo and an empty temp home, so neither this
        // machine's own `.zirv/ctx.toml` can decide the answer.
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let _env = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let load = |value: &str| {
            CtxConfig::load(repo.path(), &|key| {
                (key == "ZIRV_CTX_WORKFLOW_TELEMETRY").then(|| value.to_string())
            })
        };
        for value in ["0", "false"] {
            let cfg = load(value).expect("config loads");
            assert!(
                !TelemetryConfig::from_config(&cfg.workflow).enabled,
                "'{value}' must disable telemetry"
            );
        }
        for value in ["1", "true"] {
            let cfg = load(value).expect("config loads");
            assert!(TelemetryConfig::from_config(&cfg.workflow).enabled);
        }
        let error = load("maybe").expect_err("an unparseable value is loud, not a guess");
        assert!(error.to_string().contains("true or false"));
    }

    #[test]
    fn retention_and_event_caps_still_bound_an_operator_value() {
        let cfg = crate::commands::ctx::config::WorkflowConfig {
            telemetry_max_events: MAX_CONFIGURED_EVENTS * 4,
            telemetry_retention_days: MAX_CONFIGURED_RETENTION_DAYS * 4,
            ..Default::default()
        };
        let resolved = TelemetryConfig::from_config(&cfg);
        assert_eq!(resolved.max_events, MAX_CONFIGURED_EVENTS);
        assert_eq!(resolved.retention_days, MAX_CONFIGURED_RETENTION_DAYS);
    }

    #[test]
    fn aggregate_identifies_slowest_and_token_expensive_phases() {
        let mut implement = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        implement.phase = Some(WorkflowPhase::Implement);
        implement.duration_ms = Some(500);
        implement.input_tokens = Some(10);
        implement.token_usage_source = Some("harness-transcript-delta".into());
        implement.adapter = Some("claude".into());
        let mut review = TelemetryEvent::new(TelemetryKind::ReviewRun);
        review.phase = Some(WorkflowPhase::Review);
        review.duration_ms = Some(100);
        review.input_tokens = Some(1000);
        review.adapter = Some("codex".into());
        let stats = aggregate(&[implement, review]);
        assert_eq!(stats.slowest_phase.as_deref(), Some("implement"));
        assert_eq!(stats.most_token_expensive_phase.as_deref(), Some("review"));
        assert_eq!(stats.adapters["claude"].duration_ms, 500);
        assert_eq!(stats.adapters["codex"].input_tokens, 1000);
        assert_eq!(stats.phases["implement"].token_events, 1);
        assert_eq!(stats.adapters["codex"].token_events, 1);
        assert_eq!(stats.token_sources["harness-transcript-delta"], 1);
    }

    #[test]
    fn frontend_runs_are_aggregated_without_storing_ui_or_model_output() {
        let mut detector = TelemetryEvent::new(TelemetryKind::FrontendDetectorRun);
        detector.work_domain = Some(WorkDomain::Frontend);
        detector.succeeded = Some(false);
        detector.findings_total = 2;
        let mut render = TelemetryEvent::new(TelemetryKind::FrontendRenderRun);
        render.work_domain = Some(WorkDomain::Frontend);
        render.succeeded = Some(true);
        render.artifact_count = 2;
        let mut review = TelemetryEvent::new(TelemetryKind::FrontendVisualReview);
        review.work_domain = Some(WorkDomain::Frontend);
        review.succeeded = Some(true);

        let stats = aggregate(&[detector, render, review]);

        assert_eq!(stats.frontend_detector_runs, 1);
        assert_eq!(stats.frontend_detector_failures, 1);
        assert_eq!(stats.frontend_render_runs, 1);
        assert_eq!(stats.frontend_visual_reviews, 1);
    }

    #[test]
    fn finding_totals_use_the_latest_snapshot_instead_of_double_counting_phases() {
        let mut first = TelemetryEvent::new(TelemetryKind::PhaseFailed);
        first.id = "a".into();
        first.timestamp = 1;
        first.workflow_id = Some("workflow".into());
        first.findings_total = 2;
        first.findings_meaningful = 2;
        let mut final_state = TelemetryEvent::new(TelemetryKind::FindingUpdated);
        final_state.id = "b".into();
        final_state.timestamp = 2;
        final_state.workflow_id = Some("workflow".into());
        final_state.findings_total = 2;
        final_state.findings_meaningful = 1;
        final_state.findings_dismissed = 1;
        let stats = aggregate(&[first, final_state]);
        assert_eq!(stats.findings_total, 2);
        assert_eq!(stats.findings_meaningful, 1);
        assert_eq!(stats.findings_dismissed, 1);
    }

    /// Issue #155, Phase 2: an event must carry enough to attribute spend to
    /// a session, its parent, and its work group -- and to separate the cache
    /// classes. `role` was a free string and `worker_count` a bare integer;
    /// neither could say WHICH worker cost what.
    #[test]
    fn a_telemetry_event_carries_raw_categories_and_session_lineage() {
        let mut event = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        // `input_tokens` is the COMBINED total (raw input + both cache
        // classes -- 1_000 + 8_000 + 91_000), not the raw uncached class
        // alone: `cache_read_input_tokens` is a subset of it.
        event.input_tokens = Some(100_000);
        event.cache_creation_input_tokens = Some(8_000);
        event.cache_read_input_tokens = Some(91_000);
        event.output_tokens = Some(500);
        event.sidechain_input_tokens = Some(40);
        event.sidechain_cache_creation_input_tokens = Some(0);
        event.sidechain_cache_read_input_tokens = Some(12_000);
        event.sidechain_output_tokens = Some(90);
        event.session_id = Some("sess-child".to_string());
        event.parent_session_id = Some("sess-parent".to_string());
        event.work_group_id = Some("wg-1".to_string());

        let json = serde_json::to_string(&event).expect("serialize");
        let back: TelemetryEvent = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, event);

        assert!(
            (back.cache_hit_ratio().expect("both values present") - 0.91).abs() < 1e-9,
            "a cache-hit ratio must be computable from ONE event, dividing by the \
             combined total rather than double-counting the cache classes"
        );
    }

    #[test]
    fn cache_hit_ratio_is_none_without_data_never_a_manufactured_zero() {
        let mut event = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        assert_eq!(event.cache_hit_ratio(), None, "no data at all");

        event.input_tokens = Some(0);
        event.cache_read_input_tokens = Some(0);
        assert_eq!(
            event.cache_hit_ratio(),
            None,
            "a zero total has no ratio to report"
        );
    }

    /// Back-compatibility: an event written by 2.31.0 has none of these
    /// fields. Reading it must still work -- the telemetry directory is
    /// retained for days and an upgrade must not orphan it.
    #[test]
    fn an_event_written_before_this_change_still_deserialises() {
        let old = r#"{"schema_version":1,"id":"e1","timestamp":10,"workflow_id":null,
            "kind":"phase-completed","phase":null,"intent":null,"complexity":null,
            "risk":null,"duration_ms":null,"adapter":null,"model":null,"role":null,
            "input_tokens":7,"output_tokens":3,"succeeded":true,"findings_total":0,
            "findings_meaningful":0,"findings_dismissed":0,"fix_round":0,
            "artifact_count":0,"worker_count":0}"#;
        let event: TelemetryEvent = serde_json::from_str(old).expect("old events still parse");
        assert_eq!(event.input_tokens, Some(7));
        assert_eq!(event.cache_read_input_tokens, None);
        assert_eq!(event.session_id, None);
    }

    /// Issue #155 measurement closeout: `zirv workflow stats` previously had
    /// no per-workflow / per-`ReviewRun` breakdown at all (§1.3 of
    /// `docs/benchmarks/token-cost.md` called this out by name). Two
    /// workflows, each with tokens and review rounds, must stay separated
    /// rather than folded into the flat phase/adapter totals.
    #[test]
    fn aggregate_breaks_down_tokens_and_review_runs_per_workflow() {
        // Explicit, ordered timestamps/ids on every event: `PhaseCompleted`
        // events participate in the same latest-snapshot-per-workflow finding
        // logic `ReviewRun` events do (both are in `aggregate`'s snapshot
        // match arm), so leaving `phase_a`/`phase_b` on `TelemetryEvent::new`'s
        // real-wall-clock default timestamp would make them look newer than
        // every deliberately-small `review_a1`/`review_a2` timestamp below and
        // silently win the "latest" comparison.
        let mut phase_a = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        phase_a.workflow_id = Some("wf-a".into());
        phase_a.phase = Some(WorkflowPhase::Implement);
        phase_a.input_tokens = Some(1_000);
        phase_a.output_tokens = Some(200);
        phase_a.timestamp = 0;
        phase_a.id = "p-a".into();

        let mut review_a1 = TelemetryEvent::new(TelemetryKind::ReviewRun);
        review_a1.workflow_id = Some("wf-a".into());
        review_a1.timestamp = 1;
        review_a1.id = "a1".into();
        review_a1.findings_total = 3;
        review_a1.findings_meaningful = 2;
        review_a1.findings_dismissed = 1;

        let mut review_a2 = TelemetryEvent::new(TelemetryKind::ReviewRun);
        review_a2.workflow_id = Some("wf-a".into());
        review_a2.timestamp = 2;
        review_a2.id = "a2".into();
        review_a2.findings_total = 1;
        review_a2.findings_meaningful = 0;
        review_a2.findings_dismissed = 1;

        let mut phase_b = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        phase_b.workflow_id = Some("wf-b".into());
        phase_b.phase = Some(WorkflowPhase::Implement);
        phase_b.input_tokens = Some(50);
        phase_b.output_tokens = Some(10);
        phase_b.timestamp = 0;
        phase_b.id = "p-b".into();

        let stats = aggregate(&[phase_a, review_a1, review_a2, phase_b]);

        assert_eq!(stats.review_runs, 2);
        assert_eq!(stats.workflows.len(), 2);

        let wf_a = &stats.workflows["wf-a"];
        assert_eq!(wf_a.review_runs, 2);
        assert_eq!(wf_a.input_tokens, 1_000);
        assert_eq!(wf_a.output_tokens, 200);
        assert_eq!(wf_a.token_events, 1);
        // Latest snapshot by (timestamp, id) wins, same rule as the overall
        // finding counters -- review_a2 (timestamp 2) is newest.
        assert_eq!(wf_a.findings_total, 1);
        assert_eq!(wf_a.findings_meaningful, 0);
        assert_eq!(wf_a.findings_dismissed, 1);
        assert_eq!(wf_a.confirmed_findings_per_review, Some(0.0));

        let wf_b = &stats.workflows["wf-b"];
        assert_eq!(wf_b.review_runs, 0);
        assert_eq!(wf_b.input_tokens, 50);
        assert_eq!(wf_b.confirmed_findings_per_review, None);

        // Combined across both workflows: 0 confirmed findings / 2 review runs.
        assert_eq!(stats.review_defect_rate, Some(0.0));
    }

    #[test]
    fn review_defect_rate_is_none_without_any_review_run() {
        let mut phase = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        phase.workflow_id = Some("wf-a".into());
        let stats = aggregate(&[phase]);
        assert_eq!(stats.review_runs, 0);
        assert_eq!(stats.review_defect_rate, None);
    }

    #[test]
    fn free_form_telemetry_labels_are_bounded_before_storage() {
        let root = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let mut event = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        event.model = Some("x".repeat(MAX_LABEL_BYTES * 4));
        record(
            &state,
            repo.path(),
            &event,
            &TelemetryConfig {
                enabled: true,
                max_events: 10,
                retention_days: 1,
            },
        )
        .unwrap();
        let stored = list(&state, repo.path()).unwrap();
        assert_eq!(stored[0].model.as_deref().unwrap().len(), MAX_LABEL_BYTES);
    }
}
