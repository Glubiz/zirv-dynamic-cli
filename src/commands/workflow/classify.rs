//! Deterministic intent, complexity, and risk classification.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;

const MAX_TASK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Intent {
    Feature,
    Bugfix,
    Refactor,
    Spike,
    Review,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Complexity {
    Trivial,
    Bounded,
    Substantial,
    Architectural,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RiskBand {
    Low,
    Medium,
    High,
    Critical,
}

/// Whether the Git-based safety net that re-measures a declared or
/// previously-classified risk band actually ran. `Unavailable` is a distinct
/// state from "measured, no escalation needed" -- collapsing the two used to
/// let a mis-declared low-risk scope stand unchallenged in exactly the case
/// zirv can see least (outside a repository, or one with no commits).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskMeasurement {
    #[default]
    Measured,
    Unavailable {
        reason: String,
    },
}

/// The safer default when the net cannot run at all: raise the band one
/// step rather than trust an unmeasured declaration. `Critical` has nowhere
/// further to go.
pub(crate) fn escalate_one_band(band: RiskBand) -> RiskBand {
    match band {
        RiskBand::Low => RiskBand::Medium,
        RiskBand::Medium => RiskBand::High,
        RiskBand::High => RiskBand::Critical,
        RiskBand::Critical => RiskBand::Critical,
    }
}

fn score_floor_for_band(band: RiskBand) -> u16 {
    match band {
        RiskBand::Low => 0,
        RiskBand::Medium => 20,
        RiskBand::High => 45,
        RiskBand::Critical => 70,
    }
}

/// Marks a classification's risk measurement as unavailable and applies the
/// fail-safe policy: escalate the risk band one step (the ceiling, unmoved,
/// when already `Critical`). Returns whether the band actually moved, so a
/// caller that also re-materializes workflow steps on a risk increase can
/// share that path instead of duplicating it. See the Decision Log entry
/// "Unmeasurable risk fails safe, not open" for why escalation was chosen
/// over "keep the declared/prior band but demand its evidence".
pub(crate) fn mark_unavailable(
    classification: &mut Classification,
    reason: impl Into<String>,
) -> bool {
    let reason = reason.into();
    let previous = classification.risk;
    let escalated = escalate_one_band(previous);
    let raised = escalated != previous;
    if raised {
        classification.reasons.push(format!(
            "risk escalated to {escalated:?}: measurement unavailable ({reason})"
        ));
        classification.risk = escalated;
        classification.risk_score = classification
            .risk_score
            .max(score_floor_for_band(escalated));
    } else {
        classification.reasons.push(format!(
            "measurement unavailable ({reason}); risk already at the Critical ceiling"
        ));
    }
    classification.risk_measurement = RiskMeasurement::Unavailable { reason };
    classification.reasons.sort();
    raised
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkDomain {
    #[default]
    General,
    Frontend,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainClassification {
    pub domain: WorkDomain,
    pub score: u8,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub intent: Intent,
    pub complexity: Complexity,
    pub risk: RiskBand,
    pub risk_score: u16,
    pub changed_files: usize,
    pub changed_lines: usize,
    /// The change surface was declared on the command line (`--path`/
    /// `--changed-lines`) rather than measured from Git. Consumers can tell a
    /// measured classification from a stated one; the risk band itself is
    /// never *lower* than the measured tree would give (see
    /// [`from_args`]).
    #[serde(default)]
    pub declared_scope: bool,
    /// Orthogonal work-domain classification. This chooses methodology, not
    /// permissions; older durable state defaults safely to `general`.
    #[serde(default)]
    pub work_domain: DomainClassification,
    /// Whether the Git-based re-measurement that backs `risk` actually ran.
    /// Older durable state defaults safely to `Measured` (its pre-existing,
    /// unlabeled behavior): this field is additive, not a reinterpretation of
    /// history.
    #[serde(default)]
    pub risk_measurement: RiskMeasurement,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ClassificationInput {
    pub task: String,
    pub paths: Vec<PathBuf>,
    pub changed_lines: usize,
    pub tests_changed: bool,
    pub intent_override: Option<Intent>,
    pub complexity_override: Option<Complexity>,
    pub risk_override: Option<RiskBand>,
}

pub fn classify(input: &ClassificationInput) -> CtxResult<Classification> {
    if input.task.len() > MAX_TASK_BYTES {
        return Err(format!("task summary exceeds {MAX_TASK_BYTES} bytes").into());
    }
    let task = input.task.to_ascii_lowercase();
    let intent = input.intent_override.unwrap_or_else(|| infer_intent(&task));
    let work_domain = infer_work_domain(&task, &input.paths);
    let changed_files = input.paths.len();
    let inferred_complexity = infer_complexity(changed_files, input.changed_lines, &input.paths);
    let complexity = input.complexity_override.unwrap_or(inferred_complexity);

    let mut score = 0u16;
    let mut reasons = Vec::new();
    let lowered_paths: Vec<String> = input
        .paths
        .iter()
        .map(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase()
        })
        .collect();
    let mut sensitive_floor: Option<RiskBand> = None;
    // `max`, and derived from the signal's own return value rather than from
    // string-matching the reasons vector: a later signal must never be able to
    // lower a floor an earlier one set, and a reason worded differently must
    // never be able to drop the floor entirely.
    let raise_floor = |band: RiskBand, floor: &mut Option<RiskBand>| {
        *floor = Some(floor.map_or(band, |current: RiskBand| current.max(band)));
    };
    if add_path_signal(
        &lowered_paths,
        &["auth", "security", "permission", "credential", "secret"],
        30,
        "authentication/security surface",
        &mut score,
        &mut reasons,
    ) {
        raise_floor(RiskBand::High, &mut sensitive_floor);
    }
    if add_path_signal(
        &lowered_paths,
        &["migration", "schema", "database", "sql"],
        25,
        "database/schema surface",
        &mut score,
        &mut reasons,
    ) {
        raise_floor(RiskBand::High, &mut sensitive_floor);
    }
    add_path_signal(
        &lowered_paths,
        &[
            "deploy",
            "docker",
            ".github/workflows",
            "terraform",
            "config",
        ],
        20,
        "deployment/configuration surface",
        &mut score,
        &mut reasons,
    );
    add_path_signal(
        &lowered_paths,
        &["concurr", "thread", "async", "lock", "atomic"],
        20,
        "concurrency-sensitive surface",
        &mut score,
        &mut reasons,
    );
    add_path_signal(
        &lowered_paths,
        &["api", "public", "lib.rs", "mod.rs"],
        15,
        "public API/module boundary",
        &mut score,
        &mut reasons,
    );

    let line_points = match input.changed_lines {
        0..=20 => 0,
        21..=150 => 8,
        151..=500 => 18,
        _ => 30,
    };
    if line_points > 0 {
        score += line_points;
        reasons.push(format!(
            "{} changed lines (+{line_points})",
            input.changed_lines
        ));
    }
    if changed_files > 8 {
        score += 15;
        reasons.push(format!("cross-file change: {changed_files} files (+15)"));
    }
    let modules: BTreeSet<_> = lowered_paths
        .iter()
        .filter_map(|path| path.split('/').next())
        .collect();
    if modules.len() > 2 {
        score += 10;
        reasons.push(format!(
            "cross-module impact: {} roots (+10)",
            modules.len()
        ));
    }
    if !input.tests_changed && !matches!(intent, Intent::Spike | Intent::Review) {
        score += 10;
        reasons.push("no changed test path (+10)".to_string());
    }
    score = score.min(100);
    let inferred_risk = band_for_score(score);
    let mut risk = input.risk_override.unwrap_or(inferred_risk);
    if let Some(floor) = sensitive_floor
        && risk < floor
    {
        if input.risk_override.is_some() {
            return Err(format!(
                "risk override '{risk:?}' is below the required High floor for a sensitive surface"
            )
            .into());
        }
        risk = floor;
        reasons.push("sensitive-surface risk floor: High".to_string());
    }
    if let Some(override_band) = input.risk_override {
        reasons.push(format!("operator risk override: {override_band:?}"));
        risk = override_band;
    }
    if let Some(override_complexity) = input.complexity_override {
        reasons.push(format!(
            "operator complexity override: {override_complexity:?}"
        ));
    }
    if reasons.is_empty() {
        reasons.push("small, isolated deterministic change".to_string());
    }
    reasons.sort();

    Ok(Classification {
        intent,
        complexity,
        risk,
        risk_score: score,
        changed_files,
        changed_lines: input.changed_lines,
        declared_scope: false,
        work_domain,
        risk_measurement: RiskMeasurement::Measured,
        reasons,
    })
}

fn infer_work_domain(task: &str, paths: &[PathBuf]) -> DomainClassification {
    let mut score = 0u8;
    let mut reasons = Vec::new();
    let task_terms = [
        "frontend",
        "front-end",
        "user interface",
        " ui ",
        "component",
        "responsive",
        "accessibility",
        "landing page",
        "dashboard",
        "design system",
    ];
    // #255: capped below the 45 selection threshold -- task text alone can
    // no longer select the Frontend domain. The bare word "frontend" shows
    // up in plenty of non-UI work (permission families, CLI flags, docs);
    // Frontend must also see at least one real frontend path signal below.
    if task_terms.iter().any(|term| task.contains(term))
        || task.starts_with("ui ")
        || task.ends_with(" ui")
    {
        score = score.saturating_add(40);
        reasons.push("task describes a frontend or visual surface (+40)".into());
    }

    let lowered = paths
        .iter()
        .map(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase()
        })
        .collect::<Vec<_>>();
    if lowered.iter().any(|path| {
        matches!(
            Path::new(path).extension().and_then(|value| value.to_str()),
            Some("css" | "scss" | "sass" | "less" | "tsx" | "jsx" | "vue" | "svelte" | "html")
        )
    }) {
        score = score.saturating_add(45);
        reasons.push("changed path uses a frontend file type (+45)".into());
    }
    if lowered.iter().any(|path| {
        path.contains("/components/")
            || path.contains("/ui/")
            || path.contains("/styles/")
            || path.starts_with("components/")
            || path.starts_with("ui/")
            || path.starts_with("styles/")
    }) {
        score = score.saturating_add(25);
        reasons.push("changed path is in a component, UI, or styles boundary (+25)".into());
    }
    score = score.min(100);
    reasons.sort();
    DomainClassification {
        domain: if score >= 45 {
            WorkDomain::Frontend
        } else {
            WorkDomain::General
        },
        score,
        reasons,
    }
}

/// `true` when the signal matched, so a caller can act on it structurally
/// rather than by searching the reasons it wrote.
fn add_path_signal(
    paths: &[String],
    needles: &[&str],
    points: u16,
    reason: &str,
    score: &mut u16,
    reasons: &mut Vec<String>,
) -> bool {
    if !paths
        .iter()
        .any(|path| needles.iter().any(|needle| path.contains(needle)))
    {
        return false;
    }
    *score += points;
    reasons.push(reason.to_string());
    true
}

fn infer_intent(task: &str) -> Intent {
    if ["bug", "fix", "failure", "broken", "regression"]
        .iter()
        .any(|word| task.contains(word))
    {
        Intent::Bugfix
    } else if task.contains("refactor") || task.contains("cleanup") {
        Intent::Refactor
    } else if ["spike", "prototype", "explore", "research"]
        .iter()
        .any(|word| task.contains(word))
    {
        Intent::Spike
    } else if task.contains("review") || task.contains("audit") {
        Intent::Review
    } else if ["add", "implement", "feature", "build"]
        .iter()
        .any(|word| task.contains(word))
    {
        Intent::Feature
    } else {
        Intent::Other
    }
}

fn infer_complexity(files: usize, lines: usize, paths: &[PathBuf]) -> Complexity {
    let architectural_path = paths.iter().any(|path| {
        let value = path.to_string_lossy().to_ascii_lowercase();
        value.contains("architecture") || value.contains("migration")
    });
    if architectural_path || files > 15 || lines > 800 {
        Complexity::Architectural
    } else if files > 5 || lines > 250 {
        Complexity::Substantial
    } else if files > 2 || lines > 30 {
        Complexity::Bounded
    } else {
        Complexity::Trivial
    }
}

fn band_for_score(score: u16) -> RiskBand {
    match score {
        0..=19 => RiskBand::Low,
        20..=44 => RiskBand::Medium,
        45..=69 => RiskBand::High,
        _ => RiskBand::Critical,
    }
}

/// True for a repo-relative path whose first component is `.zirv`.
///
/// `.zirv/work/` (workflow work products) and other `.zirv/` state are
/// deliberately not gitignored, so they show up as untracked paths. That
/// state is zirv's own bookkeeping, not the operator's change surface, and
/// must never drive a workflow's classification (a stale
/// `.zirv/work/<id>/*.html` from an earlier workflow has previously flipped
/// unrelated workflows to the Frontend domain).
fn is_zirv_owned_path(path: &Path) -> bool {
    matches!(path.components().next(), Some(std::path::Component::Normal(name)) if name == ".zirv")
}

/// True for a repo-relative path whose first two components are
/// `.zirv/work` -- the workflow's own work-product directory (plans,
/// execute-plan artifacts, raw reviewer salvage). Narrower than
/// `is_zirv_owned_path` above (which also covers `.zirv/ctx.toml` and other
/// repository config a reviewer legitimately needs to see): a review
/// package and its staleness fingerprint must ignore workflow bookkeeping
/// specifically, not every `.zirv/` path, or a real change to
/// `.zirv/commands/*` would silently vanish from what gets reviewed.
///
/// #229/#232: an operator ticking a checkbox in the untracked
/// `.zirv/work/<id>/plan.md` while an independent review ran changed the
/// repository's change-set fingerprint out from under the review, so the
/// completed round was refused with "the change set changed during
/// review" even though nothing the reviewer was asked to look at had
/// changed. `review::package` and `verification::change_fingerprint` both
/// exclude paths this returns true for.
pub(crate) fn is_workflow_work_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Normal(name)) if name == ".zirv")
        && matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "work")
}

