//! Offline deterministic frontend quality detector.
//!
//! These rules deliberately avoid model or network calls. They identify
//! objective accessibility hazards and high-signal AI-UI defaults, producing
//! structured evidence that a later visual review can confirm or disposition.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use clap::{Args, ValueEnum};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

pub const DETECTOR_REPORT_SCHEMA_VERSION: u32 = 3;
const MAX_FILES: usize = 256;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_FINDINGS: usize = 256;
const MAX_SCAN_DEPTH: usize = 32;
const MAX_WAIVERS: usize = 128;
const MAX_WAIVER_BYTES: u64 = 64 * 1024;
const MAX_WAIVER_REASON_BYTES: usize = 512;
const MAX_EVIDENCE_BYTES: usize = 256;

static IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<img\b[^>]*>").expect("valid image regex"));
static DIOXUS_IMAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bimg\s*\{(?P<body>[^}]*)\}").expect("valid Dioxus image regex")
});
static DIOXUS_ALT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\balt\s*:").expect("valid Dioxus alt regex"));
static ALT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:^|\s)alt\s*="#).expect("valid alt attribute regex"));
static CLICK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(?:div|span)\b[^>]*\s(?:onclick|on:click)\s*=")
        .expect("valid click target regex")
});
static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<h([1-6])\b").expect("valid heading regex"));
static INPUT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(?:input|select|textarea)\b[^>]*>").expect("valid form control regex")
});
static MEDIA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(?:audio|video)\b[^>]*\sautoplay\b[^>]*>").expect("valid media regex")
});
static MUTED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)muted(?:\s|=|/|>)"#).expect("valid muted attribute regex")
});
static MUTED_FALSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)muted\s*=\s*(?:["']?false["']?|\{\s*false\s*\})"#)
        .expect("valid false muted attribute regex")
});
static POSITIVE_TABINDEX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)tabindex\s*=\s*["'{]?\s*[1-9]"#).expect("valid tabindex regex")
});
static VIEWPORT_ZOOM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<meta\b[^>]*(?:user-scalable\s*=\s*no|maximum-scale\s*=\s*1(?:\.0+)?)(?:\s*[,;"'][^>]*|\s*/?)>"#,
    )
        .expect("valid viewport zoom regex")
});
static FIXED_CONTENT_WIDTH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:width|min-width)\s*:\s*(?:[8-9][0-9]{2}|[1-9][0-9]{3,})px")
        .expect("valid fixed width regex")
});
static OVERSIZED_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)font-size\s*:\s*(?:[7-9]|[1-9][0-9]+)rem").expect("valid oversized type regex")
});
static LAYOUT_MOTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)transition(?:-property)?\s*:[^;]*(?:width|height|top|left|right|bottom|margin|padding)",
    )
    .expect("valid layout motion regex")
});

pub const DETECTOR_RULE_IDS: &[&str] = &[
    "a11y/dynamic-announcement",
    "a11y/focus-visible",
    "a11y/heading-order",
    "a11y/image-alt",
    "a11y/media-autoplay",
    "a11y/paste-blocked",
    "a11y/positive-tabindex",
    "a11y/semantic-action",
    "a11y/viewport-zoom",
    "content/generic-action",
    "content/generic-ai-copy",
    "content/loading-ellipsis",
    "craft/container-overuse",
    "craft/dark-glow",
    "craft/decorative-blur",
    "craft/emoji-chrome",
    "craft/gradient-text",
    "craft/hard-offset-shadow",
    "craft/overused-font",
    "craft/pill-overuse",
    "craft/purple-blue-gradient",
    "craft/unjustified-gradient",
    "form/autocomplete",
    "i18n/manual-plural",
    "i18n/physical-properties",
    "media/animated-gif",
    "media/image-dimensions",
    "motion/bounce-easing",
    "motion/infinite-animation",
    "motion/layout-properties",
    "motion/reduced-motion",
    "motion/transition-all",
    "performance/font-display",
    "performance/layout-read",
    "responsive/fixed-content-width",
    "responsive/legacy-viewport-height",
    "responsive/overflow-mask",
    "responsive/safe-area",
    "touch/small-target",
    "typography/aggressive-tracking",
    "typography/oversized-display",
    "ux/autofocus",
    "ux/dialog-overscroll",
    "ux/placeholder-only-control",
];

pub fn detector_rule_count() -> usize {
    DETECTOR_RULE_IDS.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingSeverity {
    Advisory,
    Blocking,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingDisposition {
    #[default]
    Active,
    Waived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WaiverSource {
    Operator,
    Repository,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaiverProvenance {
    pub source: WaiverSource,
    pub config_path: PathBuf,
    pub reason: String,
    pub applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum DetectorScope {
    Changed,
    All,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectorFinding {
    pub rule_id: String,
    pub severity: FindingSeverity,
    pub path: PathBuf,
    pub line: usize,
    pub summary: String,
    pub remediation: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub disposition: FindingDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver: Option<WaiverProvenance>,
    /// True when this finding's path was NOT in the workflow's since-base
    /// change set at scan time -- a full-surface (Review/Verify) scan tags
    /// every finding this way so the gate can fail closed on anything the
    /// current change introduced while still surfacing (rather than
    /// silently hiding) what was already there (#251). Always `false` for a
    /// scan that never had a baseline to compare against, such as a bare
    /// `zirv frontend check`.
    #[serde(default)]
    pub preexisting: bool,
}

impl DetectorFinding {
    fn blocks(&self) -> bool {
        self.severity == FindingSeverity::Blocking && self.disposition == FindingDisposition::Active
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectorReport {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    pub change_fingerprint: u64,
    pub profile_fingerprint: String,
    pub scope: DetectorScope,
    pub generated_at: u64,
    pub analyzed_files: Vec<PathBuf>,
    pub analyzed_bytes: usize,
    pub truncated: bool,
    pub findings: Vec<DetectorFinding>,
    #[serde(default)]
    pub waivers_loaded: usize,
    #[serde(default)]
    pub waivers_rejected: usize,
    /// True for a report with zero analyzed files that neither failed nor
    /// was truncated: "0 frontend files in scope" rather than "no evidence".
    /// A workflow gate must treat this as a pass (#255), never as missing
    /// evidence to fail closed on.
    #[serde(default)]
    pub not_applicable: bool,
}

impl DetectorReport {
    pub fn passed(&self) -> bool {
        !self.findings.iter().any(DetectorFinding::blocks)
    }

    pub fn blocking_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.blocks())
            .count()
    }

    /// Blocking findings whose path was in the since-base change set (or
    /// that were never tagged against one) -- these always fail a workflow
    /// gate, regardless of `--accept-preexisting-findings` (#251).
    pub fn introduced_blocking_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.blocks() && !finding.preexisting)
            .count()
    }

    /// Blocking findings tagged `preexisting`: not introduced by the current
    /// change, but present in the full-surface scope a Review/Verify gate
    /// scans. An operator can accept these explicitly.
    pub fn preexisting_blocking_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.blocks() && finding.preexisting)
            .count()
    }

    /// Every pre-existing finding regardless of severity, for the "N
    /// blocking / M total" acceptance record and status line.
    pub fn preexisting_total_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.preexisting)
            .count()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct WaiverDocument {
    schema_version: u32,
    #[serde(default)]
    waivers: Vec<WaiverRule>,
}

#[derive(Debug, Clone, Deserialize)]
struct WaiverRule {
    rule_id: String,
    path: Option<String>,
    value: Option<String>,
    reason: String,
}

#[derive(Debug, Clone)]
struct LoadedWaiver {
    source: WaiverSource,
    config_path: PathBuf,
    rule: WaiverRule,
}

#[derive(Debug, Args)]
pub struct DetectorArgs {
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Analyze an explicit repository-relative frontend file; repeatable.
    #[arg(long = "path", conflicts_with = "all")]
    pub paths: Vec<PathBuf>,
    /// Analyze every bounded frontend source file instead of changed files.
    #[arg(long, conflicts_with = "paths")]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchmarkCaseResult {
    pub name: String,
    pub expected_rules: Vec<String>,
    pub observed_rules: Vec<String>,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub rule_inventory: usize,
    pub covered_rules: usize,
    pub inventory_complete: bool,
    pub results: Vec<BenchmarkCaseResult>,
}

fn report_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.frontend().join(repo_slug(repo)).join("detector")
}

pub(crate) fn save_report(state: &StateDir, report: &DetectorReport) -> CtxResult<()> {
    let directory = report_dir(state, &report.repo);
    create_private_dir_all(&directory)?;
    let body = serde_json::to_string_pretty(report)?;
    write_private(&directory.join(format!("{}.json", report.id)), &body)?;
    write_private(&directory.join("latest"), &report.id)?;
    Ok(())
}

pub fn load_latest(state: &StateDir, repo: &Path) -> CtxResult<Option<DetectorReport>> {
    let directory = report_dir(state, repo);
    let pointer = directory.join("latest");
    if !pointer.exists() {
        return Ok(None);
    }
    let id = std::fs::read_to_string(pointer)?;
    let report: DetectorReport = serde_json::from_str(&std::fs::read_to_string(
        directory.join(format!("{}.json", id.trim())),
    )?)?;
    if report.schema_version != DETECTOR_REPORT_SCHEMA_VERSION {
        return Err(format!(
            "frontend detector report '{}': unsupported schema_version {}",
            report.id, report.schema_version
        )
        .into());
    }
    Ok(Some(report))
}

pub fn latest_is_fresh_and_passing(state: &StateDir, repo: &Path) -> CtxResult<bool> {
    let Some(report) = load_latest(state, repo)? else {
        return Ok(false);
    };
    let profile = super::frontend::ensure_profile(state, repo)?;
    Ok(report.passed()
        && !report.truncated
        // #255: a fresh, non-truncated `not_applicable` report (0 frontend
        // files in scope) is a legitimate pass, not missing evidence -- a
        // Frontend-profile workflow with no frontend files must not re-run
        // the detector at every single gate check.
        && (!report.analyzed_files.is_empty() || report.not_applicable)
        && report.profile_fingerprint == profile.source_fingerprint
        && report.change_fingerprint == super::verification::change_fingerprint(repo)?)
}

pub fn detect(
    state: &StateDir,
    repo: &Path,
    requested: &[PathBuf],
    all: bool,
) -> CtxResult<DetectorReport> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut truncated = false;
    let (scope, paths) = if !requested.is_empty() {
        (DetectorScope::Explicit, requested.to_vec())
    } else if all {
        let (paths, scan_truncated) = collect_all(&repo)?;
        truncated = scan_truncated;
        (DetectorScope::All, paths)
    } else {
        (
            DetectorScope::Changed,
            super::verification::changed_paths(&repo)?,
        )
    };
    detect_with_scope(state, &repo, scope, paths, truncated, None)
}

