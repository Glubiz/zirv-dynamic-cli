//! Zero-touch frontend profile inference and inspection.
//!
//! A frontend workflow must not begin with a questionnaire or an init step.
//! Zirv derives a compact profile from bounded repository evidence and gives
//! the active model authority to resolve whatever the repository does not
//! answer. The profile is cached outside the checkout and refreshed whenever
//! its source fingerprint changes.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

pub const FRONTEND_PROFILE_SCHEMA_VERSION: u32 = 1;
pub const FRONTEND_QUALITY_CONTRACT_VERSION: u32 = 2;
const MAX_SCAN_ENTRIES: usize = 4_096;
const MAX_EVIDENCE_FILES: usize = 256;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_EVIDENCE_PATHS: usize = 32;
const MAX_TOKENS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileBasis {
    ExistingSystem,
    AutonomousBaseline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomyContract {
    pub human_initialization_required: bool,
    pub unresolved_decisions: String,
    pub preservation_rule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendProfile {
    pub schema_version: u32,
    pub repo: PathBuf,
    pub source_fingerprint: String,
    pub generated_at: u64,
    pub basis: ProfileBasis,
    pub direction: String,
    pub typography: String,
    pub color: String,
    pub geometry: String,
    pub motion: String,
    pub density: String,
    pub evidence_paths: Vec<PathBuf>,
    pub observed_fonts: Vec<String>,
    pub observed_colors: Vec<String>,
    pub scan_truncated: bool,
    pub autonomy: AutonomyContract,
}

#[derive(Debug)]
struct RepositoryEvidence {
    fingerprint: String,
    paths: Vec<PathBuf>,
    fonts: Vec<String>,
    colors: Vec<String>,
    has_radius: bool,
    has_shadow: bool,
    has_motion: bool,
    truncated: bool,
}

#[derive(Debug, Args)]
pub struct FrontendArgs {
    #[command(subcommand)]
    pub command: FrontendSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum FrontendSubcommand {
    /// Infer or inspect the repository's autonomous frontend profile.
    Profile(ProfileArgs),
    /// Run Zirv's offline deterministic frontend quality detector.
    Check(super::frontend_detector::DetectorArgs),
    /// Start the app and capture bounded narrow/intermediate/wide evidence.
    Render(super::frontend_render::RenderArgs),
    /// Launch an isolated AI review of fresh render evidence.
    Review(super::frontend_render::VisualReviewArgs),
    /// Inspect provider-neutral frontend capability and skill provenance.
    Capabilities(FrontendCapabilitiesArgs),
    /// Run the built-in deterministic detector benchmark corpus.
    Benchmark(super::frontend_detector::BenchmarkArgs),
}

#[derive(Debug, Args)]
pub struct ProfileArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Rebuild even when the bounded repository fingerprint is unchanged.
    #[arg(long)]
    pub refresh: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct FrontendCapabilitiesArgs {
    /// Agent adapter whose logical capabilities should be resolved.
    #[arg(long)]
    pub agent: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct FrontendCapabilities {
    schema_version: u32,
    adapter: String,
    profile: &'static str,
    quality_contract_version: u32,
    detector: &'static str,
    detector_rules: usize,
    review_dimensions: Vec<&'static str>,
    surface_modes: Vec<&'static str>,
    phase_skills: Vec<&'static str>,
    local_browser: Option<String>,
    capabilities: super::capability::CapabilityReport,
}

fn capabilities_for(adapter: &str, probe_browser: bool) -> FrontendCapabilities {
    FrontendCapabilities {
        schema_version: 2,
        adapter: adapter.to_string(),
        profile: "frontend-profile@1 + quality-contract@2 (built-in)",
        quality_contract_version: FRONTEND_QUALITY_CONTRACT_VERSION,
        detector: "frontend-detector@3 (built-in, offline)",
        detector_rules: super::frontend_detector::detector_rule_count(),
        review_dimensions: super::frontend_render::REVIEW_DIMENSIONS.to_vec(),
        surface_modes: vec!["persuade", "operate", "read", "experience"],
        phase_skills: vec![
            "frontend-craft@1",
            "frontend-design@1",
            "frontend-plan@1",
            "frontend-implement@1",
            "frontend-debug@1",
            "frontend-test@1",
            "frontend-review@1",
            "frontend-verify@1",
        ],
        local_browser: probe_browser
            .then(super::frontend_render::discover_browser)
            .flatten(),
        capabilities: super::capability::CapabilityReport::for_adapter(adapter),
    }
}

fn write_capabilities(
    writer: &mut impl Write,
    report: &FrontendCapabilities,
    json: bool,
) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, report)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "frontend adapter: {}", report.adapter)?;
        writeln!(writer, "profile: {}", report.profile)?;
        writeln!(
            writer,
            "quality contract: {}",
            report.quality_contract_version
        )?;
        writeln!(writer, "detector: {}", report.detector)?;
        writeln!(writer, "detector rules: {}", report.detector_rules)?;
        writeln!(writer, "surface modes: {}", report.surface_modes.join(", "))?;
        writeln!(
            writer,
            "review dimensions: {}",
            report.review_dimensions.join(", ")
        )?;
        writeln!(writer, "skills: {}", report.phase_skills.join(", "))?;
        writeln!(
            writer,
            "local browser: {}",
            report.local_browser.as_deref().unwrap_or("unavailable")
        )?;
        for status in &report.capabilities.statuses {
            writeln!(writer, "{}: {}", status.capability, status.support)?;
        }
    }
    Ok(())
}