pub fn git_change_input(repo: &Path, task: String) -> CtxResult<ClassificationInput> {
    // The same base `review::package` uses (merge-base against origin/main,
    // then main, then HEAD^, then HEAD). Measuring against bare HEAD made
    // classification and review disagree about what "the change" even is:
    // everything already committed on the branch was invisible here.
    let base = super::review::default_base(repo)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--numstat", &base])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect changed paths: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let mut paths = Vec::new();
    let mut lines = 0usize;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.splitn(3, '\t');
        let added = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let removed = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let Some(path) = fields.next() else { continue };
        lines = lines.saturating_add(added).saturating_add(removed);
        paths.push(PathBuf::from(path));
    }
    let untracked = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()?;
    if !untracked.status.success() {
        return Err(format!(
            "cannot inspect untracked paths: {}",
            String::from_utf8_lossy(&untracked.stderr).trim()
        )
        .into());
    }
    for path in String::from_utf8_lossy(&untracked.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| !is_zirv_owned_path(path))
    {
        let absolute = repo.join(&path);
        if let Ok(metadata) = std::fs::symlink_metadata(&absolute)
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
        {
            let mut sample = Vec::new();
            std::fs::File::open(&absolute)?
                .take(1024 * 1024)
                .read_to_end(&mut sample)?;
            let sampled_lines = sample.iter().filter(|byte| **byte == b'\n').count()
                + usize::from(!sample.is_empty() && sample.last() != Some(&b'\n'));
            let size_estimate = usize::try_from(metadata.len() / 80)
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            lines = lines.saturating_add(sampled_lines.max(size_estimate).min(10_000));
        }
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    let tests_changed = paths.iter().any(|path| {
        let value = path.to_string_lossy().to_ascii_lowercase();
        value.contains("test") || value.contains("spec")
    });
    Ok(ClassificationInput {
        task,
        paths,
        changed_lines: lines,
        tests_changed,
        intent_override: None,
        complexity_override: None,
        risk_override: None,
    })
}