/// The workflow-facing entry point (#251/#255). Unlike the standalone
/// `detect` above, this always measures the since-base change set
/// (`verification::changed_paths_since_base`, which -- unlike
/// `changed_paths` -- still sees a change once an earlier step has
/// committed it) and uses it two ways:
///
/// - When `require_full_surface` is false (the Test-phase gate), scope is
///   ALWAYS `Changed` over that since-base set -- never a whole-repository
///   fallback, which used to fire the moment the change set went quiet
///   after a commit and fail the gate on unrelated pre-existing findings.
/// - When `require_full_surface` is true (Review/Verify), the scan still
///   covers the whole repository, but every finding is tagged
///   `preexisting` against the since-base set so the gate (and an operator
///   reading the report) can tell a finding this change introduced from one
///   that was already there.
pub fn detect_for_workflow(
    state: &StateDir,
    repo: &Path,
    require_full_surface: bool,
) -> CtxResult<DetectorReport> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let since_base = super::verification::changed_paths_since_base(&repo)?;
    if require_full_surface {
        let (paths, truncated) = collect_all(&repo)?;
        detect_with_scope(
            state,
            &repo,
            DetectorScope::All,
            paths,
            truncated,
            Some(&since_base),
        )
    } else {
        detect_with_scope(
            state,
            &repo,
            DetectorScope::Changed,
            since_base.clone(),
            false,
            Some(&since_base),
        )
    }
}

fn detect_with_scope(
    state: &StateDir,
    repo: &Path,
    scope: DetectorScope,
    mut paths: Vec<PathBuf>,
    mut truncated: bool,
    preexisting_baseline: Option<&[PathBuf]>,
) -> CtxResult<DetectorReport> {
    let profile = super::frontend::ensure_profile(state, repo)?;
    paths.retain(|path| is_frontend_source(repo, path));
    paths.sort();
    paths.dedup();
    truncated |= paths.len() > MAX_FILES;
    paths.truncate(MAX_FILES);

    let mut findings = Vec::new();
    let mut analyzed_files = Vec::new();
    let mut analyzed_bytes = 0usize;
    for requested_path in paths {
        if analyzed_bytes >= MAX_TOTAL_BYTES {
            truncated = true;
            break;
        }
        let (relative, absolute) = resolve_file(repo, &requested_path)?;
        if !absolute.exists() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&absolute)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "frontend detector refuses non-regular or symlinked path '{}'",
                relative.display()
            )
            .into());
        }
        if metadata.len() > MAX_FILE_BYTES {
            truncated = true;
            continue;
        }
        let remaining = MAX_TOTAL_BYTES - analyzed_bytes;
        let mut bytes = Vec::new();
        std::fs::File::open(&absolute)?
            .take(MAX_FILE_BYTES.min(u64::try_from(remaining).unwrap_or(u64::MAX)))
            .read_to_end(&mut bytes)?;
        analyzed_bytes = analyzed_bytes.saturating_add(bytes.len());
        analyze(&relative, &String::from_utf8_lossy(&bytes), &mut findings);
        analyzed_files.push(relative);
        if findings.len() >= MAX_FINDINGS {
            findings.truncate(MAX_FINDINGS);
            truncated = true;
            break;
        }
    }
    findings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.rule_id.cmp(&right.rule_id))
    });
    if let Some(baseline) = preexisting_baseline {
        for finding in &mut findings {
            finding.preexisting = !baseline.contains(&finding.path);
        }
    }
    let waivers = load_waivers(repo)?;
    let waivers_rejected = apply_waivers(&mut findings, &waivers);
    let not_applicable = analyzed_files.is_empty() && !truncated;

    let change_fingerprint = super::verification::change_fingerprint(repo)?;
    let report = DetectorReport {
        schema_version: DETECTOR_REPORT_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        repo: repo.to_path_buf(),
        change_fingerprint,
        profile_fingerprint: profile.source_fingerprint,
        scope,
        generated_at: now_secs(),
        analyzed_files,
        analyzed_bytes,
        truncated,
        findings,
        waivers_loaded: waivers.len(),
        waivers_rejected,
        not_applicable,
    };
    save_report(state, &report)?;
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::FrontendDetectorRun);
    event.workflow_id = super::engine::load_active(state, &report.repo)
        .ok()
        .flatten()
        .map(|workflow| workflow.id);
    event.phase = Some(super::skill::WorkflowPhase::Test);
    event.work_domain = Some(super::classify::WorkDomain::Frontend);
    event.succeeded = Some(report.passed() && !report.truncated);
    event.findings_total = u32::try_from(report.findings.len()).unwrap_or(u32::MAX);
    event.findings_meaningful = u32::try_from(report.blocking_count()).unwrap_or(u32::MAX);
    let _ = super::telemetry::record(
        state,
        &report.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&report.repo),
    );
    Ok(report)
}

fn resolve_file(repo: &Path, requested: &Path) -> CtxResult<(PathBuf, PathBuf)> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        repo.join(requested)
    };
    let normalized = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());
    if !normalized.starts_with(repo) {
        return Err(format!(
            "frontend detector path '{}' escapes repository '{}'",
            requested.display(),
            repo.display()
        )
        .into());
    }
    let relative = normalized
        .strip_prefix(repo)
        .unwrap_or(&normalized)
        .to_path_buf();
    Ok((relative, normalized))
}

/// Directory names never worth scanning: VCS internals, zirv's own state,
/// and the usual dependency/build-output noise. Applied whether the
/// candidate list comes from `git ls-files` or the hand-rolled walk below.
const DENIED_DIR_NAMES: &[&str] = &[
    ".git",
    ".zirv",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
    "vendor",
];

fn has_denied_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, std::path::Component::Normal(name)
            if DENIED_DIR_NAMES.iter().any(|denied| name.to_str() == Some(*denied)))
    })
}

/// #251: the hand-rolled walk below does not honor `.gitignore`, so a
/// generated build artifact directory a repository never bothered to add to
/// the deny list above (`.gitlab-ci-local/builds/.docker` in the wild) still
/// got scanned. When `repo` is a git work tree, ask git for the candidate
/// list instead -- tracked plus untracked-not-ignored -- which is both
/// `.gitignore`-aware and cheaper than a manual recursive walk.
///
/// `Ok(None)` when git could not be spawned OR exited non-zero -- never a
/// silent empty candidate list, which `collect_all` below would otherwise
/// be unable to tell apart from a repository that genuinely has zero
/// frontend files, failing the whole scan open into a "not applicable" pass
/// on what was actually a git failure.
fn collect_all_via_git(repo: &Path) -> CtxResult<Option<(Vec<PathBuf>, bool)>> {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
    else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let mut paths: Vec<PathBuf> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
        // `--cached` can list a file that was deleted from the working tree
        // without `git rm`; nothing downstream can scan a path that is not
        // actually there.
        .filter(|path| !has_denied_component(path) && repo.join(path).is_file())
        .filter(|path| is_frontend_source(repo, path))
        .collect();
    paths.sort();
    paths.dedup();
    let truncated = paths.len() > MAX_FILES;
    paths.truncate(MAX_FILES);
    Ok(Some((paths, truncated)))
}

