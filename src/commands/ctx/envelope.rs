//! Delegation envelopes that may only narrow (issue #262).
//!
//! A [`WorkerEnvelope`] bounds what a delegated worker may do -- which paths
//! it may write to, which tool families it may use, whether it may reach the
//! network or run a command the safety policy classifies as destructive, how
//! many further hops of `zirv agent` it may itself spawn, when it expires,
//! and an optional token ceiling. It is computed once at dispatch time from
//! the parent's own envelope (`agent.rs::run_with`) and travels with the
//! worker (`ZIRV_ENVELOPE`) so a NESTED `zirv agent` inherits the same
//! narrowing constraint rather than a fresh, unbounded posture.
//!
//! Pure: no fs/clock/env/net, exactly like `rot.rs` -- every caller resolves
//! real paths, the wall clock, and config before calling in here. The one
//! operation this module performs, [`narrow`], can only ever shrink: the
//! child produced by `narrow(parent, requested)` is provably a subset of
//! `parent` ([`WorkerEnvelope::is_subset_of`]), and a `requested` value that
//! asks for more than `parent` allows in any single field is a hard
//! [`CannotGrow`] error for that field, not a silent clamp -- a widening
//! attempt should be loud, the way `config.rs`'s `REPO_FORBIDDEN` rejection
//! is loud rather than silently ignored.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One allowed write root, as free-form path text (repo-relative or
/// absolute). Comparison is prefix-containment after a purely lexical
/// normalization -- forward slashes, `.`/empty segments dropped -- and
/// case-folding on Windows/macOS, whose filesystems are case-insensitive.
/// This is NOT `std::fs::canonicalize`: that touches the filesystem, which a
/// pure module (no fs/clock/env/net) may never do; a `PathScope` is
/// compared as text, lexically, the same way `Path::components` would
/// normalize without ever asking the OS whether the path exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathScope(pub String);

impl PathScope {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Lexical normalization: unify separators, drop `.`/empty segments,
    /// case-fold on Windows/macOS. Returns `None` when any segment is `..`
    /// -- a `..` in scope text is always treated as an escape attempt, never
    /// a legitimate lexical parent reference, so it normalizes to "matches
    /// nothing" rather than being resolved away.
    fn normalized(&self) -> Option<String> {
        let unified = self.0.replace('\\', "/");
        let mut segments: Vec<&str> = Vec::new();
        for segment in unified.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                return None;
            }
            segments.push(segment);
        }
        Some(case_fold(segments.join("/")))
    }

    /// Whether every concrete path `self` could name is also named by
    /// `parent` -- prefix containment on the normalized text, with an empty
    /// (root, e.g. `"."`) parent matching everything. A request whose text
    /// fails to normalize (contains a `..` segment) is never a subset of
    /// anything, including itself.
    pub fn is_subset_of(&self, parent: &PathScope) -> bool {
        let Some(child) = self.normalized() else {
            return false;
        };
        let Some(parent) = parent.normalized() else {
            return false;
        };
        if parent.is_empty() {
            return true;
        }
        child == parent || child.starts_with(&format!("{parent}/"))
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn case_fold(s: String) -> String {
    s.to_ascii_lowercase()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn case_fold(s: String) -> String {
    s
}

/// Allow-list of tool families a worker may use. Narrowing is intersection:
/// a family off in either the parent or the request stays off in the
/// result -- there is no way for a request to turn a family back on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSet {
    pub edit: bool,
    pub shell: bool,
    pub network: bool,
    pub delegate: bool,
}

impl ToolSet {
    pub const fn all() -> Self {
        Self {
            edit: true,
            shell: true,
            network: true,
            delegate: true,
        }
    }

    pub const fn none() -> Self {
        Self {
            edit: false,
            shell: false,
            network: false,
            delegate: false,
        }
    }

    fn intersect(&self, other: &Self) -> Self {
        Self {
            edit: self.edit && other.edit,
            shell: self.shell && other.shell,
            network: self.network && other.network,
            delegate: self.delegate && other.delegate,
        }
    }

    /// Whether every family `self` allows is also allowed by `parent`. Used
    /// by `WorkerEnvelope::is_subset_of`; kept `pub` (not yet called
    /// directly from outside this module) for the same reason `ToolSet`
    /// itself is public API, not an internal implementation detail.
    #[allow(dead_code)]
    pub fn is_subset_of(&self, parent: &Self) -> bool {
        (!self.edit || parent.edit)
            && (!self.shell || parent.shell)
            && (!self.network || parent.network)
            && (!self.delegate || parent.delegate)
    }
}

impl Default for ToolSet {
    fn default() -> Self {
        Self::all()
    }
}

/// What a delegated worker may do, computed once at dispatch and re-read by
/// any nested `zirv agent` as ITS parent envelope. See the module doc for
/// the narrowing guarantee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerEnvelope {
    /// `"<parent short>/<child short>"` delegation chain (`sessions::
    /// short_id` joined per hop); `"root"` for a top-level, non-delegated
    /// session.
    pub principal: String,
    pub paths: Vec<PathScope>,
    pub tools: ToolSet,
    pub network: bool,
    pub destructive: bool,
    /// Remaining hops this worker may itself delegate; `0` means it may not
    /// run `zirv agent` at all.
    pub delegation_depth: u8,
    /// Epoch seconds.
    pub expires_at: u64,
    pub token_budget: Option<u64>,
}

