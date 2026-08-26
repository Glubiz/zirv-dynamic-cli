//! zirv's own canonical permissions policy, and the honest translation of it
//! onto whatever the harness in front of it can actually enforce.
//!
//! The policy is stated **once**, in zirv's own vocabulary
//! ([`EffectivePolicy`], one [`Stance`] per [`Capability`]), and never in a
//! harness's. Each adapter then answers, per capability and requested stance,
//! what it can actually deliver ([`AgentAdapter::policy_support`](super::
//! adapters::AgentAdapter::policy_support) -> [`CapabilityDescriptor`]), and
//! [`evaluate`] combines the two into a [`PolicyReport`] whose every line
//! carries one of four honest states: enforced, degraded, unsupported, or
//! operator-controlled.
//!
//! **Markdown instructions are advisory context, never enforcement.** That is
//! not a comment here, it is the type system: there is no way to build a
//! [`CapabilityOutcome`] claiming [`Support::Enforced`] from prompt text,
//! because the only thing that produces an outcome is an adapter naming a
//! verified per-run *mechanism* it pins on the launch itself.
//! [`Support::Unsupported`]'s own label says "not enforced (advisory only)",
//! so a rendered report cannot read as a promise either.
//!
//! ## Layering, and why it is not `ctx.toml`'s deep merge
//!
//! `CtxConfig`'s other sections are deep-merged (home file, then repo file,
//! then env), with `REPO_FORBIDDEN` blocking the individual keys a checkout
//! must not touch at all. Policy cannot use that shape: `REPO_FORBIDDEN` is
//! all-or-nothing per key, and issue #43 requires a repo to be able to
//! *narrow* policy while never widening it. So [`resolve`] folds the three
//! layers the way `crate::settings`'s `[agents]` gate does -- a dedicated
//! asymmetric fold rather than a merge:
//!
//! ```text
//! final(capability) = env(capability)                       if set
//!                   else max(home(capability), repo(capability))
//! ```
//!
//! [`Stance`] is ordered least-to-most restrictive, so `max` *is* narrowing:
//! a repo may ratchet a stance stricter and can never loosen one, by
//! construction rather than by a check that could be forgotten. The
//! environment sits above the fold entirely and wins outright in both
//! directions -- the same escape hatch, for the same reason, that
//! `ZIRV_AGENT_<NAME>_ENABLED` gives an operator whose checkout disabled an
//! agent they need.
//!
//! Deterministic and side-effect-free, like `rot.rs`: no clock, no
//! filesystem, no process env reads (the env layer arrives as an
//! `EnvLookup` closure the caller owns).

// `resolve` has a production caller (`CtxConfig::load`); the evaluation half
// -- [`evaluate`], [`PolicyReport`] and the descriptor vocabulary the adapters
// answer with -- does not yet. Issue #43 deliberately splits building this
// model from consuming it: the context compiler (issue #44) is what pins a
// stance onto a launch, and `zirv ctx status` (issue #46) is what renders a
// report. Every item below is exercised by this module's own tests in the
// meantime, module-wide rather than per item because the whole evaluation
// surface is in the same position, not a stray unused helper.
//
// The same compiler also composes the canonical `.zirv/context/` layer
// (issue #41, `context.rs`) into a launch -- and that coupling matters here:
// repo-owned canonical text can describe a permission stance in prose, but
// describing is not enforcing, so #44 must never let canonical text cause a
// launch to be reported as `Enforced`/`Degraded` for a capability this
// module did not itself verify a mechanism for. See `docs/obsidian/Concepts/
// Untrusted Configuration.md`'s "Context vs. policy" section.
#![allow(dead_code)]

use super::CtxResult;
use super::adapters::AgentAdapter;
use super::config::EnvLookup;
use serde::Deserialize;

/// One thing zirv's policy has an opinion about. Deliberately harness-neutral:
/// these are the questions an operator asks ("may this session write outside
/// the repo?"), not the flags any particular CLI happens to expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Creating/modifying/deleting files inside the repository checkout.
    RepoFsWrite,
    /// The same, anywhere else on the machine.
    OutsideRepoFsWrite,
    /// Running shell commands at all.
    ShellExec,
    /// Outbound network access from the session.
    Network,
    /// How much the session may do unattended. `Allow` means approval prompts
    /// may be bypassed, `Ask` that they must be requested, `Deny` that the
    /// session must not attempt anything needing approval at all -- a
    /// read-only session.
    Approval,
    /// `git push`, and history-rewriting/destructive git operations.
    GitPushDestructive,
    /// Which of the harness's own tools (including MCP servers) may run.
    ToolAccess,
}

impl Capability {
    /// Every capability, in the order a report renders them. The single place
    /// the set is enumerated, so adding one does not mean hunting call sites.
    pub const ALL: [Capability; 7] = [
        Capability::RepoFsWrite,
        Capability::OutsideRepoFsWrite,
        Capability::ShellExec,
        Capability::Network,
        Capability::Approval,
        Capability::GitPushDestructive,
        Capability::ToolAccess,
    ];

    /// The `[policy]` key that sets this capability's stance.
    pub fn key(self) -> &'static str {
        match self {
            Capability::RepoFsWrite => "repo_fs_write",
            Capability::OutsideRepoFsWrite => "outside_repo_fs_write",
            Capability::ShellExec => "shell_exec",
            Capability::Network => "network",
            Capability::Approval => "approval",
            Capability::GitPushDestructive => "git_push_destructive",
            Capability::ToolAccess => "tool_access",
        }
    }

    /// The operator-only override that sits above the home/repo fold. Not an
    /// `ENV_MAP` entry: policy has its own fold (see the module doc), so its
    /// env layer is applied by [`resolve`] rather than merged into the shared
    /// config table.
    pub fn env_var(self) -> &'static str {
        match self {
            Capability::RepoFsWrite => "ZIRV_CTX_POLICY_REPO_FS_WRITE",
            Capability::OutsideRepoFsWrite => "ZIRV_CTX_POLICY_OUTSIDE_REPO_FS_WRITE",
            Capability::ShellExec => "ZIRV_CTX_POLICY_SHELL_EXEC",
            Capability::Network => "ZIRV_CTX_POLICY_NETWORK",
            Capability::Approval => "ZIRV_CTX_POLICY_APPROVAL",
            Capability::GitPushDestructive => "ZIRV_CTX_POLICY_GIT_PUSH_DESTRUCTIVE",
            Capability::ToolAccess => "ZIRV_CTX_POLICY_TOOL_ACCESS",
        }
    }

    /// Human-readable name for a rendered report.
    pub fn label(self) -> &'static str {
        match self {
            Capability::RepoFsWrite => "repository filesystem writes",
            Capability::OutsideRepoFsWrite => "writes outside the repository",
            Capability::ShellExec => "shell execution",
            Capability::Network => "network access",
            Capability::Approval => "approval/ask behavior",
            Capability::GitPushDestructive => "git push / destructive git",
            Capability::ToolAccess => "MCP/tool access",
        }
    }
}

/// How restrictive zirv wants one capability to be. **Ordered**
/// least-to-most restrictive: `Allow < Ask < Deny`, which is what makes
/// narrowing expressible as `max` (see the module doc) rather than as a
/// hand-written comparison per capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    /// zirv declares no restriction of its own. The harness's own defaults and
    /// the operator's own harness settings decide -- which is why an `Allow`
    /// stance always reports as [`Support::OperatorControlled`]: there is
    /// nothing for zirv to enforce, and claiming otherwise would be a lie in
    /// the permissive direction.
    #[default]
    Allow,
    /// Permitted only with an explicit approval from the operator.
    Ask,
    /// Not permitted.
    Deny,
}

impl Stance {
    pub fn label(self) -> &'static str {
        match self {
            Stance::Allow => "allow",
            Stance::Ask => "ask",
            Stance::Deny => "deny",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "allow" => Some(Stance::Allow),
            "ask" => Some(Stance::Ask),
            "deny" => Some(Stance::Deny),
            _ => None,
        }
    }
}