fn is_git_work_tree(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
}

fn collect_all(repo: &Path) -> CtxResult<(Vec<PathBuf>, bool)> {
    if is_git_work_tree(repo)
        && let Some(result) = collect_all_via_git(repo)?
    {
        return Ok(result);
    }
    let mut paths = Vec::new();
    let mut entries = 0usize;
    let mut truncated = false;
    collect_directory(repo, repo, 0, &mut entries, &mut paths, &mut truncated)?;
    Ok((paths, truncated))
}

fn collect_directory(
    repo: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut usize,
    paths: &mut Vec<PathBuf>,
    truncated: &mut bool,
) -> CtxResult<()> {
    if depth > MAX_SCAN_DEPTH || *entries >= 4_096 || paths.len() >= MAX_FILES {
        *truncated = true;
        return Ok(());
    }
    let mut children = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        if *entries >= 4_096 || paths.len() >= MAX_FILES {
            *truncated = true;
            break;
        }
        *entries += 1;
        let path = child.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = child.file_name();
            if !DENIED_DIR_NAMES.contains(&name.to_str().unwrap_or_default()) {
                collect_directory(repo, &path, depth + 1, entries, paths, truncated)?;
            }
        } else if metadata.is_file() {
            let relative = path.strip_prefix(repo).unwrap_or(&path).to_path_buf();
            if is_frontend_source(repo, &relative) {
                paths.push(relative);
            }
        }
    }
    Ok(())
}

fn is_frontend_source(repo: &Path, path: &Path) -> bool {
    if matches!(
        path.extension().and_then(|value| value.to_str()),
        Some(
            "css" | "scss" | "sass" | "less" | "html" | "tsx" | "jsx" | "vue" | "svelte" | "astro"
        )
    ) {
        return true;
    }
    path.extension().and_then(|value| value.to_str()) == Some("rs")
        && dioxus_root_for(repo, path).is_some()
}

fn dioxus_root_for(repo: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = repo.join(path);
    let mut directory = absolute.parent()?;
    loop {
        let dioxus = directory.join("Dioxus.toml");
        let cargo = directory.join("Cargo.toml");
        if dioxus.is_file() && bounded_manifest_contains(&cargo, "dioxus") {
            return Some(directory.to_path_buf());
        }
        if directory == repo {
            break;
        }
        directory = directory.parent()?;
    }
    None
}

fn bounded_manifest_contains(path: &Path, needle: &str) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_WAIVER_BYTES
    {
        return false;
    }
    std::fs::read_to_string(path).is_ok_and(|text| text.to_ascii_lowercase().contains(needle))
}