fn profile_path(state: &StateDir, repo: &Path) -> PathBuf {
    state.frontend().join(repo_slug(repo)).join("profile.json")
}

fn canonical_repo(repo: &Path) -> PathBuf {
    repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf())
}

fn load(state: &StateDir, repo: &Path) -> CtxResult<Option<FrontendProfile>> {
    let path = profile_path(state, repo);
    if !path.exists() {
        return Ok(None);
    }
    let profile: FrontendProfile = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if profile.schema_version != FRONTEND_PROFILE_SCHEMA_VERSION {
        return Ok(None);
    }
    if canonical_repo(&profile.repo) != canonical_repo(repo) {
        return Ok(None);
    }
    Ok(Some(profile))
}

fn save(state: &StateDir, profile: &FrontendProfile) -> CtxResult<()> {
    let path = profile_path(state, &profile.repo);
    let parent = path.parent().ok_or("frontend profile has no parent")?;
    create_private_dir_all(parent)?;
    write_private(&path, &serde_json::to_string_pretty(profile)?)?;
    Ok(())
}

/// Load a current profile or infer one without asking the operator anything.
pub fn ensure_profile(state: &StateDir, repo: &Path) -> CtxResult<FrontendProfile> {
    let repo = canonical_repo(repo);
    let evidence = scan_repository(&repo)?;
    if let Some(profile) = load(state, &repo)?
        && profile.source_fingerprint == evidence.fingerprint
    {
        return Ok(profile);
    }
    let profile = synthesize_profile(repo, evidence);
    save(state, &profile)?;
    Ok(profile)
}

pub fn refresh_profile(state: &StateDir, repo: &Path) -> CtxResult<FrontendProfile> {
    let repo = canonical_repo(repo);
    let evidence = scan_repository(&repo)?;
    let profile = synthesize_profile(repo, evidence);
    save(state, &profile)?;
    Ok(profile)
}

