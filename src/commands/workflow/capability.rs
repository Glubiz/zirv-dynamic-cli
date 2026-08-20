//! Logical capabilities requested by skills and workflows.
//!
//! A capability report describes what Zirv can arrange on a harness. It is
//! not an authorization grant. The canonical policy work in issue #43 can
//! narrow these reports through [`PolicyDecision`] without changing skill
//! manifests or teaching them provider tool names.

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityStatus {
    pub capability: CapabilityId,
    pub support: SupportLevel,
    pub reason: &'static str,
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
        let status = |capability, support, reason| CapabilityStatus {
            capability,
            support,
            reason,
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

    /// Policy may narrow support, never widen it. `Ask` remains explicitly
    /// operator-controlled; `Deny` is unsupported for this run.
    pub fn with_policy(mut self, policy: impl Fn(CapabilityId) -> PolicyDecision) -> Self {
        for status in &mut self.statuses {
            status.support = match policy(status.capability) {
                PolicyDecision::Deny => SupportLevel::Unsupported,
                PolicyDecision::Ask if status.support.satisfies_requirement() => {
                    SupportLevel::OperatorControlled
                }
                PolicyDecision::Allow => status.support,
                PolicyDecision::Ask => status.support,
            };
        }
        self
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
        let denied = CapabilityReport::for_adapter("claude")
            .with_policy(|cap| match cap {
                CapabilityId::RepoWrite => PolicyDecision::Deny,
                _ => PolicyDecision::Allow,
            });
        assert_eq!(
            denied.support(CapabilityId::RepoWrite),
            SupportLevel::Unsupported
        );

        let unknown = CapabilityReport::for_adapter("future")
            .with_policy(|_| PolicyDecision::Allow);
        assert_eq!(
            unknown.support(CapabilityId::RepoRead),
            SupportLevel::Unsupported
        );
    }
}
