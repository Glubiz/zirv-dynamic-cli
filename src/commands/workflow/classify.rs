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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub intent: Intent,
    pub complexity: Complexity,
    pub risk: RiskBand,
    pub risk_score: u16,
    pub changed_files: usize,
    pub changed_lines: usize,
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
    let mut sensitive_floor = None;
    add_path_signal(
        &lowered_paths,
        &["auth", "security", "permission", "credential", "secret"],
        30,
        "authentication/security surface",
        &mut score,
        &mut reasons,
    );
    if reasons
        .iter()
        .any(|reason| reason == "authentication/security surface")
    {
        sensitive_floor = Some(RiskBand::High);
    }
    add_path_signal(
        &lowered_paths,
        &["migration", "schema", "database", "sql"],
        25,
        "database/schema surface",
        &mut score,
        &mut reasons,
    );
    if reasons
        .iter()
        .any(|reason| reason == "database/schema surface")
    {
        sensitive_floor = Some(RiskBand::High);
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
        reasons,
    })
}

fn add_path_signal(
    paths: &[String],
    needles: &[&str],
    points: u16,
    reason: &str,
    score: &mut u16,
    reasons: &mut Vec<String>,
) {
    if paths
        .iter()
        .any(|path| needles.iter().any(|needle| path.contains(needle)))
    {
        *score += points;
        reasons.push(reason.to_string());
    }
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

pub fn git_change_input(repo: &Path, task: String) -> CtxResult<ClassificationInput> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--numstat", "HEAD"])
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
    let mut input = if args.paths.is_empty() && args.changed_lines.is_none() {
        git_change_input(&repo, args.task.clone())?
    } else {
        ClassificationInput {
            task: args.task.clone(),
            paths: args.paths.clone(),
            changed_lines: args.changed_lines.unwrap_or(0),
            tests_changed: args.tests_changed,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        }
    };
    input.intent_override = args.intent;
    input.complexity_override = args.complexity;
    input.risk_override = args.risk;
    classify(&input)
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

    #[test]
    fn classification_task_is_bounded() {
        let mut value = input(&["README.md"], 5);
        value.task = "x".repeat(MAX_TASK_BYTES + 1);
        assert!(classify(&value).unwrap_err().to_string().contains("exceeds"));
    }
}