/// zirv's canonical policy: one [`Stance`] per [`Capability`], stated once and
/// translated per harness rather than restated per harness.
///
/// Every field defaults to [`Stance::Allow`] -- "zirv declares no restriction
/// of its own" -- which is the literal truth about zirv before an operator
/// writes a `[policy]` table, and keeps a default install's behavior exactly
/// what it was. An operator opts in by naming the stances they want; the
/// report then says, per harness, which of them are real.
///
/// **`network` is the one deliberate exception (2026-08-26, codex approval-
/// posture round; refined again in the same round's correction pass --
/// see below).** Every other capability's `Allow` default means "zirv adds
/// no argv, the harness's own native default governs" -- and every harness's
/// own native default for the other six capabilities happens to already be
/// permissive, so that reads as "no restriction". Network is different:
/// codex's own native default under `--sandbox workspace-write` is *already
/// closed* (verified: a real launch carries `network_access: false` with no
/// zirv-added flag at all), so treating an unconfigured `network` the same
/// as the other six (a plain `Stance` defaulting to `Allow`) would either
/// widen codex's own native default the moment that mapping was wired up
/// (if defaulted to `Allow`), or falsely claim zirv itself is denying
/// network on every unconfigured install (if defaulted to `Stance::Deny` --
/// this module's own [`evaluate`] would then render a "network: deny" row
/// nobody asked for, because *codex's own default* is what is closed here,
/// not a zirv-imposed restriction). `network` is therefore `Option<Stance>`,
/// not a plain `Stance`: `None` means "no operator layer has ever named
/// network at all", which [`Default`] gives for free (an `Option`'s own
/// default), which [`resolve`] preserves as `None` when both layers are
/// silent (`resolve_network`'s own doc comment), and which [`evaluate`]
/// reads as "omit this row" rather than reporting a stance zirv never
/// actually chose. `Some(stance)` means an operator layer did name one, and
/// is reported exactly like any other capability from there.
///
/// `deny_unknown_fields`: a typo'd capability name hard-errors rather than
/// silently leaving that capability at `Allow`, which is the failure mode a
/// permissions surface can least afford.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectivePolicy {
    pub repo_fs_write: Stance,
    pub outside_repo_fs_write: Stance,
    pub shell_exec: Stance,
    pub network: Option<Stance>,
    pub approval: Stance,
    pub git_push_destructive: Stance,
    pub tool_access: Stance,
}

impl EffectivePolicy {
    /// For `Network`, projects the `Option<Stance>` field down to a plain
    /// `Stance` by treating "no operator layer ever named it" (`None`) as
    /// `Deny` -- the same closed answer `resolve_network`'s own "both
    /// layers silent" case produces, and what codex's own native default
    /// already does with no zirv-added flag at all. This is a convenience
    /// for callers that only need "what does zirv want to hold this
    /// harness to" (the `PolicyReport::render` baseline loop, principally);
    /// callers that need to tell "never configured" apart from "explicitly
    /// denied" (`evaluate`, to decide whether to render a row at all) must
    /// read `self.network` directly instead, which is why `evaluate` does
    /// not call this method for `Network`.
    pub fn stance(&self, capability: Capability) -> Stance {
        match capability {
            Capability::RepoFsWrite => self.repo_fs_write,
            Capability::OutsideRepoFsWrite => self.outside_repo_fs_write,
            Capability::ShellExec => self.shell_exec,
            Capability::Network => self.network.unwrap_or(Stance::Deny),
            Capability::Approval => self.approval,
            Capability::GitPushDestructive => self.git_push_destructive,
            Capability::ToolAccess => self.tool_access,
        }
    }

    /// No `Network` arm: `network`'s field type (`Option<Stance>`) cannot
    /// yield a `&mut Stance`, and both callers below (`narrowed_by`'s fold,
    /// `resolve`'s env-override loop) explicitly skip `Capability::Network`
    /// and assign `.network` directly instead -- see each one's own doc
    /// comment for why the ordinary per-capability mechanism is unsound for
    /// this one field. Reachable only if a future caller adds `Network` to
    /// one of those loops without also handling it specially first.
    fn stance_mut(&mut self, capability: Capability) -> &mut Stance {
        match capability {
            Capability::RepoFsWrite => &mut self.repo_fs_write,
            Capability::OutsideRepoFsWrite => &mut self.outside_repo_fs_write,
            Capability::ShellExec => &mut self.shell_exec,
            Capability::Network => {
                unreachable!(
                    "network has no plain Stance slot (Option<Stance>); callers must skip \
                     Capability::Network and assign .network directly -- see resolve_network"
                )
            }
            Capability::Approval => &mut self.approval,
            Capability::GitPushDestructive => &mut self.git_push_destructive,
            Capability::ToolAccess => &mut self.tool_access,
        }
    }

    /// `self` narrowed by an untrusted layer: per capability, the stricter of
    /// the two. A `narrower` value that is looser than `self` contributes
    /// nothing -- there is no code path by which it could, since `max` cannot
    /// return the smaller of two values. This is the whole privilege-widening
    /// defense for the repo layer, and it is a property of `Stance`'s
    /// ordering rather than a check a future edit could drop.
    ///
    /// **`Network` is excluded from this loop** (2026-08-26, correction
    /// round): the proof above depends on `Stance::default()` (`Allow`)
    /// being both a layer's "I said nothing" value AND the loosest value
    /// the type can express, which is exactly the property `network`'s own
    /// `Option<Stance>` deliberately breaks (see `resolve_network`'s own doc
    /// comment). `.network` is left exactly as `self` had it here; `resolve`
    /// always overwrites it afterward with `resolve_network`'s own answer,
    /// so a caller of `narrowed_by` alone (rather than through `resolve`)
    /// must not read the result's `.network` as meaningful.
    pub fn narrowed_by(self, narrower: EffectivePolicy) -> EffectivePolicy {
        let mut out = self;
        for capability in Capability::ALL {
            if capability == Capability::Network {
                continue;
            }
            let stance = self.stance(capability).max(narrower.stance(capability));
            *out.stance_mut(capability) = stance;
        }
        out
    }

    /// The failed-config-load fallback (`config::degrade_to_operator_only`,
    /// used by `optimize.rs`/`hook.rs`): full **Deny** on every capability.
    /// `EffectivePolicy::default()` (all `Allow`) is the right answer to "no
    /// `[policy]` table was ever written" -- it is the literal truth about an
    /// operator who never opted in. It is the wrong answer to "the config
    /// could not be read at all" (malformed TOML, a forbidden repo key):
    /// that is not an operator statement that no restriction is wanted, and
    /// handing it the widest policy zirv can state is a fail-open on the one
    /// surface this module exists to keep narrowing-only, now that issue #44
    /// makes `cfg.policy` load-bearing (attached to every `CompiledContext`).
    pub fn fail_closed() -> Self {
        EffectivePolicy {
            repo_fs_write: Stance::Deny,
            outside_repo_fs_write: Stance::Deny,
            shell_exec: Stance::Deny,
            network: Some(Stance::Deny),
            approval: Stance::Deny,
            git_push_destructive: Stance::Deny,
            tool_access: Stance::Deny,
        }
    }

    /// The stances zirv's own shipped INTERACTIVE projection actually
    /// delivers, before any `[policy]` table narrows anything -- the spec's
    /// own defaults table (`docs/superpowers/specs/2026-08-24-cross-harness-
    /// permissions-design.md`), stated once so `PolicyReport::render` can
    /// show an operator what an unconfigured interactive launch carries.
    ///
    /// Deliberately **not** `EffectivePolicy::default()`, and deliberately
    /// **not** an input to [`resolve`]'s fold. `Default` means "zirv declares
    /// no restriction of its own" and is what `narrowed_by`'s widening
    /// defense rests on; folding this in instead would silently narrow every
    /// headless launch too, and -- because the fold is a `max` -- would make
    /// an operator's own `ZIRV_CTX_POLICY_OUTSIDE_REPO_FS_WRITE=allow`
    /// unexpressible, since `max(Ask, Allow)` is `Ask`. This is a reported
    /// baseline: it describes what the argv in
    /// `ClaudeAdapter::default_sandbox_args` amounts to, and nothing decides
    /// anything from it.
    pub fn interactive_baseline() -> Self {
        EffectivePolicy {
            // `Edit(./**)` is pre-approved on the allow list.
            repo_fs_write: Stance::Allow,
            // Not pre-approved, so `--permission-mode default` prompts --
            // where `dontAsk` used to kill the call outright.
            outside_repo_fs_write: Stance::Ask,
            // Governed per command by `safety.rs`, which is the sole
            // prompting gate on this posture: the allow set and every
            // unclassified command run silently, the short ask set prompts,
            // the deny set is refused by rule.
            shell_exec: Stance::Ask,
            // `WebFetch`/`WebSearch` are pre-approved.
            network: Some(Stance::Allow),
            approval: Stance::Ask,
            // Force-push and history rewrites are in the built-in ask set.
            git_push_destructive: Stance::Ask,
            tool_access: Stance::Allow,
        }
    }
}

