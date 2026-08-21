---
last-verified: 2026-08-21
---

# Workflows

## Quick Reference

- **Files:** `src/commands/workflow/{mod,skill,capability,classify,engine,verification,review,artifact,telemetry}.rs`
- **Commands:** `zirv skill`, `zirv workflow`, `zirv test`, `zirv verify`, `zirv artifact`
- **State:** private platform state under `workflows/`, `verification/`, `artifacts/`, and `workflow-telemetry/`, each repository-scoped by the existing deterministic repo slug
- **Repository inputs:** `.zirv/skills/*.yaml|yml|toml` and optional `.zirv/verify.toml` — both untrusted, and both gated by an operator-only `[workflow]` config key
- **Operator config:** `[workflow]` in `ctx.toml` — `repo_checks_enabled`, `repo_skills_enabled`, `telemetry_enabled`, `telemetry_max_events`, `telemetry_retention_days`, every one `REPO_FORBIDDEN`
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

Registry precedence is deterministic and asymmetric: an operator-global manifest (`~/.zirv/skills`) may replace a built-in, because the operator is trusted; a repository manifest (`.zirv/skills`) may only **add** an id it does not already occupy. A repository id that collides with a built-in or operator-global skill is ignored, and the collision is reported as a warning naming both sides (`SkillRegistry::warnings`, printed by `zirv skill list/show`) rather than silently replacing trusted methodology text. `zirv skill list/show --built-in-only` disables both custom layers; `zirv workflow start --built-in-only` persists that trust choice in the workflow state, so resume and prompt rendering cannot silently re-enable an override; and the operator-only `workflow.repo_skills_enabled` (`REPO_FORBIDDEN`, see [[Untrusted Configuration]]) drops the repository layer entirely. Files, directory entry counts, individual instructions, and dependency-resolved stacks are bounded; unknown schema fields/versions fail; symlinked manifests or parent directories and path escapes are refused; dependency cycles/missing dependencies fail before resolution.

A repository manifest that will not load takes the workflow prompt layer with it — composition still succeeds, and the loss is announced once on the `zirv ▸` channel (`Event::WorkflowLayerSkipped`) instead of disappearing silently.

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

Classification separates intent, complexity (`trivial`, `bounded`, `substantial`, `architectural`), and risk (`low`, `medium`, `high`, `critical`). Identical path/line/task inputs produce the same score and sorted reasons. Both tracked and untracked change surfaces contribute paths and size signals, measured against the same merge-base diff base `review` uses (`origin/main`, then `main`, then `HEAD^`), so classification and review agree on what "the change" is. Sensitive auth/security or database/schema surfaces raise a High floor; an explicit `--risk` below that floor is refused outright, and every other input can only ever be *raised* toward it:

- Declared inputs (`--path`, `--changed-lines`) no longer switch Git measurement off. Both are computed and the higher risk band wins, with `declared_scope: true` recorded on the classification so a consumer can tell a stated surface from a measured one. When Git cannot be measured at all, the declared band stands and the reason says so.
- Classification is re-measured when a workflow advances *into* a review or verify step, and the band never drops (`engine::reclassify_at_gate`). Freezing it at `workflow start` measured an empty tree for the usual order of work — decide the plan, then write the code — so the review step was chosen before the change existed. If the new measurement requires a review or verify step the original materialization omitted, it is added; completed steps are never re-run, and no approval gate appears retroactively.

Low-risk work omits design/reviewer ceremony; substantial or high-risk feature/refactor work keeps an approval-gated design step, and medium/high risk adds the explicit review depth defined by `review::depth_for_risk`.

State files are atomic private JSON. The `active` pointer names one running/approval-pending workflow. Completion/failure clears it, so a later session cannot redispatch finished work. A failed step stops after three attempts.

## Verification

`zirv test changed` maps Git-changed paths to checks; when impact is empty or cannot be mapped, it deliberately falls back to all eligible checks. `zirv test all` runs all checks and `zirv verify` runs final checks. Optional `.zirv/verify.toml` schema version 1 defines check id/kind/command/path patterns/phase eligibility/timeout. Without it, Cargo commands and `npm run <script>` scripts are discovered from the manifests present.

This is the one place repository text becomes an executed shell command, so each check carries its **source** in every report:

| Source | Command text written by | Gated by `workflow.repo_checks_enabled` |
|---|---|---|
| `repo-config` | the repository's `.zirv/verify.toml` | yes |
| `discovered-script` | the repository's `package.json` (`npm run <id>`) | yes |
| `discovered-toolchain` | zirv itself (the Cargo commands) | no |

With the gate off, repo-supplied checks are still listed — with status `Skipped` and a `skipped: repo-supplied checks disabled (workflow.repo_checks_enabled)` note — and never executed. A skipped check is not a passing check, so a disabled gate cannot satisfy a step's evidence requirement. Two caps hold either way: a repo-supplied `timeout_secs` is clamped to 900s and repo-supplied checks are truncated to the first 32, each noted in the report. The per-check source label is also what makes a vacuous `command = "true"` gate visible for what it is.

