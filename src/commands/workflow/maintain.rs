//! Invoked deterministic maintenance scans that re-enter the workflow at Intent.
//!
//! Detector commands are read only from operator-controlled ctx config. No LLM
//! decides whether a breach occurred: exit status, timeout, and line-count
//! thresholds are the entire detector language.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::classify::{ClassificationInput, Complexity, Intent, RiskBand};
use super::engine::{self, WorkflowKind, WorkflowState, WorkflowStatus};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::config::{
    CtxConfig, MaintainDetectorConfig, MaintainDetectorMode,
};
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private, write_shared,
};

const MAX_DETECTOR_COMMAND_BYTES: usize = 8 * 1024;
const MAX_DETECTOR_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Args)]
pub struct MaintainArgs {
    #[command(subcommand)]
    pub command: MaintainCommand,
}

#[derive(Debug, Subcommand)]
pub enum MaintainCommand {
    /// Run every operator-configured deterministic detector once.
    Scan(ScanArgs),
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DetectorResult {
    id: String,
    mode: MaintainDetectorMode,
    threshold: u64,
    exit_code: Option<i32>,
    timed_out: bool,
    output_lines: u64,
    output_bytes: u64,
    breach: bool,
    workflow_id: Option<String>,
    issue_url: Option<String>,
    report_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ScanReport {
    repository: PathBuf,
    detectors: usize,
    breaches: usize,
    results: Vec<DetectorResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IncidentMarker {
    workflow_id: String,
    issue_url: Option<String>,
    created_at: u64,
}

fn valid_detector_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

fn validate_detector(id: &str, detector: &MaintainDetectorConfig) -> CtxResult<()> {
    if !valid_detector_id(id) {
        return Err(format!(
            "maintain detector id '{id}' must match [a-z0-9_-] and be at most 96 bytes"
        )
        .into());
    }
    if detector.command.trim().is_empty() || detector.command.len() > MAX_DETECTOR_COMMAND_BYTES {
        return Err(format!(
            "maintain detector '{id}' command must be in 1..={MAX_DETECTOR_COMMAND_BYTES} bytes"
        )
        .into());
    }
    if detector.mode == MaintainDetectorMode::LineCount && detector.threshold == 0 {
        return Err(format!("maintain detector '{id}' line-count threshold must be at least 1").into());
    }
    Ok(())
}

fn command_for_shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut value = Command::new("cmd");
        value.args(["/D", "/S", "/C", command]);
        value
    }
    #[cfg(not(windows))]
    {
        let mut value = Command::new("sh");
        value.args(["-c", command]);
        value
    }
}

fn count_stream(mut reader: impl Read) -> (u64, u64) {
    let mut lines = 0u64;
    let mut bytes = 0u64;
    let mut saw_any = false;
    let mut ended_with_newline = true;
    let mut chunk = [0u8; 8192];
    loop {
        let Ok(count) = reader.read(&mut chunk) else {
            break;
        };
        if count == 0 {
            break;
        }
        saw_any = true;
        bytes = bytes.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        lines = lines.saturating_add(
            u64::try_from(chunk[..count].iter().filter(|byte| **byte == b'\n').count())
                .unwrap_or(u64::MAX),
        );
        ended_with_newline = chunk[count - 1] == b'\n';
    }
    if saw_any && !ended_with_newline {
        lines = lines.saturating_add(1);
    }
    (lines, bytes)
}

fn run_detector(
    repo: &Path,
    id: &str,
    detector: &MaintainDetectorConfig,
    timeout_secs: u64,
) -> CtxResult<DetectorResult> {
    validate_detector(id, detector)?;
    let mut command = command_for_shell(&detector.command);
    super::isolate_process_tree(&mut command);
    command
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut job = crate::commands::ctx::supervise::JobGuard::adopt(child.id());
    let stdout = child.stdout.take().ok_or("maintain detector stdout was not captured")?;
    let stderr = child.stderr.take().ok_or("maintain detector stderr was not captured")?;
    let stdout_thread = std::thread::spawn(move || count_stream(stdout));
    let stderr_thread = std::thread::spawn(move || count_stream(stderr));
    let deadline = Instant::now()
        + Duration::from_secs(timeout_secs.clamp(1, MAX_DETECTOR_TIMEOUT_SECS));
    let (exit_code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            super::terminate_process_tree(&mut child)?;
            job.close();
            break (status.code(), false);
        }
        if Instant::now() >= deadline {
            super::terminate_process_tree(&mut child)?;
            job.close();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let (stdout_lines, stdout_bytes) = stdout_thread.join().unwrap_or((0, 0));
    let (_stderr_lines, stderr_bytes) = stderr_thread.join().unwrap_or((0, 0));
    let breach = timed_out
        || exit_code.is_none_or(|code| code != 0)
        || (detector.mode == MaintainDetectorMode::LineCount
            && stdout_lines >= detector.threshold);
    Ok(DetectorResult {
        id: id.to_string(),
        mode: detector.mode,
        threshold: detector.threshold,
        exit_code,
        timed_out,
        output_lines: stdout_lines,
        output_bytes: stdout_bytes.saturating_add(stderr_bytes),
        breach,
        workflow_id: None,
        issue_url: None,
        report_error: None,
    })
}

fn incident_key(repo: &Path, detector_id: &str) -> String {
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("{detector_id}-{}", &digest[..12])
}

fn marker_path(state_dir: &StateDir, repo: &Path, key: &str) -> PathBuf {
    state_dir
        .root()
        .join("maintenance")
        .join(repo_slug(repo))
        .join(format!("{key}.json"))
}

fn load_marker(state_dir: &StateDir, repo: &Path, key: &str) -> CtxResult<Option<IncidentMarker>> {
    let path = marker_path(state_dir, repo, key);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

fn save_marker(
    state_dir: &StateDir,
    repo: &Path,
    key: &str,
    marker: &IncidentMarker,
) -> CtxResult<()> {
    let path = marker_path(state_dir, repo, key);
    let parent = path.parent().ok_or("maintenance marker has no parent")?;
    create_private_dir_all(parent)?;
    write_private(&path, &serde_json::to_string_pretty(marker)?)?;
    Ok(())
}

fn clear_marker(state_dir: &StateDir, repo: &Path, key: &str) -> CtxResult<()> {
    let path = marker_path(state_dir, repo, key);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn incident_title(repo: &Path, detector_id: &str) -> String {
    format!("[zirv-maintain:{}] detector breach", incident_key(repo, detector_id))
}

fn incident_intent(detector: &DetectorResult) -> String {
    format!(
        "# Intent\n\n## Problem\n\nThe deterministic maintenance detector `{}` breached.\n\n## Desired outcome\n\nRestore the repository so the detector no longer breaches without weakening or disabling the detector.\n\n## Constraints\n\n- Detector mode: {:?}\n- Exit code: {}\n- Timed out: {}\n- Observed output lines: {}\n- Threshold: {}\n- Observed output bytes: {}\n- Detector command/output bodies are intentionally omitted from this committed artifact to avoid publishing operator configuration or secrets.\n\n## Open questions\n\nDetermine the root cause from repository evidence before implementation.\n\n## Acceptance criteria\n\n- [ ] The detector completes successfully and does not breach on a fresh scan.\n- [ ] Relevant regression verification passes.\n",
        detector.id,
        detector.mode,
        detector
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".into()),
        detector.timed_out,
        detector.output_lines,
        detector.threshold,
        detector.output_bytes,
    )
}

fn create_incident_workflow(
    state_dir: &StateDir,
    repo: &Path,
    detector: &DetectorResult,
) -> CtxResult<WorkflowState> {
    let task = format!("maintenance incident: detector '{}' breached", detector.id);
    let classification = super::classify::classify(&ClassificationInput {
        task: task.clone(),
        paths: Vec::new(),
        changed_lines: 0,
        tests_changed: false,
        intent_override: Some(Intent::Bugfix),
        complexity_override: Some(Complexity::Bounded),
        risk_override: Some(RiskBand::Medium),
    })?;
    let mut state = WorkflowState::start(
        repo.to_path_buf(),
        task,
        WorkflowKind::Bugfix,
        None,
        true,
        classification,
    );
    if state.current().and_then(|step| step.artifact) != Some(engine::ArtifactStage::Intent) {
        return Err("maintenance incident workflow did not materialize an Intent gate".into());
    }
    state.status = WorkflowStatus::AwaitingApproval;
    let work_dir = repo.join(".zirv").join("work").join(&state.id);
    std::fs::create_dir_all(&work_dir)?;
    write_shared(&work_dir.join("intent.md"), &incident_intent(detector))?;
    let active = engine::load_active(state_dir, repo)?.is_none();
    engine::save(state_dir, &state, active)?;
    Ok(state)
}

fn file_incident(
    repository: &str,
    repo: &Path,
    detector: &DetectorResult,
    workflow_id: &str,
) -> CtxResult<String> {
    let title = incident_title(repo, &detector.id);
    let token = crate::commands::report::resolve_token(
        dirs::home_dir().as_deref(),
        &|key| std::env::var(key).ok(),
        &crate::commands::report::gh_auth_token,
    )?;
    if let Some(number) = crate::commands::report::find_open_issue_by_title_in(
        repository,
        &token,
        "",
        &title,
    )? {
        return Ok(format!("https://github.com/{repository}/issues/{number}"));
    }
    crate::commands::report::create_issue_in(
        repository,
        &token,
        &crate::commands::report::IssueRequest {
            title,
            body: format!(
                "Zirv deterministic maintenance detected a breach.\n\n- Workflow: `{workflow_id}`\n- Detector: `{}`\n- Mode: `{:?}`\n- Exit code: `{}`\n- Timed out: `{}`\n- Output lines: `{}`\n- Threshold: `{}`\n\nDetector command and output bodies are deliberately omitted. Review the committed `.zirv/work/{workflow_id}/intent.md` artifact for the bounded incident evidence.",
                detector.id,
                detector.mode,
                detector
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into()),
                detector.timed_out,
                detector.output_lines,
                detector.threshold,
            ),
            labels: Vec::new(),
        },
    )
}

fn process_breach(
    state_dir: &StateDir,
    repo: &Path,
    report_repository: Option<&str>,
    result: &mut DetectorResult,
) -> CtxResult<()> {
    let key = incident_key(repo, &result.id);
    let mut marker = match load_marker(state_dir, repo, &key)? {
        Some(marker) => marker,
        None => {
            let state = create_incident_workflow(state_dir, repo, result)?;
            let marker = IncidentMarker {
                workflow_id: state.id.clone(),
                issue_url: None,
                created_at: now_secs(),
            };
            save_marker(state_dir, repo, &key, &marker)?;
            marker
        }
    };
    result.workflow_id = Some(marker.workflow_id.clone());

    if marker.issue_url.is_none()
        && let Some(repository) = report_repository
    {
        match file_incident(repository, repo, result, &marker.workflow_id) {
            Ok(url) => {
                marker.issue_url = Some(url.clone());
                save_marker(state_dir, repo, &key, &marker)?;
                result.issue_url = Some(url);
            }
            Err(error) => {
                result.report_error = Some(error.to_string());
            }
        }
    } else {
        result.issue_url = marker.issue_url.clone();
    }
    Ok(())
}

fn scan(args: &ScanArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let raw_repo = match &args.repo {
        Some(repo) => repo.clone(),
        None => std::env::current_dir()?,
    };
    let repo = raw_repo
        .canonicalize()
        .unwrap_or(raw_repo);
    let cfg = CtxConfig::load(&repo, &|key| std::env::var(key).ok())?;
    let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let mut results = Vec::new();

    for (id, detector) in &cfg.workflow.maintain.detectors {
        let mut result = run_detector(&repo, id, detector, cfg.workflow.maintain.timeout_secs)?;
        let key = incident_key(&repo, id);
        if result.breach {
            process_breach(
                &state_dir,
                &repo,
                cfg.report.repository.as_deref(),
                &mut result,
            )?;
        } else {
            clear_marker(&state_dir, &repo, &key)?;
        }

        let mut event =
            super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::MaintenanceScan);
        event.workflow_id = result.workflow_id.clone();
        event.phase = Some(super::skill::WorkflowPhase::Intent);
        event.succeeded = Some(!result.breach);
        let _ = super::telemetry::record(
            &state_dir,
            &repo,
            &event,
            &super::telemetry::TelemetryConfig::for_repo(&repo),
        );
        results.push(result);
    }

    let report = ScanReport {
        repository: repo,
        detectors: results.len(),
        breaches: results.iter().filter(|result| result.breach).count(),
        results,
    };
    if args.json {
        serde_json::to_writer_pretty(&mut *writer, &report)?;
        writeln!(writer)?;
    } else if report.detectors == 0 {
        writeln!(
            writer,
            "no maintenance detectors configured in operator ~/.zirv/ctx.toml"
        )?;
    } else {
        writeln!(
            writer,
            "maintenance scan: {} detector(s), {} breach(es)",
            report.detectors, report.breaches
        )?;
        for result in &report.results {
            writeln!(
                writer,
                "{}\tbreach={}\texit={}\tlines={}\tworkflow={}\tissue={}",
                result.id,
                result.breach,
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into()),
                result.output_lines,
                result.workflow_id.as_deref().unwrap_or("-"),
                result.issue_url.as_deref().unwrap_or("-"),
            )?;
            if let Some(error) = &result.report_error {
                writeln!(writer, "  issue filing deferred: {error}")?;
            }
        }
    }
    Ok(if report
        .results
        .iter()
        .any(|result| result.report_error.is_some())
    {
        2
    } else {
        0
    })
}

pub fn run(args: &MaintainArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        MaintainCommand::Scan(args) => scan(args, writer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detector_validation_is_bounded() {
        let ok = MaintainDetectorConfig {
            command: "printf x".into(),
            mode: MaintainDetectorMode::LineCount,
            threshold: 1,
        };
        validate_detector("audit", &ok).unwrap();
        assert!(validate_detector("../audit", &ok).is_err());
        let mut bad = ok.clone();
        bad.threshold = 0;
        assert!(validate_detector("audit", &bad).is_err());
    }

    #[test]
    fn line_count_detector_is_deterministic() {
        let repo = tempdir().unwrap();
        let detector = MaintainDetectorConfig {
            command: if cfg!(windows) {
                "echo one&&echo two".into()
            } else {
                "printf 'one\\ntwo\\n'".into()
            },
            mode: MaintainDetectorMode::LineCount,
            threshold: 2,
        };
        let result = run_detector(repo.path(), "count", &detector, 5).unwrap();
        assert_eq!(result.output_lines, 2);
        assert!(result.breach);
    }

    #[test]
    fn exit_detector_breaches_only_on_failure_or_timeout() {
        let repo = tempdir().unwrap();
        let success = MaintainDetectorConfig {
            command: if cfg!(windows) { "exit /B 0".into() } else { "true".into() },
            mode: MaintainDetectorMode::ExitNonzero,
            threshold: 1,
        };
        let failure = MaintainDetectorConfig {
            command: if cfg!(windows) { "exit /B 7".into() } else { "exit 7".into() },
            ..success.clone()
        };
        assert!(!run_detector(repo.path(), "ok", &success, 5).unwrap().breach);
        assert!(run_detector(repo.path(), "bad", &failure, 5).unwrap().breach);
    }

    #[test]
    fn incident_title_is_stable_per_repo_and_detector() {
        let repo = tempdir().unwrap();
        let a = incident_title(repo.path(), "audit");
        let b = incident_title(repo.path(), "audit");
        let c = incident_title(repo.path(), "lint");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn persistent_breach_reuses_one_parked_workflow_until_recovery() {
        let repo = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut first = DetectorResult {
            id: "audit".into(),
            mode: MaintainDetectorMode::ExitNonzero,
            threshold: 1,
            exit_code: Some(1),
            timed_out: false,
            output_lines: 0,
            output_bytes: 0,
            breach: true,
            workflow_id: None,
            issue_url: None,
            report_error: None,
        };
        process_breach(&state_dir, repo.path(), None, &mut first).unwrap();
        let workflow = first.workflow_id.clone().expect("workflow");
        assert!(
            repo.path()
                .join(".zirv/work")
                .join(&workflow)
                .join("intent.md")
                .exists()
        );
        let stored = engine::load(&state_dir, repo.path(), &workflow).unwrap();
        assert_eq!(stored.status, WorkflowStatus::AwaitingApproval);
        assert_eq!(
            stored.current().and_then(|step| step.artifact),
            Some(engine::ArtifactStage::Intent)
        );

        let mut second = first.clone();
        second.workflow_id = None;
        process_breach(&state_dir, repo.path(), None, &mut second).unwrap();
        assert_eq!(second.workflow_id.as_deref(), Some(workflow.as_str()));

        let key = incident_key(repo.path(), "audit");
        clear_marker(&state_dir, repo.path(), &key).unwrap();
        assert!(load_marker(&state_dir, repo.path(), &key).unwrap().is_none());
    }

    #[test]
    fn incident_intent_never_embeds_detector_command_or_output() {
        let detector = DetectorResult {
            id: "audit".into(),
            mode: MaintainDetectorMode::ExitNonzero,
            threshold: 1,
            exit_code: Some(1),
            timed_out: false,
            output_lines: 3,
            output_bytes: 100,
            breach: true,
            workflow_id: None,
            issue_url: None,
            report_error: None,
        };
        let text = incident_intent(&detector);
        assert!(text.contains("audit"));
        assert!(text.contains("Output lines"));
        assert!(!text.contains("cargo audit"));
    }
}