fn line_for(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn finding(
    findings: &mut Vec<DetectorFinding>,
    rule_id: &str,
    severity: FindingSeverity,
    path: &Path,
    line: usize,
    summary: &str,
    remediation: &str,
) {
    findings.push(DetectorFinding {
        rule_id: rule_id.into(),
        severity,
        path: path.to_path_buf(),
        line,
        summary: summary.into(),
        remediation: remediation.into(),
        evidence: String::new(),
        disposition: FindingDisposition::Active,
        waiver: None,
        preexisting: false,
    });
}

fn analyze(path: &Path, text: &str, findings: &mut Vec<DetectorFinding>) {
    let first_new = findings.len();
    let lower = text.to_ascii_lowercase();
    for image in IMAGE_RE.find_iter(text) {
        let tag = image.as_str();
        if !ALT_RE.is_match(tag) {
            finding(
                findings,
                "a11y/image-alt",
                FindingSeverity::Blocking,
                path,
                line_for(text, image.start()),
                "Image element has no alt contract.",
                "Add meaningful alt text, or alt=\"\" when the image is purely decorative.",
            );
        }
        let tag = tag.to_ascii_lowercase();
        if !(tag.contains("width=") && tag.contains("height=")) {
            finding(
                findings,
                "media/image-dimensions",
                FindingSeverity::Advisory,
                path,
                line_for(text, image.start()),
                "Image dimensions are not explicit in the element.",
                "Provide intrinsic width and height or the framework's equivalent to prevent layout shift.",
            );
        }
    }
    if path.extension().and_then(|value| value.to_str()) == Some("rs") {
        for image in DIOXUS_IMAGE_RE.captures_iter(text) {
            let Some(whole) = image.get(0) else {
                continue;
            };
            let body = image.name("body").map(|value| value.as_str()).unwrap_or("");
            if !DIOXUS_ALT_RE.is_match(body) {
                finding(
                    findings,
                    "a11y/image-alt",
                    FindingSeverity::Blocking,
                    path,
                    line_for(text, whole.start()),
                    "Dioxus image element has no alt contract.",
                    "Add an alt attribute with meaningful text, or an empty value for decorative imagery.",
                );
            }
            if !(body.contains("width:") && body.contains("height:")) {
                finding(
                    findings,
                    "media/image-dimensions",
                    FindingSeverity::Advisory,
                    path,
                    line_for(text, whole.start()),
                    "Dioxus image dimensions are not explicit in the element.",
                    "Provide intrinsic width and height to prevent layout shift.",
                );
            }
        }
    }
    for target in CLICK_RE.find_iter(text) {
        finding(
            findings,
            "a11y/semantic-action",
            FindingSeverity::Blocking,
            path,
            line_for(text, target.start()),
            "A div or span is used as a click target.",
            "Use a semantic button or link with native keyboard and focus behavior.",
        );
    }
    let mut previous_heading = None;
    for captures in HEADING_RE.captures_iter(text) {
        let Some(level_match) = captures.get(1) else {
            continue;
        };
        let Ok(level) = level_match.as_str().parse::<u8>() else {
            continue;
        };
        if previous_heading.is_some_and(|previous| level > previous + 1) {
            finding(
                findings,
                "a11y/heading-order",
                FindingSeverity::Blocking,
                path,
                captures
                    .get(0)
                    .map(|capture| line_for(text, capture.start()))
                    .unwrap_or(1),
                "Heading hierarchy skips a level.",
                "Use headings as a semantic outline and advance one level at a time.",
            );
        }
        previous_heading = Some(level);
    }
    for media in MEDIA_RE.find_iter(text) {
        let tag = media.as_str();
        if !MUTED_RE.is_match(tag) || MUTED_FALSE_RE.is_match(tag) {
            finding(
                findings,
                "a11y/media-autoplay",
                FindingSeverity::Blocking,
                path,
                line_for(text, media.start()),
                "Media can autoplay with sound.",
                "Remove autoplay or require muted playback with visible controls.",
            );
        }
    }
    for tabindex in POSITIVE_TABINDEX_RE.find_iter(text) {
        finding(
            findings,
            "a11y/positive-tabindex",
            FindingSeverity::Blocking,
            path,
            line_for(text, tabindex.start()),
            "A positive tabindex overrides the document's logical keyboard order.",
            "Use DOM order and tabindex=0 only when a custom control genuinely requires it.",
        );
    }
    for control in INPUT_RE.find_iter(text) {
        let tag = control.as_str().to_ascii_lowercase();
        if [
            "type=\"email\"",
            "type='email'",
            "type=\"tel\"",
            "type='tel'",
            "type=\"password\"",
            "type='password'",
        ]
        .iter()
        .any(|kind| tag.contains(kind))
            && !tag.contains("autocomplete=")
        {
            finding(
                findings,
                "form/autocomplete",
                FindingSeverity::Advisory,
                path,
                line_for(text, control.start()),
                "A personal-data or authentication field has no autocomplete contract.",
                "Set the precise autocomplete token so browsers and assistive tools understand the field.",
            );
        }
        if tag.contains("placeholder=")
            && !tag.contains("aria-label=")
            && !tag.contains("aria-labelledby=")
            && !tag.contains("id=")
        {
            finding(
                findings,
                "ux/placeholder-only-control",
                FindingSeverity::Advisory,
                path,
                line_for(text, control.start()),
                "A form control appears to rely on placeholder text as its only label.",
                "Associate a persistent visible label or accessible name with the control.",
            );
        }
    }
    if let Some(found) = VIEWPORT_ZOOM_RE.find(text) {
        finding(
            findings,
            "a11y/viewport-zoom",
            FindingSeverity::Blocking,
            path,
            line_for(text, found.start()),
            "Viewport configuration disables or caps user zoom.",
            "Allow pinch zoom and browser text scaling.",
        );
    }
    if (lower.contains("onpaste") || lower.contains("on:paste")) && lower.contains("preventdefault")
    {
        finding(
            findings,
            "a11y/paste-blocked",
            FindingSeverity::Blocking,
            path,
            1,
            "Paste appears to be blocked on an input path.",
            "Allow paste, including password-manager and assistive-technology input.",
        );
    }
    if (lower.contains("toast") || lower.contains("validationmessage"))
        && !lower.contains("aria-live")
        && !lower.contains("role=\"alert\"")
        && !lower.contains("role='alert'")
    {
        finding(
            findings,
            "a11y/dynamic-announcement",
            FindingSeverity::Advisory,
            path,
            1,
            "Dynamic feedback has no observed live-region announcement.",
            "Expose asynchronous status through aria-live or an appropriate alert/status role.",
        );
    }

    let has_focus_visible = lower.contains("focus-visible");
    let has_reduced_motion = lower.contains("prefers-reduced-motion")
        || lower.contains("motion-reduce:")
        || lower.contains("usereducedmotion");
    let rounded_count = lower.matches("rounded-").count() + lower.matches("border-radius").count();
    let shadow_count = lower.matches("shadow-").count() + lower.matches("box-shadow").count();
    if rounded_count >= 8 && shadow_count >= 4 {
        finding(
            findings,
            "craft/container-overuse",
            FindingSeverity::Advisory,
            path,
            1,
            "This file combines a high density of rounded containers and shadows.",
            "Re-check whether hierarchy can come from layout, spacing, type, borders, or tone instead of floating cards.",
        );
    }
    let pill_count = lower.matches("rounded-full").count()
        + lower.matches("border-radius: 999").count()
        + lower.matches("border-radius:999").count();
    if pill_count >= 5 {
        finding(
            findings,
            "craft/pill-overuse",
            FindingSeverity::Advisory,
            path,
            1,
            "Pill geometry is repeated broadly instead of being reserved for compact controls.",
            "Use the system's ordinary control and container radii; reserve pills for tags, toggles, and short actions.",
        );
    }
    let blur_count = lower.matches("backdrop-blur").count()
        + lower.matches("backdrop-filter").count()
        + lower.matches("filter: blur").count();
    if blur_count >= 3 {
        finding(
            findings,
            "craft/decorative-blur",
            FindingSeverity::Advisory,
            path,
            1,
            "Blur and glass effects are repeated as decoration.",
            "Use blur only when it represents a real layered material or interaction state.",
        );
    }
    if lower.contains("background-clip: text")
        || lower.contains("background-clip:text")
        || lower.contains("bg-clip-text")
    {
        finding(
            findings,
            "craft/gradient-text",
            FindingSeverity::Advisory,
            path,
            1,
            "Gradient or clipped text is a high-signal generic AI treatment.",
            "Use normal foreground color unless the product's established visual language specifically requires this effect.",
        );
    }
    if lower.contains("linear-gradient(") || lower.contains("radial-gradient(") {
        finding(
            findings,
            "craft/unjustified-gradient",
            FindingSeverity::Advisory,
            path,
            1,
            "A gradient needs product or existing-system justification.",
            "Prefer a semantic solid color or document the established gradient token during review.",
        );
    }
    if (lower.contains("from-purple") && lower.contains("to-blue"))
        || (lower.contains("from-blue") && lower.contains("to-purple"))
        || ((lower.contains("linear-gradient(") || lower.contains("radial-gradient("))
            && lower.contains("purple")
            && lower.contains("blue"))
    {
        finding(
            findings,
            "craft/purple-blue-gradient",
            FindingSeverity::Advisory,
            path,
            1,
            "A purple-blue gradient matches a saturated AI-generated default.",
            "Derive color from product meaning or the established system, and document why a gradient is necessary.",
        );
    }
    if (lower.contains("box-shadow") || lower.contains("drop-shadow"))
        && [
            "purple", "violet", "cyan", "magenta", "#7c3aed", "#8b5cf6", "#06b6d4",
        ]
        .iter()
        .any(|color| lower.contains(color))
    {
        finding(
            findings,
            "craft/dark-glow",
            FindingSeverity::Advisory,
            path,
            1,
            "Colored glow is being used as a generic depth or emphasis device.",
            "Use restrained offset/blur elevation or a product-specific material treatment.",
        );
    }
    if lower.contains("box-shadow:")
        && (lower.contains(" 0px;") || lower.contains(" 0;") || lower.contains("_0_"))
    {
        finding(
            findings,
            "craft/hard-offset-shadow",
            FindingSeverity::Advisory,
            path,
            1,
            "A zero-blur offset shadow reads as an unearned stylistic costume.",
            "Use it only in an explicitly established hard-shadow world; otherwise use the system's elevation language.",
        );
    }
    if [
        "font-family: inter",
        "font-family:inter",
        "font-family: arial",
        "font-family:arial",
    ]
    .iter()
    .any(|font| lower.contains(font))
        || lower.contains("font-family: system-ui")
        || lower.contains("font-family:system-ui")
    {
        finding(
            findings,
            "craft/overused-font",
            FindingSeverity::Advisory,
            path,
            1,
            "An interchangeable default font is used without an observed role or product rationale.",
            "For expressive surfaces choose a type voice grounded in the subject; for product UI document the deliberate workhorse choice.",
        );
    }
    if lower.contains("transition: all") || lower.contains("transition:all") {
        finding(
            findings,
            "motion/transition-all",
            FindingSeverity::Advisory,
            path,
            1,
            "Transitioning every property is imprecise and can animate layout unexpectedly.",
            "Name only the properties whose state change motion helps explain.",
        );
    }
    if (lower.contains("animation:") || lower.contains("transition:")) && !has_reduced_motion {
        finding(
            findings,
            "motion/reduced-motion",
            FindingSeverity::Advisory,
            path,
            1,
            "Motion is present without an observed reduced-motion path in this source.",
            "Provide prefers-reduced-motion or the framework's equivalent close to the motion definition.",
        );
    }
    if [
        "bounce",
        "elastic",
        "spring(",
        "cubic-bezier(0.68",
        "cubic-bezier(.68",
    ]
    .iter()
    .any(|motion| lower.contains(motion))
    {
        finding(
            findings,
            "motion/bounce-easing",
            FindingSeverity::Advisory,
            path,
            1,
            "Bounce or elastic easing is used as a default personality treatment.",
            "Use a restrained ease appropriate to the state change unless the product's motion language earns elasticity.",
        );
    }
    if lower.contains("infinite") && lower.contains("animation") {
        finding(
            findings,
            "motion/infinite-animation",
            FindingSeverity::Advisory,
            path,
            1,
            "An infinite animation can create distraction and persistent resource use.",
            "Stop nonessential loops, provide controls where required, and disable them under reduced motion.",
        );
    }
    if lower.contains("transition: all")
        || lower.contains("transition:all")
        || LAYOUT_MOTION_RE.is_match(text)
    {
        finding(
            findings,
            "motion/layout-properties",
            FindingSeverity::Advisory,
            path,
            1,
            "Motion appears to target layout properties that can trigger reflow.",
            "Prefer transform, opacity, clip, or another compositor-friendly representation of the state change.",
        );
    }
    if (lower.contains("outline: none") || lower.contains("outline:none")) && !has_focus_visible {
        finding(
            findings,
            "a11y/focus-visible",
            FindingSeverity::Blocking,
            path,
            1,
            "Focus outline is removed without an observed focus-visible replacement.",
            "Keep the native outline or add a clearly visible keyboard focus treatment.",
        );
    }
    if lower.contains("100vh") && !lower.contains("100dvh") && !lower.contains("100svh") {
        finding(
            findings,
            "responsive/legacy-viewport-height",
            FindingSeverity::Advisory,
            path,
            1,
            "100vh can obscure content behind mobile browser chrome.",
            "Use dynamic/small viewport units with an intentional fallback.",
        );
    }
    if let Some(found) = FIXED_CONTENT_WIDTH_RE.find(text) {
        finding(
            findings,
            "responsive/fixed-content-width",
            FindingSeverity::Advisory,
            path,
            line_for(text, found.start()),
            "A large fixed pixel width can force horizontal overflow or brittle breakpoints.",
            "Use a bounded fluid width and let the layout recompose at structural breakpoints.",
        );
    }
    if (lower.contains("fixed bottom-0")
        || lower.contains("position: fixed") && lower.contains("bottom: 0"))
        && !lower.contains("safe-area-inset-bottom")
    {
        finding(
            findings,
            "responsive/safe-area",
            FindingSeverity::Advisory,
            path,
            1,
            "A fixed bottom surface has no observed device safe-area inset.",
            "Include env(safe-area-inset-bottom) in the spacing contract.",
        );
    }
    if lower.contains("body")
        && (lower.contains("overflow-x: hidden") || lower.contains("overflow-x:hidden"))
    {
        finding(
            findings,
            "responsive/overflow-mask",
            FindingSeverity::Advisory,
            path,
            1,
            "Page-level horizontal overflow is hidden, which can mask a broken responsive child.",
            "Fix the overflowing element and constrain overflow at the narrowest correct container.",
        );
    }
    if (lower.contains("<dialog")
        || lower.contains("role=\"dialog\"")
        || lower.contains("role='dialog'"))
        && !lower.contains("overscroll-behavior")
    {
        finding(
            findings,
            "ux/dialog-overscroll",
            FindingSeverity::Advisory,
            path,
            1,
            "An overlay has no observed overscroll containment.",
            "Contain scroll chaining and verify focus does not escape or become obscured.",
        );
    }
    if lower.contains("autofocus") {
        finding(
            findings,
            "ux/autofocus",
            FindingSeverity::Advisory,
            path,
            1,
            "Autofocus can steal context and trigger disruptive mobile keyboards.",
            "Reserve autofocus for one clear desktop task and disable it for mobile entry states.",
        );
    }
    if [
        "getboundingclientrect",
        "offsetwidth",
        "offsetheight",
        "scrolltop",
    ]
    .iter()
    .any(|read| lower.contains(read))
    {
        finding(
            findings,
            "performance/layout-read",
            FindingSeverity::Advisory,
            path,
            1,
            "Synchronous layout reads require review for render-loop or read/write thrashing.",
            "Move measurement out of render, batch reads before writes, and prefer CSS layout where possible.",
        );
    }
    if lower.contains("@font-face") && !lower.contains("font-display") {
        finding(
            findings,
            "performance/font-display",
            FindingSeverity::Advisory,
            path,
            1,
            "A custom font face has no loading-display policy.",
            "Set font-display and provide metrics-compatible fallbacks to control invisible text and layout shift.",
        );
    }
    if lower.contains(".gif") {
        finding(
            findings,
            "media/animated-gif",
            FindingSeverity::Advisory,
            path,
            1,
            "Animated GIF media is typically expensive and cannot honor motion preferences well.",
            "Use compressed video with a reduced-motion still fallback when animation is necessary.",
        );
    }
    if [
        "margin-left",
        "margin-right",
        "padding-left",
        "padding-right",
        "border-left",
        "border-right",
    ]
    .iter()
    .any(|property| lower.contains(property))
        && !lower.contains("margin-inline")
        && !lower.contains("padding-inline")
        && !lower.contains("border-inline")
    {
        finding(
            findings,
            "i18n/physical-properties",
            FindingSeverity::Advisory,
            path,
            1,
            "Physical left/right properties can break direction-aware layouts.",
            "Use logical inline/block properties unless the physical direction is semantically required.",
        );
    }
    if lower.contains("count !== 1 ? 's'")
        || lower.contains("count === 1 ? '' : 's'")
        || lower.contains("count != 1 ? \"s\"")
    {
        finding(
            findings,
            "i18n/manual-plural",
            FindingSeverity::Advisory,
            path,
            1,
            "Manual English pluralization does not survive localization.",
            "Use the project's message-format or plural-rules abstraction.",
        );
    }
    if let Some(found) = OVERSIZED_TYPE_RE.find(text) {
        finding(
            findings,
            "typography/oversized-display",
            FindingSeverity::Advisory,
            path,
            line_for(text, found.start()),
            "Display type exceeds the craft floor and is likely to dominate content or break smaller viewports.",
            "Keep display type within a deliberate responsive scale and verify real copy at every breakpoint.",
        );
    }
    if [
        "-0.05em", "-0.06em", "-0.07em", "-0.08em", "-0.09em", "-0.1em",
    ]
    .iter()
    .any(|tracking| lower.contains(tracking))
    {
        finding(
            findings,
            "typography/aggressive-tracking",
            FindingSeverity::Advisory,
            path,
            1,
            "Negative tracking is tighter than the readability floor.",
            "Keep tracking at -0.04em or looser and verify the actual typeface and copy.",
        );
    }
    if lower.contains("size-8")
        || (lower.contains("w-8") && lower.contains("h-8"))
        || (lower.contains("w-6") && lower.contains("h-6"))
    {
        finding(
            findings,
            "touch/small-target",
            FindingSeverity::Advisory,
            path,
            1,
            "A control-sized element may expose a touch target below the 44px class floor.",
            "Keep the visual glyph compact but expand its interactive hit area.",
        );
    }
    for phrase in [
        "lorem ipsum",
        "unlock your potential",
        "seamless experience",
        "revolutionize your",
        "supercharge your",
        "welcome to the future",
        "elevate your experience",
        "transform your workflow",
        "built for the future",
    ] {
        if let Some(offset) = lower.find(phrase) {
            finding(
                findings,
                "content/generic-ai-copy",
                FindingSeverity::Advisory,
                path,
                line_for(text, offset),
                "Generic promotional copy is standing in for product language.",
                "Use concise, realistic copy tied to the actual user action or data.",
            );
        }
    }
    for action in [
        ">continue<",
        ">submit<",
        ">click here<",
        ">learn more<",
        ">get started<",
    ] {
        if let Some(offset) = lower.find(action) {
            finding(
                findings,
                "content/generic-action",
                FindingSeverity::Advisory,
                path,
                line_for(text, offset),
                "A generic action label hides the result of the control.",
                "Name the concrete action or destination in the user's language.",
            );
        }
    }
    for loading in ["loading...", "saving...", "processing..."] {
        if let Some(offset) = lower.find(loading) {
            finding(
                findings,
                "content/loading-ellipsis",
                FindingSeverity::Advisory,
                path,
                line_for(text, offset),
                "A loading label uses three periods instead of the ellipsis character.",
                "Use a typographic ellipsis and describe what is loading when useful.",
            );
        }
    }
    for (index, line) in text.lines().enumerate() {
        let line_lower = line.to_ascii_lowercase();
        if ["button", "nav", "menu", "icon", "toolbar"]
            .iter()
            .any(|term| line_lower.contains(term))
            && line.chars().any(is_emoji)
        {
            finding(
                findings,
                "craft/emoji-chrome",
                FindingSeverity::Advisory,
                path,
                index + 1,
                "Emoji appears to be used as interface chrome.",
                "Use the project's icon system and provide an accessible name.",
            );
        }
    }
    for finding in &mut findings[first_new..] {
        let line = text
            .lines()
            .nth(finding.line.saturating_sub(1))
            .unwrap_or("");
        finding.evidence =
            crate::utils::truncate_bytes(line.trim().to_string(), Some(MAX_EVIDENCE_BYTES));
    }
}

fn read_waiver_document(path: &Path, source: WaiverSource) -> CtxResult<Vec<LoadedWaiver>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "frontend waiver file '{}' must be a regular non-symlink file",
            path.display()
        )
        .into());
    }
    if metadata.len() > MAX_WAIVER_BYTES {
        return Err(format!(
            "frontend waiver file '{}' exceeds {MAX_WAIVER_BYTES} bytes",
            path.display()
        )
        .into());
    }
    let document: WaiverDocument = toml::from_str(&std::fs::read_to_string(path)?)?;
    if document.schema_version != 1 {
        return Err(format!(
            "frontend waiver file '{}': unsupported schema_version {}",
            path.display(),
            document.schema_version
        )
        .into());
    }
    if document.waivers.len() > MAX_WAIVERS {
        return Err(format!(
            "frontend waiver file '{}' has more than {MAX_WAIVERS} entries",
            path.display()
        )
        .into());
    }
    document
        .waivers
        .into_iter()
        .map(|rule| {
            if !DETECTOR_RULE_IDS.contains(&rule.rule_id.as_str()) {
                return Err(
                    format!("frontend waiver references unknown rule '{}'", rule.rule_id).into(),
                );
            }
            let reason = rule.reason.trim();
            if reason.is_empty() || reason.len() > MAX_WAIVER_REASON_BYTES {
                return Err(format!(
                    "frontend waiver reason must contain 1..={MAX_WAIVER_REASON_BYTES} bytes"
                )
                .into());
            }
            if let Some(path) = rule.path.as_deref() {
                let candidate = Path::new(path);
                if candidate.is_absolute()
                    || candidate.components().any(|component| {
                        matches!(
                            component,
                            std::path::Component::ParentDir
                                | std::path::Component::RootDir
                                | std::path::Component::Prefix(_)
                        )
                    })
                {
                    return Err(format!(
                        "frontend waiver path '{path}' must stay repository-relative"
                    )
                    .into());
                }
            }
            if rule
                .value
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err("frontend waiver value must not be empty".into());
            }
            Ok(LoadedWaiver {
                source,
                config_path: path.to_path_buf(),
                rule: WaiverRule {
                    reason: reason.to_string(),
                    ..rule
                },
            })
        })
        .collect()
}