Checks run in the repository through a bounded shell child with stdout/stderr drained concurrently into a fixed-size tail and cross-platform process-tree cleanup on exit/timeout. Passing output is not retained; failures keep a capped actionable tail, with control characters scrubbed (newlines and tabs kept) so repository-controlled output cannot repaint the terminal into a forged summary. A stream that ends in a read error says so rather than looking complete.

Reports include a change fingerprint over `HEAD`, raw change metadata, and content hashes for the tracked/untracked paths in the change set, plus compact per-check status. The fingerprint is taken **before** the checks run, so edits made during a long suite are not recorded as tested, and every Git call runs at the worktree root with `core.quotePath=false`, so root-relative paths and non-ASCII filenames both resolve (a `--repo <subdir>` run used to mix two path bases and see no content edits at all). A `--check`-narrowed run records what it was narrowed to and can never satisfy a step gate — a format-only run is evidence about formatting. Results are printed before they are persisted: a state-directory failure is a warning, not a lost run. Test/verify workflow steps cannot advance on stale, narrowed, or failed evidence; report schema versions are checked when evidence is reloaded.

## Review

`zirv workflow review package <id>` builds a reproducible package from task/classification, base/head SHAs, tracked and untracked changed paths, a streaming/capped relevant diff, summarized verification (including whether it is fresh), existing findings, and review round. It never includes the controller conversation or plan history. `review run --agent <name>` pipes that package to a fresh `zirv agent` worker; the worker does not share the controller conversation.

Untracked files are the working tree's own contents rather than anything a change author chose to commit, so they contribute their **path** always and their **body** only when it is safe: text (no NUL byte in the first 8 KB), at most 16 KB, and not matching a sensitive name (`.env*`, `*credential*`, `*secret*`, `*.pem`, `*.key`). Each exclusion is stated in the package, so a reviewer sees that the file exists and why its contents are absent.

The reviewer is pinned read-only by its own adapter — `AgentAdapter::read_only_args`, the same flags `distiller_cmd` uses (claude `--disallowedTools=Write,Edit,Bash,NotebookEdit`, codex `--sandbox read-only`) — because its prompt embeds an untrusted repository diff. An adapter with no registered pin is refused rather than launched unrestricted.

Findings are persisted with severity, optional path/line, and disposition (`open`, `accepted`, `dismissed`, `fixed`, `residual`). The reviewer records findings through `zirv workflow review add` against the same state file while `review run` waits, so evidence is appended to freshly re-loaded state; the pre-spawn snapshot is stale by construction and writing it back destroyed the reviewer's own findings. Successful isolated review runs are persisted against the exact change fingerprint; evidence is refused if the tree changes while the reviewer is running and becomes stale after later edits. A dashboard spawn-request acknowledgement (`ZIRV_CTX_DASH_REQUESTS`) exits 0 for a review that has not started yet — it is reported as a spawned pane and records no evidence at all. A review step cannot pass while any finding is still open or its required fresh review evidence is missing. Review phase failures use the workflow's hard retry limit. Low risk uses self-verification, medium/high require one independent reviewer, and critical risk requires two fresh review runs.

## Artifacts

`zirv artifact render <path>` registers a repository-contained regular file and returns a stable id. Later steps use the id/path rather than reinjecting payload content. The repository path is canonicalized in one shared place (`artifact_dir`), so register/load/list agree on the state slug even where the OS reports two spellings of the same directory (macOS `/var` vs `/private/var`). Static presentation is the honest default because current CLI adapters expose no verified native artifact API.

`artifact present --interactive` requires an explicit server command and an `http(s)://` URL (no other scheme reaches the platform opener), checks that the adapter reports `shell.exec` and `browser.open` support at all — a capability *report*, not an enforcement — and enforces a 1–3600 second lifetime whose clock starts at spawn rather than after the blocking browser open. Success means a live server: the child must still be running and the URL's host/port must accept a connection within 5 seconds, and a command that exits first fails with its own stderr surfaced instead of reporting success. The process tree is killed on timeout, readiness failure, or browser-open failure.

## Telemetry

`zirv workflow stats` aggregates local structured events by phase: duration, exposed token counts, verification failures, finding disposition, fix rounds, artifact counts, and worker counts. `--clear` removes this repository's telemetry. Controls live in the `[workflow]` config section — `telemetry_enabled`, `telemetry_max_events`, `telemetry_retention_days`, with `ZIRV_CTX_WORKFLOW_TELEMETRY*` as the operator's environment override — and all three are `REPO_FORBIDDEN`. They were previously plain `ZIRV_WORKFLOW_TELEMETRY*` environment reads, which any repository script could set for itself, and the boolean parse turned `0` into "enabled" because `bool::from_str` rejected it; `0`/`1` are now accepted alongside `true`/`false`, and anything else is a loud error.

Each event is a separate private bounded JSON file, which avoids concurrent-writer corruption. Default retention is 1,000 events/30 days, with hard upper bounds even when configuration asks for more; free-form labels are capped before serialization. One unreadable event file is skipped with a warning rather than failing the whole `stats` command. Aggregates include per-adapter duration/token/failure comparisons and use each workflow's latest finding snapshot instead of double-counting the same findings across phases. The schema has no prompt, source-code, diff, model-response, or command-output field. Telemetry reports evidence; it does not silently weaken safety or review policy.