fn synthesize_profile(repo: PathBuf, evidence: RepositoryEvidence) -> FrontendProfile {
    let existing = !evidence.paths.is_empty();
    FrontendProfile {
        schema_version: FRONTEND_PROFILE_SCHEMA_VERSION,
        repo,
        source_fingerprint: evidence.fingerprint,
        generated_at: now_secs(),
        basis: if existing {
            ProfileBasis::ExistingSystem
        } else {
            ProfileBasis::AutonomousBaseline
        },
        direction: if existing {
            "Extend the repository's strongest established visual language; remove local inconsistency instead of introducing a competing system."
        } else {
            "Choose one confident, product-specific visual concept from the task and content; avoid a generic dashboard or landing-page template."
        }
        .into(),
        typography: if evidence.fonts.is_empty() {
            "Select an intentional type pairing or family suited to the product; establish a restrained, legible hierarchy and never default to interchangeable AI typography."
                .into()
        } else {
            format!(
                "Preserve and rationalize the observed type system: {}.",
                evidence.fonts.join(", ")
            )
        },
        color: if evidence.colors.is_empty() {
            "Derive a compact semantic palette from the product context with accessible contrast; use accents sparingly and avoid default purple-blue gradients."
                .into()
        } else {
            format!(
                "Reuse the observed palette as evidence, promoting only semantic roles: {}.",
                evidence.colors.join(", ")
            )
        },
        geometry: match (evidence.has_radius, evidence.has_shadow) {
            (true, true) => "Match the existing radius and elevation grammar; do not turn every region into a floating rounded card.",
            (true, false) => "Preserve the existing radius rhythm and prefer borders, spacing, or tone over decorative shadows.",
            (false, true) => "Use the existing elevation language selectively and keep geometry crisp unless content requires softness.",
            (false, false) => "Choose one restrained geometry rule and create hierarchy with layout before containers or effects.",
        }
        .into(),
        motion: if evidence.has_motion {
            "Preserve purposeful existing motion, honor reduced-motion preferences, and animate state or spatial change rather than decoration."
        } else {
            "Add motion only when it explains state, hierarchy, or continuity; keep it subtle and fully reduced-motion safe."
        }
        .into(),
        density: "Infer density from the product's information and interaction load; prioritize scanability without excessive whitespace or cramped controls."
            .into(),
        evidence_paths: evidence.paths,
        observed_fonts: evidence.fonts,
        observed_colors: evidence.colors,
        scan_truncated: evidence.truncated,
        autonomy: AutonomyContract {
            human_initialization_required: false,
            unresolved_decisions: "The active AI agent must resolve missing visual decisions from the task, product semantics, and repository evidence without pausing for initialization or a design vote."
                .into(),
            preservation_rule: "Explicit operator requirements win; otherwise prefer coherent existing product evidence over generated defaults."
                .into(),
        },
    }
}

/// Compact prompt layer; raw files and discarded alternatives stay out of
/// model context.
pub fn render_profile(profile: &FrontendProfile) -> String {
    format!(
        "zirv frontend profile (autonomous; human initialization required: no)\n\
basis: {:?}\n\
direction: {}\n\
typography: {}\n\
color: {}\n\
geometry: {}\n\
motion: {}\n\
density: {}\n\
decision authority: {}\n\
preservation: {}",
        profile.basis,
        profile.direction,
        profile.typography,
        profile.color,
        profile.geometry,
        profile.motion,
        profile.density,
        profile.autonomy.unresolved_decisions,
        profile.autonomy.preservation_rule,
    ) + r#"
quality contract v2 (resolve autonomously; never ask for initialization):
- classify the current surface, not the whole product: persuade = earn a decision; operate = complete a task; read = understand; experience = encounter the work itself. Let that mode set expression, density, motion, and familiarity.
- before code, name the concrete subject, audience, single user job, product truth that cannot be invented, one design thesis, one memorable signature, one justified aesthetic risk, and the category-default arrangement this surface refuses. Preserve an established world; replace it only when the task explicitly calls for redesign.
- derive a compact system for type roles, semantic color, spacing rhythm, geometry, elevation, imagery/iconography, and motion. Spend boldness in one place. Every structural or decorative device must encode content, state, or the chosen world.
- design the whole journey: arrival, primary path, decision points, feedback, cancellation/undo, loading, empty, partial, error, success, disabled, permission, offline/slow, long-content, localization/RTL, keyboard, touch, zoom, reduced-motion, narrow, intermediate, and wide behavior where relevant.
- evaluate with the interchangeable-product test plus hierarchy, system coherence, typography, color/contrast, layout rhythm, interaction affordance, state completeness, responsive composition, accessibility, content clarity, and resilience. A clean detector is a floor, not proof of quality.
- verification is bounded: build fully, inspect all device captures together, batch fixes, confirm once, then stop. Missing or stale visual evidence never becomes a pass."#
}