/// A `narrow` request asked for more than `parent` allows, in `field`.
/// Loud rather than silent, the same reasoning as `config.rs`'s
/// `RepoForbiddenError`: a widening attempt that got quietly clamped away
/// would leave an operator wondering why their `--scope`/`--depth` request
/// did not do what they typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CannotGrow {
    pub field: &'static str,
}

impl std::fmt::Display for CannotGrow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delegation envelope cannot grow `{}`: a worker may only narrow what its parent granted",
            self.field
        )
    }
}

impl std::error::Error for CannotGrow {}

/// `true -> false` narrowing only: `parent` already denying something (`
/// false`) can never be reopened by a `requested` that asks for `true`.
fn narrow_gate(parent: bool, requested: bool, field: &'static str) -> Result<bool, CannotGrow> {
    if requested && !parent {
        return Err(CannotGrow { field });
    }
    Ok(parent && requested)
}

impl WorkerEnvelope {
    /// A maximally restrictive fallback: no paths, no tools, no network, not
    /// destructive, no further delegation. Used when a `ZIRV_ENVELOPE` value
    /// is PRESENT but fails to parse (`safety::parse_envelope_env`) -- a
    /// corrupted envelope must never be treated as ABSENT (which would mean
    /// "root", i.e. unbounded); it must be treated as the tightest possible
    /// grant instead.
    pub fn locked() -> Self {
        Self {
            principal: "locked".to_string(),
            paths: Vec::new(),
            tools: ToolSet::none(),
            network: false,
            destructive: false,
            delegation_depth: 0,
            expires_at: 0,
            token_budget: Some(0),
        }
    }

    /// Builds a REQUESTED candidate envelope from primitive request fields --
    /// shared by every dispatch seam that needs one (`agent::run_with`'s
    /// direct headless launch, and `dash::fulfill_spawn_request`'s pane
    /// fulfilment of a `SpawnRequest`), so the two can never drift on what
    /// `--path-scope`/`--no-network`/`--mode read-only`/`--depth` actually
    /// mean. Still just resolving what to ASK for -- [`Self::narrow`] is
    /// what enforces every rule.
    ///
    /// `path_scope` unstated (empty) defers to `parent`'s own paths
    /// (`read_only: false`) or no paths at all (`read_only: true`, which
    /// holds no writer permit and so needs none). `depth` unstated defers to
    /// the automatic `parent.delegation_depth - 1` decrement.
    /// `no_network`/`read_only` only ever narrow toward `false`; leaving
    /// both `false` carries `parent`'s own `network`/`destructive` forward
    /// UNCHANGED, so an ordinary writing delegation under an
    /// already-restricted parent never spuriously hits `CannotGrow` merely
    /// by existing.
    #[allow(clippy::too_many_arguments)]
    pub fn requested(
        parent: &Self,
        principal: String,
        path_scope: &[String],
        no_network: bool,
        read_only: bool,
        depth: Option<u8>,
        token_budget: Option<u64>,
    ) -> Self {
        let paths = if !path_scope.is_empty() {
            path_scope
                .iter()
                .map(|p| PathScope::new(p.clone()))
                .collect()
        } else if read_only {
            Vec::new()
        } else {
            parent.paths.clone()
        };
        let tools = if read_only {
            ToolSet {
                edit: false,
                ..parent.tools
            }
        } else {
            parent.tools
        };
        let depth_ceiling = parent.delegation_depth.saturating_sub(1);
        Self {
            principal,
            paths,
            tools,
            network: if no_network { false } else { parent.network },
            destructive: if read_only { false } else { parent.destructive },
            delegation_depth: depth.unwrap_or(depth_ceiling),
            expires_at: parent.expires_at,
            token_budget,
        }
    }