fn load_waivers(repo: &Path) -> CtxResult<Vec<LoadedWaiver>> {
    let mut waivers = Vec::new();
    if let Ok(home) = crate::utils::home_dir() {
        waivers.extend(read_waiver_document(
            &home.join(".zirv/frontend-waivers.toml"),
            WaiverSource::Operator,
        )?);
    }
    waivers.extend(read_waiver_document(
        &repo.join(".zirv/frontend-waivers.toml"),
        WaiverSource::Repository,
    )?);
    Ok(waivers)
}

fn waiver_matches(waiver: &WaiverRule, finding: &DetectorFinding) -> bool {
    if waiver.rule_id != finding.rule_id {
        return false;
    }
    if let Some(pattern) = waiver.path.as_deref() {
        let finding_path = finding.path.to_string_lossy().replace('\\', "/");
        let pattern = pattern.replace('\\', "/");
        let matches = pattern.strip_suffix("/**").map_or_else(
            || finding_path == pattern,
            |prefix| finding_path == prefix || finding_path.starts_with(&format!("{prefix}/")),
        );
        if !matches {
            return false;
        }
    }
    waiver
        .value
        .as_ref()
        .is_none_or(|value| finding.evidence.contains(value))
}

fn apply_waivers(findings: &mut [DetectorFinding], waivers: &[LoadedWaiver]) -> usize {
    let mut rejected = 0;
    for finding in findings {
        for waiver in waivers {
            if !waiver_matches(&waiver.rule, finding) {
                continue;
            }
            let applied = waiver.source == WaiverSource::Operator
                || finding.severity != FindingSeverity::Blocking;
            finding.waiver = Some(WaiverProvenance {
                source: waiver.source,
                config_path: waiver.config_path.clone(),
                reason: waiver.rule.reason.clone(),
                applied,
            });
            if applied {
                finding.disposition = FindingDisposition::Waived;
            } else {
                rejected += 1;
            }
            break;
        }
    }
    rejected
}

