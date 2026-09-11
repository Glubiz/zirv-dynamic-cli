//! Native execution authorization and containment (issue #473, roadmap N04).
//!
//! Provider output is untrusted input. It never receives filesystem,
//! process, network, MCP, artifact, or delegation authority directly. Every
//! effect is first represented as an [`ExecutionAction`] and admitted by an
//! [`ExecutionBroker`]. The broker re-reads the current policy, verifies the
//! persisted native seat and generation, checks resource/path claims and a
//! live writer permit, and binds interactive approvals to the exact action
//! and current scope. N05's concrete coding tools consume the returned
//! [`Authorization`] and [`ProcessSandboxPolicy`].
//!
//! Arbitrary subprocesses are never treated as equivalent to brokered file
//! and network tools. They require a verified platform isolation launcher.
//! If one is absent, authorization fails with [`BrokerError::IsolationUnavailable`]
//! instead of silently running unsandboxed.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::adapters::LaunchMode;
use super::super::config::{CtxConfig, env_from_process};
use super::super::permit::HeavyPermit;
use super::super::policy::{Capability, EffectivePolicy, Stance};
use super::super::safety::{self, SafetyPolicy, Verdict};
use super::super::seat;
use super::{RuntimeKind, SessionHandle};

/// Identity trusted by the executor. A model never chooses these fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionIdentity {
    pub session: String,
    pub short: String,
    pub generation: u64,
    pub role: String,
    pub task: Option<String>,
}

impl ExecutionIdentity {
    pub fn from_handle(handle: &SessionHandle, task: Option<String>) -> Result<Self, BrokerError> {
        if handle.runtime != RuntimeKind::Native {
            return Err(BrokerError::Identity(
                "native execution requires a native runtime handle".to_string(),
            ));
        }
        Ok(Self {
            session: handle.logical_id.clone(),
            short: handle.short.clone(),
            generation: handle.generation,
            role: handle.role.clone(),
            task,
        })
    }
}

/// Network scope attached by trusted runtime configuration. `Only` is usable
/// by brokered HTTP tools; arbitrary shells cannot enforce a host allowlist
/// portably and are therefore refused when they request network under it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkScope {
    #[default]
    Denied,
    Only { targets: BTreeSet<NetworkTarget> },
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NetworkTarget {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

impl NetworkTarget {
    pub fn new(scheme: &str, host: &str, port: Option<u16>) -> Result<Self, BrokerError> {
        let scheme = scheme.to_ascii_lowercase();
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https")
            || host.is_empty()
            || host
                .chars()
                .any(|character| matches!(character, '/' | '\\' | '@' | '\0'))
        {
            return Err(BrokerError::InvalidAction(
                "network target must have an http(s) scheme and a bare host".to_string(),
            ));
        }
        Ok(Self { scheme, host, port })
    }
}

/// Roots granted to one task. File tools cannot escape these roots, and
/// protected roots win over all ordinary read/write roots after symlink or
/// junction resolution. Artifact roots are reachable only via the typed
/// artifact action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceClaims {
    pub workspace_root: PathBuf,
    pub worktree_root: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub outside_write_roots: Vec<PathBuf>,
    pub git_write_roots: Vec<PathBuf>,
    pub artifact_roots: Vec<PathBuf>,
    pub protected_roots: Vec<PathBuf>,
    pub network: NetworkScope,
}

impl ResourceClaims {
    pub fn new(
        workspace_root: &Path,
        worktree_root: &Path,
        state_root: &Path,
        home_root: &Path,
        network: NetworkScope,
    ) -> Result<Self, BrokerError> {
        let workspace_root = canonical_existing_dir(workspace_root, "workspace")?;
        let worktree_root = canonical_existing_dir(worktree_root, "worktree")?;
        let state_root = normalize_scope_path(state_root)?;
        let mut protected_roots = vec![state_root];
        for relative in [
            ".zirv",
            ".ssh",
            ".aws",
            ".docker",
            ".kube",
            ".gnupg",
            ".codex",
            ".claude",
            ".config/gh",
            ".config/gcloud",
            "AppData/Local/zirv",
        ] {
            protected_roots.push(normalize_scope_path(&home_root.join(relative))?);
        }
        Ok(Self {
            read_roots: dedup_paths(vec![workspace_root.clone(), worktree_root.clone()]),
            workspace_root,
            worktree_root,
            outside_write_roots: Vec::new(),
            git_write_roots: Vec::new(),
            artifact_roots: Vec::new(),
            protected_roots: dedup_paths(protected_roots),
            network,
        })
    }

    pub fn protect(mut self, path: &Path) -> Result<Self, BrokerError> {
        self.protected_roots.push(normalize_scope_path(path)?);
        self.protected_roots = dedup_paths(self.protected_roots);
        Ok(self)
    }

    pub fn allow_artifacts(mut self, path: &Path) -> Result<Self, BrokerError> {
        self.artifact_roots.push(normalize_scope_path(path)?);
        self.artifact_roots = dedup_paths(self.artifact_roots);
        Ok(self)
    }

    pub fn allow_outside_writes(mut self, path: &Path) -> Result<Self, BrokerError> {
        self.outside_write_roots
            .push(canonical_existing_dir(path, "outside write root")?);
        self.outside_write_roots = dedup_paths(self.outside_write_roots);
        Ok(self)
    }

    /// Discovers the administrative directories for a linked worktree using
    /// git's own path resolver. A main checkout is intentionally rejected:
    /// its git-dir equals the common-dir, so making it writable would expose
    /// config/hooks and every worktree's administration rather than the
    /// narrow linked-worktree scope native writers require.
    pub fn discover_linked_worktree_git(mut self) -> Result<Self, BrokerError> {
        let git_dir = git_path(&self.worktree_root, "--git-dir")?;
        let common = git_path(&self.worktree_root, "--git-common-dir")?;
        if same_path(&git_dir, &common) {
            return Err(BrokerError::Scope(
                "native writers require a linked worktree; refusing broad write access to the main .git directory"
                    .to_string(),
            ));
        }
        let mut roots = vec![git_dir.clone()];
        for relative in ["objects", "refs", "logs"] {
            roots.push(normalize_scope_path(&common.join(relative))?);
        }
        self.git_write_roots = dedup_paths(roots);
        self.protected_roots.push(git_dir);
        self.protected_roots.push(common);
        self.protected_roots = dedup_paths(self.protected_roots);
        Ok(self)
    }

