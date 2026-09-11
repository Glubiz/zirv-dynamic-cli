# Native runtime inventory: implementation owners (issue #470)

**Date:** 2026-09-11 · **Issue:** #470, step N01 of the native-runtime roadmap (#469)

This is the exhaustive, machine-checked list step N01
(`docs/design/2026-09-11-native-runtime-contracts.md`) promises: every
command verb the binary accepts, and every place in `src/` that launches or
drives a model, each with a named implementation owner. `zirv verify
--builtin`'s `ZCHK-RUNTIME-INVENTORY` check (`src/commands/workflow/checks/
inventory.rs`) parses both tables below against the real clap model and the
real source tree on every run, so this document cannot silently drift from
the binary it describes -- a new command or a new model-calling call site
lands with an owner here in the same change, or the build fails.

Owner values:

- `shared` -- runtime-neutral today and stays so regardless of which
  `RuntimeBackend` a session uses (script runner, init, create, help,
  version, update, report, memory, artifact, frontend, test, verify,
  workflow state). A `shared` command can still contain a model-calling
  call site (e.g. `memory optimize`'s consolidation step, `frontend
  review`'s visual reviewer) -- that call site is owned individually in the
  second table below; the command's own CRUD/state contract is what stays
  neutral.
- `harness-backend` -- a legacy-specific operation whose native equivalent
  is simply a native session (`ctx wrap` is the reference case: a native
  runtime has no PTY to supervise, because it never shells out to one).
- `Nxx (#4yy)` -- owned by that step of the roadmap; see the map below.

Roadmap map: N01 #470 contracts (this step) · N02 #471 provider routes/
credentials · N03 #472 journal/receipts · N04 #473 tool permissions/sandbox ·
N05 #474 native tools/shell · N06 #475 instructions/skills/memory compile ·
N07 #476 Anthropic provider · N08 #477 OpenAI provider · N09 #478 agent
loop/steer/interrupt · N10 #479 workers/tasks/mail/delegation · N11 #480
native conversation TUI · N12 #481 Google · N13 #482 compatible endpoints ·
N14 #483 MCP/web/browser/diagnostics/artifacts · N15 #484 workflows/
verification/helper-model calls natively · N16 #485 meta-orchestrator/mixed
teams · N17 #486 compaction/rot recovery/checkpoints · N18 #487 usage/
spend/health · N19 #488 rollover · N20 #489 persistent runtime/protocol ·
N21 #490 UX · N22 #491 setup/migration/diagnostics/docs/packaging · N23 #492
parity proof.

## Commands

Every depth-1 and depth-2 verb `./target/debug/zirv commands --json`
reports, using the same depth rule as `scripts/check-readme-features.sh`
(depth counted on the words after the leading `zirv `; `zirv ctx hook audit`
contributes `ctx` at depth-1 and `ctx hook` at depth-2 -- `audit`, depth-3,
is not required here either, matching that script's own acceptance
criterion).

| Verb | Owner | Notes |
|---|---|---|
| `agent` | N10 (#479) |  |
| `artifact` | shared |  |
| `artifact list` | shared |  |
| `artifact present` | shared |  |
| `artifact render` | shared |  |
| `artifact show` | shared |  |
| `chat` | N11 (#480) |  |
| `commands` | shared |  |
| `context` | N06 (#475) |  |
| `context lint` | N06 (#475) |  |
| `context status` | N06 (#475) |  |
| `context sync` | N06 (#475) |  |
| `create` | shared |  |
| `ctx` | shared | command-group umbrella; each subcommand owned individually below |
| `ctx agent` | N10 (#479) |  |
| `ctx ask` | N15 (#484) |  |
| `ctx chat` | N11 (#480) |  |
| `ctx compile` | N06 (#475) | composes the session prompt from context/skills/memory layers |
| `ctx config` | shared |  |
| `ctx discover` | N03 (#472) | reads the compaction ledger to size Bash tool-result bloat |
| `ctx exec` | N09 (#478) |  |
| `ctx explain-status` | shared |  |
| `ctx forget` | N06 (#475) |  |
| `ctx group` | N10 (#479) |  |
| `ctx handoff` | N17 (#486) |  |
| `ctx handover` | N16 (#485) |  |
| `ctx hook` | N22 (#491) |  |
| `ctx inbox` | N10 (#479) |  |
| `ctx kill` | harness-backend | terminates a supervised OS process; a native session is interrupted via the runtime protocol instead |
| `ctx learn` | N06 (#475) |  |
| `ctx loop` | N09 (#478) |  |
| `ctx measure` | N18 (#487) | transcript-derived proportionality/health metrics with a committed baseline |
| `ctx nudge` | N10 (#479) |  |
| `ctx objective` | N09 (#478) |  |
| `ctx optimize` | N15 (#484) |  |
| `ctx output` | N03 (#472) | lists/shows stored session outputs (issue #326's `ctx run` capture) |
| `ctx permissions` | N04 (#473) | canonical policy and approval audit remain shared; native effects consume them through `runtime::enforcement` |
| `ctx provider` | N02 (#471) | `init`, `list`, `check`, and nested `credential set`; inventory tracks depth 1/2, so this is the owning depth-2 row |
| `ctx recall` | N06 (#475) |  |
| `ctx remember` | N06 (#475) |  |
| `ctx resume` | N17 (#486) |  |
| `ctx run` | N05 (#474) | runs one command and stores its output; shape a native tool executor must replicate |
| `ctx safety` | N04 (#473) | the existing command classifier is re-evaluated by the native execution broker at each process boundary |
| `ctx savings` | N18 (#487) |  |
| `ctx score` | N17 (#486) |  |
| `ctx search` | N06 (#475) | explicitly zero-model cross-session recall |
| `ctx send` | N10 (#479) |  |
| `ctx snapshot` | N14 (#483) | redacted diagnostic-state summary |
| `ctx spend` | N18 (#487) |  |
| `ctx status` | N17 (#486) |  |
| `ctx swarm` | N10 (#479) |  |
| `ctx task` | N10 (#479) |  |
| `ctx usage` | N18 (#487) |  |
| `ctx wait` | N10 (#479) |  |
| `ctx worktree` | shared |  |
| `ctx wrap` | harness-backend | PTY-supervised interactive session; the example case for this bucket |
| `frontend` | shared |  |
| `frontend benchmark` | shared |  |
| `frontend capabilities` | shared |  |
| `frontend check` | shared |  |
| `frontend profile` | shared |  |
| `frontend render` | shared |  |
| `frontend review` | shared | CRUD/capture is runtime-neutral; its model-assisted visual review step is tracked as an entry point below |
| `help` | shared |  |
| `init` | shared |  |
| `memory` | shared |  |
| `memory forget` | shared |  |
| `memory init` | shared |  |
| `memory list` | shared |  |
| `memory optimize` | shared | CRUD/storage is runtime-neutral; its model-assisted consolidation step is tracked as an entry point below |
| `memory promote` | shared |  |
| `memory recall` | shared |  |
| `memory remember` | shared |  |
| `memory rollback` | shared |  |
| `memory status` | shared |  |
| `memory verify` | shared |  |
| `report` | shared |  |
| `report bug` | shared |  |
| `report feature` | shared |  |
| `setup` | N22 (#491) |  |
| `setup apply` | N22 (#491) |  |
| `setup profile` | N22 (#491) |  |
| `setup reset` | N22 (#491) |  |
| `setup restore` | N22 (#491) |  |
| `setup status` | N22 (#491) |  |
| `skill` | N06 (#475) | prints the bundled operator orientation skill |
| `skill list` | N06 (#475) |  |
| `skill show` | N06 (#475) |  |
| `test` | shared |  |
| `test all` | shared |  |
| `test baseline` | shared |  |
| `test changed` | shared |  |
| `update` | shared |  |
| `verify` | shared |  |
| `version` | shared |  |
| `workflow` | shared |  |
| `workflow advance` | shared |  |
| `workflow agents` | N15 (#484) | dispatches/lists/shows built-in agent seats (`dispatch_agent`) |
| `workflow approve` | shared |  |
| `workflow artifacts` | shared |  |
| `workflow classify` | shared |  |
| `workflow close` | shared |  |
| `workflow context` | N06 (#475) | prints the current step's resolved skill context |
| `workflow list` | shared |  |
| `workflow maintain` | shared |  |
| `workflow reclassify` | shared |  |
| `workflow resume` | shared |  |
| `workflow review` | N15 (#484) | the model-calling cross-harness review flow (`review.rs`) |
| `workflow show` | shared |  |
| `workflow start` | shared |  |
| `workflow stats` | shared |  |
| `workflow status` | shared |  |

## Model-calling entry points

One row per place in `src/` that launches or drives a model: an adapter
launch-builder call (`headless_cmd`/`interactive_cmd`/`distiller_cmd`/
`headless_resume_cmd`/`dispatch_agent`), a helper-model child (the shared
`handoff::run_model` chokepoint that wraps `distiller_cmd`), or a
self-recursion into zirv (`Command::new(std::env::current_exe())`) that ends
in a model call. Found by starting from a known set of call sites and
completing it with:

```
grep -rnE "interactive_cmd\(|headless_cmd\(|headless_cmd_stdin\(|headless_resume_cmd\(|distiller_cmd\(|dispatch_agent\(|current_exe\(\)" src/
```

then discarding hits that are trait/adapter definitions (`src/commands/ctx/
adapters/{claude,codex,copilot,droid,gemini,opencode,pi,qwen}.rs` each
*define* these methods; they are not call sites), `#[cfg(test)]` fixtures
that stand a test binary in for `agent_bin` or re-exec `zirv` to test raw-
argv interception (`agent.rs`, `dash/mod.rs`, `fallback.rs`, `pool.rs`,
`rollover.rs`, `mod.rs`, `main.rs` each have such a test helper -- none is a
production model-calling path), and `update.rs`'s `current_exe()` (swaps the
installed binary during self-update; never spawns it).

| Entry point | Path | Symbol | Owner | Notes |
|---|---|---|---|---|
| Interactive orchestrator launch | `src/commands/ctx/chat.rs` | `build_launch` | N11 (#480) | backs `zirv ctx chat` / `zirv chat` |
| Wrap first-launch PTY spawn | `src/commands/ctx/wrap.rs` | `run_with` | harness-backend | initial `zirv ctx wrap` PTY `CommandBuilder` |
| Wrap mid-session PTY relaunch | `src/commands/ctx/wrap.rs` | `relaunch` | harness-backend | in-place restart after compaction/handoff |
| Dash pane restore | `src/commands/ctx/dash/roster.rs` | `restore_argv` | N11 (#480) | rebuilds a verified resume argv for a restored dashboard pane |
| Dash pane initial spawn | `src/commands/ctx/dash/pane.rs` | `spawn` | N11 (#480) |  |
| Dash pane harness handover | `src/commands/ctx/dash/pane.rs` | `handover` | N16 (#485) | swaps the harness under a live pane, same session identity |
| Dash spawn-request fulfillment | `src/commands/ctx/dash/mod.rs` | `fulfill_spawn_request` | N11 (#480) | services a worker/pane spawn request from mail or a dispatch |
| Headless exec spawn | `src/commands/ctx/exec.rs` | `run_with_clock_inner` | N09 (#478) | `zirv ctx exec`'s main headless spawn via `supervise::spawn_tapped` |
| Headless in-place resume/compact | `src/commands/ctx/exec.rs` | `compact_in_place` | N09 (#478) | resumes a headless session in place to compact it |
| Agent loop headless spawn | `src/commands/ctx/run_loop.rs` | `run_with_clock` | N09 (#478) | `zirv ctx loop`'s per-cycle headless spawn |
| Agent loop objective judge | `src/commands/ctx/run_loop.rs` | `evaluate_objective_after_cycle` | N09 (#478) | distinct helper-model call: judges the objective gate after a loop cycle |
| Distiller/judge chokepoint | `src/commands/ctx/handoff.rs` | `run_model` | N15 (#484) | wraps `distiller_cmd`; shared by run_loop, memory, optimize, ask below |
| Resume interactive relaunch | `src/commands/ctx/resume.rs` | `launch_command` | N17 (#486) | `zirv ctx resume` |
| Agent delegation dispatch | `src/commands/ctx/agent.rs` | `run_with` | N10 (#479) | probes `headless_resume_cmd` before a bounded structural-result retry |
| Built-in seat worker dispatch | `src/commands/workflow/agents.rs` | `dispatch_agent` | N15 (#484) | synchronous `.status()` dispatch of a built-in agent seat |
| Cross-harness review launch | `src/commands/workflow/review.rs` | `launch_reviewer` | N15 (#484) | `reviewer_argv` + self-recursion into `current_exe` |
| Frontend visual reviewer launch | `src/commands/workflow/frontend_render.rs` | `launch_visual_reviewer` | N15 (#484) | reuses `review::reviewer_argv`; self-recursion into `current_exe` |
| Auto-spawn on workflow gate transition | `src/commands/workflow/engine.rs` | `spawn_auto_worker` | N15 (#484) | issue #242: detached self-recursion into `zirv workflow review run` / `test` / `verify` |
| `ctx ask` helper-model call | `src/commands/ctx/ask.rs` | `run_model` | N15 (#484) |  |
| `ctx optimize` judgment call | `src/commands/ctx/optimize.rs` | `run_with` | N15 (#484) |  |
| Memory durable harvest | `src/commands/ctx/memory.rs` | `harvest_durable_with_tool_errors` | N06 (#475) | issue #37 durable-harvest chokepoint; called from exec.rs/wrap.rs restart and session-end seams |
| Memory optimize consolidation | `src/commands/ctx/memory_optimize.rs` | `apply_consolidation` | N06 (#475) | `zirv memory optimize`'s model-assisted merge |
| Builtin argv-shape checks | `src/commands/workflow/checks/argv.rs` | `headless_cmd` | shared | Notes: probe/check only -- `ZCHK-ARGV-CODEX-EXEC` / `ZCHK-ARGV-CLAUDE-HEADLESS` build argv to inspect it, never spawn |

Beyond the starter set given in this issue, this pass added: `src/commands/
ctx/ask.rs` (`run_model`), `src/commands/ctx/optimize.rs` (`run_with`),
`src/commands/ctx/memory.rs` (`harvest_durable_with_tool_errors`),
`src/commands/ctx/memory_optimize.rs` (`apply_consolidation`),
`src/commands/ctx/run_loop.rs`'s second call site (`evaluate_objective_
after_cycle`, distinct from its main headless spawn),
`src/commands/workflow/frontend_render.rs` (`launch_visual_reviewer`), and
`src/commands/workflow/engine.rs` (`spawn_auto_worker`).