/// Resolves the three policy layers per the module doc's fold: `home`/`repo`
/// are the `[policy]` tables lifted out of `~/.zirv/ctx.toml` and
/// `<repo>/.zirv/ctx.toml` (either absent when that file has no `[policy]`
/// section), and `env` is the operator override that sits above both.
///
/// The repo layer can only ever narrow; the environment wins outright in
/// either direction. An unparseable stance -- in a file or an env var -- is a
/// hard error rather than a silent fall back to `Allow`: quietly widening a
/// permissions surface because a value was misspelled is the one outcome this
/// module exists to prevent.
///
/// **`network` is folded separately (2026-08-26, correction round), never
/// through `narrowed_by`'s ordinary `max`.** The six other capabilities all
/// share one property `narrowed_by`'s safety proof depends on: `Stance::
/// default()` (`Allow`) is both a layer's "I said nothing" value AND the
/// loosest value the type can express, so a layer's silence is provably a
/// no-op against the fold (`max(x, Allow) == x` always). `network`'s default
/// answer is `Deny`, not `Allow` -- the loosest value in the type -- so
/// reusing the same trick (an unmentioned key silently deserializing to
/// `Stance::Deny` via `EffectivePolicy`'s own `Default`) breaks that proof:
/// a repo layer that mentions nothing would contribute `Deny` for network
/// exactly as if it had explicitly denied it, defeating an operator's own
/// home-level `network = "allow"` even when the repo took no position at
/// all. `resolve_network` below is the fix -- see its own doc comment.
pub fn resolve(
    home: Option<toml::Value>,
    repo: Option<toml::Value>,
    env: EnvLookup<'_>,
) -> CtxResult<EffectivePolicy> {
    let home_network = parse_network_layer(&home, "~/.zirv/ctx.toml")?;
    let repo_network = parse_network_layer(&repo, "<repo>/.zirv/ctx.toml")?;

    let mut resolved = parse_layer(home, "~/.zirv/ctx.toml")?
        .narrowed_by(parse_layer(repo, "<repo>/.zirv/ctx.toml")?);
    resolved.network = resolve_network(home_network, repo_network);

    // `Network` is excluded from this loop and handled separately right
    // below: its field is `Option<Stance>`, so `stance_mut` cannot name a
    // `&mut Stance` slot for it (see that method's own doc comment).
    for capability in Capability::ALL {
        if capability == Capability::Network {
            continue;
        }
        let Some(raw) = env(capability.env_var()) else {
            continue;
        };
        let Some(stance) = Stance::parse(&raw) else {
            return Err(format!(
                "{}: expected allow, ask or deny, got '{raw}'",
                capability.env_var()
            )
            .into());
        };
        *resolved.stance_mut(capability) = stance;
    }

    if let Some(raw) = env(Capability::Network.env_var()) {
        let Some(stance) = Stance::parse(&raw) else {
            return Err(format!(
                "{}: expected allow, ask or deny, got '{raw}'",
                Capability::Network.env_var()
            )
            .into());
        };
        resolved.network = Some(stance);
    }

    Ok(resolved)
}

fn parse_layer(layer: Option<toml::Value>, origin: &str) -> CtxResult<EffectivePolicy> {
    let Some(layer) = layer else {
        return Ok(EffectivePolicy::default());
    };
    layer
        .try_into()
        .map_err(|e| format!("{origin}: invalid [policy] section: {e}").into())
}

/// One `[policy]` layer's raw, unresolved opinion on `network` alone --
/// `None` when the layer's table (or the whole layer) never mentions the key
/// at all, distinct from an explicit `network = "allow"`/`"deny"`/`"ask"`.
/// `EffectivePolicy`/`Stance` cannot carry this distinction (a bare `Stance`
/// field always deserializes to *some* concrete value, `Stance::default()`
/// when the key is absent -- see `resolve`'s own doc comment for why that
/// collapse is exactly the bug this exists to avoid for `network`), so this
/// is a second, narrow, `Option`-shaped deserialization of the SAME raw TOML
/// value `parse_layer` already validates -- deliberately not the primary
/// error path: a malformed layer already fails loudly through `parse_layer`
/// (called right after this, in `resolve`, with the identical `origin`), so
/// a parse failure here is either that same error about to be raised again
/// or genuinely unreachable, never a chance to report something new.
fn parse_network_layer(layer: &Option<toml::Value>, origin: &str) -> CtxResult<Option<Stance>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct NetworkOnly {
        network: Option<Stance>,
    }
    let Some(layer) = layer else {
        return Ok(None);
    };
    let parsed: NetworkOnly = layer
        .clone()
        .try_into()
        .map_err(|e| format!("{origin}: invalid [policy] section: {e}"))?;
    Ok(parsed.network)
}

/// `network`'s own home/repo combination (2026-08-26, correction round;
/// fixed again the same round -- see below), replacing the ordinary
/// `narrowed_by` fold for this one field -- see `resolve`'s own doc comment
/// for why the shared `max`-based mechanism is unsound here.
///
/// Returns `None` only when NEITHER layer ever names `network` at all: that
/// is "no operator opinion exists", which [`EffectivePolicy::default`]
/// itself now represents the same way, and which [`evaluate`] reads as
/// "omit the row" rather than a stance zirv chose. The moment either layer
/// names `network`, the answer is `Some` from there on -- narrowing is
/// always still possible (a repo naming `ask`/`deny` can still tighten a
/// silent or `allow` home), but the *distinctness* of `ask` from `deny` is
/// preserved rather than collapsed, because both feed the same `max` as
/// home does.
///
/// **The bug this fixes (2026-08-26): the previous version only special-
/// cased `repo == Some(Deny)`, so a repo's explicit `network = "ask"`
/// fell through to `home`'s own value untouched** -- silently dropping the
/// repo's narrowing the moment home said `allow` (e.g. home `allow` + repo
/// `ask` resolved to `Allow`, when the repo layer explicitly asked for no
/// more than `Ask`). A repo may always narrow, regardless of which
/// non-`Deny` stance it names; treating `deny` as the only narrowing value
/// a repo can express was the defect. The fix folds both layers through the
/// same `Stance` ordering the other six capabilities already use, with each
/// layer's *own* silent-value substituted before the `max`:
/// `max(home.unwrap_or(Deny), repo.unwrap_or(Allow))`.
///
/// - Home's silence substitutes `Deny` (the operator never opted in, so
///   nothing pulls the result toward `Allow` on its behalf) -- this is what
///   keeps "nobody said anything" (`None, None`) from ever landing on
///   `Allow`, without needing a separate case for it below.
/// - Repo's silence substitutes `Allow` (a repo that names nothing has no
///   opinion, so it must never pull the result toward `Deny`/`Ask` on its
///   own) -- this is what lets home's own explicit `allow` survive a silent
///   repo (`max(Allow, Allow) == Allow`), which the previous version could
///   only do by special-casing "home said something" as an unconditional
///   win; here it falls out of the same formula every other case uses.
/// - Repo explicitly naming `ask` or `deny` now narrows exactly like the
///   six generically-folded capabilities do: `max` with home's value can
///   only move toward the repo's stricter stance, never away from it.
fn resolve_network(home: Option<Stance>, repo: Option<Stance>) -> Option<Stance> {
    if home.is_none() && repo.is_none() {
        return None;
    }
    Some(std::cmp::max(
        home.unwrap_or(Stance::Deny),
        repo.unwrap_or(Stance::Allow),
    ))
}

/// What zirv can honestly promise for one capability on one harness. There is
/// deliberately no "probably" or "advisory" state that reads as enforcement:
/// [`Unsupported`](Support::Unsupported) is what an adapter with only prompt
/// text to offer must report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// zirv pins a verified per-run mechanism, and the harness enforces
    /// exactly the requested stance.
    Enforced,
    /// zirv pins a verified mechanism that does not match the request exactly
    /// -- it restricts something adjacent, broader or narrower -- so the
    /// stance is only approximately carried. The `mechanism` string on the
    /// descriptor says how it differs; a report must never round this up to
    /// "enforced".
    Degraded,
    /// No verified per-run mechanism at all. Prompt text can *ask* the session
    /// to respect the stance, and prompt text is not enforcement.
    Unsupported,
    /// The harness does enforce this, but from the operator's own settings
    /// (`.claude/settings.json` permissions, `~/.codex/config.toml`), which
    /// zirv reads and never rewrites. Also the answer for a [`Stance::Allow`]
    /// capability, where zirv is imposing nothing in the first place.
    OperatorControlled,
}