#[derive(Debug, Args)]
pub struct ClassifyArgs {
    /// Task summary used for deterministic intent inference.
    #[arg(long, default_value = "")]
    pub task: String,
    /// Changed path; repeat to supply the change surface explicitly.
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Total added plus removed lines for explicit inputs.
    #[arg(long)]
    pub changed_lines: Option<usize>,
    /// Declare that the change includes a test/spec path.
    #[arg(long)]
    pub tests_changed: bool,
    #[arg(long, value_enum)]
    pub intent: Option<Intent>,
    #[arg(long, value_enum)]
    pub complexity: Option<Complexity>,
    #[arg(long, value_enum)]
    pub risk: Option<RiskBand>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

pub fn from_args(args: &ClassifyArgs) -> CtxResult<Classification> {
    let repo = args.repo.clone().unwrap_or(std::env::current_dir()?);
    let declared = !args.paths.is_empty() || args.changed_lines.is_some();
    let mut input = if declared {
        ClassificationInput {
            task: args.task.clone(),
            paths: args.paths.clone(),
            changed_lines: args.changed_lines.unwrap_or(0),
            tests_changed: args.tests_changed,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        }
    } else {
        git_change_input(&repo, args.task.clone())?
    };
    input.intent_override = args.intent;
    input.complexity_override = args.complexity;
    input.risk_override = args.risk;
    let mut classification = classify(&input)?;
    if !declared {
        return Ok(classification);
    }
    classification.declared_scope = true;
    // Declared inputs used to switch Git measurement off entirely, which
    // turned `--path README.md` into a way to talk a real auth-file change
    // down from High to Low and drop the review step with it. Declared and
    // measured are both computed; the risk band is the higher of the two.
    //
    // When Git itself cannot be measured (no repository, or one with no
    // commits) the old behavior silently kept the declared band -- treating
    // "I could not check" as "I checked and it was fine". That fails open at
    // exactly the moment a mis-declared low-risk scope is hardest to catch.
    // `mark_unavailable` fails safe instead: it records the unmeasured state
    // and escalates the risk band one step.
    let Ok(mut measured_input) = git_change_input(&repo, args.task.clone()) else {
        mark_unavailable(
            &mut classification,
            "git measurement unavailable (not a repository, or no commits)",
        );
        return Ok(classification);
    };
    measured_input.intent_override = args.intent;
    measured_input.complexity_override = args.complexity;
    let measured = classify(&measured_input)?;
    let mut raised = false;
    if measured.risk > classification.risk {
        classification
            .reasons
            .push(format!("measured-tree risk floor: {:?}", measured.risk));
        classification.risk = measured.risk;
        classification.risk_score = classification.risk_score.max(measured.risk_score);
        raised = true;
    }
    // Complexity as well as risk. Complexity selects the plan step on its own
    // (and the design gate together with risk), so a declared `--path
    // README.md` over substantial real work dropped planning even where the
    // risk band was unaffected. An explicit `--complexity` is the operator
    // speaking and stands.
    if args.complexity.is_none() && measured.complexity > classification.complexity {
        classification.reasons.push(format!(
            "measured-tree complexity: {:?}",
            measured.complexity
        ));
        classification.complexity = measured.complexity;
        raised = true;
    }
    if measured.work_domain.score > classification.work_domain.score {
        classification.work_domain = measured.work_domain;
        raised = true;
    }
    if !raised {
        classification
            .reasons
            .push("declared change scope".to_string());
    }
    classification.reasons.sort();
    Ok(classification)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(paths: &[&str], lines: usize) -> ClassificationInput {
        ClassificationInput {
            task: "implement feature".to_string(),
            paths: paths.iter().map(PathBuf::from).collect(),
            changed_lines: lines,
            tests_changed: false,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        }
    }

    #[test]
    fn identical_inputs_produce_identical_classification() {
        let value = input(&["src/lib.rs"], 12);
        assert_eq!(classify(&value).unwrap(), classify(&value).unwrap());
    }

    #[test]
    fn sensitive_paths_raise_risk_and_cannot_be_downgraded() {
        let mut value = input(&["src/auth/permissions.rs"], 20);
        let classification = classify(&value).unwrap();
        assert!(classification.risk >= RiskBand::High);
        value.risk_override = Some(RiskBand::Low);
        assert!(
            classify(&value)
                .unwrap_err()
                .to_string()
                .contains("High floor")
        );
    }

    #[test]
    fn trivial_change_takes_the_low_risk_fast_path() {
        let mut value = input(&["README.md"], 5);
        value.tests_changed = true;
        let classification = classify(&value).unwrap();
        assert_eq!(classification.complexity, Complexity::Trivial);
        assert_eq!(classification.risk, RiskBand::Low);
    }

    /// A committed repository with `file` in the working tree but not in
    /// `HEAD`, so a Git measurement has something real to see.
    fn repo_with_pending_file(file: &str) -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = Command::new("git")
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
        std::fs::write(repo.path().join("README.md"), "readme\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let path = repo.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, "fn check_password() {}\n").expect("write");
        repo
    }

    #[test]
    fn declared_inputs_cannot_talk_a_measured_sensitive_surface_down() {
        let repo = repo_with_pending_file("src/auth/session.rs");
        let args = ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            repo: Some(repo.path().to_path_buf()),
            json: false,
        };
        let declared_only = classify(&ClassificationInput {
            task: args.task.clone(),
            paths: args.paths.clone(),
            changed_lines: 2,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .unwrap();
        assert_eq!(declared_only.risk, RiskBand::Low);

        let classification = from_args(&args).unwrap();
        assert!(classification.declared_scope);
        assert!(
            classification.risk >= RiskBand::High,
            "the measured auth surface must floor the declared band: {classification:?}"
        );
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("measured-tree risk floor"))
        );
    }

