---
last-verified: 2026-08-20
---

# Workflows

## Quick Reference

- **Files:** `src/commands/workflow/{mod,skill,capability,classify,engine,verification,review,artifact,telemetry}.rs`
- **Commands:** `zirv skill`, `zirv workflow`, `zirv test`, `zirv verify`, `zirv artifact`
- **State:** private platform state under `workflows/`, `verification/`, `artifacts/`, and `workflow-telemetry/`, each repository-scoped by the existing deterministic repo slug
- **Repository inputs:** `.zirv/skills/*.yaml|yml|toml` and optional `.zirv/verify.toml`
- **If changed:** [[Built-in Commands]], [[Architecture Overview]], [[Utilities]] when prompt-layer behavior changes, and [[Untrusted Configuration]] when trust/capability rules change

## Purpose

The workflow subsystem makes Zirv own the development lifecycle instead of asking every model to reconstruct a methodology in conversation. Skills describe concise judgment. Zirv owns deterministic mechanics, phase state, risk selection, verification, review limits, artifacts, and telemetry.

Workflow state and skill context are deliberately different:

- durable state records completed/current steps, attempts, approvals, classification, and review findings outside model conversation context;
- the session prompt receives only the active step's resolved skill stack;
- moving to the next phase replaces the workflow prompt layer, so completed skills never accumulate after compaction or resume.

The current prompt integration is a narrow seam in `ctx::prompt::compose`. The future shared Context Compiler can consume the same `engine::active_skill_context`/`render_current_context` result without changing workflow state or skill manifests.

## Skills

`SkillManifest` schema version 1 includes a stable id/version, triggers, applicable phases, required/optional logical capabilities, context budget, dependencies, and an instruction body. Built-ins are embedded in the binary: `design`, `plan`, `implement`, `systematic-debugging`, `testing`, `tdd`, `review`, `verify`, `delegate`, and `parallelize`.

Registry precedence is deterministic: built-in < operator-global (`~/.zirv/skills`) < repository (`.zirv/skills`). `zirv skill list/show` reports the effective source/path; `--built-in-only` disables both custom layers. `zirv workflow start --built-in-only` persists that trust choice in the workflow state, so resume and prompt rendering cannot silently re-enable an override. Files, directory entry counts, individual instructions, and dependency-resolved stacks are bounded; unknown schema fields/versions fail; symlinked manifests or parent directories and path escapes are refused; dependency cycles/missing dependencies fail before resolution.

Repository skills are untrusted methodology. A manifest can request `repo.write`, `shell.exec`, or another logical capability, but it cannot grant one. `CapabilityReport::with_policy` only narrows adapter support; it never promotes an unsupported operation. `zirv workflow start --agent <name>` resolves every selected step's required capabilities before persisting the workflow.

## Capability vocabulary

Stable logical ids are `shell.exec`, `repo.read`, `repo.write`, `git.worktree`, `agent.spawn`, `test.run`, `artifact.render`, `browser.open`, and `network.access`. Reports distinguish `supported`, `degraded`, `unsupported`, and `operator-controlled`. Shell/filesystem/network access is operator-controlled for current harnesses; Zirv-owned supervision, verification, and static artifacts are reported separately.

This is intentionally an integration seam for the canonical `EffectivePolicy` in issue #43. Markdown skill instructions remain advisory and never claim enforcement.

## Workflow engine and classification

Schema-versioned built-in workflows are `feature`, `bugfix`, `refactor`, `spike`, and `review`. Steps declare a phase, skill, condition, approval gate, and hard attempt limit. Commands:

```text
zirv workflow list|show
zirv workflow classify --task "..."
zirv workflow start feature --task "..." [--agent claude] [--built-in-only]
zirv workflow status [id]
zirv workflow context [id]
zirv workflow approve <id>
zirv workflow advance <id> --outcome success|failure
zirv workflow resume <id>
```

Classification separates intent, complexity (`trivial`, `bounded`, `substantial`, `architectural`), and risk (`low`, `medium`, `high`, `critical`). Identical path/line/task inputs produce the same score and sorted reasons. Both tracked and untracked change surfaces contribute paths and size signals. Sensitive auth/security or database/schema surfaces are raised to a High floor automatically, and an operator cannot override them below it. Low-risk work omits design/reviewer ceremony; substantial or high-risk feature/refactor work keeps an approval-gated design step, and medium/high risk adds the explicit review depth defined by `review::depth_for_risk`.

State files are atomic private JSON. The `active` pointer names one running/approval-pending workflow. Completion/failure clears it, so a later session cannot redispatch finished work. A failed step stops after three attempts.

## Verification

`zirv test changed` maps Git-changed paths to checks; when impact is empty or cannot be mapped, it deliberately falls back to all eligible checks. `zirv test all` runs all checks and `zirv verify` runs final checks. Optional `.zirv/verify.toml` schema version 1 defines check id/kind/command/path patterns/phase eligibility/timeout. Without it, Cargo and npm scripts are discovered conservatively.

Checks run in the repository through a bounded shell child with stdout/stderr drained concurrently into a fixed-size tail and cross-platform process-tree cleanup on exit/timeout. Passing output is not retained; failures keep a capped actionable command/output. Reports include a change fingerprint over `HEAD`, raw change metadata, and content hashes for every tracked/untracked path, plus compact per-check status. Test/verify workflow steps cannot advance on stale or failed evidence; report schema versions are checked when evidence is reloaded.

## Review

`zirv workflow review package <id>` builds a reproducible package from task/classification, base/head SHAs, tracked and untracked changed paths, a streaming/capped relevant diff, summarized verification (including whether it is fresh), existing findings, and review round. It never includes the controller conversation or plan history. `review run --agent <name>` pipes that package to a fresh `zirv agent` worker; the worker does not share the controller conversation.

Findings are persisted with severity, optional path/line, and disposition (`open`, `accepted`, `dismissed`, `fixed`, `residual`). Successful isolated review runs are also persisted against the exact change fingerprint; evidence is refused if the tree changes while the reviewer is running and becomes stale after later edits. A review step cannot pass while any finding is still open or its required fresh review evidence is missing. Review phase failures use the workflow's hard retry limit. Low risk uses self-verification, medium/high require one independent reviewer, and critical risk requires two fresh review runs.

## Artifacts

`zirv artifact render <path>` registers a repository-contained regular file and returns a stable id. Later steps use the id/path rather than reinjecting payload content. Static presentation is the honest default because current CLI adapters expose no verified native artifact API. `artifact present --interactive` requires an explicit server command and URL, validates logical shell/browser support, enforces a 1–3600 second lifetime, and kills the process tree on timeout or browser-open failure.

## Telemetry

`zirv workflow stats` aggregates local structured events by phase: duration, exposed token counts, verification failures, finding disposition, fix rounds, artifact counts, and worker counts. `--clear` removes this repository's telemetry. Environment controls are `ZIRV_WORKFLOW_TELEMETRY`, `ZIRV_WORKFLOW_TELEMETRY_MAX_EVENTS`, and `ZIRV_WORKFLOW_TELEMETRY_RETENTION_DAYS`.

Each event is a separate private bounded JSON file, which avoids concurrent-writer corruption. Default retention is 1,000 events/30 days, with hard upper bounds even when environment overrides are supplied; free-form labels are capped before serialization. Aggregates include per-adapter duration/token/failure comparisons and use each workflow's latest finding snapshot instead of double-counting the same findings across phases. The schema has no prompt, source-code, diff, model-response, or command-output field. Telemetry reports evidence; it does not silently weaken safety or review policy.