fn is_emoji(value: char) -> bool {
    matches!(
        value as u32,
        0x1f300..=0x1faff | 0x2600..=0x27bf
    )
}

fn write_report(writer: &mut impl Write, report: &DetectorReport, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, report)?;
        writeln!(writer)?;
        return Ok(());
    }
    writeln!(
        writer,
        "frontend detector: {} ({} files, {} blocking, {} total)",
        if report.passed() { "passed" } else { "failed" },
        report.analyzed_files.len(),
        report.blocking_count(),
        report.findings.len()
    )?;
    if report.not_applicable {
        writeln!(
            writer,
            "frontend gate not applicable (0 frontend files in scope)"
        )?;
    }
    if report.scope == DetectorScope::All {
        writeln!(
            writer,
            "introduced {} blocking / pre-existing {} blocking",
            report.introduced_blocking_count(),
            report.preexisting_blocking_count()
        )?;
    }
    for finding in &report.findings {
        writeln!(
            writer,
            "{:?}\t{}:{}\t{}\t{}",
            finding.severity,
            finding.path.display(),
            finding.line,
            finding.rule_id,
            finding.summary
        )?;
        if let Some(waiver) = &finding.waiver {
            writeln!(
                writer,
                "  waiver: {} {:?} {} ({})",
                if waiver.applied {
                    "applied"
                } else {
                    "rejected"
                },
                waiver.source,
                waiver.reason,
                waiver.config_path.display()
            )?;
        }
    }
    if report.truncated {
        writeln!(writer, "warning: detector input or findings were truncated")?;
    }
    Ok(())
}

pub fn run(args: &DetectorArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = args.repo.clone().unwrap_or(std::env::current_dir()?);
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let report = detect(&state, &repo, &args.paths, args.all)?;
    write_report(writer, &report, args.json)?;
    Ok(if report.passed() && !report.truncated {
        0
    } else {
        1
    })
}

fn push_benchmark_case(
    results: &mut Vec<BenchmarkCaseResult>,
    covered: &mut BTreeSet<String>,
    name: &str,
    path: &Path,
    source: &str,
    expected: &[&str],
) {
    let mut findings = Vec::new();
    analyze(path, source, &mut findings);
    let mut observed_rules = findings
        .into_iter()
        .map(|finding| finding.rule_id)
        .collect::<Vec<_>>();
    observed_rules.sort();
    observed_rules.dedup();
    let mut expected_rules = expected
        .iter()
        .map(|rule| (*rule).to_string())
        .collect::<Vec<_>>();
    expected_rules.sort();
    covered.extend(expected_rules.iter().cloned());
    results.push(BenchmarkCaseResult {
        name: name.into(),
        passed: observed_rules == expected_rules,
        expected_rules,
        observed_rules,
    });
}

