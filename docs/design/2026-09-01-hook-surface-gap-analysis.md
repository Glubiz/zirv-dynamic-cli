# Hook-surface gap analysis vs Ruflo's documented hooks (issue #244)

**Date:** 2026-09-01 · **Issue:** #244 · **Status:** research + one closed gap (SessionStart)

## 1. Context

Spike candidate (d) of the [Ruflo evaluation](2026-08-31-ruflo-evaluation.md) (issue #240)
asked for a straight comparison of Ruflo's documented lifecycle hooks against zirv's own hook
surface (`src/commands/ctx/hook.rs`), to find genuine coverage gaps rather than to match
Ruflo's count. The evaluation itself counted 18 hooks from Ruflo's `hooks-automation` skill;
this pass enumerates 19 by additionally counting `notify` (Ruflo dispatches it as a distinct
`settings.json`-wired event alongside the 18 the skill markdown lists explicitly). The count
discrepancy is noted, not resolved — nothing below depends on which total is "correct".

## 2. Ruflo's documented hooks

| # | Hook | What it fires on |
|---|---|---|
| 1 | `pre-edit` | Before a file edit tool runs |
| 2 | `pre-bash` | Before a shell command runs |
| 3 | `pre-task` | Before a subagent/task dispatch |
| 4 | `pre-search` | Before a search tool runs |
| 5 | `post-edit` | After a file edit tool completes |
| 6 | `post-bash` | After a shell command completes |
| 7 | `post-task` | After a subagent/task completes |
| 8 | `post-search` | After a search tool completes |
| 9 | `mcp-initialized` | An MCP server finished initializing |
| 10 | `agent-spawned` | A new agent/worker was spawned |
| 11 | `task-orchestrated` | A task was handed to the orchestration layer |
| 12 | `neural-trained` | The SONA neural layer completed a training pass |
| 13 | `memory-write` | An entry was written to swarm memory |
| 14 | `memory-read` | An entry was read from swarm memory |
| 15 | `memory-sync` | Memory was synchronized across swarm nodes |
| 16 | `session-start` | A session begins |
| 17 | `session-restore` | A session resumes from a prior one |
| 18 | `session-end` | A session ends |
| 19 | `notify` | A generic notification event |

## 3. zirv's current hook surface

| Hook | Wired as | Role |
|---|---|---|
| Claude `Stop` | `zirv ctx hook stop` | Score the turn, advise or forward |
| Claude `UserPromptSubmit` | `zirv ctx hook prompt` | Install the rot-marker instruction, fold in adoption nudges |
| Claude `PreCompact` | `zirv ctx hook pre-compact` | Record that a compaction is starting |
| Claude `PreToolUse` (`Agent\|Task`) | `zirv ctx hook pretool` | Refuse a subagent dispatch that would silently inherit this seat's expensive model |
| Claude `PreToolUse` (`Bash\|PowerShell`) | `zirv ctx safety check` | Command-safety allow/ask/deny gate (issue #83) |
| Claude `SessionStart` (`resume\|clear`) | `zirv ctx hook session-start` | **New in this issue**: re-inject the latest stored handoff when a session resumes or starts over with a cleared context |
| Codex notify | `zirv ctx hook notify` | Same role as `Stop`, on codex's own notify mechanism |

## 4. Gap table

| Ruflo hook | Verdict | Why |
|---|---|---|
| `pre-bash` | Covered | `zirv ctx safety check` (`PreToolUse` on `Bash\|PowerShell`) is the pre-bash equivalent: allow/ask/deny before the command runs. |
| `session-end` | Covered | Claude's `Stop` hook is zirv's session-end equivalent — it is where a session's final score, advisory, and rot verdict are recorded. |
| `notify` | Covered | Codex's `notify` program is already wired 1:1 to `zirv ctx hook notify`, the same role `Stop` plays for Claude. |
| `session-restore` | **Real gap, closed by this issue** | Nothing previously re-injected a stored handoff when a session resumed or its context was cleared outside `zirv ctx resume`'s own explicit flow — a bare `claude --resume` or `/clear` saw no prior handoff at all. `zirv ctx hook session-start` closes this. |
| `agent-spawned` | Gap, deferred (telemetry-only) | `TelemetryEvent.parent_session_id: Option<String>` already exists in the schema but is never populated by any production code path (only a unit test sets it) — there is no lineage record connecting a spawned worker's telemetry back to the session that spawned it. Closing this is a telemetry-population change, not a new hook; deferred as its own follow-up. |
| `task-orchestrated` | Not a separate hook | Issue #242 (auto-spawn on workflow gate transitions) is the same substrate this hook would cover: `engine::auto_spawn_decision`/`try_auto_spawn` already fire exactly when a task (the next workflow phase) is handed off for execution. A dedicated `task-orchestrated` hook would duplicate that decision point rather than add coverage. |
| `pre-edit` / `post-edit` | Not a gap | zirv is a supervising process watching a harness's own transcript and exit behavior, not a tool-call interceptor inside the harness's edit loop — `PreToolUse`/`PostToolUse` at that granularity is Claude Code's own extension point, and zirv's `pretool` hook already uses the one slice of it (subagent dispatch) that materially affects zirv's own safety/cost posture. |
| `pre-task` / `post-task` | Not a gap | Same reasoning as `pre-edit`/`post-edit`: task-level tool interception belongs to the harness's own hook system. zirv's `pretool` hook covers the one task-shaped case (subagent/`Agent`\|`Task` dispatch) it has a real stake in. |
| `pre-search` / `post-search` | Not a gap | Search tool calls carry no cost/safety/lineage concern zirv currently governs; there is nothing for a pre/post-search hook to decide that isn't already covered by the harness's own tool permissions. |
| `mcp-initialized` | Not a gap | zirv has no MCP server of its own to initialize, and does not manage the harness's MCP server lifecycle. |
| `neural-trained` | Not a gap | zirv has no neural/ML layer (see the Ruflo evaluation's rejection of Ruflo's SONA layer, §3) — there is nothing that could fire this event. |
| `memory-write` / `memory-read` / `memory-sync` | Not a gap | zirv's own memory subsystem (`.zirv/memory/`, `memory.rs`) is read/written directly by zirv's own code paths (`memory::remember`, prompt composition), not through a harness-side hook — there is no cross-process boundary here for a hook to sit at the way there is for a harness-invoked tool call. |

## 5. Conclusion

One real, closeable gap was found (`session-restore`) and is closed by this issue's new
`zirv ctx hook session-start` handler and its `HARNESS_HOOKS` entry (matcher `resume|clear`).
One further gap (`agent-spawned` lineage) is real but is a telemetry-population fix, not a new
hook surface, and is deferred. Everything else either has an existing zirv equivalent
(`pre-bash`, `session-end`, `notify`), is already covered by the same substrate a different
issue owns (`task-orchestrated` via #242's auto-spawn), or names a lifecycle moment zirv has no
functional stake in governing (the harness-internal tool-call hooks, the MCP/neural/memory
hooks that have no zirv-side counterpart to fire from).
