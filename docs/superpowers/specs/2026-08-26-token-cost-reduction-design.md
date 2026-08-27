# Token-cost reduction across zirv sessions — design

Date: 2026-08-26
Status: approved by operator (issue #155 + its comments), pending spec review
Issue: #155 — "make zirv sessions dramatically cheaper to run without losing
response quality"
Target versions: 2.31.0 → 2.36.0, one minor per phase, one PR per phase

## Problem

A zirv-supervised session spends far more tokens than the work it performs
requires, and today nothing in the codebase can say *where* the spend goes.
The waste is structural, not incidental: it comes from what zirv itself
injects, from what zirv itself instructs a session to do, and from what zirv
itself cannot measure.

Every claim below was read out of the source on 2026-08-26 against `main` at
2.30.1. Line numbers drift; the symbol names do not.

### 1. The composed prompt invalidates its own provider cache

`prompt::compose` (`src/commands/ctx/prompt.rs`, `compose` at ~729) plus
`compile::compile_with_harness_roster`
(`src/commands/ctx/compile.rs`, ~371) produce this layer order, versioned
`v7` (`prompt::DEFAULT_PROMPT_VERSION`):

```
Default → [Orchestrator] Harness → [Orchestrator] Harnesses (roster)
        → Workflow → Memory (core) → User → Repo
        → Memory (retrieval)          ← compile.rs, second with_memory_layer
        → Context (canonical .zirv/context/)
        → Mail → ReportBack → CommandLine
```

The retrieval memory layer is selected from **live working-tree state**:
`compile::changed_repo_paths` shells out to `git diff --name-only --relative
HEAD` and `git ls-files --others --exclude-standard` on every call, and
`compile` is called fresh on every launch, every `exec` nudge relaunch, every
`run_loop` cycle and every dashboard worker spawn. During active editing the
selected entries change between one recompose and the next, so **everything
after that layer** — the canonical context layer (up to 8 KiB on this repo)
and the mail layer — sits behind a changed prefix and cannot be served from a
provider prompt cache.

`ComposedPrompt::describe()` also renders `PromptSource::Memory` **twice**
("…memory+user+repo+memory+canonical context…"), because two independent
`with_memory_layer` calls each push the same variant. Every decision-log line
written by `prompt::log_injection` has carried that duplicate.

### 2. Canonical context is truncated silently, and injected twice

`ContextConfig::max_common_bytes` and `max_harness_bytes` are both `4096`
(`src/commands/ctx/config.rs`, `ContextConfig::default`). `compile::
read_context_layer` truncates to that cap with `crate::utils::truncate_bytes`
and returns a `truncated: bool` that only ever reaches
`ContextProvenance::truncated` — a field `zirv context status` renders and
**nothing else looks at**. There is no warning, no log line, no stderr note.

This repository's own `.zirv/context/common.md` is **4917 bytes**. Every
session zirv has ever launched here has silently lost the last **821 bytes**
of it, cut mid-word — which is the whole `## Git` section (never commit to
`main`, bump `Cargo.toml`, no Co-Authored-By lines) and most of the
doc-update table above it. The single most expensive failure mode of a
budget is one nobody is told about.

Separately, `context_cli::render_generated` writes `<repo>/CLAUDE.md`
(8295 bytes here) and `<repo>/AGENTS.md` (7680 bytes) as the managed marker
plus `common.md` plus the harness file, **verbatim**. Claude Code reads
`CLAUDE.md` natively at session start, with no zirv involvement at all. Then
`compile::with_canonical_context_layer` injects the same canonical bytes
again into the system prompt, unconditionally, with no check that the native
file already carries them. Roughly **8 KiB per session, duplicated**, in the
one layer that would otherwise be perfectly cacheable.

### 3. Three stacked review mandates, each sending the full diff

Three separate places tell a session to run a review round, and none of them
knows about the others:

| Source | Where | What it demands |
| --- | --- | --- |
| zirv meta-harness layer | `prompt::HARNESS_PROMPT` (v9 text) | this harness's native full-diff review **plus** one review worker per other enabled harness via `zirv agent` |
| claude adapter orchestrator layer | `adapters::claude::ORCHESTRATOR_PROMPT` | this harness's own `/code-review` over the full diff, and explicitly: "A session that also carries the zirv meta-harness layer follows that layer's cross-harness review round **on top**." |
| workflow engine | `workflow::review::depth_for_risk` / `required_independent_reviews_for` | Low→0, Medium/High→1, Critical→2 independent reviewers, escalating to 2 on a repeated Major/Critical finding |

`review::package` (`src/commands/workflow/review.rs`) builds one package per
reviewer per round, and every one of them carries `git_diff_capped(&repo,
&base_sha)` — the **full** diff against the workflow's fixed `base_sha`,
capped at `MAX_REVIEW_DIFF_BYTES = 96 KiB` (≈24k tokens). Round 3 of a fix
loop re-sends every byte round 1 already sent. `verification::
change_fingerprint` exists and is computed on every package, but it is used
only as a freshness check (`review_round`, and the "the tree changed under
the reviewer" guard) — never to slice a delta.

`MAX_FIX_REVIEW_ROUNDS = 3` is enforced in code. "Stop as soon as a round
yields no new confirmed findings" is enforced **nowhere**: it exists only as
prose in `HARNESS_PROMPT`.

### 4. Delegation is unmeasurable and unbounded

`event::TranscriptUsage` carries exactly two numbers:

```rust
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
```

`adapters::claude::transcript_usage` folds `input_tokens +
cache_creation_input_tokens + cache_read_input_tokens` into `input_tokens`
via `context_tokens_of` **before anything downstream sees them**. Cache
creation (expensive, written once) and cache read (cheap, the dominant class
in a healthy session) become indistinguishable — so zirv cannot report a
cache-hit ratio, which is the single number that says whether any of the
prompt work in §1–§2 helped.

`workflow::telemetry::TelemetryEvent` carries `input_tokens`,
`output_tokens`, `model`, `adapter`, `role` (a free string), `worker_count`
(a bare integer) and `workflow_id`. It has **no session id, no parent link,
no tool-call count, and no cache categories**. `adapters::claude::
transcript_usage` skips `isSidechain` rows outright, so subagent spend is
dropped from workflow accounting entirely — while `window::sum_transcripts`
walks every transcript under the projects root *including* `subagents/`, so
the same tokens **do** hit the 5h/7d quota estimate. Child cost is charged
and unattributable at the same time.

`window::usage_tokens_of` is a second, differently-composed combine
(`input + cache_creation + output`, cache reads optional) that has never
agreed with `context_tokens_of` by construction.

`log::Decision { ts, session, verb, verdict, score, action, detail }` is a
rotation log. There is no delegation record anywhere.

`zirv ctx usage` is account/provider-scoped only: two percentages and a
pacing verdict. Nothing answers "what did that worker cost".

### 5. The concurrency budget counts the wrong thing

`SuperviseConfig::max_heavy_workers` (default 1, `REPO_FORBIDDEN`, env
`ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS`) exists because of issue #133 — four
kernel bugchecks in 12 minutes from two concurrent cold `cargo build` +
full-nextest workloads. `sessions::count_heavy_workers_among` implements it
as:

```rust
*liveness == Liveness::Live && matches!(record.verb, Verb::Exec | Verb::Dash)
```

That is **workload-blind**. An idle `Verb::Exec` session sitting at a prompt
consumes the whole budget; a `Verb::Chat` orchestrator running a full nextest
sweep consumes none of it. With the budget at its default of 1, a single
parked worker blocks every subsequent delegation — so the orchestrator does
the work itself, on the expensive seat, which is precisely the spend pattern
this issue exists to remove.

`dash::spawnreq::SpawnRequest { agent, prompt, cwd, requested_by, model,
interactive }` carries no role, no parent, no group and no budget, so a
spawned pane is an orphan the moment it starts. `agent::AgentArgs` has
`max_restarts`, `timeout_secs`, `flags` and `quiet` — no token or tool-call
ceiling.

`zirv ctx agent` already does one thing right: `agent::worker_launch_flags`
prepends `adapters::worker_model_args`, so a delegated worker runs on the
configured worker model rather than inheriting the operator's seat. Workers
also already get a reduced prompt — `PromptRole::{Orchestrator, Worker}`,
where a Worker never receives `HARNESS_PROMPT` or the roster, and claude's
`WORKER_PROMPT` (~783 B) replaces `ORCHESTRATOR_PROMPT` (~2886 B).

### 6. Rotation thresholds are absolute, and blind to quota

`ScoreConfig::token_floor = 100_000` / `token_ceiling = 160_000` are absolute
token counts, gating `rot::verdict_for`. No adapter reports a model's context
capacity — `event::Capabilities` has `marker_signal`, `token_usage`,
`turn_signal`, `system_prompt`, `events`, `defer_injection_submit` and
nothing about size. On a 1M-token seat the ceiling fires at 16% of capacity
and restarts a session that had 840k tokens of headroom, throwing away a warm
cache to rebuild one. On a 200k seat the same numbers are roughly right. One
pair of constants cannot serve both.

`rot.rs` and `score.rs` never read `window`/`pace` data at all, so a session
that is 97% through its five-hour window is scheduled exactly like one at 3%.

## Primary acceptance criterion

> Tokens per completed task must fall measurably, with response quality
> unchanged, and every phase must land with a before/after number.

Two consequences bind the whole design:

- **Measurement outranks optimisation.** Phase 2 (raw lineage telemetry) is
  P0 and lands second, before the three phases whose value can only be
  asserted without it. A saving nobody can compute is a claim, not a result.
- **Cheaper never means dumber.** Budgets bound *work* — they checkpoint and
  stop, they never silently downgrade a model. Automatic model downshift is
  explicitly out of scope.

## Approach

Six phases, one PR each, sequential; a later phase may branch off an earlier
one's branch. Each phase is independently revertible and independently
measurable.

| Phase | Version | Theme | Primary lever |
| --- | --- | --- | --- |
| 1 | 2.31.0 | Quick wins | stop truncating silently; one memory layer, at the cache-safe tail |
| 2 | 2.32.0 | Raw lineage telemetry | four token categories, session lineage, delegation records |
| 3 | 2.33.0 | Canonical-context dedupe | skip the injection the harness already read natively |
| 4 | 2.34.0 | Review-train convergence | one gate, delta re-review, real stop rule |
| 5 | 2.35.0 | Work groups, sub-orchestrators, budgets | bounded delegation trees; permits on operations, not sessions |
| 6 | 2.36.0 | Model-aware rotation, quota-aware scheduling | ratios of real capacity; spend pressure gates spawns, never restarts |

## Phase 1 — quick wins (2.31.0)

**(a) Truncation is never silent.** Any canonical context layer cut by its
budget produces both a `log::Decision` with `action = "context-truncated"`
and a `detail` naming the file and the exact lost byte count, and an
`eprintln!` note at compose time. The decision write is gated by an explicit
`log_truncation: bool` parameter on `compile::compile` /
`compile_with_harness_roster`, so the compiler forces every call site to
state whether it is a real launch (yes) or a read-only report
(`context_status`, no — it renders truncation in its own report already, and
a status command must not write decisions).

**(b) This repo's `common.md` fits its budget.** Editorially tightened below
4096 bytes with nothing dropped in meaning — the `## Git` section and the
doc-update table survive, because they were exactly what was being lost.

**(c)+(d) One memory layer, at the tail.** `prompt::compose` stops emitting
a memory layer and drops its `memory`/`memory_cap` parameters.
`compile_with_harness_roster` merges the core and retrieval selections into
one list (core order first, retrieval appended, deduped on `(shared,
key.to_lowercase())`) and injects it through a single
`prompt::with_memory_layer` call **after** the canonical context layer:

```
v8: Default → [Orchestrator] Harness → [Orchestrator] Harnesses
            → Workflow → User → Repo
            → Context (canonical .zirv/context/)
            → Memory (core ∪ retrieved, deduped)      ← single, at the tail
            → Mail → ReportBack → CommandLine
```

The churny, git-derived layer now sits as late as it can while still
preceding mail; everything cacheable precedes it. `describe()` lists
`memory` exactly once. `DEFAULT_PROMPT_VERSION` becomes `"v8"`, so a
decision-log line names the shape it was composed under.

`CompiledContext::core_memory` / `retrieved_memory` keep reporting the two
selections separately — `zirv context status` still shows where entries came
from; only the *injection* is unified.

**Acceptance:** `common.md` under 4096 bytes with zero truncation events on
this repo; `describe()` contains `"memory"` once and starts with `"v8 "`;
`PromptSource::Context` precedes `PromptSource::Memory` in
`compiled.composed.sources`; a truncated layer produces a `context-truncated`
decision naming the file and lost bytes.

## Phase 2 — raw lineage telemetry (2.32.0)

The measurement substrate. Nothing here changes what a session does; it
changes what zirv can say about it.

**Four raw categories, unsummed.**

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl TranscriptUsage {
    /// The pre-2.32.0 combined "real context size" number: `input_tokens`
    /// plus both cache classes. Every caller that genuinely wants one
    /// context-size figure (rot accounting, display) calls this; nothing
    /// pre-sums at the adapter boundary any more.
    pub fn context_total(&self) -> u64 { … }
}
```

`adapters::claude::transcript_usage` stops folding. `context_tokens_of`
survives as a derived helper over a raw `serde_json::Value` usage object,
used where a single combined number is genuinely wanted —
`parse_events`'s `AssistantFinal { input_tokens }` (which feeds rot's context
gate and must keep meaning "real context size") and display. Codex's
cumulative `TokenCount` totals expose no cache classes, so its two new fields
stay `0` and `context_total()` degrades to today's number exactly.

**Lineage on the telemetry event.** `TelemetryEvent` gains, all
`#[serde(default)]` so an event written by 2.31.0 still parses:

```rust
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub sidechain_input_tokens: Option<u64>,
    pub sidechain_cache_creation_input_tokens: Option<u64>,
    pub sidechain_cache_read_input_tokens: Option<u64>,
    pub sidechain_output_tokens: Option<u64>,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub work_group_id: Option<String>,
```

Sidechain rows stop being dropped: the workflow transcript reader counts them
into the `sidechain_*` bucket on the same event instead of discarding them,
so subagent spend is attributable to the phase that caused it — and the
existing main-session numbers keep their exact meaning.

**Per-delegation checkpoints.** When `agent::run_with` completes a headless
worker it appends a `log::Delegation` record (its own `delegations.jsonl`,
same private-append discipline as `log::SafetyDecision`'s daily buckets)
carrying agent, model, the four raw categories, wall-clock milliseconds and
the outcome exit code — plus one `log::Decision` line with `action =
"delegation-complete"` so the main decision log shows it too.

**`zirv ctx usage --sessions`.** A per-session breakdown of the four raw
categories over the last 24 hours, read from the same transcripts
`window::sum_transcripts` already walks. Existing output is unchanged when
the flag is absent.

**Acceptance:** all four categories non-zero on a real cached claude
transcript; a cache-hit ratio computable from a single telemetry event;
`delegations.jsonl` gains one record per completed `zirv ctx agent` run;
`--sessions` names ≥1 session with a nonzero `cache_read_input_tokens` on a
machine that has run a session today.

## Phase 3 — canonical-context dedupe (2.33.0)

`render_generated` embeds a provenance line computed over the exact canonical
inputs it rendered:

```
<!-- zirv:canonical-sha256:<64 hex chars> -->
```

The hash covers the same bytes the render used — the trimmed common text and
the trimmed harness-specific text, domain-separated so no concatenation
collision is possible. It sits immediately after `MANAGED_MARKER`, inside the
managed prefix `is_managed` already recognises, so an older binary reading a
newer file still sees a managed file.

At compose time, `with_canonical_context_layer` resolves the harness-native
target for the adapter (`CLAUDE.md` for claude, `AGENTS.md` for codex). If
that file exists, is managed, and its embedded hash equals the hash of the
current canonical sources, the canonical context layer is **skipped**: the
harness already read those exact bytes natively, and injecting them again
buys nothing. A `log::Decision` with `action = "context-dedup-skip"` records
it, and `ContextProvenance` still reports the surfaces (with
`delivered_bytes: 0`) so `zirv context status` shows what happened rather
than showing nothing.

Any mismatch — no native file, not managed, stale hash, unreadable — falls
back to injecting exactly as today. The dedupe is an optimisation over a
proven-identical byte sequence, never a guess.

Gated by `context.dedupe_native: bool`, default `true`. Unlike the byte-cap
keys beside it this is **not** `REPO_FORBIDDEN`, because a repo layer setting
it to `false` only ever causes *more* context to be injected — narrowing, in
the direction this trust model allows. Setting it `true` from a repo layer is
rejected the same way, by only ever folding a repo `false`.

**Acceptance:** on this repo, with `CLAUDE.md` freshly generated, a claude
launch skips ~8 KiB of canonical injection and logs one
`context-dedup-skip`; editing `.zirv/context/common.md` without regenerating
restores full injection on the very next compose.

## Phase 4 — review-train convergence (2.34.0)

**(a) One gate, named.** `HARNESS_PROMPT` and claude's `ORCHESTRATOR_PROMPT`
both gain explicit guard text: when a `zirv workflow` review gate is active
for the same change, that gate is the single source of truth — do not run an
additional native or cross-harness round on top of it. The claude layer's
current sentence ("follows that layer's cross-harness review round on top")
is the specific text being corrected.

**(b) Delta re-review.** `ReviewRunEvidence` records the HEAD sha the
reviewer actually reviewed. For `review_round > 1`, when the evidence chain
is intact — a previous round's evidence exists, its recorded sha still
resolves in this repository, and the fingerprint chain has not been broken —
`package()` diffs from that sha instead of `base_sha`, and states in the
package that it is a delta (`diff_base_sha`, `diff_is_delta`) alongside the
unresolved findings and the union of touched paths the reviewer needs for
context. Any break in the chain — missing evidence, an unresolvable sha, a
rebase, a fresh workflow — falls back to the full diff against `base_sha`.
A reviewer must never silently receive less than it needs.

**(c) A real stop rule.** The review loop terminates successfully when a
round records zero findings whose `finding_key` is not already present in
`state.review_findings`, regardless of how much of the `MAX_FIX_REVIEW_ROUNDS
= 3` budget remains. Reusing `finding_key` (the existing path:line / normalised
summary identity `has_repeated_meaningful_finding` already uses) means "new"
means the same thing everywhere in this module.

**Acceptance:** a Medium-risk workflow runs exactly one independent reviewer,
not three; round 2 of a fix loop sends a diff strictly smaller than round 1's
on the same change; a round returning only already-recorded findings ends the
loop with a success verdict and no further reviewer launch.

## Phase 5 — work groups, sub-orchestrators, budgets (2.35.0)

The largest phase, and the one that makes delegation cheap enough to prefer.

**(a) `PromptRole::SubOrchestrator`.** A third role between Orchestrator and
Worker: it receives a trimmed coordination layer (it may spawn Workers via
`zirv agent`; it must not spawn further sub-orchestrators), and never the
full `HARNESS_PROMPT` or the roster. The depth cap of 2 is enforced **at
spawn time**, not by prompt text: a spawn request whose parent is already a
SubOrchestrator is refused with a policy refusal, the same class as the pane
cap. Prompt text that asks nicely is not a cap.

**(b) Work groups.** A new persisted concept in ctx state:

```rust
pub struct WorkGroup {
    pub work_group_id: String,
    pub parent_session_id: String,
    pub scope: String,
    pub child_limit: u32,
    pub token_budget: Option<u64>,
    pub deadline_secs: Option<u64>,
    pub completion_contract: String,
    pub created_at: u64,
    pub closed_at: Option<u64>,
}
```

with `zirv ctx group create | status | close`. A group is the unit an
orchestrator reasons about ("this batch of work, this budget, this
contract"), replacing the current unit, which is "one process that happens to
be alive".

**(c) Lineage on the spawn request.** `SpawnRequest` gains `role:
Option<String>`, `parent_session: Option<String>` and `work_group_id:
Option<String>`, all `#[serde(default)]`. The channel is same-binary IPC on
one machine, so skew risk is low and the defaults reproduce today's behaviour
exactly for a request written by an older build.

**(d) Worker budgets that checkpoint, never downgrade.** `zirv ctx agent`
gains `--group`, `--budget-tokens` and `--max-tool-calls`. Supervision polls
the worker's transcript through Phase 2's telemetry: at 80% of a budget it
injects a wrap-up/checkpoint nudge; at 100% it checkpoints and terminates
with a structured result demand. A budget bounds *work*. It never changes
the model — a cheaper answer to the wrong question is not a saving.

**(e) Permits on operations, not sessions.** The session-type heavy count is
replaced by a machine-wide permit held for the **duration of an actual heavy
command**. `script_runner::Command::invoke` — the single seam where a zirv
script runs a shell command — classifies the substituted command against a
built-in pattern set (`cargo build`, `cargo test`, `cargo nextest`, `cargo
clippy`, `cargo package`, `cargo publish`) plus configured patterns, and
holds a permit across the child's lifetime. An idle coordinator consumes
nothing; a busy `Verb::Chat` orchestrator consumes one.

The config key is renamed `supervise.max_heavy_operations` (default 1).
`supervise.max_heavy_workers` is accepted as a **deprecated alias**, resolved
by rewriting the merged TOML table before deserialisation — this is
load-bearing: `CtxConfig`'s structs are `deny_unknown_fields`, an installed
older binary hard-errors on an unknown key, and an operator's existing
`~/.zirv/ctx.toml` must keep working across the upgrade in both directions.

**(f) The tree is visible.** `zirv ctx status` renders the group tree with
per-child raw token spend, sourced from Phase 2's delegation records.

**Acceptance:** an orchestrator can hold three idle delegated workers with
`max_heavy_operations = 1` and none of them blocks a spawn; two concurrent
`cargo nextest` runs cannot both hold a permit; a spawn from a
SubOrchestrator parent is refused; `zirv ctx status` shows a group with
per-child token spend; `max_heavy_workers = 2` in an existing
`~/.zirv/ctx.toml` still parses and still means 2.

## Phase 6 — model-aware rotation and quota-aware scheduling (2.36.0)

**(a) Capacity is a capability.** `Capabilities` gains
`context_window_tokens: Option<u64>` and the adapter trait gains
`fn context_window_tokens(&self, model: Option<&str>) -> Option<u64>`
(default `None`). Claude maps its model ids to capacities with a
conservative default for anything unrecognised; codex reports `None`, because
no capacity is verified for it and a guessed capacity is worse than an
absolute default. `ClaudeAdapter::capabilities()` fills the field from its
own conservative default, so every existing caller of `capabilities()` — and
therefore `rot::score_events` and `RotState::score`, which already receive
`Capabilities` — gets a capacity with **no new plumbing anywhere**.

**(b) Ratios, with absolutes as explicit overrides.** `ScoreConfig` gains
`token_floor_ratio: f64` (0.5), `token_ceiling_ratio: f64` (0.8),
`model_context_tokens: Option<u64>` (operator override), and
`token_floor`/`token_ceiling` become `Option<u64>` in effect — when set they
win outright, when unset the ratios apply against the resolved capacity, and
when no capacity is known at all today's absolute defaults (100_000 /
160_000) apply unchanged. The resolution is a pure function in `rot.rs`:

```rust
pub fn token_gates(cfg: &ScoreConfig, caps: Capabilities) -> (u64, u64)
```

`rot.rs` stays pure: capacity arrives inside `Capabilities`, which is already
an input to every scoring entry point. No fs, clock, env or net is added.

**(c) Quota pressure gates scheduling, never rotation.** A five-hour window
estimate above `pace.spawn_soft_pct` (default 80.0) makes `zirv ctx agent`
and a dashboard spawn print a warning; above `pace.spawn_hard_pct` (default
95.0) the spawn is refused unless `--force` is passed. Rotation is untouched:
zirv must **never** restart a session because it is expensive. A restart
throws away a warm cache and re-reads the whole context — the most expensive
possible response to a cost signal.

**(d) A benchmarks document.** `docs/benchmarks/token-cost.md` defines the
measurement procedure every phase is judged by: tokens per completed task
from the decision log plus the usage estimator, cache-hit ratio from Phase
2's raw categories, and review counts per change. Without a written
procedure, "before/after" is an opinion.

**Acceptance:** a 1M-capacity seat's ceiling resolves to 800_000, not
160_000; an explicit `score.token_ceiling` still wins; a session at 96% of
its five-hour window refuses a spawn without `--force` and never restarts
because of it.

## Configuration and trust layering

Every new key is optional; an operator who writes no config gets the new
defaults. New keys, their tables and their trust posture:

| Key | Default | Repo layer |
| --- | --- | --- |
| `context.dedupe_native` | `true` | may narrow (set `false`) |
| `supervise.max_heavy_operations` | `1` | `REPO_FORBIDDEN` (inherits `max_heavy_workers`'s posture) |
| `supervise.heavy_command_patterns` | built-in set | may only ADD patterns (narrowing) |
| `score.token_floor_ratio` | `0.5` | `REPO_FORBIDDEN` |
| `score.token_ceiling_ratio` | `0.8` | `REPO_FORBIDDEN` |
| `score.model_context_tokens` | `None` | `REPO_FORBIDDEN` |
| `pace.spawn_soft_pct` | `80.0` | `REPO_FORBIDDEN` |
| `pace.spawn_hard_pct` | `95.0` | `REPO_FORBIDDEN` |

Every `REPO_FORBIDDEN` entry needs its row in both hand-maintained trust
tables (`README.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`) —
`config.rs` has a test that enforces exactly that, and a second test that
enforces every key appearing in `.zirv/ctx.toml` active or commented.

The operator's `ctx.model_context_tokens` from the issue comments lands as
**`score.model_context_tokens`**: there is no `[ctx]` table in this config
model, and the key belongs beside `token_floor`/`token_ceiling`, which are
the only things that consume it.

## Measurement

Every phase reports the same three numbers, before and after, per the Phase 6
benchmarks doc:

1. **Tokens per completed task** — the four raw categories summed over the
   session records a task spans, from `delegations.jsonl` and the usage
   estimator.
2. **Cache-hit ratio** — `cache_read_input_tokens / (input_tokens +
   cache_creation_input_tokens + cache_read_input_tokens)`. Phases 1 and 3
   are judged primarily on this.
3. **Review count per change** — independent reviewer launches and total
   review-diff bytes shipped. Phase 4 is judged on this.

Phases 1 and 3 also report a direct byte count: bytes of prompt no longer
injected per session, which is a deterministic number the tests themselves
assert.

## Testing

- Pure classifier and composition tests stay inline in `#[cfg(test)] mod
  tests`, per repo convention; `tests/fixtures/` stays data-only.
- Layer-order tests assert the *relationship* (`Context` before `Memory`,
  `memory` appearing once), never a hardcoded full layer list, so a future
  layer does not break them spuriously.
- Serde back-compatibility tests: a `TelemetryEvent` JSON without the new
  fields must still deserialise; a `SpawnRequest` JSON without `role`/
  `parent_session`/`work_group_id` must still deserialise.
- Adapter tests never assert an exact argv that depends on an
  installed-binary probe — assert the invariant.
- Full gates on every phase: `cargo build`, `cargo nextest run
  --no-fail-fast`, `cargo test --verbose -- --test-threads=1`, `cargo fmt --
  --check`, `cargo clippy --all-targets -- -D warnings`.
- Windows baseline: 7 `commands::ctx::wrap::tests` failures pre-exist on
  `main` on the dev machine. Judge by the sorted failure-NAME diff against
  `main`, never the count, and confirm a `test result:` line exists before
  trusting any failure list (a `STATUS_ACCESS_VIOLATION` crash prints
  neither).
- Any phase touching `wrap`, `announce`, `pace` or adapter argv needs a
  Linux/Docker verification pass (Phases 5 and 6): export with `git -c
  core.autocrlf=false archive HEAD`, run on `rust:1-bookworm` as a non-root
  user, `cargo test --bin zirv wrap:: -- --test-threads=1` plus `cargo clippy
  --all-targets -- -D warnings`.

## Rollout

- One PR per phase, sequential, each raising `Cargo.toml` above its base
  (2.31.0 → 2.36.0) or CD fails on a duplicate release.
- Never commit or push to `main`/`master`; branch first, open a PR, no
  "Co-Authored-By" and no "Generated with Claude Code" lines.
- Install the new binary **before** adding config that uses new keys — an
  older installed `zirv.exe` hard-fails on an unknown `.zirv` settings key.
  This is why Phase 5's `max_heavy_workers` alias must parse rather than
  merely be documented.
- Vault updates per the CLAUDE.md doc-update table: `Modules/Ctx
  Subsystem.md`, `Modules/Ctx Adapters.md`, `Modules/Rot Engine.md`,
  `Modules/Ctx Supervisors.md`, `Modules/Usage and Pacing.md`, `Modules/
  Built-in Commands.md`, `Concepts/Untrusted Configuration.md`, and
  `Development/{Decision Log,Work Journal,Active Work}.md`.

## Out of scope

- **Automatic model downshift.** Budgets bound work and stop; they never
  quietly answer a hard question with a cheap model. Explicit operator
  decision.
- **Restarting or compacting a session because of cost.** Rotation stays
  driven by rot signals only. A cost-driven restart is the most expensive
  possible reaction to a cost signal.
- **Per-launch model plumbing for rotation capacity** (Phase 6). The adapter's
  conservative per-model default plus `score.model_context_tokens` covers the
  operator case; threading a live model string from every launch seam into
  `IncrementalScorer::poll` is a separate refactor with no measured payoff
  yet.
- **Cross-process atomicity of the permit gate.** Phase 5's permits inherit
  the documented count-then-register TOCTOU of today's heavy-worker gate: the
  budget exists to keep concurrency low, not to enforce an exact ceiling, and
  closing the race needs a cross-process lock this registry has never had.
- **Sessions launched outside zirv.** zirv governs what it launches.
- **Changing what a harness charges.** Everything here is about what zirv
  sends and how often, never about pricing.