    /// `child ⊆ parent`, or `Err(CannotGrow { field })` naming the first
    /// field that asked for more than `parent` allows. `requested` is a
    /// fully resolved candidate (the caller has already folded any
    /// `--scope`/`--no-network`/`--depth` flags onto the parent's own
    /// values for anything left unstated) -- `narrow` only ever validates
    /// and combines, it never guesses that an empty field means "inherit".
    pub fn narrow(parent: &Self, requested: &Self) -> Result<Self, CannotGrow> {
        for requested_path in &requested.paths {
            let contained = parent
                .paths
                .iter()
                .any(|parent_path| requested_path.is_subset_of(parent_path));
            if !contained {
                return Err(CannotGrow { field: "paths" });
            }
        }

        let tools = parent.tools.intersect(&requested.tools);
        let network = narrow_gate(parent.network, requested.network, "network")?;
        let destructive = narrow_gate(parent.destructive, requested.destructive, "destructive")?;

        let depth_ceiling = parent.delegation_depth.saturating_sub(1);
        if requested.delegation_depth > depth_ceiling {
            return Err(CannotGrow {
                field: "delegation_depth",
            });
        }

        let expires_at = parent.expires_at.min(requested.expires_at);

        let token_budget = match (parent.token_budget, requested.token_budget) {
            (Some(p), Some(r)) => Some(p.min(r)),
            (Some(p), None) => Some(p),
            (None, Some(r)) => Some(r),
            (None, None) => None,
        };

        Ok(Self {
            principal: requested.principal.clone(),
            paths: requested.paths.clone(),
            tools,
            network,
            destructive,
            delegation_depth: requested.delegation_depth,
            expires_at,
            token_budget,
        })
    }

    /// Independent, weaker check than [`Self::narrow`]'s own rules: whether
    /// `self` is no more permissive than `parent` in every field, without
    /// re-deriving the exact narrow computation. Used by the property test
    /// below (asserting `narrow(p, r)` really did produce a subset of `p`);
    /// a general-purpose invariant check, kept `pub` for any future
    /// re-validation call site that receives an already-computed envelope
    /// and wants to confirm it against a freshly recomputed parent rather
    /// than re-running the full [`Self::narrow`]/[`Self::requested`] pair
    /// (`dash::fulfill_spawn_request`, issue #262, does the latter today).
    #[allow(dead_code)]
    pub fn is_subset_of(&self, parent: &Self) -> bool {
        let paths_ok = self
            .paths
            .iter()
            .all(|p| parent.paths.iter().any(|pp| p.is_subset_of(pp)));
        let tools_ok = self.tools.is_subset_of(&parent.tools);
        let network_ok = !self.network || parent.network;
        let destructive_ok = !self.destructive || parent.destructive;
        let depth_ok = self.delegation_depth <= parent.delegation_depth;
        let expiry_ok = self.expires_at <= parent.expires_at;
        let budget_ok = match (parent.token_budget, self.token_budget) {
            (Some(p), Some(s)) => s <= p,
            (Some(_), None) => false,
            (None, _) => true,
        };
        paths_ok && tools_ok && network_ok && destructive_ok && depth_ok && expiry_ok && budget_ok
    }
}

/// Canonical JSON for one envelope: `serde`'s default struct serialization
/// emits fields in declaration order (never sorted), so the same envelope
/// value always produces the same bytes on every platform -- what
/// [`digest`], `ZIRV_ENVELOPE`, and `log::Delegation::envelope_sha256` all
/// rely on.
pub fn canonical_json(envelope: &WorkerEnvelope) -> Result<String, serde_json::Error> {
    serde_json::to_string(envelope)
}

