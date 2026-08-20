//! Compact skill manifests, layered registry, and inspection commands.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use super::capability::{CapabilityId, CapabilityReport};
use crate::commands::ctx::CtxResult;

pub const SKILL_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 32 * 1024;
pub const MAX_INSTRUCTION_BUDGET: usize = 8 * 1024;
const MAX_SKILL_DIRECTORY_ENTRIES: usize = 512;
const MAX_RESOLVED_CONTEXT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowPhase {
    Design,
    Plan,
    Implement,
    Debug,
    Test,
    Review,
    Verify,
    Delegate,
    Present,
}

impl std::fmt::Display for WorkflowPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = serde_json::to_value(self).map_err(|_| std::fmt::Error)?;
        f.write_str(value.as_str().ok_or(std::fmt::Error)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillManifest {
    pub schema_version: u32,
    pub id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<CapabilityId>,
    #[serde(default)]
    pub optional_capabilities: Vec<CapabilityId>,
    pub context_budget_bytes: usize,
    #[serde(default)]
    pub phases: Vec<WorkflowPhase>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub instructions: String,
}

impl SkillManifest {
    pub fn validate(&self) -> CtxResult<()> {
        if self.schema_version != SKILL_SCHEMA_VERSION {
            return Err(format!(
                "skill '{}': unsupported schema_version {}; supported version is {}",
                self.id, self.schema_version, SKILL_SCHEMA_VERSION
            )
            .into());
        }
        if !valid_id(&self.id) {
            return Err(format!("skill id '{}' must match [a-z0-9][a-z0-9._-]*", self.id).into());
        }
        if self.version == 0 {
            return Err(format!("skill '{}': version must be at least 1", self.id).into());
        }
        if self.name.trim().is_empty() || self.description.trim().is_empty() {
            return Err(format!("skill '{}': name and description are required", self.id).into());
        }
        if self.context_budget_bytes == 0 || self.context_budget_bytes > MAX_INSTRUCTION_BUDGET {
            return Err(format!(
                "skill '{}': context_budget_bytes must be in 1..={MAX_INSTRUCTION_BUDGET}",
                self.id
            )
            .into());
        }
        if self.instructions.trim().is_empty() {
            return Err(format!("skill '{}': instructions must not be empty", self.id).into());
        }
        if self.instructions.len() > self.context_budget_bytes {
            return Err(format!(
                "skill '{}': instructions are {} bytes, over the {} byte context budget",
                self.id,
                self.instructions.len(),
                self.context_budget_bytes
            )
            .into());
        }

        let mut capabilities = BTreeSet::new();
        for capability in self
            .required_capabilities
            .iter()
            .chain(&self.optional_capabilities)
        {
            if !capabilities.insert(*capability) {
                return Err(format!(
                    "skill '{}': capability '{}' is declared more than once",
                    self.id, capability
                )
                .into());
            }
        }
        for dependency in &self.dependencies {
            if !valid_id(dependency) || dependency == &self.id {
                return Err(
                    format!("skill '{}': invalid dependency '{}'", self.id, dependency).into(),
                );
            }
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillSource {
    BuiltIn,
    OperatorGlobal,
    Repository,
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BuiltIn => "built-in",
            Self::OperatorGlobal => "operator-global",
            Self::Repository => "repository-untrusted",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisteredSkill {
    #[serde(flatten)]
    pub manifest: SkillManifest,
    pub source: SkillSource,
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, RegisteredSkill>,
}

impl SkillRegistry {
    pub fn load(repo: &Path, home: Option<&Path>, include_custom: bool) -> CtxResult<Self> {
        let mut skills = BTreeMap::new();
        for manifest in builtin_manifests()? {
            skills.insert(
                manifest.id.clone(),
                RegisteredSkill {
                    manifest,
                    source: SkillSource::BuiltIn,
                    source_path: None,
                },
            );
        }

        if include_custom {
            if let Some(home) = home {
                load_dir(
                    &home.join(".zirv").join("skills"),
                    home,
                    SkillSource::OperatorGlobal,
                    &mut skills,
                )?;
            }
            load_dir(
                &repo.join(".zirv").join("skills"),
                repo,
                SkillSource::Repository,
                &mut skills,
            )?;
        }

        let registry = Self { skills };
        registry.validate_dependencies()?;
        Ok(registry)
    }

    pub fn list(&self) -> impl Iterator<Item = &RegisteredSkill> {
        self.skills.values()
    }

    pub fn get(&self, requested: &str) -> CtxResult<&RegisteredSkill> {
        let (id, version) = requested
            .rsplit_once('@')
            .and_then(|(id, version)| version.parse::<u32>().ok().map(|version| (id, version)))
            .map_or((requested, None), |(id, version)| (id, Some(version)));
        let skill = self
            .skills
            .get(id)
            .ok_or_else(|| format!("unknown skill '{id}'"))?;
        if let Some(version) = version
            && skill.manifest.version != version
        {
            return Err(format!(
                "skill '{id}' resolved to version {}, not requested version {version}",
                skill.manifest.version
            )
            .into());
        }
        Ok(skill)
    }

    /// Dependencies first, then the requested skill. The returned slice is
    /// the complete set that may consume context for one step; unrelated
    /// registry entries contribute no prompt text.
    pub fn resolve_stack(&self, requested: &str) -> CtxResult<Vec<&RegisteredSkill>> {
        let root = self.get(requested)?;
        let mut resolved = Vec::new();
        let mut seen = BTreeSet::new();
        self.resolve_dependencies(&root.manifest.id, &mut seen, &mut resolved)?;
        let context_bytes = resolved
            .iter()
            .map(|skill| skill.manifest.instructions.len())
            .sum::<usize>();
        if context_bytes > MAX_RESOLVED_CONTEXT_BYTES {
            return Err(format!(
                "skill '{}' resolves to {context_bytes} instruction bytes; limit is {MAX_RESOLVED_CONTEXT_BYTES}",
                root.manifest.id
            )
            .into());
        }
        Ok(resolved)
    }

    pub fn ensure_supported(&self, requested: &str, report: &CapabilityReport) -> CtxResult<()> {
        for skill in self.resolve_stack(requested)? {
            for capability in &skill.manifest.required_capabilities {
                let support = report.support(*capability);
                if !support.satisfies_requirement() {
                    return Err(format!(
                        "skill '{}' requires capability '{}' which is unsupported on adapter '{}'",
                        skill.manifest.id, capability, report.adapter
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    fn resolve_dependencies<'a>(
        &'a self,
        id: &str,
        seen: &mut BTreeSet<String>,
        resolved: &mut Vec<&'a RegisteredSkill>,
    ) -> CtxResult<()> {
        if !seen.insert(id.to_string()) {
            return Ok(());
        }
        let skill = self
            .skills
            .get(id)
            .ok_or_else(|| format!("unknown skill dependency '{id}'"))?;
        for dependency in &skill.manifest.dependencies {
            self.resolve_dependencies(dependency, seen, resolved)?;
        }
        resolved.push(skill);
        Ok(())
    }

    fn validate_dependencies(&self) -> CtxResult<()> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Mark {
            Visiting,
            Done,
        }
        fn visit(
            id: &str,
            skills: &BTreeMap<String, RegisteredSkill>,
            marks: &mut BTreeMap<String, Mark>,
        ) -> CtxResult<()> {
            match marks.get(id) {
                Some(Mark::Visiting) => {
                    return Err(format!("cyclic skill dependency involving '{id}'").into());
                }
                Some(Mark::Done) => return Ok(()),
                None => {}
            }
            let skill = skills
                .get(id)
                .ok_or_else(|| format!("missing skill dependency '{id}'"))?;
            marks.insert(id.to_string(), Mark::Visiting);
            for dependency in &skill.manifest.dependencies {
                if !skills.contains_key(dependency) {
                    return Err(format!(
                        "skill '{}' depends on missing skill '{}'",
                        skill.manifest.id, dependency
                    )
                    .into());
                }
                visit(dependency, skills, marks)?;
            }
            marks.insert(id.to_string(), Mark::Done);
            Ok(())
        }

        let mut marks = BTreeMap::new();
        for id in self.skills.keys() {
            visit(id, &self.skills, &mut marks)?;
        }
        Ok(())
    }
}

fn load_dir(
    root: &Path,
    allowed_root: &Path,
    source: SkillSource,
    skills: &mut BTreeMap<String, RegisteredSkill>,
) -> CtxResult<()> {
    if !root.exists() {
        return Ok(());
    }
    let root_metadata = std::fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() {
        return Err(format!("refusing symlinked skill directory '{}'", root.display()).into());
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|err| format!("cannot resolve skill directory '{}': {err}", root.display()))?;
    let canonical_allowed_root = allowed_root.canonicalize().map_err(|err| {
        format!(
            "cannot resolve skill trust root '{}': {err}",
            allowed_root.display()
        )
    })?;
    if !canonical_root.starts_with(&canonical_allowed_root) {
        return Err(format!(
            "skill directory '{}' escapes trust root '{}'",
            root.display(),
            allowed_root.display()
        )
        .into());
    }
    if !canonical_root.is_dir() {
        return Err(format!("skill path '{}' is not a directory", root.display()).into());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&canonical_root)? {
        if entries.len() == MAX_SKILL_DIRECTORY_ENTRIES {
            return Err(format!(
                "skill directory '{}' has more than {MAX_SKILL_DIRECTORY_ENTRIES} entries",
                root.display()
            )
            .into());
        }
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("refusing symlinked skill manifest '{}'", path.display()).into());
        }
        if !metadata.is_file() {
            continue;
        }
        let extension = path.extension().and_then(|value| value.to_str());
        if !matches!(extension, Some("yaml" | "yml" | "toml")) {
            continue;
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&canonical_root) {
            return Err(format!(
                "skill manifest escapes '{}': {}",
                root.display(),
                path.display()
            )
            .into());
        }
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if size > MAX_MANIFEST_BYTES {
            return Err(format!(
                "skill manifest '{}' is {size} bytes; limit is {MAX_MANIFEST_BYTES}",
                path.display()
            )
            .into());
        }
        let text = std::fs::read_to_string(&canonical)?;
        let manifest: SkillManifest = match extension {
            Some("toml") => toml::from_str(&text)
                .map_err(|err| format!("invalid skill '{}': {err}", path.display()))?,
            _ => serde_yaml_ng::from_str(&text)
                .map_err(|err| format!("invalid skill '{}': {err}", path.display()))?,
        };
        manifest.validate()?;
        skills.insert(
            manifest.id.clone(),
            RegisteredSkill {
                manifest,
                source,
                source_path: Some(path),
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Mirrors the small, fixed SkillManifest schema at call sites.
fn manifest(
    id: &str,
    name: &str,
    description: &str,
    triggers: &[&str],
    required_capabilities: &[CapabilityId],
    optional_capabilities: &[CapabilityId],
    phases: &[WorkflowPhase],
    dependencies: &[&str],
    instructions: &str,
) -> SkillManifest {
    SkillManifest {
        schema_version: SKILL_SCHEMA_VERSION,
        id: id.to_string(),
        version: 1,
        name: name.to_string(),
        description: description.to_string(),
        triggers: triggers.iter().map(|value| (*value).to_string()).collect(),
        required_capabilities: required_capabilities.to_vec(),
        optional_capabilities: optional_capabilities.to_vec(),
        context_budget_bytes: instructions.len().max(1),
        phases: phases.to_vec(),
        dependencies: dependencies
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        instructions: instructions.to_string(),
    }
}

pub fn builtin_manifests() -> CtxResult<Vec<SkillManifest>> {
    use CapabilityId as Cap;
    use WorkflowPhase as Phase;

    let skills = vec![
        manifest(
            "design",
            "Design",
            "Clarify intent and choose a proportional design.",
            &["feature", "architecture", "design"],
            &[Cap::RepoRead],
            &[],
            &[Phase::Design],
            &[],
            "Establish the goal, constraints, affected boundaries, and acceptance criteria. Inspect existing architecture before proposing changes. Compare only materially different options. Choose the simplest design that meets the need, record important tradeoffs, and request approval only when the workflow marks a gate.",
        ),
        manifest(
            "plan",
            "Plan",
            "Turn substantial work into ordered executable units.",
            &["plan", "substantial", "architectural"],
            &[Cap::RepoRead],
            &[],
            &[Phase::Plan],
            &[],
            "Break the accepted design into dependency-ordered units with concrete files, behavior, verification, and completion evidence. Keep units independently reviewable. Omit ceremony for bounded work. Identify external dependencies and explicit approval gates without treating conversation text as durable workflow state.",
        ),
        manifest(
            "implement",
            "Implement",
            "Make scoped changes with continuous evidence.",
            &["feature", "bugfix", "refactor"],
            &[Cap::RepoRead, Cap::RepoWrite],
            &[Cap::TestRun],
            &[Phase::Implement],
            &[],
            "Read the affected code and repository instructions before editing. Keep the change scoped and preserve established interfaces unless the task requires otherwise. Run the fastest relevant deterministic check after meaningful edits. Do not overwrite unrelated working-tree changes. Report exact blockers and evidence, never inferred success.",
        ),
        manifest(
            "systematic-debugging",
            "Systematic debugging",
            "Reproduce and isolate a failure before changing code.",
            &["bug", "failure", "debug"],
            &[Cap::RepoRead],
            &[Cap::ShellExec, Cap::TestRun, Cap::RepoWrite],
            &[Phase::Debug, Phase::Implement],
            &[],
            "Reproduce the failure with the smallest reliable command. Separate symptoms from causes, inspect the data path, and form a falsifiable hypothesis. Change one cause at a time. Add a regression test where practical, prove it fails for the original reason, then implement and rerun relevant checks. Do not patch around an unexplained failure.",
        ),
        manifest(
            "testing",
            "Testing",
            "Select proportional deterministic verification.",
            &["test", "verify"],
            &[Cap::TestRun],
            &[Cap::RepoRead],
            &[Phase::Test, Phase::Verify],
            &[],
            "Use repository-configured checks. During implementation run targeted checks mapped to changed paths; when impact is uncertain, fall back to broader checks. Final verification must be fresh and proportional to risk. Preserve structured summaries for reviewers and include verbose output only for failures or when requested.",
        ),
        manifest(
            "tdd",
            "Test-driven development",
            "Use a focused red, green, refactor loop when it adds value.",
            &["tdd", "regression"],
            &[Cap::TestRun, Cap::RepoWrite],
            &[Cap::RepoRead],
            &[Phase::Test, Phase::Implement],
            &["testing"],
            "Write the smallest behavior-focused test first and confirm it fails for the intended missing behavior, not setup noise. Implement the minimum change that makes it pass. Refactor only while tests remain green. Skip TDD for generated files, pure configuration, exploratory spikes, or changes whose useful assertion exists only at a broader integration boundary.",
        ),
        manifest(
            "review",
            "Review",
            "Review the requirement and diff with independent evidence.",
            &["review", "risk"],
            &[Cap::RepoRead],
            &[Cap::AgentSpawn],
            &[Phase::Review],
            &[],
            "Review the requirement, base/head identifiers, relevant diff, and structured verification evidence. Look for correctness, security, data loss, compatibility, and missing tests. Report only concrete findings with severity, location, reasoning, and a proposed disposition. Do not restate the implementation. Respect the workflow's bounded fix and re-review limit.",
        ),
        manifest(
            "verify",
            "Verify",
            "Require fresh completion evidence.",
            &["complete", "verify"],
            &[Cap::TestRun],
            &[Cap::RepoRead],
            &[Phase::Verify],
            &["testing"],
            "Before claiming completion, inspect the final change set and run the configured final checks required by its risk band. Confirm outputs are current and correspond to the final files. State exactly what passed, failed, or was skipped. A prior run before later edits is not completion evidence.",
        ),
        manifest(
            "delegate",
            "Delegate",
            "Create bounded, isolated worker briefs.",
            &["delegate", "worker"],
            &[Cap::AgentSpawn],
            &[Cap::RepoRead],
            &[Phase::Delegate],
            &[],
            "Delegate only a concrete bounded unit with explicit inputs, output, scope, constraints, and verification. Give the worker only relevant context and name ownership boundaries. Avoid overlapping writes. The parent retains integration responsibility, validates the result, and records progress in workflow state rather than relying on chat narration.",
        ),
        manifest(
            "parallelize",
            "Parallelize",
            "Run independent work concurrently without overlapping ownership.",
            &["parallel", "independent"],
            &[Cap::AgentSpawn],
            &[Cap::GitWorktree],
            &[Phase::Delegate],
            &["delegate"],
            "Parallelize only units with independent inputs and non-overlapping write ownership. Keep shared prerequisites in the parent. Batch small same-shape read-only tasks when setup overhead dominates. Establish how results return, then integrate and verify centrally. Stop dispatching when coordination cost exceeds the expected latency reduction.",
        ),
    ];
    for skill in &skills {
        skill.validate()?;
    }
    Ok(skills)
}

#[derive(Debug, Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List resolved skills and their provenance.
    List(SkillListArgs),
    /// Show one resolved skill and capability diagnostics.
    Show(SkillShowArgs),
}

#[derive(Debug, Args)]
pub struct SkillListArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
    /// Ignore operator-global and repository-provided skills.
    #[arg(long)]
    pub built_in_only: bool,
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SkillShowArgs {
    /// Stable skill id, optionally suffixed with @version.
    pub id: String,
    /// Report required capability availability for this adapter.
    #[arg(long)]
    pub agent: Option<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
    /// Ignore operator-global and repository-provided skills.
    #[arg(long)]
    pub built_in_only: bool,
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Serialize)]
struct SkillShow<'a> {
    skill: &'a RegisteredSkill,
    dependency_order: Vec<&'a str>,
    capability_report: Option<CapabilityReport>,
}

fn registry(repo: Option<&Path>, built_in_only: bool) -> CtxResult<SkillRegistry> {
    let repo = match repo {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    SkillRegistry::load(&repo, dirs::home_dir().as_deref(), !built_in_only)
}

pub fn run(args: &SkillArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        SkillCommand::List(args) => {
            let registry = registry(args.repo.as_deref(), args.built_in_only)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &registry.list().collect::<Vec<_>>())?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "ID\tVERSION\tSOURCE\tBUDGET")?;
                for skill in registry.list() {
                    writeln!(
                        writer,
                        "{}\t{}\t{}\t{} B",
                        skill.manifest.id,
                        skill.manifest.version,
                        skill.source,
                        skill.manifest.context_budget_bytes
                    )?;
                }
            }
            Ok(0)
        }
        SkillCommand::Show(args) => {
            let registry = registry(args.repo.as_deref(), args.built_in_only)?;
            let skill = registry.get(&args.id)?;
            let dependency_order = registry
                .resolve_stack(&args.id)?
                .into_iter()
                .map(|skill| skill.manifest.id.as_str())
                .collect();
            let capability_report = args.agent.as_deref().map(CapabilityReport::for_adapter);
            if let Some(report) = &capability_report {
                registry.ensure_supported(&args.id, report)?;
            }
            if args.json {
                serde_json::to_writer_pretty(
                    &mut *writer,
                    &SkillShow {
                        skill,
                        dependency_order,
                        capability_report,
                    },
                )?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "{}@{}", skill.manifest.id, skill.manifest.version)?;
                writeln!(writer, "source: {}", skill.source)?;
                if let Some(path) = &skill.source_path {
                    writeln!(writer, "path: {}", path.display())?;
                }
                writeln!(
                    writer,
                    "budget: {} bytes",
                    skill.manifest.context_budget_bytes
                )?;
                if !dependency_order.is_empty() {
                    writeln!(writer, "resolution: {}", dependency_order.join(" -> "))?;
                }
                if let Some(report) = capability_report {
                    writeln!(writer, "capabilities ({}):", report.adapter)?;
                    for capability in skill
                        .manifest
                        .required_capabilities
                        .iter()
                        .chain(&skill.manifest.optional_capabilities)
                    {
                        writeln!(writer, "  {capability}: {}", report.support(*capability))?;
                    }
                }
                writeln!(writer, "\n{}", skill.manifest.instructions)?;
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write fixture");
    }

    #[test]
    fn builtins_are_valid_compact_and_provider_neutral() {
        let skills = builtin_manifests().expect("valid builtins");
        assert_eq!(skills.len(), 10);
        let total: usize = skills.iter().map(|skill| skill.instructions.len()).sum();
        assert!(total < 12 * 1024, "built-ins should stay compact: {total}");
        for skill in skills {
            assert!(skill.instructions.len() <= skill.context_budget_bytes);
            for forbidden in ["Claude", "Codex", "Bash tool", "Agent tool"] {
                assert!(
                    !skill.instructions.contains(forbidden),
                    "{} contains provider-specific text '{forbidden}'",
                    skill.id
                );
            }
        }
    }

    #[test]
    fn unselected_skills_contribute_no_instruction_text() {
        let repo = tempdir().unwrap();
        let registry = SkillRegistry::load(repo.path(), None, false).unwrap();
        let stack = registry.resolve_stack("design").unwrap();
        assert_eq!(stack.len(), 1);
        assert_eq!(stack[0].manifest.id, "design");
    }

    #[test]
    fn stable_id_and_version_resolution_is_identical_across_adapters() {
        let repo = tempdir().unwrap();
        let registry = SkillRegistry::load(repo.path(), None, false).unwrap();
        for adapter in ["claude", "codex"] {
            let report = CapabilityReport::for_adapter(adapter);
            registry.ensure_supported("design@1", &report).unwrap();
            assert_eq!(registry.get("design@1").unwrap().manifest.id, "design");
        }
    }

    #[test]
    fn repository_overrides_global_and_builtin_with_visible_provenance() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let global = home.path().join(".zirv/skills");
        let project = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let yaml = |name: &str| {
            format!(
                "schema_version: 1\nid: design\nversion: 2\nname: {name}\ndescription: custom\ncontext_budget_bytes: 64\nphases: [design]\ninstructions: custom\n"
            )
        };
        write(&global.join("design.yaml"), &yaml("Global"));
        write(&project.join("design.yaml"), &yaml("Project"));

        let registry = SkillRegistry::load(repo.path(), Some(home.path()), true).unwrap();
        let skill = registry.get("design@2").unwrap();
        assert_eq!(skill.manifest.name, "Project");
        assert_eq!(skill.source, SkillSource::Repository);
    }

    #[test]
    fn custom_skills_cannot_widen_capability_policy() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir.join("danger.yaml"),
            "schema_version: 1\nid: danger\nversion: 1\nname: Danger\ndescription: test\nrequired_capabilities: [repo.write]\ncontext_budget_bytes: 64\nphases: [implement]\ninstructions: write files\n",
        );
        let registry = SkillRegistry::load(repo.path(), None, true).unwrap();
        let report = CapabilityReport::for_adapter("claude").with_policy(|capability| {
            if capability == CapabilityId::RepoWrite {
                super::super::capability::PolicyDecision::Deny
            } else {
                super::super::capability::PolicyDecision::Allow
            }
        });
        let error = registry.ensure_supported("danger", &report).unwrap_err();
        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn unsupported_versions_unknown_fields_and_cycles_fail_safely() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir.join("bad.yaml"),
            "schema_version: 2\nid: bad\nversion: 1\nname: Bad\ndescription: bad\ncontext_budget_bytes: 16\ninstructions: bad\n",
        );
        let error = SkillRegistry::load(repo.path(), None, true).unwrap_err();
        assert!(error.to_string().contains("unsupported schema_version"));

        std::fs::remove_file(dir.join("bad.yaml")).unwrap();
        write(
            &dir.join("bad.yaml"),
            "schema_version: 1\nid: bad\nversion: 1\nname: Bad\ndescription: bad\ncontext_budget_bytes: 16\ninstructions: bad\nsurprise: true\n",
        );
        let error = SkillRegistry::load(repo.path(), None, true).unwrap_err();
        assert!(error.to_string().contains("unknown field"));

        std::fs::remove_file(dir.join("bad.yaml")).unwrap();
        for (id, dependency) in [("one", "two"), ("two", "one")] {
            write(
                &dir.join(format!("{id}.yaml")),
                &format!(
                    "schema_version: 1\nid: {id}\nversion: 1\nname: {id}\ndescription: cycle\ncontext_budget_bytes: 16\ndependencies: [{dependency}]\ninstructions: cycle\n"
                ),
            );
        }
        let error = SkillRegistry::load(repo.path(), None, true).unwrap_err();
        assert!(error.to_string().contains("cyclic"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_manifests_are_refused() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let dir = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &outside.path().join("outside.yaml"),
            "schema_version: 1\nid: outside\nversion: 1\nname: Outside\ndescription: test\ncontext_budget_bytes: 16\ninstructions: test\n",
        );
        symlink(
            outside.path().join("outside.yaml"),
            dir.join("outside.yaml"),
        )
        .unwrap();
        let error = SkillRegistry::load(repo.path(), None, true).unwrap_err();
        assert!(error.to_string().contains("symlinked"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_cannot_move_repository_skills_outside_the_repo() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let skills = outside.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write(
            &skills.join("outside.yaml"),
            "schema_version: 1\nid: outside\nversion: 1\nname: Outside\ndescription: test\ncontext_budget_bytes: 16\ninstructions: test\n",
        );
        symlink(outside.path(), repo.path().join(".zirv")).unwrap();
        let error = SkillRegistry::load(repo.path(), None, true).unwrap_err();
        assert!(error.to_string().contains("escapes trust root"));
    }
}