fn scan_repository(repo: &Path) -> CtxResult<RepositoryEvidence> {
    let mut candidates = Vec::new();
    let mut entries = 0usize;
    let mut truncated = false;
    collect_candidates(repo, repo, 0, &mut entries, &mut candidates, &mut truncated)?;
    candidates.sort();
    candidates.truncate(MAX_EVIDENCE_FILES);

    let mut hash = StableHash::new();
    let mut paths = Vec::new();
    let mut fonts = BTreeSet::new();
    let mut colors = BTreeSet::new();
    let mut has_radius = false;
    let mut has_shadow = false;
    let mut has_motion = false;
    let mut total = 0usize;
    for path in candidates {
        if total >= MAX_TOTAL_BYTES {
            truncated = true;
            break;
        }
        let relative = path.strip_prefix(repo).unwrap_or(&path).to_path_buf();
        let remaining = MAX_TOTAL_BYTES - total;
        let sample = read_bounded(&path, remaining.min(MAX_FILE_BYTES as usize))?;
        total = total.saturating_add(sample.len());
        hash.write(relative.to_string_lossy().as_bytes());
        hash.write(&sample);
        let text = String::from_utf8_lossy(&sample);
        extract_fonts(&text, &mut fonts);
        extract_colors(&text, &mut colors);
        let lower = text.to_ascii_lowercase();
        has_radius |= lower.contains("border-radius") || lower.contains("rounded-");
        has_shadow |= lower.contains("box-shadow") || lower.contains("shadow-");
        has_motion |= lower.contains("transition") || lower.contains("animation");
        if paths.len() < MAX_EVIDENCE_PATHS {
            paths.push(relative);
        }
    }
    hash.write(if truncated { b"truncated" } else { b"complete" });

    Ok(RepositoryEvidence {
        fingerprint: hash.finish(),
        paths,
        fonts: fonts.into_iter().take(MAX_TOKENS).collect(),
        colors: colors.into_iter().take(MAX_TOKENS).collect(),
        has_radius,
        has_shadow,
        has_motion,
        truncated,
    })
}

fn collect_candidates(
    root: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut usize,
    candidates: &mut Vec<PathBuf>,
    truncated: &mut bool,
) -> CtxResult<()> {
    if depth > 12 || *entries >= MAX_SCAN_ENTRIES || candidates.len() >= MAX_EVIDENCE_FILES {
        *truncated = true;
        return Ok(());
    }
    let mut children = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        *entries += 1;
        if *entries > MAX_SCAN_ENTRIES || candidates.len() >= MAX_EVIDENCE_FILES {
            *truncated = true;
            break;
        }
        let path = child.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = child.file_name();
            let name = name.to_string_lossy();
            if !matches!(
                name.as_ref(),
                ".git"
                    | ".zirv"
                    | "node_modules"
                    | "target"
                    | "dist"
                    | "build"
                    | ".next"
                    | ".cache"
                    | "vendor"
            ) {
                collect_candidates(root, &path, depth + 1, entries, candidates, truncated)?;
            }
        } else if metadata.is_file()
            && metadata.len() <= MAX_FILE_BYTES
            && is_frontend_evidence(root, &path)
        {
            candidates.push(path);
        }
    }
    Ok(())
}

fn is_frontend_evidence(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if matches!(
        name,
        "package.json"
            | "tailwind.config.js"
            | "tailwind.config.ts"
            | "vite.config.js"
            | "vite.config.ts"
            | "next.config.js"
            | "next.config.mjs"
    ) {
        return true;
    }
    let extension = path.extension().and_then(|value| value.to_str());
    matches!(
        extension,
        Some("css" | "scss" | "sass" | "less" | "html" | "tsx" | "jsx" | "vue" | "svelte")
    ) || relative.components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some("components" | "ui" | "styles")
        )
    })
}

fn read_bounded(path: &Path, cap: usize) -> CtxResult<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(u64::try_from(cap).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn extract_fonts(text: &str, values: &mut BTreeSet<String>) {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(position) = lower.find("font-family") else {
            continue;
        };
        let Some((_, value)) = line[position..].split_once(':') else {
            continue;
        };
        let value = value
            .split([';', '}'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(|character| character == '\'' || character == '"');
        if !value.is_empty() && value.len() <= 96 {
            values.insert(value.to_string());
        }
    }
}

fn extract_colors(text: &str, values: &mut BTreeSet<String>) {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b'#' {
            continue;
        }
        let length = (3..=8)
            .rev()
            .find(|length| {
                start + 1 + length <= bytes.len()
                    && bytes[start + 1..start + 1 + length]
                        .iter()
                        .all(u8::is_ascii_hexdigit)
            })
            .unwrap_or(0);
        if matches!(length, 3 | 4 | 6 | 8) {
            values.insert(text[start..start + 1 + length].to_ascii_lowercase());
        }
    }
}

struct StableHash(u64);

impl StableHash {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(&self) -> String {
        format!("{:016x}", self.0)
    }
}

fn write_profile(writer: &mut impl Write, profile: &FrontendProfile, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, profile)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "{}", render_profile(profile))?;
        writeln!(writer, "fingerprint: {}", profile.source_fingerprint)?;
        writeln!(writer, "evidence files: {}", profile.evidence_paths.len())?;
    }
    Ok(())
}