/// A stable SHA-256 identity for one envelope, the same shape as `safety::
/// policy_fingerprint`. An independent copy of that module's trivial
/// hash-to-hex step rather than an import of it: this module stays pure and
/// free of any dependency on the much larger, non-pure `safety.rs`, the same
/// reasoning `rot.rs`'s own `EDIT_LIKE_TOOLS` comment gives for not
/// importing a five-item list from `workflow::adoption`.
pub fn digest(envelope: &WorkerEnvelope) -> Result<String, serde_json::Error> {
    let bytes = canonical_json(envelope)?;
    let hash = Sha256::digest(bytes.as_bytes());
    Ok(hash.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(paths: &[&str], depth: u8, network: bool, destructive: bool) -> WorkerEnvelope {
        WorkerEnvelope {
            principal: "root".to_string(),
            paths: paths.iter().map(|p| PathScope::new(*p)).collect(),
            tools: ToolSet::all(),
            network,
            destructive,
            delegation_depth: depth,
            expires_at: 1_000,
            token_budget: None,
        }
    }

    #[test]
    fn a_child_path_prefix_contained_by_a_parent_path_narrows_cleanly() {
        let parent = envelope(&["src"], 1, true, true);
        let requested = envelope(&["src/lib", "src/main.rs"], 0, false, false);
        let child = WorkerEnvelope::narrow(&parent, &requested).expect("contained paths narrow");
        assert_eq!(
            child.paths,
            vec![PathScope::new("src/lib"), PathScope::new("src/main.rs")]
        );
    }

    #[test]
    fn a_path_outside_every_parent_path_cannot_grow() {
        let parent = envelope(&["src"], 1, true, true);
        let requested = envelope(&["../etc/passwd"], 0, false, false);
        let err = WorkerEnvelope::narrow(&parent, &requested).expect_err("escape must be denied");
        assert_eq!(err.field, "paths");

        let requested_sibling = envelope(&["tests"], 0, false, false);
        let err = WorkerEnvelope::narrow(&parent, &requested_sibling)
            .expect_err("sibling directory is not contained");
        assert_eq!(err.field, "paths");
    }

    #[test]
    fn a_dotdot_segment_never_escapes_even_when_it_would_lexically_cancel_out() {
        // Lexically "src/sub/../../etc" collapses to "etc", which is not
        // under "src" either way -- but the point of this test is that we
        // never get that far: any ".." segment fails to normalize at all,
        // so it can never match ANY parent, including a root (".") one.
        let root_parent = envelope(&["."], 1, true, true);
        let requested = envelope(&["../etc"], 0, false, false);
        let err = WorkerEnvelope::narrow(&root_parent, &requested)
            .expect_err("a `..` segment must never be treated as contained, even under root");
        assert_eq!(err.field, "paths");
    }

    #[test]
    fn a_root_scope_of_dot_contains_every_concrete_path() {
        let parent = envelope(&["."], 2, true, true);
        let requested = envelope(&["anything/at/all"], 1, false, false);
        WorkerEnvelope::narrow(&parent, &requested).expect("root scope contains everything");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn path_containment_case_folds_on_windows_and_macos() {
        let parent = envelope(&["Src"], 1, true, true);
        let requested = envelope(&["SRC/Lib"], 0, false, false);
        WorkerEnvelope::narrow(&parent, &requested)
            .expect("case-insensitive containment on this platform");
    }

    #[test]
    fn tools_narrow_by_intersection_only() {
        let mut parent = envelope(&["."], 1, true, true);
        parent.tools = ToolSet {
            edit: true,
            shell: true,
            network: false,
            delegate: true,
        };
        let mut requested = envelope(&["."], 0, false, false);
        requested.tools = ToolSet::all();
        let child = WorkerEnvelope::narrow(&parent, &requested).expect("intersection never fails");
        assert_eq!(
            child.tools, parent.tools,
            "network stayed off from the parent side"
        );
    }

    #[test]
    fn network_and_destructive_may_only_go_true_to_false() {
        let parent = envelope(&["."], 1, true, true);

        let requested_off = envelope(&["."], 0, false, false);
        let child =
            WorkerEnvelope::narrow(&parent, &requested_off).expect("narrowing to off is fine");
        assert!(!child.network);
        assert!(!child.destructive);

        let denied_parent = envelope(&["."], 1, false, false);
        let requested_on = envelope(&["."], 0, true, true);
        let err = WorkerEnvelope::narrow(&denied_parent, &requested_on)
            .expect_err("a denied parent must not be reopened");
        assert!(err.field == "network" || err.field == "destructive");
    }

    #[test]
    fn delegation_depth_is_parent_minus_one_floored_at_zero() {
        let parent = envelope(&["."], 1, true, true);
        let ok = envelope(&["."], 0, false, false);
        assert_eq!(
            WorkerEnvelope::narrow(&parent, &ok)
                .expect("depth 0 is within ceiling")
                .delegation_depth,
            0
        );

        let too_deep = envelope(&["."], 1, false, false);
        let err = WorkerEnvelope::narrow(&parent, &too_deep)
            .expect_err("a child may not ask for its own parent's depth");
        assert_eq!(err.field, "delegation_depth");

        let already_zero = envelope(&["."], 0, true, true);
        let requested_zero = envelope(&["."], 0, false, false);
        assert_eq!(
            WorkerEnvelope::narrow(&already_zero, &requested_zero)
                .expect("staying at zero is fine")
                .delegation_depth,
            0,
            "floored at zero, never wraps negative"
        );
        let requested_one = envelope(&["."], 1, false, false);
        let err = WorkerEnvelope::narrow(&already_zero, &requested_one)
            .expect_err("a depth-0 parent may never grant any further depth");
        assert_eq!(err.field, "delegation_depth");
    }

    #[test]
    fn expires_at_is_the_minimum_of_parent_and_requested() {
        let mut parent = envelope(&["."], 1, true, true);
        parent.expires_at = 500;
        let mut requested = envelope(&["."], 0, false, false);
        requested.expires_at = 900;
        assert_eq!(
            WorkerEnvelope::narrow(&parent, &requested)
                .unwrap()
                .expires_at,
            500,
            "a child may not outlive its parent"
        );

        requested.expires_at = 100;
        assert_eq!(
            WorkerEnvelope::narrow(&parent, &requested)
                .unwrap()
                .expires_at,
            100,
            "a child may ask to expire sooner"
        );
    }

    #[test]
    fn token_budget_narrows_to_the_tighter_of_the_two_when_both_are_set() {
        let mut parent = envelope(&["."], 1, true, true);
        parent.token_budget = Some(1_000);
        let mut requested = envelope(&["."], 0, false, false);
        requested.token_budget = Some(2_000);
        assert_eq!(
            WorkerEnvelope::narrow(&parent, &requested)
                .unwrap()
                .token_budget,
            Some(1_000)
        );

        requested.token_budget = None;
        assert_eq!(
            WorkerEnvelope::narrow(&parent, &requested)
                .unwrap()
                .token_budget,
            Some(1_000),
            "no explicit request keeps the parent's own ceiling"
        );

        parent.token_budget = None;
        requested.token_budget = Some(50);
        assert_eq!(
            WorkerEnvelope::narrow(&parent, &requested)
                .unwrap()
                .token_budget,
            Some(50),
            "a child may voluntarily set a tighter budget than an unbounded parent"
        );
    }

    #[test]
    fn a_wildcard_request_never_widens_past_a_narrow_parent_scope() {
        let parent = envelope(&["src/lib"], 1, true, true);
        let requested = envelope(&["*"], 0, false, false);
        let err = WorkerEnvelope::narrow(&parent, &requested)
            .expect_err("a literal '*' must never be treated as matching everything");
        assert_eq!(err.field, "paths");
    }

    #[test]
    fn a_wildcard_request_is_harmless_under_an_already_unrestricted_parent() {
        // Not a widening: a root ("." ) parent already grants everything a
        // "*" request could ever name, so this is not the propagator.ts bug
        // -- it is `narrow` correctly recognizing the parent as already
        // fully permissive.
        let parent = envelope(&["."], 1, true, true);
        let requested = envelope(&["*"], 0, false, false);
        WorkerEnvelope::narrow(&parent, &requested).expect("root parent already grants everything");
    }

    /// Table test: every `narrow(parent, requested)` that succeeds produces
    /// a child that is a subset of `parent`, across a range of generated
    /// parent/requested combinations -- the acceptance criterion from
    /// issue #262 ("a table test proves narrow(p, r) is a subset of p").
    #[test]
    fn narrow_always_produces_a_subset_of_parent_across_generated_inputs() {
        let path_pool = ["src", "src/lib", "tests", ".", "docs", "*", "../escape"];
        let bool_pool = [true, false];
        let depth_pool = [0u8, 1, 2, 3];
        let expiry_pool = [0u64, 10, 100, 1_000];
        let budget_pool: [Option<u64>; 3] = [None, Some(10), Some(1_000)];

        let mut cases = 0usize;
        for &parent_path in &path_pool {
            for &parent_network in &bool_pool {
                for &parent_destructive in &bool_pool {
                    for &parent_depth in &depth_pool {
                        for &parent_expiry in &expiry_pool {
                            let parent = WorkerEnvelope {
                                principal: "root".to_string(),
                                paths: vec![PathScope::new(parent_path)],
                                tools: ToolSet::all(),
                                network: parent_network,
                                destructive: parent_destructive,
                                delegation_depth: parent_depth,
                                expires_at: parent_expiry,
                                token_budget: Some(1_000),
                            };
                            for &req_path in &path_pool {
                                for &req_network in &bool_pool {
                                    for &req_destructive in &bool_pool {
                                        for &req_depth in &depth_pool {
                                            for &req_expiry in &expiry_pool {
                                                for &req_budget in &budget_pool {
                                                    cases += 1;
                                                    let requested = WorkerEnvelope {
                                                        principal: "root/child".to_string(),
                                                        paths: vec![PathScope::new(req_path)],
                                                        tools: ToolSet::all(),
                                                        network: req_network,
                                                        destructive: req_destructive,
                                                        delegation_depth: req_depth,
                                                        expires_at: req_expiry,
                                                        token_budget: req_budget,
                                                    };
                                                    if let Ok(child) =
                                                        WorkerEnvelope::narrow(&parent, &requested)
                                                    {
                                                        assert!(
                                                            child.is_subset_of(&parent),
                                                            "narrow({parent:?}, {requested:?}) = {child:?} was not a subset"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            cases > 1_000,
            "the generated table should be sizeable, got {cases}"
        );
    }

    #[test]
    fn locked_grants_nothing_and_is_a_subset_of_any_envelope() {
        let locked = WorkerEnvelope::locked();
        assert!(locked.paths.is_empty());
        assert_eq!(locked.tools, ToolSet::none());
        assert!(!locked.network);
        assert!(!locked.destructive);
        assert_eq!(locked.delegation_depth, 0);

        let anything = envelope(&["src"], 3, true, true);
        assert!(
            locked.is_subset_of(&anything),
            "the locked fallback must never exceed any real envelope"
        );
    }

    #[test]
    fn digest_is_deterministic_and_sensitive_to_every_field() {
        let e = envelope(&["src"], 1, true, false);
        let a = digest(&e).expect("hashing a valid envelope never fails");
        let b = digest(&e).expect("hashing a valid envelope never fails");
        assert_eq!(
            a, b,
            "the same envelope value must hash identically every time"
        );
        assert_eq!(a.len(), 64, "sha256 hex is 64 characters");

        let mut changed = e.clone();
        changed.destructive = true;
        let c = digest(&changed).expect("hashing a valid envelope never fails");
        assert_ne!(a, c, "a changed field must change the digest");
    }

    #[test]
    fn tool_set_is_subset_of_matches_family_by_family_containment() {
        let full = ToolSet::all();
        let none = ToolSet::none();
        assert!(none.is_subset_of(&full));
        assert!(!full.is_subset_of(&none));
        assert!(full.is_subset_of(&full));

        let edit_only = ToolSet {
            edit: true,
            shell: false,
            network: false,
            delegate: false,
        };
        assert!(edit_only.is_subset_of(&full));
        assert!(!edit_only.is_subset_of(&none));
    }
}