    fn fingerprint(&self) -> Result<String, BrokerError> {
        digest_json(self)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEffects {
    pub repo_write: bool,
    pub outside_write: bool,
    pub network: bool,
    pub git_metadata_write: bool,
    pub git_push_or_destructive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProcessInvocation {
    Argv {
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
    },
    Shell {
        program: String,
        args: Vec<String>,
        script: String,
        cwd: PathBuf,
    },
}

impl ProcessInvocation {
    fn cwd(&self) -> &Path {
        match self {
            Self::Argv { cwd, .. } | Self::Shell { cwd, .. } => cwd,
        }
    }

    fn safety_command(&self) -> String {
        match self {
            Self::Argv { program, args, .. } => std::iter::once(program.as_str())
                .chain(args.iter().map(String::as_str))
                .map(shell_quote_for_classification)
                .collect::<Vec<_>>()
                .join(" "),
            Self::Shell { script, .. } => script.clone(),
        }
    }

    fn command_parts(&self) -> (&str, Vec<&str>) {
        match self {
            Self::Argv { program, args, .. } => {
                (program.as_str(), args.iter().map(String::as_str).collect())
            }
            Self::Shell {
                program,
                args,
                script,
                ..
            } => {
                let mut command_args: Vec<&str> = args.iter().map(String::as_str).collect();
                command_args.push(script.as_str());
                (program.as_str(), command_args)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionAction {
    ReadFile {
        path: PathBuf,
    },
    WriteFile {
        path: PathBuf,
    },
    Process {
        invocation: ProcessInvocation,
        effects: ProcessEffects,
    },
    Network {
        target: NetworkTarget,
    },
    Mcp {
        server: String,
        tool: String,
        arguments: serde_json::Value,
        /// Capability effects declared by the trusted MCP registry, never by
        /// provider output. An unknown MCP tool receives the conservative
        /// all-effects declaration before it reaches this broker.
        effects: ProcessEffects,
    },
    ArtifactRead {
        path: PathBuf,
    },
    ArtifactWrite {
        path: PathBuf,
    },
    Delegate {
        role: String,
        task: String,
    },
}

/// Full policy snapshot reloaded at every effect boundary. Its fingerprint is
/// part of approval scope, so widening or narrowing after an approval was
/// issued invalidates that approval.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicySnapshot {
    pub effective: EffectivePolicy,
    pub safety: SafetyPolicy,
    pub fingerprint: String,
}

impl PolicySnapshot {
    pub fn new(effective: EffectivePolicy, safety: SafetyPolicy) -> Result<Self, BrokerError> {
        #[derive(Serialize)]
        struct Body<'a> {
            effective: &'a EffectivePolicy,
            safety: &'a SafetyPolicy,
        }
        let fingerprint = digest_json(&Body {
            effective: &effective,
            safety: &safety,
        })?;
        Ok(Self {
            effective,
            safety,
            fingerprint,
        })
    }
}

pub trait PolicySource: std::fmt::Debug + Send + Sync {
    fn current(&self) -> Result<PolicySnapshot, BrokerError>;
}

#[derive(Clone, Debug)]
pub struct StaticPolicySource(pub PolicySnapshot);

impl PolicySource for StaticPolicySource {
    fn current(&self) -> Result<PolicySnapshot, BrokerError> {
        Ok(self.0.clone())
    }
}

/// Production source: reloads both operator and repo layers for every effect.
#[derive(Clone, Debug)]
pub struct ConfigPolicySource {
    repo: PathBuf,
}

impl ConfigPolicySource {
    pub fn new(repo: PathBuf) -> Self {
        Self { repo }
    }
}

impl PolicySource for ConfigPolicySource {
    fn current(&self) -> Result<PolicySnapshot, BrokerError> {
        let env = env_from_process();
        let cfg = CtxConfig::load_for_launch(&self.repo, &env)
            .map_err(|error| BrokerError::PolicyUnavailable(error.to_string()))?;
        if let Some(layer) = cfg.unparsable_layers.first() {
            return Err(BrokerError::PolicyUnavailable(format!(
                "{}: {}; native effects fail closed while any policy layer is unparsable",
                layer.path.display(),
                layer.message
            )));
        }
        PolicySnapshot::new(cfg.policy, cfg.safety)
    }
}

pub trait GenerationFence: std::fmt::Debug + Send + Sync {
    fn verify(&self, identity: &ExecutionIdentity) -> Result<(), BrokerError>;
}

/// Effect-time fence backed by the canonical persisted seat store.
#[derive(Clone, Debug)]
pub struct StoredSeatFence {
    state: super::super::state::StateDir,
}

impl StoredSeatFence {
    pub fn new(state: super::super::state::StateDir) -> Self {
        Self { state }
    }
}

impl GenerationFence for StoredSeatFence {
    fn verify(&self, identity: &ExecutionIdentity) -> Result<(), BrokerError> {
        let current = seat::load(&self.state, &identity.short).ok_or_else(|| {
            BrokerError::Identity(format!("seat {} no longer exists", identity.short))
        })?;
        if current.session != identity.session
            || current.short != identity.short
            || current.role != identity.role
            || current.runtime != RuntimeKind::Native
        {
            return Err(BrokerError::Identity(format!(
                "seat {} no longer matches this native session identity",
                identity.short
            )));
        }
        if current.generation != identity.generation {
            return Err(BrokerError::StaleGeneration {
                expected: current.generation,
                got: identity.generation,
            });
        }
        if matches!(current.phase, seat::Phase::Parked { .. }) {
            return Err(BrokerError::Identity(format!(
                "seat {} is parked and cannot perform effects",
                identity.short
            )));
        }
        Ok(())
    }
}

pub trait WriterLease: std::fmt::Debug + Send + Sync {
    fn covers(&self, worktree: &Path) -> bool;
}

impl WriterLease for HeavyPermit {
    fn covers(&self, worktree: &Path) -> bool {
        self.writer_tree().is_some_and(|tree| {
            super::super::permit::tree_key(tree) == super::super::permit::tree_key(worktree)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Interactive,
    Headless,
}

/// A request shown to the operator. All fields that can change authority are
/// folded into `scope_digest`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub scope_digest: String,
    pub identity: ExecutionIdentity,
    pub action: ExecutionAction,
    pub policy_fingerprint: String,
    pub claims_fingerprint: String,
    pub resolved_paths: Vec<PathBuf>,
    pub execution_scope_fingerprint: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalGrant {
    scope_digest: String,
    approved_by: String,
    issued_at: u64,
    expires_at: Option<u64>,
    signature: String,
}

/// Process-local approval signer. Provider/model code receives grants, never
/// this authority.
#[derive(Debug)]
pub struct ApprovalAuthority {
    secret: [u8; 32],
}

impl ApprovalAuthority {
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(uuid::Uuid::new_v4().as_bytes());
        hasher.update(uuid::Uuid::new_v4().as_bytes());
        hasher.update(std::process::id().to_le_bytes());
        let secret: [u8; 32] = hasher.finalize().into();
        Self { secret }
    }

    pub fn approve(
        &self,
        request: &ApprovalRequest,
        approved_by: impl Into<String>,
        issued_at: u64,
        expires_at: Option<u64>,
    ) -> Result<ApprovalGrant, BrokerError> {
        #[derive(Serialize)]
        struct Scope<'a> {
            identity: &'a ExecutionIdentity,
            action: &'a ExecutionAction,
            policy_fingerprint: &'a str,
            claims_fingerprint: &'a str,
            resolved_paths: &'a [PathBuf],
            execution_scope_fingerprint: &'a str,
        }
        let expected = digest_json(&Scope {
            identity: &request.identity,
            action: &request.action,
            policy_fingerprint: &request.policy_fingerprint,
            claims_fingerprint: &request.claims_fingerprint,
            resolved_paths: &request.resolved_paths,
            execution_scope_fingerprint: &request.execution_scope_fingerprint,
        })?;
        if expected != request.scope_digest {
            return Err(BrokerError::InvalidAction(
                "approval request scope digest is invalid".to_string(),
            ));
        }
        let approved_by = approved_by.into();
        let signature = self.sign_grant(&request.scope_digest, &approved_by, issued_at, expires_at);
        Ok(ApprovalGrant {
            scope_digest: request.scope_digest.clone(),
            approved_by,
            issued_at,
            expires_at,
            signature,
        })
    }

    fn verify(&self, grant: &ApprovalGrant, request: &ApprovalRequest, now: u64) -> bool {
        grant.scope_digest == request.scope_digest
            && grant.issued_at <= now
            && grant.expires_at.is_none_or(|expiry| now <= expiry)
            && constant_time_eq(
                grant.signature.as_bytes(),
                self.sign_grant(
                    &request.scope_digest,
                    &grant.approved_by,
                    grant.issued_at,
                    grant.expires_at,
                )
                .as_bytes(),
            )
    }

    fn sign_grant(
        &self,
        digest: &str,
        approved_by: &str,
        issued_at: u64,
        expires_at: Option<u64>,
    ) -> String {
        let mut inner = Sha256::new();
        inner.update(digest.as_bytes());
        inner.update([0]);
        inner.update(approved_by.as_bytes());
        inner.update([0]);
        inner.update(issued_at.to_le_bytes());
        inner.update(expires_at.unwrap_or(u64::MAX).to_le_bytes());
        let inner = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.secret);
        outer.update(inner);
        outer.update(self.secret);
        hex_digest(outer.finalize())
    }
}

impl Default for ApprovalAuthority {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSandboxPolicy {
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub masked_roots: Vec<PathBuf>,
    pub network: bool,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mechanism", rename_all = "snake_case")]
pub enum PlatformIsolation {
    LinuxBubblewrap { executable: PathBuf },
    MacOsSeatbelt { executable: PathBuf },
    WindowsRestrictedToken { helper: PathBuf },
    Unavailable { platform: String, reason: String },
}

impl PlatformIsolation {
    pub fn detect() -> Self {
        #[cfg(target_os = "linux")]
        {
            find_executable("bwrap")
                .map(|executable| Self::LinuxBubblewrap { executable })
                .unwrap_or_else(|| Self::Unavailable {
                    platform: "linux".to_string(),
                    reason: "bubblewrap (bwrap) is not installed or not executable".to_string(),
                })
        }
        #[cfg(target_os = "macos")]
        {
            let executable = PathBuf::from("/usr/bin/sandbox-exec");
            return if executable.is_file() {
                Self::MacOsSeatbelt { executable }
            } else {
                Self::Unavailable {
                    platform: "macos".to_string(),
                    reason: "macOS sandbox-exec is unavailable".to_string(),
                }
            };
        }
        #[cfg(windows)]
        {
            Self::Unavailable {
                platform: "windows".to_string(),
                reason: "the Zirv restricted-token/AppContainer helper is not installed; a Job Object alone is not containment".to_string(),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            Self::Unavailable {
                platform: std::env::consts::OS.to_string(),
                reason: "no verified native process isolation backend exists for this platform"
                    .to_string(),
            }
        }
    }

    pub fn is_available(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }

    pub fn mechanism(&self) -> &'static str {
        match self {
            Self::LinuxBubblewrap { .. } => "linux-bubblewrap",
            Self::MacOsSeatbelt { .. } => "macos-seatbelt",
            Self::WindowsRestrictedToken { .. } => "windows-restricted-token-appcontainer",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    pub fn windows_helper(helper: PathBuf) -> Result<Self, BrokerError> {
        if !helper.is_file() {
            return Err(BrokerError::IsolationUnavailable(format!(
                "Windows sandbox helper {} is not a regular file",
                helper.display()
            )));
        }
        Ok(Self::WindowsRestrictedToken { helper })
    }

    pub fn prepare(
        &self,
        invocation: &ProcessInvocation,
        policy: &ProcessSandboxPolicy,
    ) -> Result<SandboxLaunch, BrokerError> {
        let (program, command_args) = invocation.command_parts();
        match self {
            Self::Unavailable { platform, reason } => Err(BrokerError::IsolationUnavailable(
                format!("{platform}: {reason}"),
            )),
            Self::LinuxBubblewrap { executable } => {
                let mut args = strings([
                    "--die-with-parent",
                    "--new-session",
                    "--unshare-all",
                    "--ro-bind",
                    "/",
                    "/",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--clearenv",
                ]);
                if policy.network {
                    args.push("--share-net".into());
                }
                for root in &policy.write_roots {
                    args.extend(strings(["--bind"]));
                    args.push(root.as_os_str().to_owned());
                    args.push(root.as_os_str().to_owned());
                }
                // Apply masks after writable binds so a protected child path
                // cannot be re-exposed by a broader parent write root.
                for root in &policy.masked_roots {
                    if root.is_dir() {
                        args.extend(strings(["--tmpfs"]));
                        args.push(root.as_os_str().to_owned());
                    } else if root.exists() {
                        args.extend(strings(["--ro-bind", "/dev/null"]));
                        args.push(root.as_os_str().to_owned());
                    }
                }
                for (key, value) in &policy.environment {
                    args.extend(strings(["--setenv"]));
                    args.push(OsString::from(key.as_str()));
                    args.push(OsString::from(value.as_str()));
                }
                args.extend(strings(["--chdir"]));
                args.push(invocation.cwd().as_os_str().to_owned());
                args.push("--".into());
                args.push(program.into());
                args.extend(command_args.into_iter().map(OsString::from));
                Ok(SandboxLaunch {
                    program: executable.clone(),
                    args,
                    cwd: invocation.cwd().to_path_buf(),
                    environment: BTreeMap::new(),
                })
            }
            Self::MacOsSeatbelt { executable } => {
                let profile = seatbelt_profile(policy);
                let mut args = strings(["-p"]);
                args.push(profile.into());
                args.push(program.into());
                args.extend(command_args.into_iter().map(OsString::from));
                Ok(SandboxLaunch {
                    program: executable.clone(),
                    args,
                    cwd: invocation.cwd().to_path_buf(),
                    environment: policy.environment.clone(),
                })
            }
            Self::WindowsRestrictedToken { helper } => {
                let profile = serde_json::to_string(policy)
                    .map_err(|error| BrokerError::Internal(error.to_string()))?;
                let mut args = strings(["--profile-json"]);
                args.push(profile.into());
                args.push("--".into());
                args.push(program.into());
                args.extend(command_args.into_iter().map(OsString::from));
                Ok(SandboxLaunch {
                    program: helper.clone(),
                    args,
                    cwd: invocation.cwd().to_path_buf(),
                    environment: policy.environment.clone(),
                })
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Authorization {
    action_digest: String,
    action_fingerprint: String,
    policy_fingerprint: String,
    approved_by: Option<String>,
    approval_expires_at: Option<u64>,
    /// Canonical targets checked at the effect boundary. Concrete file tools
    /// use these rather than reopening the unresolved model-supplied spelling.
    resolved_paths: Vec<PathBuf>,
    process_sandbox: Option<ProcessSandboxPolicy>,
}

impl Authorization {
    pub fn policy_fingerprint(&self) -> &str {
        &self.policy_fingerprint
    }

    pub fn approved_by(&self) -> Option<&str> {
        self.approved_by.as_deref()
    }

    pub fn resolved_paths(&self) -> &[PathBuf] {
        &self.resolved_paths
    }

    pub fn process_sandbox(&self) -> Option<&ProcessSandboxPolicy> {
        self.process_sandbox.as_ref()
    }
}

pub struct ExecutionBroker {
    identity: ExecutionIdentity,
    claims: ResourceClaims,
    approval_mode: ApprovalMode,
    policy: Arc<dyn PolicySource>,
    fence: Arc<dyn GenerationFence>,
    approval_authority: Arc<ApprovalAuthority>,
    writer: Option<Box<dyn WriterLease>>,
    isolation: PlatformIsolation,
    protected_env_names: BTreeSet<String>,
}

impl std::fmt::Debug for ExecutionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionBroker")
            .field("identity", &self.identity)
            .field("claims", &self.claims)
            .field("approval_mode", &self.approval_mode)
            .field("isolation", &self.isolation)
            .finish_non_exhaustive()
    }
}

impl ExecutionBroker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: ExecutionIdentity,
        claims: ResourceClaims,
        approval_mode: ApprovalMode,
        policy: Arc<dyn PolicySource>,
        fence: Arc<dyn GenerationFence>,
        approval_authority: Arc<ApprovalAuthority>,
        writer: Option<Box<dyn WriterLease>>,
        isolation: PlatformIsolation,
        protected_env_names: BTreeSet<String>,
    ) -> Result<Self, BrokerError> {
        if let Some(writer) = writer.as_ref()
            && !writer.covers(&claims.worktree_root)
        {
            return Err(BrokerError::WriterPermit(
                "writer permit does not cover this worktree".to_string(),
            ));
        }
        Ok(Self {
            identity,
            claims,
            approval_mode,
            policy,
            fence,
            approval_authority,
            writer,
            isolation,
            protected_env_names: protected_env_names
                .into_iter()
                .map(|name| name.to_ascii_uppercase())
                .collect(),
        })
    }

    pub fn isolation_status(&self) -> (&'static str, bool) {
        (self.isolation.mechanism(), self.isolation.is_available())
    }

    pub fn authorize(
        &self,
        action: &ExecutionAction,
        grant: Option<&ApprovalGrant>,
    ) -> Result<Authorization, BrokerError> {
        self.authorize_at(action, grant, super::super::state::now_secs())
    }

    pub fn authorize_at(
        &self,
        action: &ExecutionAction,
        grant: Option<&ApprovalGrant>,
        now: u64,
    ) -> Result<Authorization, BrokerError> {
        self.fence.verify(&self.identity)?;
        let snapshot = self.policy.current()?;
        let validation = self.validate_action(action, &snapshot)?;
        let request = self.approval_request(
            action,
            &snapshot,
            &validation.resolved_paths,
            validation.process_sandbox.as_ref(),
            now,
        )?;
        let mut approved_by = None;
        let mut approval_expires_at = None;

        if validation.needs_approval {
            if snapshot.effective.approval == Stance::Deny {
                return Err(BrokerError::Denied(
                    "policy denies actions that require approval".to_string(),
                ));
            }
            if self.approval_mode == ApprovalMode::Headless {
                return Err(BrokerError::ApprovalUnavailable(Box::new(request)));
            }
            match grant {
                Some(grant) if self.approval_authority.verify(grant, &request, now) => {
                    approved_by = Some(grant.approved_by.clone());
                    approval_expires_at = grant.expires_at;
                }
                Some(_) => return Err(BrokerError::InvalidApproval(Box::new(request))),
                None => return Err(BrokerError::ApprovalRequired(Box::new(request))),
            }
        } else if grant.is_some() {
            // A grant for an action that no longer needs one is ignored. It
            // never broadens the freshly reloaded current policy.
        }

        Ok(Authorization {
            action_digest: request.scope_digest,
            action_fingerprint: digest_json(action)?,
            policy_fingerprint: snapshot.fingerprint,
            approved_by,
            approval_expires_at,
            resolved_paths: validation.resolved_paths,
            process_sandbox: validation.process_sandbox,
        })
    }

    pub fn prepare_process(
        &self,
        action: &ExecutionAction,
        authorization: &Authorization,
    ) -> Result<SandboxLaunch, BrokerError> {
        let ExecutionAction::Process { invocation, .. } = action else {
            return Err(BrokerError::InvalidAction(
                "prepare_process requires a process action".to_string(),
            ));
        };
        self.fence.verify(&self.identity)?;
        let snapshot = self.policy.current()?;
        if snapshot.fingerprint != authorization.policy_fingerprint {
            return Err(BrokerError::InvalidAction(
                "process authorization was invalidated by a policy change".to_string(),
            ));
        }
        if authorization
            .approval_expires_at
            .is_some_and(|expiry| super::super::state::now_secs() > expiry)
        {
            return Err(BrokerError::InvalidAction(
                "process authorization approval has expired".to_string(),
            ));
        }
        if authorization.action_fingerprint != digest_json(action)? {
            return Err(BrokerError::InvalidAction(
                "authorization belongs to a different process action".to_string(),
            ));
        }
        let current = self.validate_action(action, &snapshot)?;
        if current.resolved_paths != authorization.resolved_paths
            || current.process_sandbox.as_ref() != authorization.process_sandbox.as_ref()
        {
            return Err(BrokerError::InvalidAction(
                "process authorization was invalidated by an execution-scope change".to_string(),
            ));
        }
        let policy = authorization.process_sandbox.as_ref().ok_or_else(|| {
            BrokerError::InvalidAction("authorization has no process sandbox".to_string())
        })?;
        self.isolation.prepare(invocation, policy)
    }

    fn approval_request(
        &self,
        action: &ExecutionAction,
        snapshot: &PolicySnapshot,
        resolved_paths: &[PathBuf],
        process_sandbox: Option<&ProcessSandboxPolicy>,
        created_at: u64,
    ) -> Result<ApprovalRequest, BrokerError> {
        let claims_fingerprint = self.claims.fingerprint()?;
        #[derive(Serialize)]
        struct Scope<'a> {
            identity: &'a ExecutionIdentity,
            action: &'a ExecutionAction,
            policy_fingerprint: &'a str,
            claims_fingerprint: &'a str,
            resolved_paths: &'a [PathBuf],
            execution_scope_fingerprint: &'a str,
        }
        #[derive(Serialize)]
        struct ExecutionScope<'a> {
            isolation: &'a str,
            process_sandbox: Option<&'a ProcessSandboxPolicy>,
        }
        let execution_scope_fingerprint = digest_json(&ExecutionScope {
            isolation: self.isolation.mechanism(),
            process_sandbox,
        })?;
        let scope_digest = digest_json(&Scope {
            identity: &self.identity,
            action,
            policy_fingerprint: &snapshot.fingerprint,
            claims_fingerprint: &claims_fingerprint,
            resolved_paths,
            execution_scope_fingerprint: &execution_scope_fingerprint,
        })?;
        Ok(ApprovalRequest {
            scope_digest,
            identity: self.identity.clone(),
            action: action.clone(),
            policy_fingerprint: snapshot.fingerprint.clone(),
            claims_fingerprint,
            resolved_paths: resolved_paths.to_vec(),
            execution_scope_fingerprint,
            created_at,
        })
    }

    fn validate_action(
        &self,
        action: &ExecutionAction,
        snapshot: &PolicySnapshot,
    ) -> Result<ActionValidation, BrokerError> {
        let mut required = vec![Capability::ToolAccess];
        let mut needs_writer = false;
        let mut process_sandbox = None;
        let mut resolved_paths = Vec::new();

        match action {
            ExecutionAction::ReadFile { path } => resolved_paths.push(self.validate_read(path)?),
            ExecutionAction::WriteFile { path } => {
                let (path, capability) = self.validate_write(path)?;
                resolved_paths.push(path);
                required.push(capability);
                needs_writer = true;
            }
            ExecutionAction::Network { target } => {
                self.validate_network_target(target)?;
                required.push(Capability::Network);
            }
            ExecutionAction::ArtifactRead { path } | ExecutionAction::ArtifactWrite { path } => {
                resolved_paths.push(self.validate_artifact(path)?);
            }
            ExecutionAction::Mcp {
                server,
                tool,
                effects,
                ..
            } => {
                if server.trim().is_empty() || tool.trim().is_empty() {
                    return Err(BrokerError::InvalidAction(
                        "MCP server and tool names must not be empty".to_string(),
                    ));
                }
                if effects.repo_write || effects.git_metadata_write {
                    required.push(Capability::RepoFsWrite);
                    needs_writer = true;
                }
                if effects.outside_write {
                    if self.claims.outside_write_roots.is_empty() {
                        return Err(BrokerError::Scope(
                            "MCP tool requested outside writes without an outside-write resource claim"
                                .to_string(),
                        ));
                    }
                    required.push(Capability::OutsideRepoFsWrite);
                    needs_writer = true;
                }
                if effects.network {
                    if !matches!(self.claims.network, NetworkScope::Any) {
                        return Err(BrokerError::Scope(
                            "MCP tools may use network only with an unrestricted operator network scope; use a brokered network tool for host-scoped access"
                                .to_string(),
                        ));
                    }
                    required.push(Capability::Network);
                }
                if effects.git_metadata_write && self.claims.git_write_roots.is_empty() {
                    return Err(BrokerError::Scope(
                        "MCP git metadata writes require linked-worktree git resource claims"
                            .to_string(),
                    ));
                }
                if effects.git_push_or_destructive {
                    required.push(Capability::GitPushDestructive);
                }
            }
            ExecutionAction::Delegate { role, task } => {
                if role.trim().is_empty() || task.trim().is_empty() {
                    return Err(BrokerError::InvalidAction(
                        "delegation role and task must not be empty".to_string(),
                    ));
                }
            }
            ExecutionAction::Process {
                invocation,
                effects,
            } => {
                if !self.isolation.is_available() {
                    let PlatformIsolation::Unavailable { platform, reason } = &self.isolation
                    else {
                        unreachable!()
                    };
                    return Err(BrokerError::IsolationUnavailable(format!(
                        "{platform}: {reason}"
                    )));
                }
                resolved_paths.push(self.validate_read(invocation.cwd())?);
                required.push(Capability::ShellExec);
                if effects.repo_write || effects.git_metadata_write {
                    required.push(Capability::RepoFsWrite);
                    needs_writer = true;
                }
                if effects.outside_write {
                    if self.claims.outside_write_roots.is_empty() {
                        return Err(BrokerError::Scope(
                            "process requested outside writes without an outside-write resource claim"
                                .to_string(),
                        ));
                    }
                    required.push(Capability::OutsideRepoFsWrite);
                    needs_writer = true;
                }
                if effects.network {
                    if !matches!(self.claims.network, NetworkScope::Any) {
                        return Err(BrokerError::Scope(
                            "arbitrary processes may use network only with an unrestricted operator network scope; use a brokered network tool for host-scoped access"
                                .to_string(),
                        ));
                    }
                    required.push(Capability::Network);
                }
                if effects.git_push_or_destructive {
                    required.push(Capability::GitPushDestructive);
                }
                match safety::evaluate(
                    &snapshot.safety,
                    &invocation.safety_command(),
                    match self.approval_mode {
                        ApprovalMode::Interactive => LaunchMode::Interactive,
                        ApprovalMode::Headless => LaunchMode::Headless,
                    },
                )
                .verdict
                {
                    Verdict::Deny => {
                        return Err(BrokerError::Denied(
                            "command is denied by the current safety policy".to_string(),
                        ));
                    }
                    Verdict::Ask => required.push(Capability::Approval),
                    Verdict::Allow => {}
                }
                process_sandbox = Some(self.process_policy(effects)?);
            }
        }

        if needs_writer
            && self
                .writer
                .as_ref()
                .is_none_or(|writer| !writer.covers(&self.claims.worktree_root))
        {
            return Err(BrokerError::WriterPermit(
                "a live writer permit for this exact worktree is required".to_string(),
            ));
        }

        required.sort_by_key(|capability| capability.key());
        required.dedup();
        let mut needs_approval = false;
        for capability in required {
            if capability == Capability::Approval {
                needs_approval = true;
                continue;
            }
            match snapshot.effective.stance(capability) {
                Stance::Allow => {}
                Stance::Ask => needs_approval = true,
                Stance::Deny => {
                    return Err(BrokerError::Denied(format!(
                        "{} is denied by the current policy",
                        capability.label()
                    )));
                }
            }
        }
        Ok(ActionValidation {
            needs_approval,
            process_sandbox,
            resolved_paths,
        })
    }

    fn validate_read(&self, path: &Path) -> Result<PathBuf, BrokerError> {
        let path = resolve_action_path(path, &self.claims.worktree_root)?;
        if inside_any(&path, &self.claims.protected_roots) {
            return Err(BrokerError::ProtectedPath(path));
        }
        if !inside_any(&path, &self.claims.read_roots) {
            return Err(BrokerError::Scope(format!(
                "{} is outside the task's read roots",
                path.display()
            )));
        }
        Ok(path)
    }

    fn validate_write(&self, path: &Path) -> Result<(PathBuf, Capability), BrokerError> {
        let path = resolve_action_path(path, &self.claims.worktree_root)?;
        if inside_any(&path, &self.claims.protected_roots) {
            return Err(BrokerError::ProtectedPath(path));
        }
        if path_within(&path, &self.claims.worktree_root) {
            return Ok((path, Capability::RepoFsWrite));
        }
        if inside_any(&path, &self.claims.outside_write_roots) {
            return Ok((path, Capability::OutsideRepoFsWrite));
        }
        Err(BrokerError::Scope(format!(
            "{} is outside the task's write roots",
            path.display()
        )))
    }

    fn validate_artifact(&self, path: &Path) -> Result<PathBuf, BrokerError> {
        let path = resolve_action_path(path, &self.claims.worktree_root)?;
        if inside_any(&path, &self.claims.artifact_roots) {
            Ok(path)
        } else {
            Err(BrokerError::Scope(format!(
                "{} is outside the task's artifact roots",
                path.display()
            )))
        }
    }

    fn validate_network_target(&self, target: &NetworkTarget) -> Result<(), BrokerError> {
        match &self.claims.network {
            NetworkScope::Denied => Err(BrokerError::Scope(
                "this task has no network resource claim".to_string(),
            )),
            NetworkScope::Any => Ok(()),
            NetworkScope::Only { targets } if targets.contains(target) => Ok(()),
            NetworkScope::Only { .. } => Err(BrokerError::Scope(format!(
                "network target {}://{} is outside the task's allowlist",
                target.scheme, target.host
            ))),
        }
    }

    fn process_policy(
        &self,
        effects: &ProcessEffects,
    ) -> Result<ProcessSandboxPolicy, BrokerError> {
        let mut write_roots = Vec::new();
        if effects.repo_write {
            write_roots.push(self.claims.worktree_root.clone());
        }
        if effects.git_metadata_write {
            if self.claims.git_write_roots.is_empty() {
                return Err(BrokerError::Scope(
                    "git metadata writes require linked-worktree git resource claims".to_string(),
                ));
            }
            write_roots.extend(self.claims.git_write_roots.clone());
        }
        if effects.outside_write {
            write_roots.extend(self.claims.outside_write_roots.clone());
        }
        Ok(ProcessSandboxPolicy {
            read_roots: self.claims.read_roots.clone(),
            write_roots: dedup_paths(write_roots),
            masked_roots: self
                .claims
                .protected_roots
                .iter()
                .filter(|protected| {
                    !self
                        .claims
                        .git_write_roots
                        .iter()
                        .any(|git_root| path_within(git_root, protected))
                })
                .cloned()
                .collect(),
            network: effects.network,
            environment: scrub_tool_environment(std::env::vars_os(), &self.protected_env_names),
        })
    }
}

#[derive(Debug)]
struct ActionValidation {
    needs_approval: bool,
    process_sandbox: Option<ProcessSandboxPolicy>,
    resolved_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrokerError {
    Identity(String),
    StaleGeneration { expected: u64, got: u64 },
    PolicyUnavailable(String),
    InvalidAction(String),
    Scope(String),
    ProtectedPath(PathBuf),
    WriterPermit(String),
    Denied(String),
    ApprovalRequired(Box<ApprovalRequest>),
    ApprovalUnavailable(Box<ApprovalRequest>),
    InvalidApproval(Box<ApprovalRequest>),
    IsolationUnavailable(String),
    Internal(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(message)
            | Self::PolicyUnavailable(message)
            | Self::InvalidAction(message)
            | Self::Scope(message)
            | Self::WriterPermit(message)
            | Self::Denied(message)
            | Self::IsolationUnavailable(message)
            | Self::Internal(message) => f.write_str(message),
            Self::StaleGeneration { expected, got } => {
                write!(
                    f,
                    "stale seat generation {got}; current generation is {expected}"
                )
            }
            Self::ProtectedPath(path) => {
                write!(
                    f,
                    "protected path is not available to native tools: {}",
                    path.display()
                )
            }
            Self::ApprovalRequired(_) => f.write_str("operator approval is required"),
            Self::ApprovalUnavailable(_) => {
                f.write_str("operator approval is required but this session is headless")
            }
            Self::InvalidApproval(_) => f.write_str(
                "approval does not match the current action, policy, scope, or generation",
            ),
        }
    }
}

impl std::error::Error for BrokerError {}

/// Builds the environment visible to native subprocesses. Provider secrets
/// and common credential-bearing variables are removed; invalid Unicode is
/// dropped rather than passed through an alternate byte channel.
pub fn scrub_tool_environment<I>(
    environment: I,
    protected_names: &BTreeSet<String>,
) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let upper = key.to_ascii_uppercase();
            if protected_names.contains(&upper) || sensitive_env_name(&upper) {
                return None;
            }
            Some((key, value.into_string().ok()?))
        })
        .collect()
}

fn sensitive_env_name(upper: &str) -> bool {
    const EXACT: &[&str] = &[
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "GIT_ASKPASS",
        "GIT_SSH_COMMAND",
        "GH_CONFIG_DIR",
        "DOCKER_CONFIG",
        "KUBECONFIG",
        "GNUPGHOME",
        "AWS_PROFILE",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ];
    EXACT.contains(&upper)
        || upper.split(['_', '-']).any(|part| {
            matches!(
                part,
                "TOKEN" | "SECRET" | "PASSWORD" | "CREDENTIAL" | "CREDENTIALS"
            )
        })
        || upper.ends_with("_API_KEY")
        || upper.ends_with("_ACCESS_KEY")
        || upper.ends_with("_PRIVATE_KEY")
}

fn canonical_existing_dir(path: &Path, label: &str) -> Result<PathBuf, BrokerError> {
    let path = std::fs::canonicalize(path).map_err(|error| {
        BrokerError::Scope(format!(
            "could not resolve {label} {}: {error}",
            path.display()
        ))
    })?;
    if !path.is_dir() {
        return Err(BrokerError::Scope(format!(
            "{label} {} is not a directory",
            path.display()
        )));
    }
    Ok(strip_windows_verbatim(path))
}

fn resolve_action_path(path: &Path, base: &Path) -> Result<PathBuf, BrokerError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_scope_path(&joined)
}

/// Canonicalizes the nearest existing ancestor, then appends missing path
/// components. This catches symlink/junction escapes for both reads and new
/// write targets without requiring the final file to exist already.
fn normalize_scope_path(path: &Path) -> Result<PathBuf, BrokerError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(BrokerError::Scope(format!(
            "path must not contain `..`: {}",
            path.display()
        )));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| BrokerError::Internal(error.to_string()))?
            .join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            BrokerError::Scope(format!("could not resolve path {}", absolute.display()))
        })?;
        missing.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| {
            BrokerError::Scope(format!("could not resolve path {}", absolute.display()))
        })?;
    }
    let mut resolved = std::fs::canonicalize(ancestor).map_err(|error| {
        BrokerError::Scope(format!("could not resolve {}: {error}", ancestor.display()))
    })?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(strip_windows_verbatim(resolved))
}