pub fn run(args: &FrontendArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        FrontendSubcommand::Profile(args) => {
            let repo = args.repo.clone().unwrap_or(std::env::current_dir()?);
            let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
            let profile = if args.refresh {
                refresh_profile(&state, &repo)?
            } else {
                ensure_profile(&state, &repo)?
            };
            write_profile(writer, &profile, args.json)?;
        }
        FrontendSubcommand::Check(args) => {
            return super::frontend_detector::run(args, writer);
        }
        FrontendSubcommand::Render(args) => {
            return super::frontend_render::run_render(args, writer);
        }
        FrontendSubcommand::Review(args) => {
            return super::frontend_render::run_review(args, writer);
        }
        FrontendSubcommand::Capabilities(args) => {
            let report = capabilities_for(&args.agent, true);
            write_capabilities(writer, &report, args.json)?;
        }
        FrontendSubcommand::Benchmark(args) => {
            return super::frontend_detector::run_benchmark(args, writer);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_bootstraps_without_operator_input_and_reuses_current_cache() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(
            repo.path().join("app.css"),
            "body { font-family: 'Sora'; color: #123456; border-radius: 8px; }",
        )
        .expect("css");
        let root = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(root.path().to_path_buf());

        let first = ensure_profile(&state, repo.path()).expect("profile");
        let second = ensure_profile(&state, repo.path()).expect("cached profile");

        assert!(!first.autonomy.human_initialization_required);
        assert_eq!(first, second);
        assert_eq!(first.basis, ProfileBasis::ExistingSystem);
        assert!(
            first
                .observed_fonts
                .iter()
                .any(|font| font.contains("Sora"))
        );
        assert!(first.observed_colors.contains(&"#123456".to_string()));
    }

    #[test]
    fn profile_refreshes_when_repository_evidence_changes() {
        let repo = tempfile::tempdir().expect("repo");
        let css = repo.path().join("app.css");
        std::fs::write(&css, "body { color: #111111; }").expect("css");
        let root = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(root.path().to_path_buf());
        let first = ensure_profile(&state, repo.path()).expect("profile");

        std::fs::write(&css, "body { color: #eeeeee; }").expect("changed css");
        let second = ensure_profile(&state, repo.path()).expect("refreshed profile");

        assert_ne!(first.source_fingerprint, second.source_fingerprint);
        assert!(second.observed_colors.contains(&"#eeeeee".to_string()));
    }

    #[test]
    fn empty_repository_gets_an_autonomous_baseline() {
        let repo = tempfile::tempdir().expect("repo");
        let root = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(root.path().to_path_buf());

        let profile = ensure_profile(&state, repo.path()).expect("profile");
        let context = render_profile(&profile);

        assert_eq!(profile.basis, ProfileBasis::AutonomousBaseline);
        assert!(context.contains("human initialization required: no"));
        assert!(context.contains("without pausing for initialization or a design vote"));
    }

    #[test]
    fn claude_and_codex_get_the_same_frontend_skill_and_capability_contract() {
        let claude = capabilities_for("claude", false);
        let codex = capabilities_for("codex", false);

        assert_eq!(claude.phase_skills, codex.phase_skills);
        assert_eq!(claude.profile, codex.profile);
        assert_eq!(claude.quality_contract_version, 2);
        assert_eq!(claude.detector, codex.detector);
        assert_eq!(claude.detector_rules, 44);
        assert_eq!(
            claude.review_dimensions.as_slice(),
            super::super::frontend_render::REVIEW_DIMENSIONS
        );
        assert_eq!(claude.surface_modes.len(), 4);
        assert_eq!(claude.capabilities.statuses, codex.capabilities.statuses);
    }
}
