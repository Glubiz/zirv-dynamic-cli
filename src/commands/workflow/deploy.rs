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

pub fn effective_tier(repo: &Path) -> CtxResult<DeployTier> {
    let cfg = crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())?;
    Ok(cfg.workflow.deploy.tier)
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
        return Err(
            "production deploy requires fresh passing final verification; run `zirv verify`".into(),
        );
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
}
