//! A harness-neutral model for a single context/instruction surface: which
//! system produced it, what kind of content it carries, where it sits
//! relative to the repository checkout, and whether zirv may ever treat its
//! content as operator-authoritative.
//!
//! `optimize.rs`'s `Layer` enum (CLAUDE.md layers, `.claude/settings*.json`)
//! is the first consumer: each of its variants maps onto one `(Provider,
//! Kind, Scope)` triple, and its `is_settings()`/`is_repo_owned()` helpers are
//! now derived from that mapping rather than hand-rolled per variant. Tasks
//! 10-16 build new surfaces (AGENTS.md, harness-native settings, memory,
//! session/handoff/mail) on the same vocabulary.
//!
//! The load-bearing property is `Scope::trust`: `Trust` is never a field a
//! caller sets independently, only a value derived from `Scope`. A repo-owned
//! surface cannot be promoted to operator authority because there is no
//! constructor that lets it name `Trust::Operator` directly -- see its doc.

use std::path::{Path, PathBuf};

/// Which system produced or reads a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// zirv's own `ctx.toml`, `.settings.toml`, memory bank, mail, session
    /// state. No production surface names this yet -- `optimize.rs` only
    /// collects Claude surfaces today (issue #39); a later task wires
    /// zirv's own surfaces through the same model.
    #[allow(dead_code)]
    Zirv,
    Claude,
    /// No production surface names this yet either -- `optimize.rs`'s
    /// `Layer` only maps to `Claude` today; a follow-up task (issue #40)
    /// adds Codex's own CLAUDE.md/settings.json analogues.
    #[allow(dead_code)]
    Codex,
}

/// What kind of content a surface carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Free-form prose/bullet instructions (CLAUDE.md, AGENTS.md).
    Instructions,
    /// The cross-session durable-fact bank. Deliberately outside
    /// `optimize.rs`'s own collection today (N7: a memory key/body must
    /// never reach the judgment model) -- a later task wires this through.
    #[allow(dead_code)]
    Memory,
    /// Structured settings/policy: hooks, permissions, tool allow/deny.
    PolicySettings,
    /// Session/handoff/mail state. No production surface names this yet.
    #[allow(dead_code)]
    Session,
    /// Harness-native config not covered by the kinds above. No production
    /// surface names this yet -- Codex's `config.toml` fits `PolicySettings`
    /// instead.
    #[allow(dead_code)]
    Harness,
}

/// Where a surface sits relative to the repository checkout and the
/// operator's home directory. This is the only input to `trust()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Outside the repo checkout entirely: the operator's own home directory.
    Global,
    /// The repo root, shared with every checkout.
    Repo,
    /// A subdirectory of the repo, found by nested/bounded discovery.
    Nested,
    /// Inside the repo checkout but meant to be personal/uncommitted (e.g.
    /// `.claude/settings.local.json`). Still physically inside the checkout,
    /// so still not operator authority -- see `trust()`.
    LocalPrivate,
}

impl Scope {
    /// The only place a `Trust` value is decided. `Global` names the
    /// operator's own home directory; every other scope names a path inside
    /// the repository checkout, which anyone with repo write access can
    /// edit. zirv has no way to verify a "local" file is actually
    /// gitignored, so it gets the same untrusted treatment as a committed
    /// one -- the same asymmetry `REPO_FORBIDDEN` enforces for `ctx.toml`.
    pub fn trust(self) -> Trust {
        match self {
            Scope::Global => Trust::Operator,
            Scope::Repo | Scope::Nested | Scope::LocalPrivate => Trust::RepoUntrusted,
        }
    }

    /// Whether this scope names a path inside the repository checkout.
    pub fn is_repo_owned(self) -> bool {
        matches!(self, Scope::Repo | Scope::Nested | Scope::LocalPrivate)
    }
}

/// Authority a surface's content may carry. `RepoUntrusted` content may
/// steer or inform a session but must never enable or widen zirv's own
/// behavior -- the same asymmetry `REPO_FORBIDDEN` enforces for `ctx.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trust {
    Operator,
    RepoUntrusted,
}

/// One context/instruction surface: provenance plus enough metadata for
/// analysis to reason about it without re-deriving trust by hand.
///
/// Deliberately holds no content: `path` plus `provider`/`kind`/`scope` is
/// provenance, not the surface's text, which callers already have from their
/// own read (`optimize::Surface::text`, for instance) and cap independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSurface {
    provider: Provider,
    kind: Kind,
    scope: Scope,
    path: PathBuf,
}

