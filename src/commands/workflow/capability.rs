//! Logical capabilities requested by skills and workflows.
//!
//! A capability report describes what Zirv can arrange on a harness. It is
//! not an authorization grant. The canonical policy work in issue #43 can
//! narrow these reports through [`PolicyDecision`] without changing skill
//! manifests or teaching them provider tool names.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::policy::{Capability as PolicyCapability, EffectivePolicy, Stance};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CapabilityId {
    #[serde(rename = "shell.exec")]
    ShellExec,
    #[serde(rename = "repo.read")]
    RepoRead,
    #[serde(rename = "repo.write")]
    RepoWrite,
    #[serde(rename = "git.worktree")]
    GitWorktree,
    #[serde(rename = "agent.spawn")]
    AgentSpawn,
    #[serde(rename = "test.run")]
    TestRun,
    #[serde(rename = "artifact.render")]
    ArtifactRender,
    #[serde(rename = "browser.open")]
    BrowserOpen,
    #[serde(rename = "network.access")]
    NetworkAccess,
}

impl CapabilityId {
    pub const ALL: [Self; 9] = [
        Self::ShellExec,
        Self::RepoRead,
        Self::RepoWrite,
        Self::GitWorktree,
        Self::AgentSpawn,
        Self::TestRun,
        Self::ArtifactRender,
        Self::BrowserOpen,
        Self::NetworkAccess,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShellExec => "shell.exec",
            Self::RepoRead => "repo.read",
            Self::RepoWrite => "repo.write",
            Self::GitWorktree => "git.worktree",
            Self::AgentSpawn => "agent.spawn",
            Self::TestRun => "test.run",
            Self::ArtifactRender => "artifact.render",
            Self::BrowserOpen => "browser.open",
            Self::NetworkAccess => "network.access",
        }
    }
}

impl std::fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportLevel {
    Supported,
    Degraded,
    Unsupported,
    OperatorControlled,
}