pub fn benchmark() -> BenchmarkReport {
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "semantic-control",
            "<button type=\"button\">Save changes</button>",
            &[],
        ),
        (
            "missing-image-alt",
            "<img src={avatar} />",
            &["a11y/image-alt", "media/image-dimensions"],
        ),
        (
            "non-semantic-action",
            "<div onClick={save}>Save</div>",
            &["a11y/semantic-action"],
        ),
        (
            "heading-order",
            "<h1>Account</h1><h3>Security</h3>",
            &["a11y/heading-order"],
        ),
        (
            "autoplay-media",
            "<video autoplay src=\"demo.mp4\"></video>",
            &["a11y/media-autoplay"],
        ),
        (
            "hostile-input-contracts",
            "<meta content=\"width=device-width, maximum-scale=1\"><div tabIndex={2}></div><input id=\"code\" onPaste={(event) => event.preventDefault()}>",
            &[
                "a11y/paste-blocked",
                "a11y/positive-tabindex",
                "a11y/viewport-zoom",
            ],
        ),
        (
            "dynamic-feedback",
            "export function Toast() { return <div>{validationMessage}</div>; }",
            &["a11y/dynamic-announcement"],
        ),
        (
            "removed-focus",
            ".button { outline: none; }",
            &["a11y/focus-visible"],
        ),
        (
            "form-contract",
            "<input type=\"email\" placeholder=\"Email address\">",
            &["form/autocomplete", "ux/placeholder-only-control"],
        ),
        (
            "container-monoculture",
            "rounded-lg shadow-md rounded-lg shadow-md rounded-lg shadow-md rounded-lg shadow-md rounded-lg rounded-lg rounded-lg rounded-lg",
            &["craft/container-overuse"],
        ),
        (
            "pill-monoculture",
            "rounded-full rounded-full rounded-full rounded-full rounded-full",
            &["craft/pill-overuse"],
        ),
        (
            "generic-gradient",
            ".hero { background: linear-gradient(purple, blue); background-clip: text; }",
            &[
                "craft/gradient-text",
                "craft/purple-blue-gradient",
                "craft/unjustified-gradient",
            ],
        ),
        (
            "generic-depth",
            ".card { box-shadow: 4px 4px 0px; color: purple; }",
            &["craft/dark-glow", "craft/hard-offset-shadow"],
        ),
        (
            "decorative-glass",
            "backdrop-filter: blur(8px); backdrop-blur-lg backdrop-blur-md",
            &["craft/decorative-blur"],
        ),
        (
            "emoji-navigation",
            "<nav><button>🚀 Launch</button></nav>",
            &["craft/emoji-chrome"],
        ),
        (
            "interchangeable-font",
            ".title { font-family: Inter, sans-serif; }",
            &["craft/overused-font"],
        ),
        (
            "generic-content",
            "<button>Continue</button><p>Unlock your potential</p><span>Loading...</span>",
            &[
                "content/generic-action",
                "content/generic-ai-copy",
                "content/loading-ellipsis",
            ],
        ),
        (
            "imprecise-motion",
            ".card { width: 20rem; transition: all .2s; animation: bounce 1s infinite; }",
            &[
                "motion/bounce-easing",
                "motion/infinite-animation",
                "motion/layout-properties",
                "motion/reduced-motion",
                "motion/transition-all",
            ],
        ),
        (
            "reduced-motion-covered",
            ".card { transition: opacity .2s; } @media (prefers-reduced-motion: reduce) { .card { transition: none; } }",
            &[],
        ),
        (
            "brittle-responsive-shell",
            "body { overflow-x: hidden; } .dock { position: fixed; bottom: 0; width: 1200px; height: 100vh; }",
            &[
                "responsive/fixed-content-width",
                "responsive/legacy-viewport-height",
                "responsive/overflow-mask",
                "responsive/safe-area",
            ],
        ),
        (
            "uncontained-dialog",
            "<dialog open><button>Close</button></dialog>",
            &["ux/dialog-overscroll"],
        ),
        (
            "render-performance",
            "@font-face { font-family: Brand; src: url(brand.woff2); } const box = node.getBoundingClientRect();",
            &["performance/font-display", "performance/layout-read"],
        ),
        (
            "localization-fragility",
            "margin-left: 1rem; const label = `${count} item${count !== 1 ? 's' : ''}`;",
            &["i18n/manual-plural", "i18n/physical-properties"],
        ),
        (
            "animated-gif",
            "const tutorial = '/assets/tutorial.gif';",
            &["media/animated-gif"],
        ),
        (
            "display-type-excess",
            ".hero { font-size: 8rem; letter-spacing: -0.06em; }",
            &[
                "typography/aggressive-tracking",
                "typography/oversized-display",
            ],
        ),
        (
            "small-touch-target",
            "<button className=\"w-8 h-8\">Menu</button>",
            &["touch/small-target"],
        ),
        (
            "mobile-autofocus",
            "<input id=\"search\" autoFocus>",
            &["ux/autofocus"],
        ),
    ];
    let framework_cases: &[(&str, &str, &str, &[&str])] = &[
        (
            "plain-html-clean",
            "index.html",
            "<img src=\"logo.png\" alt=\"Zirv\" width=\"64\" height=\"64\">",
            &[],
        ),
        (
            "react-semantic-hazard",
            "src/App.tsx",
            "export function App() { return <div onClick={save}>Save</div>; }",
            &["a11y/semantic-action"],
        ),
        (
            "vue-clean",
            "src/App.vue",
            "<template><button type=\"button\">Save changes</button></template>",
            &[],
        ),
        (
            "vue-image-hazard",
            "src/App.vue",
            "<template><img src=\"/hero.png\"></template>",
            &["a11y/image-alt", "media/image-dimensions"],
        ),
        (
            "svelte-clean",
            "src/routes/+page.svelte",
            "<button type=\"button\" on:click={save}>Save changes</button>",
            &[],
        ),
        (
            "svelte-semantic-hazard",
            "src/routes/+page.svelte",
            "<div on:click={save}>Save</div>",
            &["a11y/semantic-action"],
        ),
        (
            "astro-clean",
            "src/pages/index.astro",
            "<img src={hero} alt=\"Product overview\" width=\"1200\" height=\"630\">",
            &[],
        ),
        (
            "astro-layout-shift-hazard",
            "src/pages/index.astro",
            "<img src={hero} alt=\"Product overview\">",
            &["media/image-dimensions"],
        ),
        (
            "dioxus-clean",
            "src/main.rs",
            "rsx! { img { src: \"logo.png\", alt: \"Zirv\", width: \"64\", height: \"64\" } }",
            &[],
        ),
        (
            "dioxus-image-hazard",
            "src/main.rs",
            "rsx! { img { src: \"logo.png\" } }",
            &["a11y/image-alt", "media/image-dimensions"],
        ),
        (
            "next-clean",
            "app/page.tsx",
            "export default function Page() { return <button type=\"button\">Create project</button>; }",
            &[],
        ),
        (
            "nuxt-image-hazard",
            "pages/index.vue",
            "<template><img src=\"/cover.png\" alt=\"Release cover\"></template>",
            &["media/image-dimensions"],
        ),
        (
            "tailwind-touch-target-clean",
            "src/components/Menu.tsx",
            "<button className=\"min-w-11 min-h-11\">Open menu</button>",
            &[],
        ),
        (
            "tailwind-touch-target-hazard",
            "src/components/Menu.tsx",
            "<button className=\"w-8 h-8\">Menu</button>",
            &["touch/small-target"],
        ),
    ];
    let mut results = Vec::new();
    let mut covered = BTreeSet::new();
    for &(name, source, expected) in cases {
        push_benchmark_case(
            &mut results,
            &mut covered,
            name,
            Path::new("benchmark.tsx"),
            source,
            expected,
        );
    }
    for &(name, path, source, expected) in framework_cases {
        push_benchmark_case(
            &mut results,
            &mut covered,
            name,
            Path::new(path),
            source,
            expected,
        );
    }
    let passed = results.iter().filter(|result| result.passed).count();
    let inventory = DETECTOR_RULE_IDS
        .iter()
        .map(|rule| (*rule).to_string())
        .collect::<BTreeSet<_>>();
    let inventory_complete = covered == inventory;
    BenchmarkReport {
        schema_version: 2,
        cases: results.len(),
        passed,
        failed: results.len() - passed + usize::from(!inventory_complete),
        rule_inventory: inventory.len(),
        covered_rules: covered.intersection(&inventory).count(),
        inventory_complete,
        results,
    }
}