#[cfg(windows)]
fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    let rendered = path.to_string_lossy();
    PathBuf::from(rendered.strip_prefix(r"\\?\").unwrap_or(rendered.as_ref()))
}

#[cfg(not(windows))]
fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    path
}

fn path_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        fn normalized(path: &Path) -> String {
            let rendered = path.to_string_lossy().replace('/', "\\");
            let rendered = rendered
                .strip_prefix(r"\\?\UNC\")
                .map(|rest| format!(r"\\{rest}"))
                .or_else(|| rendered.strip_prefix(r"\\?\").map(str::to_string))
                .unwrap_or(rendered);
            rendered.to_ascii_lowercase()
        }

        let path = normalized(path);
        let mut root = normalized(root);
        if !root.ends_with('\\') {
            root.push('\\');
        }
        path == root.trim_end_matches('\\') || path.starts_with(&root)
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_within(left, right) && path_within(right, left)
}

fn inside_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path_within(path, root))
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !out.iter().any(|existing| same_path(existing, &path)) {
            out.push(path);
        }
    }
    out
}

fn git_path(worktree: &Path, flag: &str) -> Result<PathBuf, BrokerError> {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--path-format=absolute", flag])
        .output()
        .map_err(|error| BrokerError::Scope(format!("git rev-parse {flag}: {error}")))?;
    if !output.status.success() {
        return Err(BrokerError::Scope(format!(
            "git rev-parse {flag}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    normalize_scope_path(Path::new(String::from_utf8_lossy(&output.stdout).trim()))
}

#[cfg(target_os = "linux")]
fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            if !candidate.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                candidate
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

fn shell_quote_for_classification(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_./:@%+=,-".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn seatbelt_profile(policy: &ProcessSandboxPolicy) -> String {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow file-read*)\n",
    );
    for root in &policy.masked_roots {
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{}\"))\n",
            seatbelt_escape(root)
        ));
    }
    for root in &policy.write_roots {
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            seatbelt_escape(root)
        ));
    }
    if policy.network {
        profile.push_str("(allow network*)\n");
    }
    profile
}

