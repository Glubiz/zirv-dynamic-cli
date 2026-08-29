# AI-native SDLC flows — design

- **Date:** 2026-08-28
- **Issue:** [#187](https://github.com/Glubiz/zirv-dynamic-cli/issues/187)
- **Status:** Approved design; implementation phased into five sub-projects (see Phasing)

## Summary

Align `zirv workflow` with the Anthropic AI-native SDLC playbook (Plan → Design → Build →
Test → Deploy → Maintain) so that a harness wrapped in zirv needs **no harness-side process
plugin**: intent capture, committed artifact chain, the full process-skill set, deploy-tier
gating, a maintenance feedback loop, and a provider-neutral agent registry are all driven by
zirv itself, identically for every adapter.

One engine, extended in place. The existing kinds (feature/bugfix/refactor/spike/review),
`StepCondition` gating, re-classification, review machinery, capability model, and telemetry
store are the substrate; nothing here introduces a parallel engine or store.

**Design principle (explicit requirement): the pipeline adapts to task size and complexity.**
A trivial bugfix must not be forced through intent/spec/plan ceremony; a substantial,
high-risk feature must not be able to skip it. Classification — not the user's patience —
decides which stages exist, via the `StepCondition` mechanism the engine already has.

## Current state (what this builds on)

- **Engine** (`src/commands/workflow/engine.rs`): `WorkflowDefinition` per kind;
  `WorkflowStep {id, phase, skill, condition, approval, max_attempts}`;
  `WorkflowPhase {Design, Plan, Implement, Debug, Test, Review, Verify, Delegate, Present}`;
  durable `WorkflowState` (private JSON, `WORKFLOW_SCHEMA_VERSION = 1`, hard-error on
  mismatch); `advance_with_evidence` enforces per-phase evidence; `reclassify_at_gate`
  re-measures risk before Review/Verify and may insert steps, never remove completed ones.
- **Skills** (`skill.rs`): 18 provider-neutral built-ins; layering built-in → operator
  (`~/.zirv/skills/`, may replace) → repo (`.zirv/skills/`, may only add non-colliding ids,
  collisions dropped with warnings); 32 KiB resolved-context cap.
- **Review** (`review.rs`): compact secret-redacted packages, isolated read-only reviewer
  subprocess, findings with dispositions, risk-mapped review depth.
- **Telemetry** (`telemetry.rs`): one-file-per-event JSON store, schema v2, aggregated at
  read time by `zirv workflow stats`.
- **Capabilities** (`capability.rs`): nine ids incl. `git.worktree` (currently skill-less)
  and `agent.spawn`; policy narrows support, never widens.
- **Trust model**: repo-owned surfaces are untrusted and may only narrow
  (`REPO_FORBIDDEN` in `ctx/config.rs`; asymmetric `max(home, repo)` fold in `policy.rs`).
- **Issue plumbing**: `zirv report` (#176) files issues against a hardcoded destination;
  `zirv ctx permissions propose` (#178) has the dedupe/comment patterns.

## Goals

1. Committed artifact chain (`intent.md` → `spec.md` → `plan.md`) with acceptance gates.
2. Intent-capture stage closing the superpowers `brainstorming` gap.
3. Full superpowers-parity built-in skill set; no harness-side process plugin required.
4. Deploy-tier gating expressible only by the operator; repo layers can only tighten.
5. Maintain loop: deterministic detection files an intent + issue and opens a new cycle.
6. Workflow steps addressable by agent role, honored identically by claude and codex.
7. Provider-neutral agent registry with skill-registry layering; `.claude/agents/`
   definitions migrated.

## Non-goals

- No second workflow engine or parallel telemetry store (explicit issue constraint).
- No deployment *execution* (no CD, no environment provisioning): zirv gates the flow
  around the PR lifecycle; running deploys stays with the repo's own tooling.
- No resident daemon or scheduler: maintenance scans are invoked (by the operator, a cron
  entry, CI, or a ctx hook), never self-hosted.
- No harness-specific text in any skill or agent instruction body (existing constraint,
  enforced by test).
- No LLM judgment in maintenance detection: detectors are deterministic commands and
  thresholds; the LLM enters only after a human triages the filed intent.

## 1. Artifact chain

### Location and trust

Artifacts live in the repository at `.zirv/work/<workflow-id>/`:

```
.zirv/work/<workflow-id>/
  intent.md      # Plan stage output
  spec.md        # Design stage output
  plan.md        # Plan(ning) stage output
  notes/         # optional free-form (spike findings, incident timelines)
```

`<workflow-id>` is the engine's existing workflow id. The directory is created by
`zirv workflow start` and committed by the normal course of work — the audit trail lives in
git history, satisfying the playbook's "committed artifacts" requirement.

Trust: `.zirv/work/` is a **work-product surface, not a config surface**. The engine never
parses it as configuration; script lookup ignores it (it is a subdirectory, and the script
runner only reads top-level `.zirv/*` script files). When artifact content is folded into
prompts (`zirv workflow context`), it enters as capped, labeled untrusted repository text —
the same treatment every other repo-owned surface already gets. No new `REPO_FORBIDDEN`
concern arises because artifacts carry no authority: gates key off *acceptance records in
private state*, never off claims inside the files.

There is no configurable artifact directory in v1 of this design. A fixed location keeps
path validation trivial (no traversal surface) and the audit trail predictable across
repos. If a real need appears, an operator-only key can be added later under the existing
`REPO_FORBIDDEN` discipline.

### State model

`WorkflowState` gains (schema bump to `WORKFLOW_SCHEMA_VERSION = 2`, keeping the existing
hard-error-on-mismatch behavior — operators finish running workflows before upgrading):

```rust
pub struct ArtifactRecord {
    pub stage: ArtifactStage,        // Intent | Spec | Plan
    pub rel_path: String,            // ".zirv/work/<id>/intent.md"
    pub accepted_hash: Option<String>, // content digest pinned at acceptance
    pub accepted_at: Option<String>, // RFC3339
}
// WorkflowState { ..., artifacts: BTreeMap<String, ArtifactRecord>, ... }
```

The digest uses whatever hash is already in the dependency tree (no new crypto
dependency for a tamper-*evidence* — not tamper-*proofing* — mechanism).

### Templates

`zirv workflow start` (and each artifact step's activation) writes a template if the file
does not exist:

- **`intent.md`** — Problem, Desired outcome, Constraints, Open questions, Acceptance
  criteria. A variant template for maintenance-filed intents adds Incident summary,
  Detection source, Evidence.
- **`spec.md`** — Context, Goals/Non-goals, Design (sectioned), Testing strategy, Risks.
- **`plan.md`** — Ordered tasks, each with: files touched, verification command, status
  checkbox — plus an **execution ledger** section (task id → started/finished/evidence)
  that makes execution resume-safe across sessions.

Templates are code (Rust string constants beside the engine), not repo files: repo layers
must not be able to alter what an "accepted intent" looks like.

### Acceptance gates

Artifact steps are `approval: true` steps. The gate sequence:

1. Step activates → template written if absent → status `AwaitingApproval`.
2. Human (or an operator-configured auto-accept for low tiers — see Deploy) reviews the
   file, then runs `zirv workflow approve <id>`.
3. Approve validates the file exists and differs from the bare template, pins
   `accepted_hash` + `accepted_at`, and advances.
4. Every later `advance_with_evidence` re-checks the pinned hash of all accepted
   artifacts. Drift (file edited after acceptance) re-opens `AwaitingApproval` on the
   owning step: edits are allowed, but they re-enter the gate. Missing file = hard block.

This makes "nothing is implemented without an accepted plan" a structural property with
the same shape as the existing review-disposition and verification-freshness gates.

### CLI surface

- `zirv workflow artifacts <id>` — list artifact records, paths, acceptance state, drift.
- `zirv workflow approve <id>` — unchanged verb; now also performs artifact pinning when
  the pending step is an artifact step.
- `zirv workflow context` — folds accepted artifacts (capped) into the generated context.

## 2. Intent stage (Plan, in playbook terms)

New `WorkflowPhase::Intent`, ordered before `Design`. A new built-in skill `brainstorm`
(§3) drives it: turn a raw task statement into a filled `intent.md` — problem, outcome,
constraints, open questions — then stop for the human acceptance gate.

`zirv workflow start` behavior: classification runs first (as today); when the resulting
step list includes an Intent step, the work dir and `intent.md` template are created
immediately so the very first thing the cycle produces is the intent draft.

## 3. Adaptive pipeline — which stages a task gets

No new mechanism: artifact steps carry the existing `StepCondition`s. The defaults
(`definitions()`):

| Kind | Trivial | Moderate | Substantial / high-risk |
|---|---|---|---|
| Feature | intent | intent + plan | intent + spec + plan |
| Bugfix | — (debug ledger only) | intent | intent + plan |
| Refactor | — | plan | intent + plan |
| Spike | intent | intent | intent (+ findings note; output is an answer, not kept code) |
| Review | — | — | — |

- Bugfix keeps `systematic-debugging` as its spine; the artifact chain only attaches when
  complexity/risk warrants it.
- `reclassify_at_gate` already inserts steps when risk grows mid-run; artifact steps join
  that set, so a task that turns out bigger than classified **upgrades its chain** — the
  one-way ratchet. Nothing ever removes an accepted artifact from the record.
- Approval steps remain skippable only by conditions, never by flags: there is
  deliberately no `--no-artifacts` escape hatch. The escape hatch *is* classification.

## 4. Superpowers-parity skills

Five new built-ins (18 → 23), provider-neutral like the rest, `deny_unknown_fields`,
counted in the existing built-in validity test:

| id | phase | closes gap | notes |
|---|---|---|---|
| `brainstorm` | Intent | superpowers `brainstorming` | classify ceremony to task size; one question at a time; output = `intent.md`; hard acceptance gate |
| `write-plan` | Plan | `writing-plans` | produces `plan.md` in the template format: bite-sized tasks, exact files, verification per task |
| `execute-plan` | Implement | `executing-plans` | consumes accepted `plan.md`; maintains the execution ledger; per-task verify-then-tick; resume-safe |
| `worktree` | Implement | `using-git-worktrees` | isolation guidance; requires the existing `git.worktree` capability (finally giving it a skill) |
| `finish-branch` | Deploy | `finishing-a-development-branch` | verify clean → branch → PR → merge-decision flow; consumes deploy-tier gate state (§6) |

Audit of existing built-ins against superpowers counterparts (`delegate`/`parallelize` ↔
subagent-driven-development, `tdd`, `systematic-debugging`, `review` ↔
requesting/receiving-code-review, `verify` ↔ verification-before-completion): folded into
the same phase as an in-place text revision where the playbook adds substance — no new
ids, no duplicates.

Skill/definition wiring: Feature gains `intent` (brainstorm) as step 1; `plan` steps
reference `write-plan` guidance via the existing dependency mechanism; `implement` gains
`execute-plan` + `worktree` as conditional dependencies at substantial+ complexity.

## 5. Agent registry

### Manifest

New module `src/commands/workflow/agents.rs`, mirroring `skill.rs` deliberately:

```rust
pub struct AgentManifest {
    pub schema_version: u32,          // AGENT_SCHEMA_VERSION = 1
    pub id: String,                   // "reviewer"
    pub version: String,
    pub name: String,
    pub description: String,
    pub role: String,                 // playbook role label: "engineer", "tech-lead", ...
    pub instructions: String,         // provider-neutral body
    pub required_capabilities: Vec<CapabilityId>,
    pub optional_capabilities: Vec<CapabilityId>,
    pub model_tier: ModelTier,        // Fast | Standard | Deep — a hint, never a binding
    pub read_only: bool,
    pub context_budget_bytes: usize,
}
```

`#[serde(deny_unknown_fields)]`, same size caps discipline as skills.

### Layering (identical to skills, by design)

1. Built-ins (code): `implementer` (engineer), `reviewer` (tech lead, `read_only`),
   `doc-keeper`, `security-scanner` (security lead, `read_only`), `explorer` (`read_only`).
2. Operator `~/.zirv/agents/*.{yaml,yml,toml}` may **replace** built-in ids (trusted).
3. Repo `.zirv/agents/*` may only **add** non-colliding ids; collisions dropped into
   `warnings()`. Gated by a new operator-only `workflow.repo_agents_enabled`
   (`REPO_FORBIDDEN`, default off, fail-closed like `repo_skills_enabled`).

A repo-added agent can never exceed the session's effective policy: dispatch runs the
manifest's capabilities through `CapabilityReport::with_policy`, so the narrowing fold
applies to agents exactly as it does to skills. `read_only: true` is a floor the dispatch
enforces via the adapter's `read_only_args()` — a repo manifest cannot un-set another
manifest's read-only (ids can't collide) and gains nothing by declaring itself writable
that policy doesn't already allow.

### Dispatch

`AgentAdapter` gains one method with a default implementation composed from the existing
surface (`headless_cmd`, `read_only_args`, system-prompt layers):

```rust
fn dispatch_agent(&self, manifest: &AgentManifest, task: &AgentTask) -> Result<Command>
```

- claude: headless invocation with the manifest instructions as the worker system-prompt
  layer.
- codex: worker argv with the equivalent flags.

**Amendment, reviewed design decision (2026-08-29):** the implementation deliberately does
not map `ModelTier` to a concrete model id in either adapter. `model_tier` is passed through
as a routing hint only — each adapter (and, through it, the operator's own config) decides
what concrete model that hint resolves to, unless an operator/caller explicitly supplies a
model id, in which case the explicit pin always wins. This section originally described
`dispatch_agent` guessing a provider-specific model per tier (Fast→haiku-class,
Standard→sonnet-class, Deep→opus-class for claude; an equivalent codex mapping); that was
never built, on purpose. Hardcoding vendor model names in this provider-neutral layer would
rot the moment a vendor renames or retires a model class, and — worse — codex has no
haiku/sonnet/opus ladder to map onto at all, so the same hardcoded table could never hold
for both adapters without silently favoring one of them. See `AgentAdapter::dispatch_agent`'s
own doc comment (`src/commands/ctx/adapters/mod.rs`) for the implemented contract.

`WorkflowStep` gains `agent: Option<String>` (manifest id). The review machinery's
independent-reviewer seat becomes `agent: Some("reviewer")` instead of ad-hoc argv — same
subprocess isolation, now defined data. Session-registry role addressing from #177
(`zirv ctx send --to-role`) is unchanged; manifest `role` labels align with those role
strings so a step can require "an independent tech-lead seat" and mail can reach it.

### Migration

`.claude/agents/vault-keeper.md` → `.zirv/agents/vault-keeper.yaml` (repo layer, additive
id — exactly what the repo layer is for). The doc-update contract text becomes the
manifest's instruction body; the Claude-specific file is deleted once parity is verified.

## 6. Deploy stage

Two new `WorkflowPhase` variants carry the playbook's outer stages: `Intent` (§2) and
`Deploy` (this section). Maintain is not a phase — it is the verb that opens new cycles
(§7).

### Environment tiers

```
Development < Staging < Production      (ordering = strictness)
```

- Operator config (`~/.zirv/ctx.toml`): `[workflow.deploy] tier = "development"` plus
  optional per-repo overrides. The whole `[workflow]` table is already `REPO_FORBIDDEN`.
- Repo may declare a **minimum** tier for itself. Because repo layers cannot touch
  `[workflow]`, this rides the same asymmetric fold as `policy.rs` stances:
  `effective = env/flag if set else max(operator, repo_min)` with the strictness ordering
  above — a repo can force *more* gating (its right), never less. This is the one narrow
  carve-out from the all-or-nothing `[workflow]` ban, implemented as its own explicitly
  folded key, not by un-forbidding the table.

### Tier semantics on Deploy-phase steps

| Tier | Deploy steps (`finish-branch`, PR package/review) |
|---|---|
| Development | auto-advance; evidence gates still apply |
| Staging | `approval: true` on PR-creating/merging steps |
| Production | approval **and** an independent `reviewer`-seat run **and** fresh `zirv verify` evidence, non-negotiable |

Tier is evaluated at step activation *and* re-checked by `reclassify_at_gate` — config
tightened mid-run tightens the run.

### PR lifecycle

Extends the existing review machinery rather than adding new machinery:

- `zirv workflow review package --pr <n>`: build a `ReviewPackage` from an incoming PR's
  diff (same caps, same secret redaction) so the `reviewer` agent can review third-party
  PRs against policy.
- Addressing review comments on own PRs: PR review comments ingest as `ReviewFinding`s
  (severity mapped, disposition `Open`), flowing through the existing
  disposition-completeness gate — "address review comments" becomes the same structural
  obligation as internal findings.

## 7. Maintain stage

`zirv workflow maintain scan` — invoked, never resident (operator cron, CI schedule, or a
ctx hook):

1. Run the operator-configured detectors: named commands + thresholds
   (`[workflow.maintain]` — operator-only, `REPO_FORBIDDEN`; running configured commands
   is exactly the class of authority repo layers must never hold). Detectors are
   deterministic: exit status / count thresholds, no LLM judgment.
2. On a breach: create `.zirv/work/<new-id>/intent.md` from the incident template
   (problem = detector finding, evidence attached), file a GitHub issue via the `report`
   plumbing generalized with an operator-configured destination repo
   (`[report] repository` — operator-only; the hardcoded zirv repo remains the default
   for `zirv report` itself), dedupe via the #178 `find_open_issue_by_title` pattern.
3. Start a workflow cycle parked at the Intent gate (`AwaitingApproval`).

The loop closes: findings re-enter Stage 1 without a human starting the process; humans
triage by accepting or cancelling the parked cycle. Filing is idempotent per
detector+finding key so a scheduled scan never floods.

## 8. Telemetry and docs

- `TelemetryKind` additions: `ArtifactAccepted`, `DeployGateEvaluated`,
  `MaintenanceScan`, `AgentDispatched`. New optional fields (`artifact_stage`,
  `deploy_tier`, `agent_id`) ride `#[serde(default)]`; schema bump to v3; same store,
  same pruning, `zirv workflow stats` extended (e.g. artifact-acceptance latency,
  deploy-gate outcomes).
- Vault pages per phase: `Modules/Workflows.md`, `Modules/Built-in Commands.md`,
  `Concepts/Untrusted Configuration.md` (work-product surface, agent repo layer,
  deploy-tier fold), `Modules/Ctx Adapters.md` (dispatch method), Development pages.
  Canonical context `.zirv/context/common.md` module map gains `agents`, `maintain`.

## 9. Testing strategy

- Engine/gate logic stays pure where possible (rot-engine discipline): hash-pinning,
  drift detection, tier folding, and condition evaluation are pure functions over state +
  inputs; I/O confined to the state layer. Inline `#[cfg(test)] mod tests` per module.
- Registry tests mirror the skill registry's: layering precedence, repo-add-only with
  collision warnings, fail-closed gates, provider-neutrality string assertions extended
  to agent instruction bodies.
- Tier fold gets the same can-narrow-never-widen property tests as `policy.rs`.
- Full gate matrix: artifact missing / template-only / drifted / accepted; re-approval on
  drift; reclassification inserting artifact steps.
- All five verification commands (build, nextest, serial cargo test, fmt, clippy) per
  phase; Windows path handling covered (state uses existing `StateDir` helpers).

## 10. Phasing

Each phase is an independently shippable PR-sized sub-issue with its own version bump,
vault-docs pass, and telemetry wiring. Order matters:

| # | Scope | Key deliverables | Acceptance criteria covered |
|---|---|---|---|
| 1 | **Artifact chain + Intent + adaptive conditions** | `ArtifactRecord`, schema v2, templates, approve/pin/drift gates, `WorkflowPhase::Intent`, `brainstorm` skill, adaptive matrix in `definitions()`, `workflow artifacts` verb | AC-1, AC-2 (intent half) |
| 2 | **Parity skills** | `write-plan`, `execute-plan`, `worktree`, `finish-branch` (skill text; deploy wiring stubs until phase 4), built-in audit revisions | AC-2 (rest) |
| 3 | **Agent registry + step seats** | `agents.rs`, manifests + layering + gates, `dispatch_agent`, `WorkflowStep.agent`, reviewer-seat migration, vault-keeper manifest migration | AC-5, AC-6 |
| 4 | **Deploy tiers + PR lifecycle** | tier config + fold, tier semantics on Deploy steps, `review package --pr`, PR-comment finding ingestion, `finish-branch` fully wired | AC-3 |
| 5 | **Maintain loop** | `maintain scan`, detector config, incident intent template, generalized report destination, parked-cycle filing | AC-4 |

AC-7 (documentation + telemetry) is woven into every phase, not a separate one.

Rationale: 1 is the spine everything reads from; 2 fills the stages 1 created; 3 must
precede 4 (production gating requires a defined reviewer seat); 5 composes report
plumbing with the chain and lands last.

## Risks

- **Schema bumps** (workflow v2, telemetry v3) hard-error on old state by existing
  policy; release notes must say "finish running workflows before upgrading". Telemetry
  uses `#[serde(default)]` so old *events* still aggregate.
- **`.zirv/work/` in third-party repos**: repos that gitignore `.zirv/` lose the
  committed audit trail. Mitigation: `zirv workflow start` warns when the work dir
  matches a gitignore rule.
- **Hash-drift friction**: legitimate artifact edits re-open the gate. Intended behavior,
  but the `workflow artifacts` verb must make re-approval one obvious step.
- **Agent dispatch divergence** between adapters: mitigated by making `dispatch_agent` a
  default trait implementation over the already-proven per-adapter primitives, plus a
  shared conformance test asserting invariants (never exact argv — installed-binary
  probes vary by machine).
