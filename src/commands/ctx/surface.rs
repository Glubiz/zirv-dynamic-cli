//! A harness-neutral model for a single context/instruction surface: which
//! system produced it, what kind of content it carries, where it sits
//! relative to the repository checkout, and whether zirv may ever treat its
//! content as operator-authoritative.
//!
//! `optimize.rs`'s `Layer` enum is the first consumer: each of its variants
//! maps onto one `(Provider, Kind, Scope)` triple, and its `is_settings()`/
//! `is_repo_owned()` helpers are derived from that mapping rather than
//! hand-rolled per variant. As of issue #40, `Layer` maps both Claude's
//! CLAUDE.md/`settings.json` and Codex's own AGENTS.md/`config.toml` through
//! it; Tasks 11-16 build the remaining surfaces (memory, session/handoff/
//! mail) on the same vocabulary.
//!
//! The load-bearing property is `Scope::trust`: `Trust` is never a field a
//! caller sets independently, only a value derived from `Scope`. That alone
//! is not enough, though, if `Scope` itself can be set independently of the
//! `path` it is supposed to describe -- `ContextSurface`'s only public
//! constructor, `for_path`, closes that gap by deriving `Global` vs.
//! repo-owned from the path itself, so a repo-owned surface cannot be
//! promoted to operator authority even by a caller trying to do so on
//! purpose. See `ContextSurface`'s own doc for why an earlier, more
//! permissive constructor shape was not enough.

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
    /// Codex's own AGENTS.md and `config.toml` analogues (issue #40) --
    /// `optimize.rs`'s `Layer::GlobalAgentsMd`/`RepoAgentsMd`/
    /// `NestedAgentsMd`/`CodexUserSettings`/`CodexProjectSettings` all map
    /// here. Claude contributes a `LocalPrivate`-scoped surface
    /// (`settings.local.json`); Codex does not have an equivalent yet --
    /// see `Layer::scope`'s own doc and the Handoff API note in issue #40's
    /// report before comparing the two providers' surface counts directly.
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

/// Authority a surface's content may carry. Answers one question only --
/// "may this content grant authority (change what zirv itself runs, or
/// widen a security setting)?" -- never "who wrote this?" or "is this
/// prose trustworthy to read?" (untrusted content is still read and shown;
/// it just cannot act). `RepoUntrusted` content may steer or inform a
/// session but must never enable or widen zirv's own behavior -- the same
/// asymmetry `REPO_FORBIDDEN` enforces for `ctx.toml`.
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
///
/// Constructed only through `for_path` (which derives `Global` vs. repo-owned
/// from the path itself, not from a caller-supplied flag) and the two
/// refinement methods below, which can only move `scope` among the
/// repo-owned variants. There is deliberately no general constructor that
/// takes a free-standing `Scope` alongside an unrelated `path`: a prior
/// version of this type did, and `ContextSurface::new(Provider::Claude,
/// Kind::Instructions, Scope::Global, repo.join("CLAUDE.md"))` would have
/// minted `Trust::Operator` for a repo-controlled path despite this module's
/// own claim that such a promotion is impossible by construction. Closing
/// that required removing the free parameter, not just documenting against
/// using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSurface {
    provider: Provider,
    kind: Kind,
    scope: Scope,
    path: PathBuf,
}

impl ContextSurface {
    /// The classifier every real collector should use. `scope` starts as
    /// `Global` or `Repo` purely from whether `path` sits inside `repo` --
    /// the one distinction that is actually trust-relevant (`Scope::trust`
    /// only distinguishes `Global` from everything else). A caller with more
    /// specific provenance than "this path is inside the repo checkout"
    /// (found via nested/bounded discovery, or known to be a
    /// personal/uncommitted override) refines the result with
    /// `into_nested()`/`into_local_private()` below, which can only move `scope`
    /// among the repo-owned variants -- never back to `Global`, and never set
    /// independently of this classification. This is what makes a
    /// repo-owned path minting `Trust::Operator` unreachable through the
    /// public API, not merely discouraged by convention.
    pub fn for_path(
        provider: Provider,
        kind: Kind,
        path: PathBuf,
        repo: &Path,
        home: Option<&Path>,
    ) -> Self {
        let scope = if path.starts_with(repo) {
            Scope::Repo
        } else {
            Scope::Global
        };
        // The load-bearing check: a path inside the repo checkout must never
        // classify as `Global`. Tautological given the `if`/`else` above --
        // a regression guard against a future refactor of this function
        // breaking that invariant, not a runtime discovery.
        debug_assert!(
            !(scope == Scope::Global && path.starts_with(repo)),
            "a path under the repo checkout classified as Global: {path:?} is under {repo:?}"
        );
        // A secondary, non-load-bearing sanity check: when the caller does
        // know the operator's home directory, a genuinely global surface
        // should actually live under it.
        if let Some(home) = home {
            debug_assert!(
                scope != Scope::Global || path.starts_with(home),
                "a Global-scoped path is not under the given home directory: {path:?}"
            );
        }
        Self::new(provider, kind, scope, path)
    }