    /// Risk is not the only band a declared scope could talk down: complexity
    /// selects the plan step on its own, so a declared one-file scope over
    /// substantial real work dropped planning even where the risk band was
    /// unaffected.
    #[test]
    fn declared_inputs_cannot_talk_a_measured_complexity_down() {
        let repo = repo_with_pending_file("src/one.rs");
        for index in 0..8 {
            std::fs::write(
                repo.path().join(format!("src/extra-{index}.rs")),
                "fn extra() {}\n",
            )
            .unwrap();
        }
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            repo: Some(repo.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert!(
            classification.complexity >= Complexity::Substantial,
            "the measured tree is substantial: {classification:?}"
        );
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("measured-tree complexity"))
        );
    }

    /// #88: outside a git repository the old safety net silently kept
    /// whatever band the declared inputs alone produced. It must now report
    /// the unmeasured state and escalate the band one step rather than trust
    /// the declaration.
    #[test]
    fn declared_inputs_fail_safe_when_git_is_unavailable_outside_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            repo: Some(dir.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert!(classification.declared_scope);
        assert!(
            matches!(
                classification.risk_measurement,
                RiskMeasurement::Unavailable { .. }
            ),
            "{classification:?}"
        );
        // The declared scope alone (README.md, 2 lines, tests changed) scores
        // Low; fail-safe escalates it one band rather than trusting a
        // declaration the net could not check.
        assert_eq!(classification.risk, RiskBand::Medium);
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("risk escalated"))
        );
    }

    /// #88: a repository that exists but has no commits fails `git rev-parse
    /// HEAD` the same way a non-repository does, and must fail the same safe
    /// way.
    #[test]
    fn declared_inputs_fail_safe_when_the_repository_has_no_commits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            repo: Some(dir.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert!(
            matches!(
                classification.risk_measurement,
                RiskMeasurement::Unavailable { .. }
            ),
            "{classification:?}"
        );
        assert_eq!(classification.risk, RiskBand::Medium);
    }

    /// No change to behavior when measurement succeeds: the existing
    /// measured-tree tests already cover the risk-band outcome, this pins
    /// that the new field stays `Measured` alongside them.
    #[test]
    fn risk_measurement_stays_measured_when_git_succeeds() {
        let repo = repo_with_pending_file("src/one.rs");
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            repo: Some(repo.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert_eq!(classification.risk_measurement, RiskMeasurement::Measured);
    }

    #[test]
    fn classification_task_is_bounded() {
        let mut value = input(&["README.md"], 5);
        value.task = "x".repeat(MAX_TASK_BYTES + 1);
        assert!(
            classify(&value)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
    }

    #[test]
    fn frontend_domain_is_inferred_from_task_and_a_frontend_path_without_an_init_flag() {
        // #255: task text alone is capped below the selection threshold, so
        // this now needs one real frontend path signal alongside it.
        let classification = classify(&ClassificationInput {
            task: "Build a responsive billing dashboard UI".into(),
            paths: vec![PathBuf::from("src/dashboard/Billing.tsx")],
            changed_lines: 12,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::Frontend);
        assert!(classification.work_domain.score >= 45);
    }

    /// #255 repro: a task that only *mentions* "frontend" in passing (here,
    /// documenting a permission family named after it) must not select the
    /// Frontend methodology when the actual changed paths are not frontend
    /// surfaces at all.
    #[test]
    fn task_text_mentioning_frontend_without_a_frontend_path_stays_general() {
        let classification = classify(&ClassificationInput {
            task: "Document the zirv frontend permission family boundaries".into(),
            paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
            changed_lines: 40,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::General);
    }

    #[test]
    fn frontend_domain_is_inferred_from_changed_file_types() {
        let classification = classify(&ClassificationInput {
            task: "Adjust the settings experience".into(),
            paths: vec![PathBuf::from("src/settings/Panel.tsx")],
            changed_lines: 12,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::Frontend);
    }

    #[test]
    fn mixed_monorepository_backend_only_work_does_not_select_frontend_methodology() {
        let classification = classify(&ClassificationInput {
            task: "Fix database retry handling in the API service".into(),
            paths: vec![
                PathBuf::from("services/api/src/retry.rs"),
                PathBuf::from("services/api/tests/retry.rs"),
            ],
            changed_lines: 24,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::General);
    }

    #[test]
    fn is_zirv_owned_path_matches_only_a_leading_zirv_component() {
        assert!(is_zirv_owned_path(Path::new(".zirv/work/abc/mock.html")));
        assert!(is_zirv_owned_path(Path::new(".zirv/ctx.toml")));
        assert!(!is_zirv_owned_path(Path::new("src/x.tsx")));
        assert!(!is_zirv_owned_path(Path::new("docs/.zirv/notes.md")));
    }

    /// #229/#232: narrower than `is_zirv_owned_path` -- only the workflow's
    /// own `.zirv/work/**` bookkeeping must be excluded from a review
    /// package or its staleness fingerprint, not sibling `.zirv/` config
    /// like `.zirv/ctx.toml` or `.zirv/commands/*`, which are real
    /// repository content a reviewer needs to see.
    #[test]
    fn is_workflow_work_path_matches_only_zirv_work_not_other_zirv_state() {
        assert!(is_workflow_work_path(Path::new(".zirv/work/abc/plan.md")));
        assert!(is_workflow_work_path(Path::new(".zirv/work")));
        assert!(!is_workflow_work_path(Path::new(".zirv/ctx.toml")));
        assert!(!is_workflow_work_path(Path::new(
            ".zirv/commands/build.yaml"
        )));
        assert!(!is_workflow_work_path(Path::new("src/x.tsx")));
        assert!(!is_workflow_work_path(Path::new(
            "docs/.zirv/work/notes.md"
        )));
    }

    #[test]
    fn git_change_input_ignores_untracked_zirv_state_but_keeps_other_untracked_paths() {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = Command::new("git")
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
        std::fs::write(repo.path().join("README.md"), "readme\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        let zirv_work_dir = repo.path().join(".zirv").join("work").join("old-id");
        std::fs::create_dir_all(&zirv_work_dir).expect("create .zirv/work dir");
        std::fs::write(
            zirv_work_dir.join("dash-v3-mock.html"),
            "<html>\n".repeat(50),
        )
        .expect("write stale workflow artifact");

        std::fs::create_dir_all(repo.path().join("src")).expect("create src dir");
        std::fs::write(
            repo.path().join("src").join("x.tsx"),
            "export const X = () => null;\n",
        )
        .expect("write untracked tsx");

        let input = git_change_input(repo.path(), "unrelated change".into()).expect("input");

        assert!(
            !input.paths.iter().any(|path| path.starts_with(".zirv")),
            "expected no .zirv paths in {:?}",
            input.paths
        );
        assert!(
            input
                .paths
                .iter()
                .any(|path| path == Path::new("src/x.tsx")),
            "expected src/x.tsx in {:?}",
            input.paths
        );
        // Only the tsx contributes lines; the stale .zirv/work html must not.
        assert!(
            input.changed_lines < 50,
            "expected .zirv/work content excluded from changed_lines, got {}",
            input.changed_lines
        );
    }
}
