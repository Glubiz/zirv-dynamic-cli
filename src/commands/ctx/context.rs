//! The canonical `.zirv/context/` instruction layer (issue #41): one place a
//! project's AI working instructions live once for every Zirv-launched
//! harness, with optional harness-specific additions layered on top.
//! `super::optimize::collect_surfaces` reads it through `Layer::ContextCommon`/
//! `ContextClaude`/`ContextCodex`; this module owns the file locations and
//! the deterministic precedence rule between this layer and the native
//! instruction files (CLAUDE.md/AGENTS.md) it coexists with.
//!
//! **Context vs. memory -- explicit, not incidental.** This module (plus
//! CLAUDE.md/AGENTS.md) answers *how an agent should work*: conventions,
//! process, style -- authored by a person, read fresh every session, and
//! never accumulated automatically. `super::memory` answers a different
//! question, *what a past session learned*: durable facts written by
//! `remember`/handoff-harvest and recalled by key. Neither is a substitute
//! for the other -- a `.zirv/context/common.md` rule does not belong in the
//! memory bank, and a memory fact (e.g. "the staging DB migration needs a
//! manual grant") does not belong in `common.md`. `optimize.rs`'s own N7
//! boundary (a memory key/body never reaches the judgment model) protects
//! the same split from the opposite direction: this layer's whole point is
//! to be read and analysed as instructions, memory's whole point is that it
//! is not.
//!
//! **Trust.** Like every repo-owned surface, `.zirv/context/*.md` is
//! untrusted content: `Layer::ContextCommon`/`ContextClaude`/`ContextCodex`
//! all map to `Scope::Repo`, so `Scope::trust` gives them `RepoUntrusted`,
//! the same as CLAUDE.md/AGENTS.md. It can steer a session's prose
//! instructions; it can never change what zirv itself runs or widen a
//! security setting -- see the test at the bottom of this module and
//! `REPO_FORBIDDEN` in `config.rs` for the same asymmetry enforced on
//! `ctx.toml`.

use std::path::{Path, PathBuf};

use super::optimize::Layer;

/// The subdirectory holding zirv's own canonical instruction layer.
pub const CONTEXT_DIR: &str = ".zirv/context";

/// `<repo>/.zirv/context/common.md` -- instructions common to every
/// Zirv-launched harness. Optional: a repo with none of this module's three
/// files analyses to nothing extra, exactly like a repo with no CLAUDE.md.
pub fn common_path(repo: &Path) -> PathBuf {
    repo.join(CONTEXT_DIR).join("common.md")
}

/// `<repo>/.zirv/context/claude.md` -- optional Claude-specific additions.
pub fn claude_path(repo: &Path) -> PathBuf {
    repo.join(CONTEXT_DIR).join("claude.md")
}

/// `<repo>/.zirv/context/codex.md` -- optional Codex-specific additions.
pub fn codex_path(repo: &Path) -> PathBuf {
    repo.join(CONTEXT_DIR).join("codex.md")
}

/// Where one `Instructions`-kind layer sits in the deterministic precedence
/// order a future compiler (issue #44) composes layers in: canonical common
/// content applies first, a harness-specific canonical addition layers on
/// top of it, and a harness's own native instruction file (CLAUDE.md /
/// AGENTS.md, at any scope) composes last -- closest to the session, so a
/// conflicting instruction there is read as refining or overriding the
/// canonical layer rather than the other way around. `PartialOrd`/`Ord` are
/// derived from declaration order, the same technique `Severity` (above)
/// uses for its own three-value ranking. Consumed by `drift.rs`'s
/// precedence/shadowing findings (issue #42); Task 14's compiler is the
/// second real consumer, for actual layer ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrecedenceTier {
    CanonicalCommon,
    CanonicalHarnessSpecific,
    Native,
}

impl PrecedenceTier {
    pub fn label(&self) -> &'static str {
        match self {
            PrecedenceTier::CanonicalCommon => "canonical common",
            PrecedenceTier::CanonicalHarnessSpecific => "canonical harness-specific",
            PrecedenceTier::Native => "native",
        }
    }
}

/// `None` for anything that is not an `Instructions`-kind layer (settings/
/// policy surfaces are a different dimension entirely -- see `Layer::kind`
/// and `Layer::is_settings`). Every `Instructions` layer has an opinion here;
/// `every_instructions_layer_has_a_defined_tier` (below) keeps a future
/// variant from being added to one enum without the other.
pub fn precedence_tier(layer: Layer) -> Option<PrecedenceTier> {
    match layer {
        Layer::ContextCommon => Some(PrecedenceTier::CanonicalCommon),
        Layer::ContextClaude | Layer::ContextCodex => {
            Some(PrecedenceTier::CanonicalHarnessSpecific)
        }
        Layer::GlobalClaudeMd
        | Layer::RepoClaudeMd
        | Layer::NestedClaudeMd
        | Layer::GlobalAgentsMd
        | Layer::RepoAgentsMd
        | Layer::NestedAgentsMd => Some(PrecedenceTier::Native),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::surface::Trust;

    #[test]
    fn precedence_ranks_canonical_common_below_harness_specific_below_native() {
        assert!(PrecedenceTier::CanonicalCommon < PrecedenceTier::CanonicalHarnessSpecific);
        assert!(PrecedenceTier::CanonicalHarnessSpecific < PrecedenceTier::Native);
    }

    #[test]
    fn every_instructions_layer_has_a_defined_tier() {
        for layer in [
            Layer::ContextCommon,
            Layer::ContextClaude,
            Layer::ContextCodex,
            Layer::GlobalClaudeMd,
            Layer::RepoClaudeMd,
            Layer::NestedClaudeMd,
            Layer::GlobalAgentsMd,
            Layer::RepoAgentsMd,
            Layer::NestedAgentsMd,
        ] {
            assert!(precedence_tier(layer).is_some(), "{layer:?} has no tier");
        }
    }

    #[test]
    fn settings_layers_have_no_precedence_tier() {
        for layer in [
            Layer::UserSettings,
            Layer::ProjectSettings,
            Layer::LocalSettings,
            Layer::CodexUserSettings,
            Layer::CodexProjectSettings,
        ] {
            assert_eq!(precedence_tier(layer), None, "{layer:?}");
        }
    }

    #[test]
    fn path_helpers_join_the_repo_root() {
        let repo = Path::new("/repo");
        assert_eq!(common_path(repo), repo.join(".zirv/context/common.md"));
        assert_eq!(claude_path(repo), repo.join(".zirv/context/claude.md"));
        assert_eq!(codex_path(repo), repo.join(".zirv/context/codex.md"));
    }

    /// Issue #41's binding requirement: repo-owned canonical context can
    /// never grant operator authority, no matter what its content says.
    /// Proven the same way Task 9/10 proved it for CLAUDE.md/AGENTS.md --
    /// every new `Layer` variant's `scope()` is `Repo`, and `Scope::trust`
    /// has no path from `Repo` to `Trust::Operator`.
    #[test]
    fn repo_owned_canonical_context_can_never_grant_operator_permissions() {
        for layer in [
            Layer::ContextCommon,
            Layer::ContextClaude,
            Layer::ContextCodex,
        ] {
            assert_eq!(
                layer.trust(),
                Trust::RepoUntrusted,
                "{layer:?} must never be operator-trusted, no matter its content"
            );
            assert!(layer.is_repo_owned());
        }
    }
}
