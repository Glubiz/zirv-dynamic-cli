//! Offline deterministic frontend quality detector.
//!
//! These rules deliberately avoid model or network calls. They identify
//! objective accessibility hazards and high-signal AI-UI defaults, producing
//! structured evidence that a later visual review can confirm or disposition.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::{Args, ValueEnum};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

pub const DETECTOR_REPORT_SCHEMA_VERSION: u32 = 1;
const MAX_FILES: usize = 256;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_FINDINGS: usize = 256;

static IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<img\b[^>]*>").expect("valid image regex"));
static ALT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\balt\s*="#).expect("valid alt regex"));
static CLICK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(div|span)\b[^>]*(onclick|on:click)\s*=")
        .expect("valid click target regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingSeverity {
    Advisory,
    Blocking,
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
}

impl DetectorReport {
    pub fn passed(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|finding| finding.severity == FindingSeverity::Blocking)
    }

    pub fn blocking_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == FindingSeverity::Blocking)
            .count()
    }
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

fn report_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state
        .frontend()
        .join(repo_slug(repo))
        .join("detector")
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
        && !report.analyzed_files.is_empty()
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
    let profile = super::frontend::ensure_profile(state, &repo)?;
    let mut truncated = false;
    let (scope, mut paths) = if !requested.is_empty() {
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
    paths.retain(|path| is_frontend_source(path));
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
        let (relative, absolute) = resolve_file(&repo, &requested_path)?;
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

    let change_fingerprint = super::verification::change_fingerprint(&repo)?;
    let report = DetectorReport {
        schema_version: DETECTOR_REPORT_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        repo,
        change_fingerprint,
        profile_fingerprint: profile.source_fingerprint,
        scope,
        generated_at: now_secs(),
        analyzed_files,
        analyzed_bytes,
        truncated,
        findings,
    };
    save_report(state, &report)?;
    Ok(report)
}

fn resolve_file(repo: &Path, requested: &Path) -> CtxResult<(PathBuf, PathBuf)> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        repo.join(requested)
    };
    let normalized = absolute
        .canonicalize()
        .unwrap_or_else(|_| absolute.clone());
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

fn collect_all(repo: &Path) -> CtxResult<(Vec<PathBuf>, bool)> {
    let mut paths = Vec::new();
    let mut entries = 0usize;
    let mut truncated = false;
    collect_directory(
        repo,
        repo,
        &mut entries,
        &mut paths,
        &mut truncated,
    )?;
    Ok((paths, truncated))
}

fn collect_directory(
    repo: &Path,
    directory: &Path,
    entries: &mut usize,
    paths: &mut Vec<PathBuf>,
    truncated: &mut bool,
) -> CtxResult<()> {
    if *entries >= 4_096 || paths.len() >= MAX_FILES {
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
            if !matches!(
                name.to_str(),
                Some(
                    ".git"
                        | ".zirv"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | ".next"
                        | ".cache"
                        | "vendor"
                )
            ) {
                collect_directory(repo, &path, entries, paths, truncated)?;
            }
        } else if metadata.is_file() {
            let relative = path.strip_prefix(repo).unwrap_or(&path).to_path_buf();
            if is_frontend_source(&relative) {
                paths.push(relative);
            }
        }
    }
    Ok(())
}

fn is_frontend_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some(
            "css"
                | "scss"
                | "sass"
                | "less"
                | "html"
                | "tsx"
                | "jsx"
                | "vue"
                | "svelte"
                | "astro"
        )
    )
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
    });
}

fn analyze(path: &Path, text: &str, findings: &mut Vec<DetectorFinding>) {
    let lower = text.to_ascii_lowercase();
    for image in IMAGE_RE.find_iter(text) {
        if !ALT_RE.is_match(image.as_str()) {
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
    for phrase in [
        "lorem ipsum",
        "unlock your potential",
        "seamless experience",
        "revolutionize your",
        "supercharge your",
        "welcome to the future",
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
    }
    if report.truncated {
        writeln!(writer, "warning: detector input or findings were truncated")?;
    }
    Ok(())
}

pub fn run(args: &DetectorArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = args
        .repo
        .clone()
        .unwrap_or(std::env::current_dir()?);
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let report = detect(&state, &repo, &args.paths, args.all)?;
    write_report(writer, &report, args.json)?;
    Ok(if report.passed() && !report.truncated {
        0
    } else {
        1
    })
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
        assert!(findings.iter().any(|finding| finding.rule_id == "a11y/image-alt"));
        assert!(findings.iter().any(|finding| finding.rule_id == "a11y/semantic-action"));
    }

    #[test]
    fn anti_slop_signals_are_structured_advisories() {
        let mut findings = Vec::new();
        analyze(
            Path::new("src/Hero.css"),
            ".title { background: linear-gradient(red, blue); background-clip: text; transition: all .2s; }",
            &mut findings,
        );

        assert!(findings.iter().any(|finding| finding.rule_id == "craft/gradient-text"));
        assert!(findings.iter().any(|finding| finding.rule_id == "craft/unjustified-gradient"));
        assert!(findings.iter().any(|finding| finding.rule_id == "motion/transition-all"));
    }

    #[test]
    fn explicit_paths_cannot_escape_the_repository() {
        let repo = tempfile::tempdir().expect("repo");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let error = resolve_file(repo.path(), outside.path()).unwrap_err();
        assert!(error.to_string().contains("escapes repository"));
    }
}
