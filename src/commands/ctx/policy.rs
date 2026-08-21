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
/// `deny_unknown_fields`: a typo'd capability name hard-errors rather than
/// silently leaving that capability at `Allow`, which is the failure mode a
/// permissions surface can least afford.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectivePolicy {
    pub repo_fs_write: Stance,
    pub outside_repo_fs_write: Stance,
    pub shell_exec: Stance,
    pub network: Stance,
    pub approval: Stance,
    pub git_push_destructive: Stance,
    pub tool_access: Stance,
}

impl EffectivePolicy {
    pub fn stance(&self, capability: Capability) -> Stance {
        match capability {
            Capability::RepoFsWrite => self.repo_fs_write,
            Capability::OutsideRepoFsWrite => self.outside_repo_fs_write,
            Capability::ShellExec => self.shell_exec,
            Capability::Network => self.network,
            Capability::Approval => self.approval,
            Capability::GitPushDestructive => self.git_push_destructive,
            Capability::ToolAccess => self.tool_access,
        }
    }

    fn stance_mut(&mut self, capability: Capability) -> &mut Stance {
        match capability {
            Capability::RepoFsWrite => &mut self.repo_fs_write,
            Capability::OutsideRepoFsWrite => &mut self.outside_repo_fs_write,
            Capability::ShellExec => &mut self.shell_exec,
            Capability::Network => &mut self.network,
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
    pub fn narrowed_by(self, narrower: EffectivePolicy) -> EffectivePolicy {
        let mut out = self;
        for capability in Capability::ALL {
            let stance = self.stance(capability).max(narrower.stance(capability));
            *out.stance_mut(capability) = stance;
        }
        out
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
pub fn resolve(
    home: Option<toml::Value>,
    repo: Option<toml::Value>,
    env: EnvLookup<'_>,
) -> CtxResult<EffectivePolicy> {
    let mut resolved = parse_layer(home, "~/.zirv/ctx.toml")?
        .narrowed_by(parse_layer(repo, "<repo>/.zirv/ctx.toml")?);

    for capability in Capability::ALL {
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

    /// Whether zirv itself is holding this capability to *something*, even if
    /// only approximately (`Degraded`). False for everything an operator or a
    /// prompt is carrying instead. See [`is_fully_enforced`](Self::is_fully_enforced)
    /// for the stricter question of whether the requested stance itself is met.
    pub fn is_enforced_by_zirv(self) -> bool {
        matches!(self, Support::Enforced | Support::Degraded)
    }

    /// Whether the harness enforces *exactly* the requested stance, with
    /// nothing left for prompt text or the operator's own settings to carry
    /// instead. `Degraded` deliberately answers `false` here: a mechanism
    /// that only approximates the request is real, but it is not the same
    /// guarantee as `Enforced`, and a report must never treat the two alike.
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

/// What one policy actually means on one harness. Built only by [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReport {
    pub adapter: &'static str,
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
        let mut out = format!("policy on {}:\n", self.adapter);
        for outcome in &self.outcomes {
            out.push_str(&format!(
                "  {}: {} -- {} ({})\n",
                outcome.capability.label(),
                outcome.stance.label(),
                outcome.support.label(),
                outcome.mechanism
            ));
        }
        out
    }
}

/// Translates one canonical policy onto one adapter. Pure: it asks the adapter
/// for descriptors and combines them, and neither side reads a clock, the
/// filesystem or the environment.
///
/// A [`Stance::Allow`] capability is answered here rather than by the adapter:
/// zirv is imposing nothing, so there is no mechanism to name and nothing an
/// adapter could usefully say. That is also why every adapter's own
/// `policy_support` may leave `Allow` to a catch-all arm -- it is never asked.
pub fn evaluate(policy: &EffectivePolicy, adapter: &dyn AgentAdapter) -> PolicyReport {
    let outcomes = Capability::ALL
        .into_iter()
        .map(|capability| {
            let stance = policy.stance(capability);
            let descriptor = match stance {
                Stance::Allow => CapabilityDescriptor::operator_controlled(
                    "zirv declares no restriction; the harness's own defaults and the operator's \
                     own settings decide",
                ),
                _ => adapter.policy_support(capability, stance),
            };
            CapabilityOutcome {
                capability,
                stance,
                support: descriptor.support,
                mechanism: descriptor.mechanism,
            }
        })
        .collect();
    PolicyReport {
        adapter: adapter.name(),
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

    #[test]
    fn stances_are_ordered_least_to_most_restrictive() {
        assert!(Stance::Allow < Stance::Ask);
        assert!(Stance::Ask < Stance::Deny);
        assert_eq!(Stance::default(), Stance::Allow);
    }

    #[test]
    fn a_default_policy_declares_no_restriction_at_all() {
        let policy = EffectivePolicy::default();
        for capability in Capability::ALL {
            assert_eq!(
                policy.stance(capability),
                Stance::Allow,
                "{} should default to allow",
                capability.key()
            );
        }
    }

    /// The privilege-widening defense, stated directly on the fold: whatever
    /// an untrusted layer says, the result is never looser than the operator's
    /// own stance.
    #[test]
    fn narrowing_never_loosens_any_capability() {
        let operator = EffectivePolicy {
            repo_fs_write: Stance::Ask,
            outside_repo_fs_write: Stance::Deny,
            shell_exec: Stance::Deny,
            network: Stance::Ask,
            approval: Stance::Ask,
            git_push_destructive: Stance::Deny,
            tool_access: Stance::Ask,
        };
        let widening_attempt = EffectivePolicy::default();
        assert_eq!(operator.narrowed_by(widening_attempt), operator);
    }

    #[test]
    fn narrowing_takes_the_stricter_of_the_two_per_capability() {
        let operator = EffectivePolicy {
            shell_exec: Stance::Ask,
            network: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let repo = EffectivePolicy {
            shell_exec: Stance::Deny,
            network: Stance::Allow,
            repo_fs_write: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let narrowed = operator.narrowed_by(repo);
        assert_eq!(narrowed.shell_exec, Stance::Deny);
        assert_eq!(narrowed.network, Stance::Deny);
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
        assert_eq!(resolved.network, Stance::Deny);
        assert_eq!(resolved.approval, Stance::Ask);
        assert_eq!(resolved.git_push_destructive, Stance::Deny);
        assert_eq!(resolved.tool_access, Stance::Ask);
    }

    /// The other half of "may narrow, never widen": a repo tightening a stance
    /// the operator left loose is honored, because narrowing is always safe.
    #[test]
    fn a_repo_policy_table_may_tighten_a_stance_the_operator_left_loose() {
        let repo =
            table("[policy]\nshell_exec = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.shell_exec, Stance::Deny);
        assert_eq!(resolved.network, Stance::Allow);
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
        assert_eq!(resolved.network, Stance::Deny);
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
    /// restricting anything.
    #[test]
    fn an_allow_stance_reports_as_operator_controlled_on_every_adapter() {
        let policy = EffectivePolicy::default();
        for adapter in adapters::all(None) {
            let report = evaluate(&policy, adapter.as_ref());
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
                *policy.stance_mut(capability) = stance;
            }
            for adapter in adapters::all(None) {
                let report = evaluate(&policy, adapter.as_ref());
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
        assert!(!Support::Unsupported.is_enforced_by_zirv());
        assert!(!Support::OperatorControlled.is_enforced_by_zirv());
        assert!(Support::Enforced.is_enforced_by_zirv());
        assert!(Support::Degraded.is_enforced_by_zirv());
        assert!(Support::Unsupported.label().contains("not enforced"));
        assert!(Support::Unsupported.label().contains("advisory"));
        assert!(
            CapabilityDescriptor::advisory_only()
                .mechanism
                .contains("not enforcement")
        );
    }

    /// `is_fully_enforced` is the stricter question `is_enforced_by_zirv`
    /// does not answer: only `Enforced` means the requested stance itself is
    /// met with nothing left for prompt text or the operator's own settings
    /// to carry. `Degraded` answers `true` to the looser question but `false`
    /// here -- that gap is exactly what `PolicyReport::unenforced` now keys
    /// off of, instead of the looser predicate that used to hide `Degraded`
    /// cells from a report.
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
        let rendered = evaluate(&policy, &claude).render();
        assert!(rendered.starts_with("policy on claude:"));
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
        let enforced = |capability| claude.policy_support(capability, Stance::Deny).support;
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
        let descriptor = claude.policy_support(Capability::ToolAccess, Stance::Deny);
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
        let descriptor = claude.policy_support(Capability::Approval, Stance::Deny);
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
            let descriptor = claude.policy_support(capability, Stance::Ask);
            assert!(
                !descriptor.support.is_enforced_by_zirv(),
                "{} must not claim a per-run ask mechanism",
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
                let descriptor = codex.policy_support(capability, stance);
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
                .policy_support(Capability::RepoFsWrite, Stance::Deny)
                .support,
            Support::Degraded
        );
        assert!(
            codex
                .policy_support(Capability::RepoFsWrite, Stance::Deny)
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
        let descriptor = codex.policy_support(Capability::ShellExec, Stance::Deny);
        assert_eq!(descriptor.support, Support::Unsupported);
        assert!(descriptor.mechanism.contains("write"));
    }

    /// The sandbox flag is not codex's approval mechanism at all -- that is
    /// codex's own `approval` setting in `~/.codex/config.toml`, which zirv
    /// reads but never rewrites, and which has no verified `Deny`-shaped
    /// flag. `Approval` at `Deny` must be `Unsupported`, not `Degraded`.
    #[test]
    fn codex_approval_at_deny_is_unsupported_not_degraded() {
        let codex = CodexAdapter::new(None);
        let descriptor = codex.policy_support(Capability::Approval, Stance::Deny);
        assert_eq!(descriptor.support, Support::Unsupported);
        assert!(descriptor.mechanism.contains("approval mechanism"));
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
                codex.policy_support(capability, Stance::Deny).support,
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
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let codex = CodexAdapter::new(None);
        let claude_report = evaluate(&policy, &claude);
        let codex_report = evaluate(&policy, &codex);
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
            network: Stance::Deny,
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude);
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
            network: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude);

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

    /// `Capability::ALL` is hand-maintained, so a new `Capability` variant
    /// can be added without anyone remembering to list it there too. The
    /// match below is exhaustive (no wildcard arm): adding a variant fails
    /// this file to compile until an arm exists for it here, and the
    /// `assert_eq!` then catches a variant that was added to the match but
    /// never added to `Capability::ALL` itself.
    #[test]
    fn capability_all_lists_every_capability_variant() {
        fn is_a_known_capability(capability: Capability) {
            match capability {
                Capability::RepoFsWrite
                | Capability::OutsideRepoFsWrite
                | Capability::ShellExec
                | Capability::Network
                | Capability::Approval
                | Capability::GitPushDestructive
                | Capability::ToolAccess => {}
            }
        }
        for capability in Capability::ALL {
            is_a_known_capability(capability);
        }
        assert_eq!(
            Capability::ALL.len(),
            7,
            "a Capability variant exists that the exhaustive match above already covers but \
             Capability::ALL does not list -- add it there, then bump this count"
        );
    }
}