    /// Refines an already repo-owned `for_path` result to `Nested` (found via
    /// bounded directory-walk discovery, not a fixed repo-relative path).
    /// Debug-asserts the surface was already repo-owned: this can only move
    /// `scope` among `Repo`/`Nested`/`LocalPrivate`, never touch `Global`.
    pub fn into_nested(mut self) -> Self {
        debug_assert!(
            self.scope.is_repo_owned(),
            "into_nested called on a non-repo-owned surface: {:?}",
            self.scope
        );
        self.scope = Scope::Nested;
        self
    }

    /// Refines an already repo-owned `for_path` result to `LocalPrivate`
    /// (e.g. `.claude/settings.local.json`): personal/uncommitted by
    /// convention, but zirv cannot verify that, so trust is unaffected -- see
    /// `Scope::trust`. Downstream reporting must phrase a `LocalPrivate`
    /// surface by what `Scope` says it is ("an uncommitted local file, not
    /// operator-authoritative"), never describe it as ordinary repository
    /// content -- it is still physically inside the checkout and still
    /// `RepoUntrusted`, but it is not the same thing as a committed,
    /// team-shared file either. Same non-`Global` guarantee as `into_nested`.
    pub fn into_local_private(mut self) -> Self {
        debug_assert!(
            self.scope.is_repo_owned(),
            "into_local_private called on a non-repo-owned surface: {:?}",
            self.scope
        );
        self.scope = Scope::LocalPrivate;
        self
    }

    /// Not `pub`: the only way to reach this from outside the module is
    /// through `for_path` (which derives `scope` from `path`) and the two
    /// refinement methods above (which only narrow an already repo-owned
    /// `scope`). Kept private rather than removed so this module's own tests
    /// can still construct a bare `ContextSurface` to test `trust`/accessor
    /// behavior directly, independent of path classification.
    fn new(provider: Provider, kind: Kind, scope: Scope, path: PathBuf) -> Self {
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

    /// The load-bearing property this task's review found missing: `for_path`
    /// takes no `Scope` parameter at all, so there is nothing for a caller to
    /// set wrong -- a path physically inside the repo checkout can never
    /// classify as `Global`/`Trust::Operator`, no matter what provider/kind
    /// is attached to it.
    #[test]
    fn for_path_can_never_promote_a_repo_owned_path_to_operator_trust() {
        let repo = Path::new("/repo");
        let surface = ContextSurface::for_path(
            Provider::Claude,
            Kind::Instructions,
            repo.join("CLAUDE.md"),
            repo,
            None,
        );
        assert_ne!(surface.scope(), Scope::Global);
        assert_eq!(surface.trust(), Trust::RepoUntrusted);

        // Even a nested path, or one belonging to a provider/kind that has
        // nothing to do with CLAUDE.md, still classifies purely from its
        // relationship to `repo`.
        let nested = ContextSurface::for_path(
            Provider::Codex,
            Kind::PolicySettings,
            repo.join("crates/inner/config.toml"),
            repo,
            None,
        );
        assert_ne!(nested.scope(), Scope::Global);
        assert_eq!(nested.trust(), Trust::RepoUntrusted);
    }

    #[test]
    fn for_path_classifies_a_path_outside_the_repo_as_global() {
        let repo = Path::new("/repo");
        let home = Path::new("/home/operator");
        let surface = ContextSurface::for_path(
            Provider::Claude,
            Kind::Instructions,
            home.join("CLAUDE.md"),
            repo,
            Some(home),
        );
        assert_eq!(surface.scope(), Scope::Global);
        assert_eq!(surface.trust(), Trust::Operator);
    }

    /// `into_nested`/`into_local_private` only ever narrow an already repo-owned
    /// surface -- calling either on a `Global` one is a caller bug, and must
    /// panic in a debug build rather than silently mint a mislabeled
    /// surface. Neither call could produce `Trust::Operator` even if it
    /// didn't panic (both target scopes are `RepoUntrusted`), so this test
    /// is about correctness, not the trust boundary itself.
    #[test]
    fn into_nested_and_into_local_private_refuse_a_global_surface() {
        let repo = Path::new("/repo");
        let home = Path::new("/home/operator");

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let global = || {
            ContextSurface::for_path(
                Provider::Claude,
                Kind::Instructions,
                home.join("CLAUDE.md"),
                repo,
                Some(home),
            )
        };
        let nested_result = std::panic::catch_unwind(|| global().into_nested());
        let local_private_result = std::panic::catch_unwind(|| global().into_local_private());

        std::panic::set_hook(previous_hook);

        assert!(
            nested_result.is_err(),
            "into_nested must refuse a Global-scoped surface"
        );
        assert!(
            local_private_result.is_err(),
            "into_local_private must refuse a Global-scoped surface"
        );
    }
}