impl ContextSurface {
    pub fn new(provider: Provider, kind: Kind, scope: Scope, path: PathBuf) -> Self {
        Self {
            provider,
            kind,
            scope,
            path,
        }
    }

    // No production caller yet for any of the five accessors below --
    // `optimize::Surface::context_surface()` constructs a `ContextSurface`
    // for future tasks (issue #39) but nothing reads one back yet; the
    // module's own tests exercise every accessor in the meantime.
    #[allow(dead_code)]
    pub fn provider(&self) -> Provider {
        self.provider
    }

    #[allow(dead_code)]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    #[allow(dead_code)]
    pub fn scope(&self) -> Scope {
        self.scope
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Derived from `scope` alone, never stored independently -- see
    /// `Scope::trust`. There is no way to construct a `ContextSurface` that
    /// names `Trust::Operator` for a repo-owned `scope`.
    #[allow(dead_code)]
    pub fn trust(&self) -> Trust {
        self.scope.trust()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_scope_is_operator_trusted() {
        assert_eq!(Scope::Global.trust(), Trust::Operator);
    }

    #[test]
    fn repo_nested_and_local_private_scopes_are_repo_untrusted() {
        assert_eq!(Scope::Repo.trust(), Trust::RepoUntrusted);
        assert_eq!(Scope::Nested.trust(), Trust::RepoUntrusted);
        assert_eq!(Scope::LocalPrivate.trust(), Trust::RepoUntrusted);
    }

    #[test]
    fn only_global_scope_sits_outside_the_repo_checkout() {
        assert!(!Scope::Global.is_repo_owned());
        assert!(Scope::Repo.is_repo_owned());
        assert!(Scope::Nested.is_repo_owned());
        assert!(Scope::LocalPrivate.is_repo_owned());
    }

    /// The load-bearing property: a `ContextSurface`'s trust always tracks
    /// its scope, with no independent field a caller could set to promote a
    /// repo-owned surface to operator authority.
    #[test]
    fn a_context_surface_trust_always_matches_its_scope() {
        let repo_owned = ContextSurface::new(
            Provider::Claude,
            Kind::Instructions,
            Scope::Repo,
            PathBuf::from("/repo/CLAUDE.md"),
        );
        assert_eq!(repo_owned.trust(), Trust::RepoUntrusted);

        let operator_owned = ContextSurface::new(
            Provider::Claude,
            Kind::Instructions,
            Scope::Global,
            PathBuf::from("/home/CLAUDE.md"),
        );
        assert_eq!(operator_owned.trust(), Trust::Operator);
    }

    /// Issue #39 lists the memory bank and session/handoff/mail as future
    /// context sources alongside CLAUDE.md/AGENTS.md/settings; this proves
    /// the vocabulary already models them, before any task wires a real
    /// collector through it (memory stays deliberately outside
    /// `optimize.rs`'s own collection today -- see its module doc).
    #[test]
    fn the_vocabulary_already_covers_sources_no_task_has_wired_up_yet() {
        let memory = ContextSurface::new(
            Provider::Zirv,
            Kind::Memory,
            Scope::Repo,
            PathBuf::from("/state/memory/repo-slug/0000000001-fact.md"),
        );
        assert_eq!(memory.trust(), Trust::RepoUntrusted);

        let session = ContextSurface::new(
            Provider::Zirv,
            Kind::Session,
            Scope::LocalPrivate,
            PathBuf::from("/state/sessions/abcd1234.json"),
        );
        assert_eq!(session.trust(), Trust::RepoUntrusted);

        let harness = ContextSurface::new(
            Provider::Codex,
            Kind::Harness,
            Scope::Global,
            PathBuf::from("/home/.codex/config.toml"),
        );
        assert_eq!(harness.trust(), Trust::Operator);
    }

    #[test]
    fn accessors_return_what_new_was_given() {
        let surface = ContextSurface::new(
            Provider::Codex,
            Kind::PolicySettings,
            Scope::LocalPrivate,
            PathBuf::from("/repo/.codex/config.toml"),
        );
        assert_eq!(surface.provider(), Provider::Codex);
        assert_eq!(surface.kind(), Kind::PolicySettings);
        assert_eq!(surface.scope(), Scope::LocalPrivate);
        assert_eq!(surface.path(), Path::new("/repo/.codex/config.toml"));
    }
}
