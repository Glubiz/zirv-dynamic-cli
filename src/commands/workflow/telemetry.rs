//! Privacy-conscious workflow telemetry and aggregate statistics.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;
use serde::{Deserialize, Serialize};

use super::classify::{Complexity, Intent, RiskBand};
use super::review::FindingDisposition;
use super::skill::WorkflowPhase;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, prune_to_newest, repo_slug, write_private,
};

const TELEMETRY_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_EVENTS: usize = 1000;
const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_CONFIGURED_EVENTS: usize = 100_000;
const MAX_CONFIGURED_RETENTION_DAYS: u64 = 3650;
const MAX_LABEL_BYTES: usize = 256;
const MAX_EVENT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub max_events: usize,
    pub retention_days: u64,
}

impl TelemetryConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("ZIRV_WORKFLOW_TELEMETRY")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(true);
        let max_events = std::env::var("ZIRV_WORKFLOW_TELEMETRY_MAX_EVENTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .map(|value: usize| value.min(MAX_CONFIGURED_EVENTS))
            .unwrap_or(DEFAULT_MAX_EVENTS);
        let retention_days = std::env::var("ZIRV_WORKFLOW_TELEMETRY_RETENTION_DAYS")
            .ok()
            .and_then(|value| value.parse().ok())
            .map(|value: u64| value.min(MAX_CONFIGURED_RETENTION_DAYS))
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        Self {
            enabled,
            max_events,
            retention_days,
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
    WorkflowCompleted,
    FindingUpdated,
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
    pub duration_ms: Option<u64>,
    pub adapter: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub succeeded: Option<bool>,
    pub findings_total: u32,
    pub findings_meaningful: u32,
    pub findings_dismissed: u32,
    pub fix_round: u8,
    pub artifact_count: u32,
    pub worker_count: u32,
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
            duration_ms: None,
            adapter: None,
            model: None,
            role: None,
            input_tokens: None,
            output_tokens: None,
            succeeded: None,
            findings_total: 0,
            findings_meaningful: 0,
            findings_dismissed: 0,
            fix_round: 0,
            artifact_count: 0,
            worker_count: 0,
        }
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
    ] {
        if let Some(value) = value {
            *value = crate::utils::truncate_bytes(value.clone(), Some(MAX_LABEL_BYTES));
        }
    }
    let body = serde_json::to_string(&event)?;
    if body.len() > MAX_EVENT_BYTES {
        return Err(format!("workflow telemetry event exceeds {MAX_EVENT_BYTES} bytes").into());
    }
    let dir = event_dir(state, repo);
    create_private_dir_all(&dir)?;
    let path = dir.join(format!("{:020}-{}.json", event.timestamp, event.id));
    write_private(&path, &body)?;
    prune_expired(&dir, event.timestamp, config.retention_days);
    prune_to_newest(&dir, config.max_events);
    Ok(())
}

fn prune_expired(dir: &Path, now: u64, days: u64) {
    if days == 0 {
        return;
    }
    let cutoff = now.saturating_sub(days.saturating_mul(86_400));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let timestamp = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split('-').next())
            .and_then(|value| value.parse::<u64>().ok());
        if timestamp.is_some_and(|timestamp| timestamp < cutoff) {
            let _ = std::fs::remove_file(path);
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
        if let Ok(event) = serde_json::from_str::<TelemetryEvent>(&std::fs::read_to_string(entry.path())?)
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
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failures: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AdapterStats {
    pub events: usize,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failures: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsReport {
    pub events: usize,
    pub phases: BTreeMap<String, PhaseStats>,
    pub adapters: BTreeMap<String, AdapterStats>,
    pub slowest_phase: Option<String>,
    pub most_token_expensive_phase: Option<String>,
    pub verification_runs: usize,
    pub verification_failures: usize,
    pub findings_total: u64,
    pub findings_meaningful: u64,
    pub findings_dismissed: u64,
}

pub fn aggregate(events: &[TelemetryEvent]) -> StatsReport {
    let mut phases: BTreeMap<String, PhaseStats> = BTreeMap::new();
    let mut adapters: BTreeMap<String, AdapterStats> = BTreeMap::new();
    let mut verification_runs = 0usize;
    let mut verification_failures = 0usize;
    let mut finding_snapshots: BTreeMap<String, (u64, String, u32, u32, u32)> =
        BTreeMap::new();
    for event in events {
        if let Some(phase) = event.phase {
            let entry = phases.entry(phase.to_string()).or_default();
            entry.events += 1;
            entry.duration_ms = entry.duration_ms.saturating_add(event.duration_ms.unwrap_or(0));
            entry.input_tokens = entry.input_tokens.saturating_add(event.input_tokens.unwrap_or(0));
            entry.output_tokens = entry.output_tokens.saturating_add(event.output_tokens.unwrap_or(0));
            if event.succeeded == Some(false) {
                entry.failures += 1;
            }
        }
        if let Some(adapter) = &event.adapter {
            let entry = adapters.entry(adapter.clone()).or_default();
            entry.events += 1;
            entry.duration_ms = entry.duration_ms.saturating_add(event.duration_ms.unwrap_or(0));
            entry.input_tokens = entry.input_tokens.saturating_add(event.input_tokens.unwrap_or(0));
            entry.output_tokens = entry.output_tokens.saturating_add(event.output_tokens.unwrap_or(0));
            if event.succeeded == Some(false) {
                entry.failures += 1;
            }
        }
        if event.kind == TelemetryKind::VerificationRun {
            verification_runs += 1;
            if event.succeeded == Some(false) {
                verification_failures += 1;
            }
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
        slowest_phase,
        most_token_expensive_phase,
        verification_runs,
        verification_failures,
        findings_total,
        findings_meaningful,
        findings_dismissed,
    }
}

pub fn finding_counts(
    findings: &[super::review::ReviewFinding],
) -> (u32, u32, u32) {
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
                "{phase}: {} events, {} ms, {} tokens, {} failures",
                stats.events,
                stats.duration_ms,
                stats.input_tokens.saturating_add(stats.output_tokens),
                stats.failures
            )?;
        }
        for (adapter, stats) in &report.adapters {
            writeln!(
                writer,
                "adapter {adapter}: {} events, {} ms, {} tokens, {} failures",
                stats.events,
                stats.duration_ms,
                stats.input_tokens.saturating_add(stats.output_tokens),
                stats.failures
            )?;
        }
        writeln!(writer, "slowest phase: {}", report.slowest_phase.as_deref().unwrap_or("unknown"))?;
        writeln!(writer, "most token-expensive phase: {}", report.most_token_expensive_phase.as_deref().unwrap_or("unknown"))?;
        writeln!(writer, "verification: {} runs, {} failures", report.verification_runs, report.verification_failures)?;
        writeln!(writer, "findings: {} total, {} meaningful, {} dismissed", report.findings_total, report.findings_meaningful, report.findings_dismissed)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn telemetry_schema_has_no_prompt_source_or_response_fields() {
        let json = serde_json::to_value(TelemetryEvent::new(TelemetryKind::PhaseCompleted)).unwrap();
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

    #[test]
    fn aggregate_identifies_slowest_and_token_expensive_phases() {
        let mut implement = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        implement.phase = Some(WorkflowPhase::Implement);
        implement.duration_ms = Some(500);
        implement.input_tokens = Some(10);
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