fn seatbelt_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn digest_json(value: &impl Serialize) -> Result<String, BrokerError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| BrokerError::Internal(error.to_string()))?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct TestFence(Mutex<u64>);

    impl GenerationFence for TestFence {
        fn verify(&self, identity: &ExecutionIdentity) -> Result<(), BrokerError> {
            let current = *self.0.lock().expect("generation lock");
            if current == identity.generation {
                Ok(())
            } else {
                Err(BrokerError::StaleGeneration {
                    expected: current,
                    got: identity.generation,
                })
            }
        }
    }

    #[derive(Debug)]
    struct TestWriter(PathBuf);

    impl WriterLease for TestWriter {
        fn covers(&self, worktree: &Path) -> bool {
            same_path(&self.0, worktree)
        }
    }

    #[derive(Debug)]
    struct MutablePolicy(Mutex<PolicySnapshot>);

    impl PolicySource for MutablePolicy {
        fn current(&self) -> Result<PolicySnapshot, BrokerError> {
            Ok(self.0.lock().expect("policy lock").clone())
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        worktree: PathBuf,
        outside: PathBuf,
        broker: ExecutionBroker,
        authority: Arc<ApprovalAuthority>,
        policy: Arc<MutablePolicy>,
        generation: Arc<TestFence>,
    }

    fn fixture(
        effective: EffectivePolicy,
        approval_mode: ApprovalMode,
        with_writer: bool,
    ) -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let worktree = root.path().join("worktree");
        let outside = root.path().join("outside");
        let state = root.path().join("state");
        for path in [&workspace, &worktree, &outside, &state] {
            std::fs::create_dir_all(path).expect("create root");
        }
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let worktree = std::fs::canonicalize(worktree).expect("canonical worktree");
        let claims = ResourceClaims::new(&workspace, &worktree, &state, &home, NetworkScope::Any)
            .expect("claims")
            .allow_outside_writes(&outside)
            .expect("outside claim");
        let policy = Arc::new(MutablePolicy(Mutex::new(
            PolicySnapshot::new(effective, SafetyPolicy::default()).expect("policy"),
        )));
        let generation = Arc::new(TestFence(Mutex::new(7)));
        let authority = Arc::new(ApprovalAuthority::new());
        let writer =
            with_writer.then(|| Box::new(TestWriter(worktree.clone())) as Box<dyn WriterLease>);
        let broker = ExecutionBroker::new(
            ExecutionIdentity {
                session: "session-1".to_string(),
                short: "abcd1234".to_string(),
                generation: 7,
                role: "worker".to_string(),
                task: Some("task-1".to_string()),
            },
            claims,
            approval_mode,
            policy.clone(),
            generation.clone(),
            authority.clone(),
            writer,
            PlatformIsolation::Unavailable {
                platform: "test".to_string(),
                reason: "no test sandbox".to_string(),
            },
            BTreeSet::from(["OPENAI_API_KEY".to_string()]),
        )
        .expect("broker");
        Fixture {
            _root: root,
            worktree,
            outside: std::fs::canonicalize(outside).expect("canonical outside"),
            broker,
            authority,
            policy,
            generation,
        }
    }

    #[test]
    fn read_only_policy_cannot_mutate_through_file_or_mcp_paths() {
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny,
            outside_repo_fs_write: Stance::Deny,
            shell_exec: Stance::Deny,
            network: Some(Stance::Deny),
            approval: Stance::Deny,
            git_push_destructive: Stance::Deny,
            // Read-only tools themselves remain available. The mutation is
            // rejected from the MCP registry's declared effect, not merely
            // because all tool access happened to be disabled.
            tool_access: Stance::Allow,
        };
        let fixture = fixture(policy, ApprovalMode::Headless, true);
        let write = ExecutionAction::WriteFile {
            path: fixture.worktree.join("changed.rs"),
        };
        assert!(matches!(
            fixture.broker.authorize_at(&write, None, 10),
            Err(BrokerError::Denied(_))
        ));
        let mcp = ExecutionAction::Mcp {
            server: "filesystem".to_string(),
            tool: "write".to_string(),
            arguments: serde_json::json!({"path": "changed.rs"}),
            effects: ProcessEffects {
                repo_write: true,
                ..ProcessEffects::default()
            },
        };
        assert!(matches!(
            fixture.broker.authorize_at(&mcp, None, 10),
            Err(BrokerError::Denied(_))
        ));
    }

    #[test]
    fn workspace_write_needs_the_exact_writer_permit_and_scope() {
        let policy = EffectivePolicy {
            approval: Stance::Allow,
            ..EffectivePolicy::default()
        };
        let permitted = fixture(policy, ApprovalMode::Headless, true);
        let action = ExecutionAction::WriteFile {
            path: permitted.worktree.join("src/new.rs"),
        };
        permitted
            .broker
            .authorize_at(&action, None, 10)
            .expect("worktree write");

        let no_permit = fixture(policy, ApprovalMode::Headless, false);
        let action = ExecutionAction::WriteFile {
            path: no_permit.worktree.join("src/new.rs"),
        };
        assert!(matches!(
            no_permit.broker.authorize_at(&action, None, 10),
            Err(BrokerError::WriterPermit(_))
        ));

        let escaped = ExecutionAction::WriteFile {
            path: permitted
                .outside
                .parent()
                .expect("parent")
                .join("not-claimed/file"),
        };
        assert!(matches!(
            permitted.broker.authorize_at(&escaped, None, 10),
            Err(BrokerError::Scope(_))
        ));
    }

    #[test]
    fn approval_is_bound_to_action_policy_generation_and_parent_identity() {
        let effective = EffectivePolicy {
            repo_fs_write: Stance::Ask,
            approval: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let fixture = fixture(effective, ApprovalMode::Interactive, true);
        let first = ExecutionAction::WriteFile {
            path: fixture.worktree.join("one.rs"),
        };
        let request = match fixture.broker.authorize_at(&first, None, 10) {
            Err(BrokerError::ApprovalRequired(request)) => request,
            other => panic!("expected approval request, got {other:?}"),
        };
        let grant = fixture
            .authority
            .approve(&request, "operator", 10, Some(20))
            .expect("valid approval request");
        fixture
            .broker
            .authorize_at(&first, Some(&grant), 11)
            .expect("exact grant");

        let changed_args = ExecutionAction::WriteFile {
            path: fixture.worktree.join("two.rs"),
        };
        assert!(matches!(
            fixture.broker.authorize_at(&changed_args, Some(&grant), 11),
            Err(BrokerError::InvalidApproval(_))
        ));

        *fixture.generation.0.lock().expect("generation lock") = 8;
        assert!(matches!(
            fixture.broker.authorize_at(&first, Some(&grant), 11),
            Err(BrokerError::StaleGeneration { .. })
        ));
        *fixture.generation.0.lock().expect("generation lock") = 7;

        let mut changed = effective;
        changed.outside_repo_fs_write = Stance::Deny;
        *fixture.policy.0.lock().expect("policy lock") =
            PolicySnapshot::new(changed, SafetyPolicy::default()).expect("changed policy");
        assert!(matches!(
            fixture.broker.authorize_at(&first, Some(&grant), 11),
            Err(BrokerError::InvalidApproval(_))
        ));

        *fixture.policy.0.lock().expect("policy lock") =
            PolicySnapshot::new(effective, SafetyPolicy::default()).expect("original policy");
        let child = ExecutionBroker::new(
            ExecutionIdentity {
                session: "session-2".to_string(),
                short: "dcba4321".to_string(),
                generation: 7,
                role: "reviewer".to_string(),
                task: Some("task-2".to_string()),
            },
            fixture.broker.claims.clone(),
            ApprovalMode::Interactive,
            fixture.policy.clone(),
            fixture.generation.clone(),
            fixture.authority.clone(),
            Some(Box::new(TestWriter(fixture.worktree.clone()))),
            PlatformIsolation::Unavailable {
                platform: "test".to_string(),
                reason: "no test sandbox".to_string(),
            },
            BTreeSet::new(),
        )
        .expect("child broker");
        assert!(matches!(
            child.authorize_at(&first, Some(&grant), 11),
            Err(BrokerError::InvalidApproval(_))
        ));
    }

    #[test]
    fn headless_approval_is_an_explicit_refusal() {
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Ask,
            approval: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let fixture = fixture(policy, ApprovalMode::Headless, true);
        let action = ExecutionAction::WriteFile {
            path: fixture.worktree.join("one.rs"),
        };
        assert!(matches!(
            fixture.broker.authorize_at(&action, None, 10),
            Err(BrokerError::ApprovalUnavailable(_))
        ));
    }

    #[test]
    fn symlink_escape_and_protected_state_are_blocked() {
        let policy = EffectivePolicy {
            approval: Stance::Allow,
            ..EffectivePolicy::default()
        };
        let fixture = fixture(policy, ApprovalMode::Headless, true);
        let protected = fixture._root.path().join("state/secret");
        std::fs::write(&protected, "secret").expect("write protected fixture");
        let action = ExecutionAction::ReadFile { path: protected };
        assert!(matches!(
            fixture.broker.authorize_at(&action, None, 10),
            Err(BrokerError::ProtectedPath(_))
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&fixture.outside, fixture.worktree.join("escape"))
                .expect("symlink");
            let action = ExecutionAction::ReadFile {
                path: fixture.worktree.join("escape/file"),
            };
            assert!(matches!(
                fixture.broker.authorize_at(&action, None, 10),
                Err(BrokerError::Scope(_))
            ));
        }

        #[cfg(windows)]
        {
            let junction = fixture.worktree.join("escape-junction");
            let status = Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(&junction)
                .arg(&fixture.outside)
                .status()
                .expect("create junction");
            assert!(status.success(), "Windows CI must support a test junction");
            let action = ExecutionAction::ReadFile {
                path: junction.join("file"),
            };
            assert!(matches!(
                fixture.broker.authorize_at(&action, None, 10),
                Err(BrokerError::Scope(_))
            ));
        }
    }

    #[test]
    fn provider_credentials_are_scrubbed_from_tool_environment() {
        let env = vec![
            ("PATH".into(), "/bin".into()),
            ("OPENAI_API_KEY".into(), "secret".into()),
            ("GITHUB_TOKEN".into(), "secret".into()),
            ("PROJECT_NAME".into(), "zirv".into()),
        ];
        let clean = scrub_tool_environment(env, &BTreeSet::from(["OPENAI_API_KEY".to_string()]));
        assert_eq!(clean.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(clean.get("PROJECT_NAME").map(String::as_str), Some("zirv"));
        assert!(!clean.contains_key("OPENAI_API_KEY"));
        assert!(!clean.contains_key("GITHUB_TOKEN"));
    }

    #[test]
    fn unavailable_process_isolation_never_falls_back_to_a_plain_spawn() {
        let fixture = fixture(EffectivePolicy::default(), ApprovalMode::Headless, true);
        let action = ExecutionAction::Process {
            invocation: ProcessInvocation::Argv {
                program: "git".to_string(),
                args: vec!["status".to_string()],
                cwd: fixture.worktree.clone(),
            },
            effects: ProcessEffects::default(),
        };
        assert!(matches!(
            fixture.broker.authorize_at(&action, None, 10),
            Err(BrokerError::IsolationUnavailable(_))
        ));
    }

    #[test]
    fn sandbox_profiles_mask_credentials_and_expose_only_declared_writes() {
        let root = tempfile::tempdir().expect("tempdir");
        let protected = root.path().join("credentials");
        let writable = root.path().join("worktree");
        std::fs::create_dir_all(&protected).expect("protected");
        std::fs::create_dir_all(&writable).expect("writable");
        let policy = ProcessSandboxPolicy {
            read_roots: vec![writable.clone()],
            write_roots: vec![writable.clone()],
            masked_roots: vec![protected.clone()],
            network: false,
            environment: BTreeMap::from([("PATH".to_string(), "/bin".to_string())]),
        };
        let profile = seatbelt_profile(&policy);
        assert!(profile.contains("(deny file-read* file-write*"));
        assert!(profile.contains("(allow file-write*"));
        assert!(!profile.contains("(allow network*)"));

        let invocation = ProcessInvocation::Argv {
            program: "git".to_string(),
            args: vec!["status".to_string()],
            cwd: writable,
        };
        let launch = PlatformIsolation::LinuxBubblewrap {
            executable: PathBuf::from("/usr/bin/bwrap"),
        }
        .prepare(&invocation, &policy)
        .expect("linux launch plan");
        let rendered = launch
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("--unshare-all"));
        assert!(!rendered.contains("--share-net"));
        assert!(rendered.contains("--clearenv"));
        assert!(rendered.contains("--bind"));
    }

    #[test]
    fn linked_worktree_git_claims_are_narrow_and_main_git_dir_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let main = root.path().join("main");
        let linked = root.path().join("linked");
        std::fs::create_dir_all(&main).expect("main");
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&main)
            .status()
            .expect("git init");
        assert!(status.success());
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=Zirv Test",
                "-c",
                "user.email=test@zirv.invalid",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "initial",
            ])
            .current_dir(&main)
            .status()
            .expect("git commit");
        assert!(status.success());
        let status = Command::new("git")
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&linked)
            .current_dir(&main)
            .status()
            .expect("git worktree add");
        assert!(status.success());
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).expect("state");
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).expect("home");

        let linked_claims =
            ResourceClaims::new(&main, &linked, &state, &home, NetworkScope::Denied)
                .expect("claims")
                .discover_linked_worktree_git()
                .expect("linked git claims");
        assert!(!linked_claims.git_write_roots.is_empty());
        assert!(
            linked_claims
                .git_write_roots
                .iter()
                .any(|path| path.to_string_lossy().contains("worktrees"))
        );

        let main_claims = ResourceClaims::new(&main, &main, &state, &home, NetworkScope::Denied)
            .expect("main claims")
            .discover_linked_worktree_git();
        assert!(matches!(main_claims, Err(BrokerError::Scope(_))));
    }

    /// Runs on Linux in the main CI job and on Linux/macOS/Windows in the
    /// focused platform matrix. It records the honest host capability rather
    /// than assuming a helper exists. Unavailable is a supported, fail-closed
    /// result and must carry a reason.
    #[test]
    fn platform_isolation_detection_is_explicit() {
        let detected = PlatformIsolation::detect();
        match detected {
            PlatformIsolation::Unavailable { platform, reason } => {
                assert!(!platform.is_empty());
                assert!(!reason.is_empty());
            }
            available => {
                assert!(available.is_available());
                assert_ne!(available.mechanism(), "unavailable");
            }
        }
    }
}