impl Support {
    /// Report wording. `Unsupported`'s own label states that it is not
    /// enforced, so a rendered line cannot be misread as a guarantee even out
    /// of context.
    pub fn label(self) -> &'static str {
        match self {
            Support::Enforced => "enforced",
            Support::Degraded => "degraded (partially enforced)",
            Support::Unsupported => "not enforced (advisory only)",
            Support::OperatorControlled => "operator-controlled",
        }
    }

    /// Whether the harness enforces *exactly* the requested stance, with
    /// nothing left for prompt text or the operator's own settings to carry
    /// instead. `Degraded` deliberately answers `false` here: a mechanism
    /// that only approximates the request is real, but it is not the same
    /// guarantee as `Enforced`, and a report must never treat the two alike
    /// -- this is the predicate `PolicyReport::unenforced` keys off of. There
    /// used to be a looser `is_enforced_by_zirv` (true for `Enforced` *or*
    /// `Degraded`) that `unenforced` was built on; that looser question is
    /// exactly what let a `Degraded` cell hide from a report as if it were
    /// `Enforced`, so it was removed rather than kept alongside this one.
    pub fn is_fully_enforced(self) -> bool {
        matches!(self, Support::Enforced)
    }
}

/// One adapter's answer for one (capability, stance) pair: what it can
/// deliver, and the verified mechanism it would deliver it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub support: Support,
    /// The actual verified mechanism, named rather than paraphrased
    /// (`"--disallowedTools=Write,Edit,Bash,NotebookEdit"`), or -- for the
    /// three non-`Enforced` states -- why there is no matching one. Goes
    /// straight into the report, so it is written to be read by an operator.
    pub mechanism: &'static str,
}

impl CapabilityDescriptor {
    pub fn enforced(mechanism: &'static str) -> Self {
        Self {
            support: Support::Enforced,
            mechanism,
        }
    }

    pub fn degraded(mechanism: &'static str) -> Self {
        Self {
            support: Support::Degraded,
            mechanism,
        }
    }

    pub fn operator_controlled(mechanism: &'static str) -> Self {
        Self {
            support: Support::OperatorControlled,
            mechanism,
        }
    }

    /// `Unsupported` with a specific reason -- for a capability/stance pair
    /// where a mechanism exists but has been checked and ruled out (e.g. a
    /// flag that scopes something adjacent, not the capability asked about),
    /// as opposed to [`advisory_only`](Self::advisory_only)'s generic "no
    /// mechanism at all" answer.
    pub fn unsupported(mechanism: &'static str) -> Self {
        Self {
            support: Support::Unsupported,
            mechanism,
        }
    }

    /// The trait default, and the honest answer for any harness/capability
    /// pair zirv has not verified a mechanism for. Named for what it leaves
    /// behind: an instruction in the prompt, which no harness is obliged to
    /// obey.
    pub fn advisory_only() -> Self {
        Self::unsupported(
            "no verified per-run mechanism; prompt text is advisory context, not enforcement",
        )
    }
}

/// One rendered row of a [`PolicyReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityOutcome {
    pub capability: Capability,
    pub stance: Stance,
    pub support: Support,
    pub mechanism: &'static str,
}

/// What one policy actually means on one harness, for one launch posture.
/// Built only by [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReport {
    pub adapter: &'static str,
    /// Which posture this report describes. The same policy on the same
    /// adapter genuinely means two different things (2026-08-24): an `Ask`
    /// stance is a real prompt on an interactive launch and a fail-closed
    /// refusal on a headless one, so a report that did not say which it was
    /// describing was ambiguous by construction.
    pub mode: super::adapters::LaunchMode,
    pub outcomes: Vec<CapabilityOutcome>,
}

impl PolicyReport {
    /// Every stance zirv asked for that the harness does not fully hold to --
    /// the lines an operator actually needs to see. This includes `Degraded`
    /// (a real mechanism that only approximates the request), `Unsupported`
    /// (only advisory prompt text) and `OperatorControlled` (only the
    /// operator's own harness settings): none of these is the exact
    /// guarantee `Enforced` is, so a report must never hide any of them.
    /// [`partially_enforced`](Self::partially_enforced) narrows this to the
    /// `Degraded` subset specifically.
    pub fn unenforced(&self) -> Vec<&CapabilityOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| {
                outcome.stance != Stance::Allow && !outcome.support.is_fully_enforced()
            })
            .collect()
    }

    /// The subset of [`unenforced`](Self::unenforced) where zirv pins a real,
    /// verified mechanism that only approximates the requested stance
    /// (`Support::Degraded`) -- worth calling out on its own, since a
    /// degraded pin is doing something, unlike `Unsupported` or
    /// `OperatorControlled`.
    pub fn partially_enforced(&self) -> Vec<&CapabilityOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.support == Support::Degraded)
            .collect()
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "policy on {} ({} launch):\n",
            self.adapter,
            self.mode.label()
        );
        for outcome in &self.outcomes {
            out.push_str(&format!(
                "  {}: {} -- {} ({})\n",
                outcome.capability.label(),
                outcome.stance.label(),
                outcome.support.label(),
                outcome.mechanism
            ));
        }
        // Only interactively: the headless baseline is `dontAsk`'s
        // deny-by-omission, which the per-capability lines above already
        // describe. Printing an "interactive baseline" under a headless
        // report would be a claim about a launch this report is not about.
        if self.mode.is_interactive() {
            out.push_str("  shipped interactive baseline (before any [policy] table):\n");
            let baseline = EffectivePolicy::interactive_baseline();
            for capability in Capability::ALL {
                out.push_str(&format!(
                    "    {}: {}\n",
                    capability.label(),
                    baseline.stance(capability).label()
                ));
            }
        }
        out
    }
}