pub fn run_benchmark(args: &BenchmarkArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let report = benchmark();
    if args.json {
        serde_json::to_writer_pretty(&mut *writer, &report)?;
        writeln!(writer)?;
    } else {
        writeln!(
            writer,
            "frontend detector benchmark: {}/{} passed",
            report.passed, report.cases
        )?;
        writeln!(
            writer,
            "rule coverage: {}/{} ({})",
            report.covered_rules,
            report.rule_inventory,
            if report.inventory_complete {
                "complete"
            } else {
                "incomplete"
            }
        )?;
        for result in &report.results {
            writeln!(
                writer,
                "{}\t{}",
                if result.passed { "pass" } else { "fail" },
                result.name
            )?;
        }
    }
    Ok(if report.failed == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objective_accessibility_hazards_are_blocking() {
        let mut findings = Vec::new();
        analyze(
            Path::new("src/Card.tsx"),
            "<div onClick={open}><img src={avatar} /></div>",
            &mut findings,
        );

        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.severity == FindingSeverity::Blocking)
                .count(),
            2
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "a11y/image-alt")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "a11y/semantic-action")
        );
    }

    #[test]
    fn anti_slop_signals_are_structured_advisories() {
        let mut findings = Vec::new();
        analyze(
            Path::new("src/Hero.css"),
            ".title { background: linear-gradient(red, blue); background-clip: text; transition: all .2s; }",
            &mut findings,
        );

        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "craft/gradient-text")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "craft/unjustified-gradient")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "motion/transition-all")
        );
    }

    #[test]
    fn explicit_paths_cannot_escape_the_repository() {
        let repo = tempfile::tempdir().expect("repo");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let error = resolve_file(repo.path(), outside.path()).unwrap_err();
        assert!(error.to_string().contains("escapes repository"));
    }

    /// #251: the hand-rolled walk never honored `.gitignore`, so a build
    /// artifact directory a repository never bothered to add to the
    /// hardcoded deny list still got scanned. A git work tree must instead
    /// enumerate via `git ls-files`, which is `.gitignore`-aware.
    #[test]
    fn collect_all_honors_gitignore_via_git_ls_files() {
        let repo = tempfile::tempdir().expect("repo");
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
        std::fs::write(repo.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(repo.path().join("ignored")).unwrap();
        std::fs::write(
            repo.path().join("ignored/Bad.tsx"),
            "export const Bad = () => <img src={x} />;\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("Good.tsx"),
            "export const Good = () => <img src={x} alt=\"\" />;\n",
        )
        .unwrap();
        git(&["add", ".gitignore", "Good.tsx"]);
        git(&["commit", "-q", "-m", "base"]);

        let (paths, truncated) = collect_all(repo.path()).unwrap();
        assert!(!truncated);
        assert!(paths.contains(&PathBuf::from("Good.tsx")), "{paths:?}");
        assert!(
            !paths
                .iter()
                .any(|path| path.to_string_lossy().contains("ignored")),
            "a gitignored directory must not be scanned: {paths:?}"
        );
    }

    /// Reviewer finding: `collect_all_via_git` used to check only whether
    /// `git` could be spawned, not whether `ls-files` actually exited zero
    /// -- a non-zero exit produced an empty stdout, which read as "zero
    /// frontend files in scope" and made the whole scan fail open into a
    /// pass. `git ls-files` against a directory that is not a git
    /// repository (or otherwise unusable) exits non-zero and must instead
    /// signal the caller to fall back, never `Some(([], false))`.
    #[test]
    fn collect_all_via_git_falls_back_on_a_nonzero_exit() {
        let dir = tempfile::tempdir().expect("dir");
        assert_eq!(
            collect_all_via_git(dir.path()).unwrap(),
            None,
            "a non-zero `git ls-files` exit must signal fallback, not an empty result"
        );
    }

    /// End-to-end: `collect_all` itself must still find real frontend files
    /// via the hand-rolled walk when git enumeration is unusable, rather
    /// than silently reporting zero files in scope.
    #[test]
    fn collect_all_falls_back_to_the_directory_walk_when_git_is_unusable() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("Good.tsx"),
            "export const Good = () => <img src={x} alt=\"\" />;\n",
        )
        .unwrap();

        let (paths, truncated) = collect_all(dir.path()).unwrap();
        assert!(!truncated);
        assert!(paths.contains(&PathBuf::from("Good.tsx")), "{paths:?}");
    }

    /// #255 follow-up: a cached `not_applicable` report (0 frontend files in
    /// scope) must be reused as fresh-and-passing, so a Frontend-profile
    /// workflow with no frontend files does not re-run the detector at
    /// every gate check. The ordinary change-fingerprint freshness check
    /// must still apply unchanged: a stale fingerprint still forces a
    /// re-run.
    #[test]
    fn latest_is_fresh_and_passing_reuses_a_cached_not_applicable_report() {
        let repo = tempfile::tempdir().expect("repo");
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

        let root = tempfile::tempdir().expect("state root");
        let state = StateDir::from_root(root.path().to_path_buf());
        let repo_path = repo.path().canonicalize().unwrap();
        let profile = super::super::frontend::ensure_profile(&state, &repo_path).unwrap();
        let fingerprint = super::super::verification::change_fingerprint(&repo_path).unwrap();

        let report = DetectorReport {
            schema_version: DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: repo_path.clone(),
            change_fingerprint: fingerprint,
            profile_fingerprint: profile.source_fingerprint,
            scope: DetectorScope::Changed,
            generated_at: now_secs(),
            analyzed_files: Vec::new(),
            analyzed_bytes: 0,
            truncated: false,
            findings: Vec::new(),
            waivers_loaded: 0,
            waivers_rejected: 0,
            not_applicable: true,
        };
        save_report(&state, &report).unwrap();

        assert!(
            latest_is_fresh_and_passing(&state, &repo_path).unwrap(),
            "a cached not-applicable report must be reused as fresh and passing"
        );

        // A further change moves the fingerprint out from under the cached
        // report; it must no longer be considered fresh even though it is
        // still `not_applicable`.
        std::fs::write(repo.path().join("other.txt"), "changed\n").unwrap();
        assert!(
            !latest_is_fresh_and_passing(&state, &repo_path).unwrap(),
            "a stale change fingerprint must still force a re-run"
        );
    }

    #[test]
    fn benchmark_corpus_has_no_detector_drift() {
        let report = benchmark();
        assert_eq!(report.failed, 0, "{:#?}", report.results);
        assert!(report.inventory_complete);
        assert_eq!(report.covered_rules, detector_rule_count());
    }

    #[test]
    fn blocking_attribute_rules_do_not_accept_substrings_or_prefix_values() {
        let mut findings = Vec::new();
        analyze(
            Path::new("precision.tsx"),
            r#"<meta content="maximum-scale=10"><img data-alt="caption" width="10" height="10"><div data-onClick={save} data-tabIndex={2}></div><video data-autoplay muted></video><video autoplay muted></video>"#,
            &mut findings,
        );
        let rules = findings
            .iter()
            .map(|finding| finding.rule_id.as_str())
            .collect::<BTreeSet<_>>();

        assert!(rules.contains("a11y/image-alt"));
        assert!(!rules.contains("a11y/media-autoplay"));
        assert!(!rules.contains("a11y/positive-tabindex"));
        assert!(!rules.contains("a11y/semantic-action"));
        assert!(!rules.contains("a11y/viewport-zoom"));
    }

    #[test]
    fn explicitly_false_muted_media_remains_blocking() {
        let mut findings = Vec::new();
        analyze(
            Path::new("media.tsx"),
            "<video autoplay muted={false}></video>",
            &mut findings,
        );

        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "a11y/media-autoplay")
        );
    }

    #[test]
    fn unrelated_width_does_not_look_like_layout_motion() {
        let mut findings = Vec::new();
        analyze(
            Path::new("precision.css"),
            ".card { width: 20rem; transition: opacity .2s; } @media (prefers-reduced-motion: reduce) { .card { transition: none; } }",
            &mut findings,
        );

        assert!(
            !findings
                .iter()
                .any(|finding| finding.rule_id == "motion/layout-properties")
        );
    }

    #[test]
    fn dioxus_rsx_is_discovered_and_analyzed_as_frontend_source() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(
            repo.path().join("Dioxus.toml"),
            "[application]\nname='demo'\n",
        )
        .expect("dioxus");
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n[dependencies]\ndioxus='0.7'\n",
        )
        .expect("cargo");
        std::fs::create_dir_all(repo.path().join("src")).expect("src");
        let relative = Path::new("src/main.rs");
        assert!(is_frontend_source(repo.path(), relative));

        let mut findings = Vec::new();
        analyze(
            relative,
            r#"fn app() -> Element { rsx! { img { src: asset!("/hero.png") } } }"#,
            &mut findings,
        );
        assert!(findings.iter().any(|finding| {
            finding.rule_id == "a11y/image-alt" && finding.severity == FindingSeverity::Blocking
        }));
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "media/image-dimensions")
        );
    }

    #[test]
    fn repository_waivers_are_value_scoped_and_cannot_hide_blocking_rules() {
        let root = tempfile::tempdir().expect("root");
        let config = root.path().join("frontend-waivers.toml");
        std::fs::write(
            &config,
            r#"schema_version = 1

[[waivers]]
rule_id = "craft/gradient-text"
path = "src/Hero.css"
value = "linear-gradient"
reason = "Established campaign treatment approved by design."

[[waivers]]
rule_id = "a11y/image-alt"
path = "src/Hero.tsx"
reason = "Repository cannot waive operator-required accessibility rules."
"#,
        )
        .expect("waivers");
        let waivers = read_waiver_document(&config, WaiverSource::Repository).expect("load");
        let mut findings = Vec::new();
        analyze(
            Path::new("src/Hero.css"),
            ".title { background: linear-gradient(red, blue); background-clip: text; }",
            &mut findings,
        );
        analyze(
            Path::new("src/Hero.tsx"),
            "<img src={hero} width=\"10\" height=\"10\">",
            &mut findings,
        );

        assert_eq!(apply_waivers(&mut findings, &waivers), 1);
        let advisory = findings
            .iter()
            .find(|finding| finding.rule_id == "craft/gradient-text")
            .expect("advisory");
        assert_eq!(advisory.disposition, FindingDisposition::Waived);
        assert!(
            advisory
                .waiver
                .as_ref()
                .is_some_and(|waiver| waiver.applied)
        );
        let blocking = findings
            .iter()
            .find(|finding| finding.rule_id == "a11y/image-alt")
            .expect("blocking");
        assert!(blocking.blocks());
        assert!(
            blocking
                .waiver
                .as_ref()
                .is_some_and(|waiver| !waiver.applied)
        );
    }

    #[test]
    fn operator_waiver_can_explicitly_dispose_a_blocking_finding() {
        let root = tempfile::tempdir().expect("root");
        let config = root.path().join("frontend-waivers.toml");
        std::fs::write(
            &config,
            r#"schema_version = 1

[[waivers]]
rule_id = "a11y/image-alt"
path = "src/decorative.tsx"
reason = "Operator accepted this generated decorative surface temporarily."
"#,
        )
        .expect("waivers");
        let waivers = read_waiver_document(&config, WaiverSource::Operator).expect("load");
        let mut findings = Vec::new();
        analyze(
            Path::new("src/decorative.tsx"),
            "<img src={decoration} width=\"10\" height=\"10\">",
            &mut findings,
        );
        assert_eq!(apply_waivers(&mut findings, &waivers), 0);
        assert!(!findings[0].blocks());
        assert_eq!(findings[0].disposition, FindingDisposition::Waived);
    }

    #[test]
    fn waiver_reasons_are_mandatory() {
        let root = tempfile::tempdir().expect("root");
        let config = root.path().join("frontend-waivers.toml");
        std::fs::write(
            &config,
            "schema_version=1\n[[waivers]]\nrule_id='craft/gradient-text'\nreason='  '\n",
        )
        .expect("waivers");
        let error = read_waiver_document(&config, WaiverSource::Repository)
            .expect_err("blank reason must fail");
        assert!(error.to_string().contains("reason"));
    }
}
