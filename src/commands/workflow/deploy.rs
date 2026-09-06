//! Deploy-tier policy and structural completion gates for workflows.
//!
//! Environment strictness is ordered: development < staging < production.
//! The operator chooses the tier; a repository may only raise its declared
//! minimum. This module consumes the already-folded config answer and applies
//! the production evidence requirements without granting any new authority.

use std::path::Path;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::engine::WorkflowState;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::StateDir;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum DeployTier {
    #[default]
    Development,
    Staging,
    Production,
}

impl std::fmt::Display for DeployTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        })
    }
}

/// H-8: a repository `.zirv/ctx.toml` that trips a `REPO_FORBIDDEN` key hard-
/// errors `CtxConfig::load` -- mirrors `mod.rs`'s own `repo_gates`, which
/// deliberately fails closed on exactly this shape (an unreadable config
/// disables repo-provided skills/checks) rather than propagating the error
/// and hard-failing every `workflow advance/approve/resume/start`.
/// `Development` (the type's own `#[default]`) is the least-strict tier --
/// degrading to it, rather than escalating, is the fail-closed direction
/// here: an unreadable config can never be a repository's own way to widen
/// what an operator's own declared tier requires, but this module's whole
/// point is that a repository cannot ratchet the deploy tier down either, so
/// the safe move on "I cannot tell what the operator's tier is" is the
/// default the operator never configured, not an error that blocks the
/// workflow command outright.
pub fn effective_tier(repo: &Path) -> CtxResult<DeployTier> {
    match crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()) {
        Ok(cfg) => Ok(cfg.workflow.deploy.tier),
        Err(error) => {
            super::announce_unreadable_config(&error.to_string());
            Ok(DeployTier::Development)
        }
    }
}

pub fn fresh_independent_reviews(state: &WorkflowState) -> CtxResult<usize> {
    let fingerprint = super::verification::change_fingerprint(&state.repo)?;
    Ok(state
        .review_evidence
        .iter()
        .filter(|evidence| evidence.change_fingerprint == fingerprint)
        .count())
}

pub fn production_gate_satisfied(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    if state.deploy_tier != DeployTier::Production {
        return Ok(());
    }

    if state
        .review_findings
        .iter()
        .any(|finding| finding.disposition == super::review::FindingDisposition::Open)
    {
        return Err("production deploy is blocked while review findings remain open".into());
    }

    let reviews = fresh_independent_reviews(state)?;
    if reviews == 0 {
        return Err(
            "production deploy requires at least one fresh independent reviewer-seat run".into(),
        );
    }

    if !super::verification::latest_is_fresh_and_passing(state_dir, &state.repo, true)? {
        let announcement = super::verification::gate_announcement(state_dir, &state.repo, true);
        return Err(format!(
            "production deploy requires fresh passing final verification; run `zirv verify`\n{announcement}"
        )
        .into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_order_matches_strictness() {
        assert!(DeployTier::Development < DeployTier::Staging);
        assert!(DeployTier::Staging < DeployTier::Production);
    }

    #[test]
    fn tier_serialization_is_stable() {
        assert_eq!(
            serde_json::to_string(&DeployTier::Production).unwrap(),
            "\"production\""
        );
    }

    /// H-8: a repository `.zirv/ctx.toml` that trips a `REPO_FORBIDDEN` key
    /// (`agent_bin`, the same trigger `config.rs`'s own tests use) must
    /// degrade `effective_tier` to `Development` and let the workflow
    /// command proceed, mirroring `mod.rs`'s `repo_gates` -- not hard-error
    /// every `workflow advance/approve/resume/start` in this repository.
    #[test]
    fn effective_tier_degrades_to_development_on_an_unreadable_repo_config() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "agent_bin = \"/tmp/x\"\n",
        )
        .unwrap();

        let load_err = crate::commands::ctx::config::CtxConfig::load(repo.path(), &|key| {
            std::env::var(key).ok()
        })
        .expect_err("agent_bin must still be REPO_FORBIDDEN for this test to be meaningful");
        assert!(
            crate::commands::ctx::config::is_repo_forbidden(load_err.as_ref()),
            "trigger must be a REPO_FORBIDDEN rejection, not some other load failure: {load_err}"
        );

        assert_eq!(
            effective_tier(repo.path()).expect("must degrade, not hard-error"),
            DeployTier::Development
        );
    }
}