/// Translates one canonical policy onto one adapter, for one launch posture.
/// Pure: it asks the adapter for descriptors and combines them, and neither
/// side reads a clock, the filesystem or the environment.
///
/// A [`Stance::Allow`] capability is answered here rather than by the adapter:
/// zirv is imposing nothing, so there is no mechanism to name and nothing an
/// adapter could usefully say. That is also why every adapter's own
/// `policy_support` may leave `Allow` to a catch-all arm -- it is never asked.
///
/// **`Network` renders no row at all when `policy.network` is `None`**
/// (2026-08-26, correction round): `None` means no operator layer has ever
/// named `network`, which is not a stance zirv chose to report -- unlike
/// every other capability, whose `Stance::default()` (`Allow`) is itself a
/// real, reportable answer ("operator-controlled, zirv imposes nothing").
/// Before this, `EffectivePolicy::default()`'s own `network: Stance::Deny`
/// meant an unconfigured install's report carried a "network: deny" line
/// implying zirv itself was denying network, when the true state was "no
/// opinion was ever expressed" -- codex's own native default happens to
/// already be closed, with no zirv-added flag at all. This is why `Network`
/// is handled here directly from `policy.network` rather than through the
/// generic `policy.stance(capability)` call every other capability uses:
/// `stance()` cannot express "omit this row", only a concrete `Stance`.
pub fn evaluate(
    policy: &EffectivePolicy,
    adapter: &dyn AgentAdapter,
    mode: super::adapters::LaunchMode,
) -> PolicyReport {
    let outcomes = Capability::ALL
        .into_iter()
        .filter_map(|capability| {
            let stance = if capability == Capability::Network {
                policy.network?
            } else {
                policy.stance(capability)
            };
            let descriptor = match stance {
                Stance::Allow => CapabilityDescriptor::operator_controlled(
                    "zirv declares no restriction; the harness's own defaults and the operator's \
                     own settings decide",
                ),
                _ => adapter.policy_support(capability, stance, mode),
            };
            Some(CapabilityOutcome {
                capability,
                stance,
                support: descriptor.support,
                mechanism: descriptor.mechanism,
            })
        })
        .collect();
    PolicyReport {
        adapter: adapter.name(),
        mode,
        outcomes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn table(toml_text: &str) -> Option<toml::Value> {
        Some(toml::from_str::<toml::Value>(toml_text).expect("test toml parses"))
    }

    /// The spec's own interactive defaults table, stated once so a report can
    /// show an operator what an unconfigured interactive launch carries.
    #[test]
    fn the_interactive_baseline_is_the_specs_own_defaults_table() {
        let baseline = EffectivePolicy::interactive_baseline();
        assert_eq!(baseline.repo_fs_write, Stance::Allow);
        assert_eq!(baseline.outside_repo_fs_write, Stance::Ask);
        assert_eq!(baseline.network, Some(Stance::Allow));
        assert_eq!(baseline.shell_exec, Stance::Ask);
        assert_eq!(baseline.approval, Stance::Ask);
        assert_eq!(baseline.git_push_destructive, Stance::Ask);
        assert_eq!(baseline.tool_access, Stance::Allow);
    }

    /// SECURITY: the baseline is a REPORTED fact, never a fold input.
    /// `EffectivePolicy::default()` must stay all-`Allow` except `network`
    /// (its own documented exception, `EffectivePolicy`'s `Default` impl) --
    /// the rest is what `narrowed_by`'s widening defense and `resolve`'s fold
    /// rest on, and what makes `ZIRV_CTX_POLICY_*` able to loosen at all.
    #[test]
    fn the_interactive_baseline_does_not_touch_the_default_or_the_fold() {
        assert_ne!(
            EffectivePolicy::interactive_baseline(),
            EffectivePolicy::default()
        );
        for capability in Capability::ALL {
            let expected = if capability == Capability::Network {
                Stance::Deny
            } else {
                Stance::Allow
            };
            assert_eq!(
                EffectivePolicy::default().stance(capability),
                expected,
                "{} must still default to {}",
                capability.key(),
                expected.label()
            );
        }
    }

    /// The bug this round fixes (2026-08-26): before `network` became
    /// `Option<Stance>`, `EffectivePolicy::default()`'s own `network:
    /// Stance::Deny` meant `evaluate` always produced a `Network` outcome on
    /// an unconfigured install, and its rendered line read "network access:
    /// deny -- ..." -- implying zirv itself was denying network, when in
    /// truth no operator layer had ever named it (codex's own native
    /// default already denies it, with no zirv-added flag at all). A report
    /// built from a wholly-default policy must contain no `Network` row and
    /// no "network" text at all.
    #[test]
    fn a_default_policys_report_carries_no_spurious_network_row() {
        let policy = EffectivePolicy::default();
        let claude = ClaudeAdapter::new(None);
        let codex = CodexAdapter::new(None);
        for adapter in [&claude as &dyn AgentAdapter, &codex as &dyn AgentAdapter] {
            let report = evaluate(&policy, adapter, adapters::LaunchMode::Headless);
            assert!(
                !report
                    .outcomes
                    .iter()
                    .any(|outcome| outcome.capability == Capability::Network),
                "{}: a default (unconfigured) policy must render no Network row at all",
                report.adapter
            );
            assert!(
                !report.render().contains("network"),
                "{}: the rendered report must not mention network at all when unconfigured",
                report.adapter
            );
        }
    }

    /// Once an operator layer explicitly names `network` (whatever the
    /// stance), the row comes back -- `evaluate` only omits it for `None`,
    /// never for a `Some` the operator chose, even `Some(Stance::Deny)`.
    #[test]
    fn an_explicitly_configured_network_stance_still_renders_its_row() {
        let policy = EffectivePolicy {
            network: Some(Stance::Deny),
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        assert!(
            report
                .outcomes
                .iter()
                .any(|outcome| outcome.capability == Capability::Network),
            "an operator-chosen network stance must still be reported"
        );
    }

    /// A report says which posture it describes, and an interactive one shows
    /// the shipped baseline underneath the per-capability lines.
    #[test]
    fn a_rendered_report_names_the_launch_mode_and_the_interactive_baseline() {
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);

        let interactive = evaluate(&policy, &claude, adapters::LaunchMode::Interactive).render();
        assert!(interactive.starts_with("policy on claude (interactive launch):"));
        assert!(interactive.contains("shipped interactive baseline"));
        assert!(interactive.contains("writes outside the repository: ask"));

        let headless = evaluate(&policy, &claude, adapters::LaunchMode::Headless).render();
        assert!(headless.starts_with("policy on claude (headless launch):"));
        assert!(
            !headless.contains("shipped interactive baseline"),
            "a headless report must not advertise the interactive baseline"
        );
    }

    /// The honesty half of the posture split: on an INTERACTIVE claude launch
    /// zirv really does pin a mechanism for an `Ask` stance now
    /// (`--permission-mode default` plus the safety hook as sole gate), so
    /// those cells stop being `OperatorControlled`. Headless is unchanged --
    /// under `dontAsk` a hook `ask` is suppressed, so there is nothing to
    /// claim.
    #[test]
    fn claude_claims_an_ask_mechanism_only_on_an_interactive_launch() {
        let claude = ClaudeAdapter::new(None);
        for capability in [
            Capability::ShellExec,
            Capability::Approval,
            Capability::OutsideRepoFsWrite,
        ] {
            let interactive =
                claude.policy_support(capability, Stance::Ask, adapters::LaunchMode::Interactive);
            assert_eq!(
                interactive.support,
                Support::Degraded,
                "{} must report a real, partial ask mechanism interactively",
                capability.key()
            );
            let headless =
                claude.policy_support(capability, Stance::Ask, adapters::LaunchMode::Headless);
            assert_eq!(
                headless.support,
                Support::OperatorControlled,
                "{} must claim nothing headlessly",
                capability.key()
            );
        }
        // Never `Enforced`: the hook is registered for the Bash tool only.
        assert_ne!(
            claude
                .policy_support(
                    Capability::ToolAccess,
                    Stance::Ask,
                    adapters::LaunchMode::Interactive,
                )
                .support,
            Support::Enforced
        );
    }

    /// Codex's own honest answer for the same question. The mechanism string
    /// must say what codex's approval actually is -- a SANDBOX-boundary
    /// escalation, whose granularity is codex's own -- and must state that
    /// zirv's per-command classification is not carried onto this harness at
    /// all. Anything vaguer reads as parity with claude, which is the
    /// over-claim `policy.rs` exists to prevent.
    #[test]
    fn codex_reports_its_interactive_ask_posture_as_degraded_and_names_the_gap() {
        let codex = CodexAdapter::new(None).with_on_request_approval_forced(true);
        let descriptor = codex.policy_support(
            Capability::Approval,
            Stance::Ask,
            adapters::LaunchMode::Interactive,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert!(descriptor.mechanism.contains("on-request"));
        assert!(
            descriptor.mechanism.contains("sandbox"),
            "the report must say the sandbox is what contains damage: {}",
            descriptor.mechanism
        );
        assert!(
            descriptor.mechanism.contains("per-command"),
            "the report must name what codex cannot match: {}",
            descriptor.mechanism
        );

        let unsure = CodexAdapter::new(None).with_on_request_approval_forced(false);
        assert_eq!(
            unsure
                .policy_support(
                    Capability::Approval,
                    Stance::Ask,
                    adapters::LaunchMode::Interactive,
                )
                .support,
            Support::OperatorControlled,
            "an install that cannot take `on-request` must claim nothing"
        );
    }

    #[test]
    fn stances_are_ordered_least_to_most_restrictive() {
        assert!(Stance::Allow < Stance::Ask);
        assert!(Stance::Ask < Stance::Deny);
        assert_eq!(Stance::default(), Stance::Allow);
    }

    #[test]
    fn fail_closed_denies_every_capability_and_differs_from_default() {
        let closed = EffectivePolicy::fail_closed();
        for capability in Capability::ALL {
            assert_eq!(
                closed.stance(capability),
                Stance::Deny,
                "{} should be denied by the fail-closed fallback",
                capability.key()
            );
        }
        assert_ne!(closed, EffectivePolicy::default());
    }

    /// `network` is the one deliberate exception (see `EffectivePolicy`'s own
    /// doc comment): every other capability's default is `Allow`, "zirv
    /// declares no restriction of its own"; `network`'s default is `None`,
    /// "no operator layer has ever named it" -- distinct from `Some(Deny)`,
    /// which would claim zirv itself denies network on an unconfigured
    /// install.
    #[test]
    fn a_default_policy_declares_no_restriction_at_all_except_network() {
        let policy = EffectivePolicy::default();
        for capability in Capability::ALL {
            if capability == Capability::Network {
                continue;
            }
            assert_eq!(
                policy.stance(capability),
                Stance::Allow,
                "{} should default to allow",
                capability.key()
            );
        }
        assert_eq!(
            policy.network, None,
            "network should default to None -- no operator layer has ever named it, matching \
             what an unwired install has always done without claiming zirv denies it"
        );
    }

    /// The privilege-widening defense, stated directly on the fold: whatever
    /// an untrusted layer says, the result is never looser than the operator's
    /// own stance. Deliberately an explicit all-`Allow` literal, not
    /// `EffectivePolicy::default()`: `default()` is not the uniformly loosest
    /// possible value (`network` defaults to `None`, outside the fold this
    /// test exercises), so it would not represent "an attempt to widen every
    /// capability to `Allow`". `network` is left at its own default (`None`)
    /// on both sides here rather than given a value: it is deliberately
    /// excluded from `narrowed_by`'s generic loop (see that method's own doc
    /// comment) and has its own narrowing tests through `resolve`/
    /// `resolve_network` below, so a value here would only assert that
    /// `narrowed_by` leaves it untouched, not that anything narrows.
    #[test]
    fn narrowing_never_loosens_any_capability() {
        let operator = EffectivePolicy {
            repo_fs_write: Stance::Ask,
            outside_repo_fs_write: Stance::Deny,
            shell_exec: Stance::Deny,
            network: None,
            approval: Stance::Ask,
            git_push_destructive: Stance::Deny,
            tool_access: Stance::Ask,
        };
        let widening_attempt = EffectivePolicy {
            repo_fs_write: Stance::Allow,
            outside_repo_fs_write: Stance::Allow,
            shell_exec: Stance::Allow,
            network: None,
            approval: Stance::Allow,
            git_push_destructive: Stance::Allow,
            tool_access: Stance::Allow,
        };
        assert_eq!(operator.narrowed_by(widening_attempt), operator);
    }

    /// `network` is deliberately left at its own default (`None`) on both
    /// sides: `narrowed_by` excludes it from the generic fold entirely (see
    /// that method's own doc comment), so this test's job is the other six
    /// capabilities. `network`'s own narrowing is exercised through
    /// `resolve`/`resolve_network` in the tests below instead.
    #[test]
    fn narrowing_takes_the_stricter_of_the_two_per_capability() {
        let operator = EffectivePolicy {
            shell_exec: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let repo = EffectivePolicy {
            shell_exec: Stance::Deny,
            repo_fs_write: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let narrowed = operator.narrowed_by(repo);
        assert_eq!(narrowed.shell_exec, Stance::Deny);
        assert_eq!(narrowed.repo_fs_write, Stance::Ask);
    }

    /// SECURITY: the cloned-repository privilege-widening case, exercised
    /// through `resolve` rather than the fold helper -- a repo `[policy]`
    /// table naming the loosest stance for every capability must not move a
    /// single one of the operator's own.
    #[test]
    fn a_repo_policy_table_cannot_widen_any_operator_stance() {
        let home = table(
            "[policy]\nrepo_fs_write = \"ask\"\noutside_repo_fs_write = \"deny\"\nshell_exec = \
             \"deny\"\nnetwork = \"deny\"\napproval = \"ask\"\ngit_push_destructive = \
             \"deny\"\ntool_access = \"ask\"\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let repo = table(
            "[policy]\nrepo_fs_write = \"allow\"\noutside_repo_fs_write = \
             \"allow\"\nshell_exec = \"allow\"\nnetwork = \"allow\"\napproval = \
             \"allow\"\ngit_push_destructive = \"allow\"\ntool_access = \"allow\"\n",
        )
        .and_then(|v| v.get("policy").cloned());

        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.repo_fs_write, Stance::Ask);
        assert_eq!(resolved.outside_repo_fs_write, Stance::Deny);
        assert_eq!(resolved.shell_exec, Stance::Deny);
        assert_eq!(resolved.network, Some(Stance::Deny));
        assert_eq!(resolved.approval, Stance::Ask);
        assert_eq!(resolved.git_push_destructive, Stance::Deny);
        assert_eq!(resolved.tool_access, Stance::Ask);
    }

    /// The other half of "may narrow, never widen": a repo tightening a stance
    /// the operator left loose is honored, because narrowing is always safe.
    /// `network` stays `None` here, unlike every other untouched capability
    /// (`repo_fs_write` etc., implicitly `Allow` via the assertions below) --
    /// but for a different reason than those six: nothing (neither home nor
    /// repo) ever names `network` at all here, which is `resolve_network`'s
    /// own "both layers silent" case, not a per-field default -- see that
    /// function's own doc comment.
    #[test]
    fn a_repo_policy_table_may_tighten_a_stance_the_operator_left_loose() {
        let repo =
            table("[policy]\nshell_exec = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.shell_exec, Stance::Deny);
        assert_eq!(resolved.repo_fs_write, Stance::Allow);
        assert_eq!(resolved.network, None);
    }

    /// Nothing anywhere ever names `network` -- `resolve_network`'s own
    /// "both layers silent" case: `None`, not a stance, since no operator
    /// layer ever expressed an opinion (see `EffectivePolicy`'s own doc
    /// comment for why `None` rather than a defaulted `Deny` matters here).
    #[test]
    fn network_resolves_to_none_when_nothing_is_configured_anywhere() {
        let vars = env_from(&[]);
        let resolved = resolve(None, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, None);
    }

    /// The actual correction this round makes (2026-08-26): home's own
    /// explicit `network = "allow"` must survive a repo whose `[policy]`
    /// table never mentions `network` at all -- the repo's silence carries
    /// no opinion of its own, so it cannot defeat the operator's. Before this
    /// round, a bare `Stance` field made "repo said nothing" and "repo
    /// explicitly denied" indistinguishable, so this used to (wrongly)
    /// resolve to `Deny`.
    #[test]
    fn network_opens_when_home_allows_it_and_the_repo_says_nothing_at_all() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Allow));
    }

    /// A repo may still always narrow: its own explicit `network = "deny"`
    /// defeats home's `allow`, exactly like every other capability.
    #[test]
    fn network_stays_denied_when_the_repo_explicitly_denies_it_even_though_home_allows_it() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let repo = table("[policy]\nnetwork = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Deny));
    }

    /// The bug this round fixes (2026-08-26): a repo's explicit `network =
    /// "ask"` must narrow a looser home stance exactly like `"deny"` does --
    /// the previous `resolve_network` only special-cased `repo == Some(Deny)`
    /// and fell through to home's own value for any other repo stance
    /// (including `Ask`), so this scenario silently resolved to `Allow`,
    /// dropping the repo's narrowing entirely. `max(home, repo)` treats every
    /// repo stance as a potential narrowing input, not just `Deny`.
    #[test]
    fn network_ask_narrows_a_looser_home_allow() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let repo = table("[policy]\nnetwork = \"ask\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(
            resolved.network,
            Some(Stance::Ask),
            "the repo's explicit ask must narrow home's allow, not be dropped in favor of it"
        );
    }

    /// The other half of "a repo may never widen, only narrow": a repo's own
    /// bare `network = "allow"`, with home silent, must NOT grant network on
    /// its own -- only the operator (home or env) can ever move `network`
    /// toward `Allow`.
    #[test]
    fn network_stays_denied_when_only_the_repo_explicitly_allows_it_and_home_says_nothing() {
        let repo = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Deny));
    }

    /// When the repo's own `[policy]` table explicitly agrees (`network =
    /// "allow"`), home's own explicit `allow` is what actually carries --
    /// repo's matching `allow` is a no-op agreement, never a grant of its
    /// own (see the "repo alone" test above, which pins that half).
    #[test]
    fn network_opens_when_home_and_repo_both_explicitly_allow_it() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let repo = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Allow));
    }

    /// The environment override sits above the fold entirely (`resolve`'s own
    /// doc comment), so it is the one path guaranteed to open `network`
    /// regardless of what any repo says -- including a repo that actively
    /// tries to deny it.
    #[test]
    fn env_can_open_network_regardless_of_what_the_repo_says() {
        let repo = table("[policy]\nnetwork = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[("ZIRV_CTX_POLICY_NETWORK", "allow")]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Allow));
    }

    /// The operator's escape hatch above the fold, mirroring
    /// `ZIRV_AGENT_<NAME>_ENABLED`: the environment wins outright, including
    /// in the loosening direction a repo file can never take.
    #[test]
    fn the_environment_sits_above_the_fold_in_both_directions() {
        let repo = table("[policy]\nshell_exec = \"deny\"\nnetwork = \"deny\"\n")
            .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[("ZIRV_CTX_POLICY_SHELL_EXEC", "allow")]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.shell_exec, Stance::Allow);
        assert_eq!(resolved.network, Some(Stance::Deny));
    }

    #[test]
    fn an_unparseable_stance_is_an_error_not_a_silent_allow() {
        let vars = env_from(&[("ZIRV_CTX_POLICY_NETWORK", "maybe")]);
        let err = resolve(None, None, &|k| vars.get(k).cloned()).expect_err("must reject");
        assert!(
            err.to_string().contains("ZIRV_CTX_POLICY_NETWORK"),
            "error should name the variable: {err}"
        );

        let bad_file =
            table("[policy]\nnetwork = \"sometimes\"\n").and_then(|v| v.get("policy").cloned());
        let empty = env_from(&[]);
        let err = resolve(bad_file, None, &|k| empty.get(k).cloned()).expect_err("must reject");
        assert!(
            err.to_string().contains("[policy]"),
            "error should name the section: {err}"
        );
    }

    #[test]
    fn a_typoed_capability_name_is_rejected_rather_than_ignored() {
        let bad = table("[policy]\nshel_exec = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let empty = env_from(&[]);
        assert!(resolve(bad, None, &|k| empty.get(k).cloned()).is_err());
    }

    /// The honesty rule at its most load-bearing: an `Allow` capability is
    /// never reported as something zirv enforces, because zirv is not
    /// restricting anything. `network` is pinned explicitly to `Allow` here
    /// rather than left at `EffectivePolicy::default()`'s own `Deny`: this
    /// test is about the `Allow` stance specifically, on every capability,
    /// not about the default policy.
    #[test]
    fn an_allow_stance_reports_as_operator_controlled_on_every_adapter() {
        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        for adapter in adapters::all(None) {
            let report = evaluate(&policy, adapter.as_ref(), adapters::LaunchMode::Headless);
            for outcome in &report.outcomes {
                assert_eq!(
                    outcome.support,
                    Support::OperatorControlled,
                    "{} / {} should be operator-controlled at allow",
                    report.adapter,
                    outcome.capability.key()
                );
            }
            assert!(report.unenforced().is_empty());
        }
    }

    /// Every registered adapter answers every capability at every restrictive
    /// stance, and a report always covers the whole policy -- no capability
    /// silently omitted because an adapter had nothing to say about it.
    #[test]
    fn every_adapter_answers_every_capability_at_every_restrictive_stance() {
        for stance in [Stance::Ask, Stance::Deny] {
            let mut policy = EffectivePolicy::default();
            for capability in Capability::ALL {
                if capability == Capability::Network {
                    policy.network = Some(stance);
                    continue;
                }
                *policy.stance_mut(capability) = stance;
            }
            for adapter in adapters::all(None) {
                let report = evaluate(&policy, adapter.as_ref(), adapters::LaunchMode::Headless);
                assert_eq!(report.outcomes.len(), Capability::ALL.len());
                for outcome in &report.outcomes {
                    assert_eq!(outcome.stance, stance);
                    assert!(
                        !outcome.mechanism.is_empty(),
                        "{} / {} must name a mechanism or say why there is none",
                        report.adapter,
                        outcome.capability.key()
                    );
                }
            }
        }
    }

    /// The honesty rule as a type-level property: nothing that is only
    /// advisory may render as enforced, and `Unsupported`'s own wording has to
    /// say so out loud.
    #[test]
    fn an_unsupported_capability_never_renders_as_enforcement() {
        assert!(Support::Unsupported.label().contains("not enforced"));
        assert!(Support::Unsupported.label().contains("advisory"));
        assert!(
            CapabilityDescriptor::advisory_only()
                .mechanism
                .contains("not enforcement")
        );
    }

    /// Only `Enforced` means the requested stance itself is fully met, with
    /// nothing left for prompt text or the operator's own settings to carry
    /// -- see `is_fully_enforced`'s own doc for why the looser
    /// `Enforced`-or-`Degraded` question this replaced is gone rather than
    /// kept alongside it.
    #[test]
    fn only_enforced_is_fully_enforced() {
        assert!(Support::Enforced.is_fully_enforced());
        assert!(!Support::Degraded.is_fully_enforced());
        assert!(!Support::Unsupported.is_fully_enforced());
        assert!(!Support::OperatorControlled.is_fully_enforced());
    }

    /// A rendered report names the stance, the honest state and the mechanism
    /// on every line -- the three facts an operator needs to tell a real
    /// guarantee from an instruction.
    #[test]
    fn a_rendered_report_names_stance_state_and_mechanism_per_line() {
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let rendered = evaluate(&policy, &claude, adapters::LaunchMode::Headless).render();
        assert!(rendered.contains("shell execution: deny -- enforced"));
        assert!(rendered.contains("--disallowedTools=Write,Edit,Bash,NotebookEdit"));
    }

    /// Claude's verified mechanisms, from
    /// docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md: the
    /// `--disallowedTools` pin is the one thing zirv can hold a claude launch
    /// to, and it only fully covers repo writes and shell execution -- the
    /// two capabilities the four denied tools (`Write`/`Edit`/`Bash`/
    /// `NotebookEdit`) actually deny outright. Tool access is only
    /// `Degraded` (the pin denies four tools, not every tool -- `Read`,
    /// `Grep`, `WebFetch`, `WebSearch`, `Task` and MCP tools remain), and
    /// approval is `Unsupported` (the pin does not address approvals at
    /// all): claiming either as `Enforced` over-claims what a verified
    /// four-tool pin can back.
    #[test]
    fn claude_enforces_what_its_verified_tool_pin_covers_and_nothing_else() {
        let claude = ClaudeAdapter::new(None);
        let enforced = |capability| {
            claude
                .policy_support(capability, Stance::Deny, adapters::LaunchMode::Headless)
                .support
        };
        assert_eq!(enforced(Capability::RepoFsWrite), Support::Enforced);
        assert_eq!(enforced(Capability::ShellExec), Support::Enforced);
        assert_eq!(enforced(Capability::ToolAccess), Support::Degraded);
        assert_eq!(enforced(Capability::Approval), Support::Unsupported);
        assert_eq!(enforced(Capability::Network), Support::Unsupported);
        assert_eq!(
            enforced(Capability::GitPushDestructive),
            Support::Unsupported
        );
        assert_eq!(
            enforced(Capability::OutsideRepoFsWrite),
            Support::Unsupported
        );
    }

    /// The tool-access `Degraded` mechanism must actually name what remains
    /// available, not just what is denied -- otherwise an operator reads
    /// "degraded" without learning that Read/Grep/WebFetch/WebSearch/Task and
    /// every MCP server's tools are still reachable.
    #[test]
    fn claude_tool_access_degraded_mechanism_names_what_still_runs() {
        let claude = ClaudeAdapter::new(None);
        let descriptor = claude.policy_support(
            Capability::ToolAccess,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert!(descriptor.mechanism.contains("Write"));
        assert!(descriptor.mechanism.contains("MCP"));
    }

    /// Approval has no verified per-run mechanism on claude at all -- the
    /// four-tool pin never addresses approvals, so it must not be reported
    /// as even `Degraded`.
    #[test]
    fn claude_approval_at_deny_is_unsupported_not_enforced() {
        let claude = ClaudeAdapter::new(None);
        let descriptor = claude.policy_support(
            Capability::Approval,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Unsupported);
        assert!(descriptor.mechanism.contains("approval"));
    }

    /// An `Ask` stance is a different question from a `Deny` one: claude's
    /// verified per-run pin can only deny outright, so asking for "ask"
    /// lands on the operator's own settings rather than on a zirv guarantee.
    /// `--permission-mode plan` was probed and does not resolve in headless
    /// `-p` mode, so it is not claimed here.
    #[test]
    fn claude_does_not_claim_to_pin_an_ask_stance() {
        let claude = ClaudeAdapter::new(None);
        for capability in Capability::ALL {
            let descriptor =
                claude.policy_support(capability, Stance::Ask, adapters::LaunchMode::Headless);
            assert!(
                !matches!(descriptor.support, Support::Enforced | Support::Degraded),
                "{} must not claim a per-run ask mechanism, not even a degraded one",
                capability.key()
            );
        }
    }

    /// Codex's descriptors come from the repo's recorded facts
    /// (docs/superpowers/notes/2026-07-31-codex-cli-facts.md), not from a live
    /// CLI -- codex is not runnable on this machine. `--sandbox read-only` is
    /// verified to exist, but the repo's own notes record that it scopes what
    /// an executed shell command may touch rather than which of codex's tools
    /// may run, so every stance it carries is `Degraded`, never `Enforced`.
    #[test]
    fn codex_never_claims_full_enforcement_from_its_sandbox_pin() {
        let codex = CodexAdapter::new(None);
        for capability in Capability::ALL {
            for stance in [Stance::Ask, Stance::Deny] {
                let descriptor =
                    codex.policy_support(capability, stance, adapters::LaunchMode::Headless);
                assert_ne!(
                    descriptor.support,
                    Support::Enforced,
                    "codex must not claim full enforcement for {} at {}",
                    capability.key(),
                    stance.label()
                );
            }
        }
        assert_eq!(
            codex
                .policy_support(
                    Capability::RepoFsWrite,
                    Stance::Deny,
                    adapters::LaunchMode::Headless,
                )
                .support,
            Support::Degraded
        );
        assert!(
            codex
                .policy_support(
                    Capability::RepoFsWrite,
                    Stance::Deny,
                    adapters::LaunchMode::Headless,
                )
                .mechanism
                .contains("--sandbox read-only")
        );
    }

    /// `--sandbox read-only` scopes writes, not execution -- a command still
    /// runs under it and can read anything the process can reach. Shell
    /// execution at `Deny` must therefore be `Unsupported`, not `Degraded`:
    /// reporting `Degraded` would claim the sandbox restricts *something*
    /// about whether commands run, which it does not.
    #[test]
    fn codex_shell_exec_at_deny_is_unsupported_not_degraded() {
        let codex = CodexAdapter::new(None);
        let descriptor = codex.policy_support(
            Capability::ShellExec,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Unsupported);
        assert!(descriptor.mechanism.contains("write"));
    }

    /// Revised 2026-08-22: `-a, --ask-for-approval never` is real and
    /// verified against the installed `codex-cli 0.147.0` (the original
    /// `Unsupported` verdict here predates that finding -- see the
    /// 2026-08-22 addendum to `docs/superpowers/notes/2026-07-31-codex-cli-
    /// facts.md`). Not `Enforced`: in isolation it only suppresses the
    /// escalation prompt, it does not by itself decide what the sandbox
    /// blocks -- see `CodexAdapter::policy_support`'s own doc comment for
    /// why the pairing with `--sandbox read-only` is what actually closes
    /// the loop. `Approval` at `Deny` is therefore `Degraded`, not
    /// `Unsupported` or `Enforced`.
    #[test]
    fn codex_approval_at_deny_is_degraded_not_unsupported_or_enforced() {
        let codex = CodexAdapter::new(None);
        let descriptor = codex.policy_support(
            Capability::Approval,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert!(descriptor.mechanism.contains("ask-for-approval"));
    }

    /// Codex has no verified per-tool deny and no verified network control, so
    /// those stay advisory -- the asymmetry with claude is reported, not
    /// smoothed over.
    #[test]
    fn codex_reports_its_unverified_capabilities_as_advisory_only() {
        let codex = CodexAdapter::new(None);
        for capability in [
            Capability::Network,
            Capability::ToolAccess,
            Capability::GitPushDestructive,
        ] {
            assert_eq!(
                codex
                    .policy_support(capability, Stance::Deny, adapters::LaunchMode::Headless)
                    .support,
                Support::Unsupported,
                "{} should be advisory-only on codex",
                capability.key()
            );
        }
    }

    /// One canonical policy, evaluated against both harnesses, produces two
    /// different honest answers -- issue #43's acceptance criterion, and the
    /// reason the policy is not written per harness in the first place.
    /// Claude fully enforces `repo_fs_write` at `Deny` (its four-tool pin
    /// denies `Write`/`Edit` outright); codex only degrades it (its sandbox
    /// flag scopes writes, it does not deny a tool), so codex's report must
    /// still surface that capability as unenforced even though a real,
    /// verified mechanism is doing something.
    #[test]
    fn one_policy_evaluates_differently_against_claude_and_codex() {
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny,
            // Pinned explicitly to `Allow` (not left at `default()`'s own
            // `Deny`): this test is about `repo_fs_write` alone, and network
            // at its own default would also show up as claude-unenforced
            // (advisory-only), which is not what this test pins.
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let codex = CodexAdapter::new(None);
        let claude_report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let codex_report = evaluate(&policy, &codex, adapters::LaunchMode::Headless);
        assert_ne!(claude_report.outcomes, codex_report.outcomes);
        assert!(claude_report.unenforced().is_empty());
        let codex_unenforced: Vec<_> = codex_report
            .unenforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(codex_unenforced, vec![Capability::RepoFsWrite]);
        let codex_partial: Vec<_> = codex_report
            .partially_enforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(codex_partial, vec![Capability::RepoFsWrite]);
    }

    /// The lines an operator has to read: a stance zirv asked for that only
    /// prompt text or the operator's own harness settings are carrying.
    #[test]
    fn unenforced_lists_exactly_the_stances_zirv_cannot_hold_the_harness_to() {
        let policy = EffectivePolicy {
            network: Some(Stance::Deny),
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let unenforced: Vec<_> = report
            .unenforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(unenforced, vec![Capability::Network]);
    }

    /// `partially_enforced` must isolate exactly the `Degraded` cells, not
    /// every unenforced one: claude's tool-access pin is `Degraded` at
    /// `Deny`, but its approval and network answers are `Unsupported`, which
    /// must not show up here even though both also appear in `unenforced`.
    #[test]
    fn partially_enforced_lists_only_the_degraded_cells() {
        let policy = EffectivePolicy {
            tool_access: Stance::Deny,
            approval: Stance::Deny,
            network: Some(Stance::Deny),
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);

        let partial: Vec<_> = report
            .partially_enforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(partial, vec![Capability::ToolAccess]);

        let unenforced: Vec<_> = report
            .unenforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(
            unenforced,
            vec![
                Capability::Network,
                Capability::Approval,
                Capability::ToolAccess
            ],
            "unenforced must still include the Degraded cell, not just the Unsupported ones"
        );
    }

    /// `Capability::ALL` is hand-maintained, and so is the match below -- so
    /// what does this actually catch?
    ///
    /// The match is exhaustive (no wildcard arm): the moment `Capability`
    /// gains a new variant, this file fails to compile until an arm exists
    /// for it here, one line saying which position that variant belongs at
    /// in `Capability::ALL`. The test then confirms each entry *already in*
    /// `Capability::ALL` sits at the exact position its own arm claims, and
    /// that no two entries claim the same position -- so a variant that got
    /// **duplicated or reordered** relative to `Capability::ALL` (the
    /// realistic way this list actually drifts: copy-pasting an existing arm
    /// instead of adding a fresh one, or an entry moved without updating its
    /// neighbours) is caught.
    ///
    /// What it provably does **not** catch: a variant added to the enum,
    /// given its own honest arm here, but never appended to `Capability::ALL`
    /// at all. This function is only ever called with values already drawn
    /// from `Capability::ALL`'s own contents, so an arm for a variant absent
    /// from that array is simply never exercised -- no test in this file can
    /// call `capability_all_index` with a variant it has no way to name
    /// without already knowing about the very omission it would need to
    /// detect. Closing that gap for real needs either a derive macro (e.g.
    /// `strum::EnumIter`) or nightly's unstable `variant_count`, neither of
    /// which this fix pulls in -- so a brand new variant appended to
    /// `Capability` and never added to `Capability::ALL` still relies on
    /// code review, not this test, to be caught.
    #[test]
    fn capability_all_entries_are_at_their_declared_position_with_no_duplicates() {
        fn capability_all_index(capability: Capability) -> usize {
            match capability {
                Capability::RepoFsWrite => 0,
                Capability::OutsideRepoFsWrite => 1,
                Capability::ShellExec => 2,
                Capability::Network => 3,
                Capability::Approval => 4,
                Capability::GitPushDestructive => 5,
                Capability::ToolAccess => 6,
            }
        }

        let mut claimed_positions = std::collections::HashSet::new();
        for (position, &capability) in Capability::ALL.iter().enumerate() {
            let claimed = capability_all_index(capability);
            assert_eq!(
                claimed, position,
                "{capability:?} is at Capability::ALL[{position}] but claims position {claimed} \
                 -- reordered, or a stale/duplicated entry"
            );
            assert!(
                claimed_positions.insert(claimed),
                "{capability:?}'s position {claimed} is claimed by more than one Capability::ALL \
                 entry"
            );
        }
    }
}