impl SupportLevel {
    pub fn satisfies_requirement(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

impl std::fmt::Display for SupportLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Supported => "supported",
            Self::Degraded => "degraded",
            Self::Unsupported => "unsupported",
            Self::OperatorControlled => "operator-controlled",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityStatus {
    pub capability: CapabilityId,
    pub support: SupportLevel,
    pub authorization: PolicyDecision,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityReport {
    pub adapter: String,
    pub statuses: Vec<CapabilityStatus>,
}

impl CapabilityReport {
    /// Honest baseline for current adapters. Filesystem/shell/network
    /// permissions remain operator-controlled because markdown cannot enforce
    /// them. Zirv-owned operations can be reported as supported independently
    /// of a vendor's native tool vocabulary.
    pub fn for_adapter(adapter: &str) -> Self {
        let known = matches!(adapter, "claude" | "codex");
        let status = |capability, support, reason: &'static str| CapabilityStatus {
            capability,
            support,
            authorization: PolicyDecision::Allow,
            reason: reason.to_string(),
        };
        let statuses = if known {
            vec![
                status(
                    CapabilityId::ShellExec,
                    SupportLevel::OperatorControlled,
                    "the harness and operator policy control shell access",
                ),
                status(
                    CapabilityId::RepoRead,
                    SupportLevel::OperatorControlled,
                    "the harness and operator policy control repository reads",
                ),
                status(
                    CapabilityId::RepoWrite,
                    SupportLevel::OperatorControlled,
                    "the harness and operator policy control repository writes",
                ),
                status(
                    CapabilityId::GitWorktree,
                    SupportLevel::Degraded,
                    "available through shell execution; no native adapter operation",
                ),
                status(
                    CapabilityId::AgentSpawn,
                    SupportLevel::Supported,
                    "provided by Zirv supervision",
                ),
                status(
                    CapabilityId::TestRun,
                    SupportLevel::Supported,
                    "provided by Zirv's deterministic verification runner",
                ),
                status(
                    CapabilityId::ArtifactRender,
                    SupportLevel::Supported,
                    "provided by Zirv's artifact registry and static fallback",
                ),
                status(
                    CapabilityId::BrowserOpen,
                    SupportLevel::Degraded,
                    "available only when a browser-capable harness is configured",
                ),
                status(
                    CapabilityId::NetworkAccess,
                    SupportLevel::OperatorControlled,
                    "network access is controlled outside skill instructions",
                ),
            ]
        } else {
            CapabilityId::ALL
                .into_iter()
                .map(|capability| {
                    status(
                        capability,
                        SupportLevel::Unsupported,
                        "no capability mapping exists for this adapter",
                    )
                })
                .collect()
        };
        Self {
            adapter: adapter.to_string(),
            statuses,
        }
    }

    pub fn support(&self, capability: CapabilityId) -> SupportLevel {
        self.statuses
            .iter()
            .find(|status| status.capability == capability)
            .map(|status| status.support)
            .unwrap_or(SupportLevel::Unsupported)
    }

    pub fn authorization(&self, capability: CapabilityId) -> PolicyDecision {
        self.statuses
            .iter()
            .find(|status| status.capability == capability)
            .map(|status| status.authorization)
            .unwrap_or(PolicyDecision::Deny)
    }

    /// Resolve logical workflow capabilities against the effective canonical
    /// policy for `repo`. Policy loading uses the same asymmetric operator /
    /// repository fold as every AI launch, so repository content can narrow
    /// permissions but cannot grant itself a capability.
    pub fn for_repo(adapter: &str, repo: &Path) -> CtxResult<Self> {
        let config =
            crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())?;
        Ok(Self::for_policy(adapter, &config.policy))
    }

    pub fn for_policy(adapter: &str, policy: &EffectivePolicy) -> Self {
        Self::for_adapter(adapter).with_policy(|capability| policy_decision(policy, capability))
    }

    /// Policy may narrow support, never widen it. `Ask` remains explicitly
    /// operator-controlled; `Deny` is unsupported for this run.
    pub fn with_policy(mut self, policy: impl Fn(CapabilityId) -> PolicyDecision) -> Self {
        for status in &mut self.statuses {
            let decision = policy(status.capability);
            status.authorization = decision;
            status.support = match decision {
                PolicyDecision::Deny => SupportLevel::Unsupported,
                PolicyDecision::Ask if status.support.satisfies_requirement() => {
                    SupportLevel::OperatorControlled
                }
                PolicyDecision::Allow => status.support,
                PolicyDecision::Ask => status.support,
            };
            match decision {
                PolicyDecision::Deny => {
                    status.reason = "denied by Zirv's effective canonical policy".into();
                }
                PolicyDecision::Ask if status.support.satisfies_requirement() => {
                    status.reason =
                        "requires operator approval under Zirv's effective canonical policy".into();
                }
                PolicyDecision::Allow | PolicyDecision::Ask => {}
            }
        }
        self
    }
}

fn policy_decision(policy: &EffectivePolicy, capability: CapabilityId) -> PolicyDecision {
    let relevant: &[PolicyCapability] = match capability {
        CapabilityId::ShellExec | CapabilityId::TestRun | CapabilityId::BrowserOpen => {
            &[PolicyCapability::ShellExec]
        }
        CapabilityId::RepoWrite | CapabilityId::ArtifactRender => &[PolicyCapability::RepoFsWrite],
        CapabilityId::GitWorktree => &[PolicyCapability::ShellExec, PolicyCapability::RepoFsWrite],
        CapabilityId::NetworkAccess => &[PolicyCapability::Network],
        CapabilityId::RepoRead | CapabilityId::AgentSpawn => &[],
    };
    let stance = relevant
        .iter()
        .map(|capability| policy.stance(*capability))
        .max()
        .unwrap_or(Stance::Allow);
    match stance {
        Stance::Allow => PolicyDecision::Allow,
        Stance::Ask => PolicyDecision::Ask,
        Stance::Deny => PolicyDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_adapters_resolve_the_same_logical_capability_names() {
        let claude = CapabilityReport::for_adapter("claude");
        let codex = CapabilityReport::for_adapter("codex");
        assert_eq!(
            claude
                .statuses
                .iter()
                .map(|s| s.capability)
                .collect::<Vec<_>>(),
            codex
                .statuses
                .iter()
                .map(|s| s.capability)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn policy_can_narrow_but_not_promote_unsupported_adapter_support() {
        let denied = CapabilityReport::for_adapter("claude").with_policy(|cap| match cap {
            CapabilityId::RepoWrite => PolicyDecision::Deny,
            _ => PolicyDecision::Allow,
        });
        assert_eq!(
            denied.support(CapabilityId::RepoWrite),
            SupportLevel::Unsupported
        );

        let unknown =
            CapabilityReport::for_adapter("future").with_policy(|_| PolicyDecision::Allow);
        assert_eq!(
            unknown.support(CapabilityId::RepoRead),
            SupportLevel::Unsupported
        );
    }

    #[test]
    fn canonical_policy_maps_to_provider_neutral_prerequisites() {
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            repo_fs_write: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let report = CapabilityReport::for_policy("claude", &policy);
        assert_eq!(
            report.support(CapabilityId::ShellExec),
            SupportLevel::Unsupported
        );
        assert_eq!(
            report.support(CapabilityId::TestRun),
            SupportLevel::Unsupported
        );
        assert_eq!(
            report.support(CapabilityId::GitWorktree),
            SupportLevel::Unsupported
        );
        assert_eq!(
            report.support(CapabilityId::RepoWrite),
            SupportLevel::OperatorControlled
        );
        assert_eq!(
            report.support(CapabilityId::RepoRead),
            SupportLevel::OperatorControlled
        );
    }
}
