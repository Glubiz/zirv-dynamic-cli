# zirv ctx: Autonomous Context Management for AI Coding Agents

Date: 2026-07-31
Status: Approved design, pending implementation plan
Target release: zirv 2.5.0 (minor)

## Problem

Long-running AI agent sessions (Claude Code, Codex) degrade as their context window fills: instruction-following slips, tools get called in loops, hallucinations appear. Built-in auto-compaction fires too late, after degradation has already started. The existing mitigation (a shell Stop-hook "canary" that watches a `[josj]` reply prefix) detects rot from a single noisy signal, sometimes misfires, and cannot clean up context itself; recovery is a manual `/compact` or session restart.

The zirv-fitness-tracking autonomous issue loop makes this acute: its orchestrator runs as one long-lived `/loop 15m` session whose context grows every cycle, and its headless workers can rot mid-run on large issues. GitHub is already the loop's only state store, so orchestrator session continuity is unnecessary in principle, but no tooling exploits that.

## Goals

- Fully autonomous context management for headless agent runs: loops and one-shot workers never rot without being detected, distilled, and restarted, with no human involved.
- Autonomous intervention for interactive TUI sessions: early compaction or restart-with-handoff performed by a supervisor, never by the user, injected only when provably safe.
- Multi-signal rot detection with real token counts, replacing the single-marker canary and its misfires.
- Agent-agnostic: Claude Code and Codex supported in v1 through an adapter layer; more agents addable without touching core logic.
- Reusable across repos and machines: ships inside the zirv CLI's standard command suite, configured per-repo via `.zirv/`, distributed through zirv's existing brew/choco/install.sh lanes.

## Non-goals

- No ML or learned scoring; deterministic heuristics only.
- No daemon; every mode is a foreground process owning its children.
- No modification of agent internals; zirv only observes transcripts, launches processes, and injects input through channels the agents already expose.
- No replacement of loop business logic (issue triage, labels, PR policy); callers keep owning policy, zirv ctx owns session hygiene.

## Architecture overview

A new built-in clap command family `zirv ctx <verb>`, implemented as `src/commands/ctx/` with one submodule per verb, following the existing `src/commands/` pattern (init, create, help, version). Built-ins resolve before YAML scripts; a repo-local `.zirv/ctx.yaml` script would be shadowed, which is accepted and documented.

```
zirv ctx score     # rot-score a session transcript (JSON out) - the shared engine
zirv ctx loop      # stateless loop runner: fresh headless session per cycle
zirv ctx exec      # supervise one headless run (kill / distill / restart)
zirv ctx wrap      # supervise an interactive TUI via PTY
zirv ctx handoff   # distill a handoff from a transcript via a fresh model call
zirv ctx resume    # start a clean interactive session with the latest handoff injected
zirv ctx hook      # thin entrypoints for agent hooks (stop, prompt, pre-compact, notify, statusline tee)
zirv ctx status    # show supervised sessions, scores, last handoffs
zirv ctx usage     # show current usage-window state (collector + estimator)
```

Core layers, each agent-agnostic:

1. **Adapter layer** (`AgentAdapter` trait): everything agent-specific.
2. **Rot engine**: pure scoring over a normalized event stream.
3. **Supervisors**: `loop`, `exec`, `wrap` drive processes using verdicts from the rot engine.
4. **Handoff**: distillation and injection of session state across restarts.

## Agent adapters

```rust
trait AgentAdapter {
    fn name(&self) -> &'static str;
    fn detect(command: &[String]) -> bool;            // auto-select from wrapped argv
    fn headless_cmd(&self, prompt: &str, session_id: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    fn transcript_path(&self, session: &SessionRef) -> PathBuf;
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>; // turns, tool calls, tool errors, token usage
    fn compact_command(&self) -> Option<&'static str>; // "/compact" for both v1 adapters
    fn capabilities(&self) -> Capabilities;            // which rot signals are available
    fn register_turn_signal(&self) -> TurnSignalSetup; // how turn-boundary events reach the socket
}
```

`NormalizedEvent` is the only currency the rot engine and supervisors understand: `TurnStart`, `AssistantFinal { text, input_tokens }`, `ToolCall { name, input_hash }`, `ToolResult { is_error }`, `Compaction`.

v1 adapters:

| Aspect | claude | codex |
|---|---|---|
| Headless launch | `claude -p <prompt> --session-id <uuid> ...` | `codex exec <prompt> ...` |
| Transcript | `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` | `~/.codex/sessions/**` rollout JSONL |
| Token usage | API-reported usage on assistant events | usage fields in rollout events |
| Turn-boundary signal | Stop hook running `zirv ctx hook stop` | `notify` config program running `zirv ctx hook notify` |
| Marker signal | Yes (UserPromptSubmit hook) | No (capability absent; scoring uses remaining signals) |
| Compaction | `/compact` in TUI | `/compact` in TUI |

Adapter selection is automatic from the wrapped command's argv (`detect`), overridable with `--agent <name>`. Codex file formats and notify semantics must be verified against the installed codex CLI during implementation; the adapter is written from its real behavior, not from this table.

## Rot engine (`zirv ctx score`)

Input: a transcript path (plus adapter). Output: JSON `{ score: 0-100, verdict, signals: {...}, context_tokens }`.

Signals over a trailing window (default: last 10 turns), each weighted, all tunable:

1. **Real context size** (gate, not vote): API-reported input tokens from the most recent assistant event. Below the floor (default 100k) the verdict is always `healthy`. Above the ceiling (default 160k) the verdict is at least `compact`.
2. **Tool-failure rate**: fraction of tool results with `is_error` in the window.
3. **Repetition loops**: identical `(tool, input_hash)` appearing >= 3 times in the window.
4. **Instruction-following marker** (optional, capability-gated): miss rate of a configured reply-prefix marker on turn-final assistant messages. Marker text comes from config (default `[zirv]`); the tool ships nothing user-specific. Signal activates only when the marker hook is installed for that agent.

Verdicts: score >= 40 `advise`, >= 60 `compact`, >= 80 `restart`; token ceiling with score >= 60 also yields `restart`. Scoring is pure and deterministic: same transcript in, same verdict out. The existing canary's eight synthetic-transcript test cases port directly as Rust unit tests.

## Interactive supervision (`zirv ctx wrap`)

`zirv ctx wrap -- claude [args]` (or `-- codex [args]`) spawns the real TUI inside a PTY owned by zirv, proxying stdin/stdout byte-for-byte. A shell alias makes it the default (`alias claude='zirv ctx wrap -- claude'`). The wrapped experience must be indistinguishable from an unwrapped one until intervention.

Injection preconditions (both required):

1. **Turn boundary**: the agent's turn-end signal (Stop hook / notify program) writes `{session_id, turn, score}` to the session's unix socket under the state dir. This is the platform-authoritative "agent finished responding" event.
2. **User idle**: zirv proxies keystrokes, so it knows whether the user typed anything after the turn ended. Injection happens only when the input buffer is untouched and the PTY has been output-quiet for a debounce interval (default 3s).

Escalation ladder by verdict:

- `advise`: one-line advisory written to the terminal; no injection.
- `compact`: inject the adapter's compaction command with focus instructions (preserve task state, current file paths, unresolved errors). Early compaction on a still-healthy transcript is the primary fix for "auto-compact fires too late".
- `restart`: generate a handoff via `zirv ctx handoff`, gracefully quit the TUI, relaunch the agent in the same PTY with the handoff as the initial prompt.

Compaction cooldown: after an injected compaction, no further injection until the score has been recomputed on post-compaction turns, preventing compaction loops. Sessions not started through the wrap degrade to advisory (via the hook) plus manual `zirv ctx resume`.

## Headless supervision

### `zirv ctx loop`

Replaces long-lived orchestrator sessions (e.g. `/loop 15m /issue-loop`). Each cycle launches a fresh headless session with a clean context; durable state lives wherever the prompt's own conventions keep it (GitHub, for the issue loop). Context rot at the orchestrator becomes structurally impossible.

Per-loop config: prompt, interval, agent, model/extra args, max cycle duration, backoff policy, `on_failure` command. Session ids are generated up front so transcript paths are known deterministically. Cycle overruns are killed at the deadline. Repeated failures back off exponentially and eventually stop with non-zero exit plus the `on_failure` hook.

### `zirv ctx exec -- <agent headless command>`

Supervises a single headless run. Tails the transcript as it grows, scoring periodically. On `restart` verdict or wall-clock timeout: SIGTERM the child, distill a handoff, relaunch with original prompt + handoff. Default max 2 restarts, then non-zero exit so the caller applies its own policy (the issue-loop dispatcher applies `bot:blocked` exactly as today). Replaces pidfile-timeout supervision with productive restarts.

## Handoff (`zirv ctx handoff`, `zirv ctx resume`)

- Distillation runs a **fresh** headless model call (cheap model, default `claude -p --model haiku`-class via the adapter) over the transcript tail, using a versioned prompt template. A rotted session is never asked to summarize itself.
- Handoff format (markdown): Task, Done, Remaining, Next step, Files touched, Gotchas learned.
- Fallback: if distillation fails, a structural handoff is extracted mechanically (last N user messages + files touched from tool calls). A restart always has something to stand on.
- Handoffs are stored under the state dir keyed by repo + session; `zirv ctx resume` launches a fresh interactive session with the latest handoff for the current repo injected as the initial prompt.

## Usage pacing

Autonomous loops must never die mid-run because a subscription usage window (Claude: 5-hour rolling and 7-day) ran dry. zirv paces supervised work so a window reaches at most a configured percentage (`pace_max_percent`, default 99).

Three data layers, best available wins (facts verified 2026-07-31 on this machine):

1. **Collector (server-authoritative)**: Claude Code's statusline input JSON documents `rate_limits.five_hour.used_percentage` / `.resets_at` and `rate_limits.seven_day.*` (Pro/Max, present after the first API response of a session; each window may be independently absent). The user's `statusLine.command` is wired to `zirv ctx usage tee`, which persists any `rate_limits` fields to a shared state file under the state dir, then chains to the original statusline script unchanged. Every live interactive or wrapped session passively keeps machine-wide window state fresh. No such data is persisted by Claude Code itself, and no headless query exists (both verified).
2. **Estimator (approximation)**: when collector data is stale or absent, sum `usage.*` token fields across all local transcripts (including `subagents/` files) over the trailing window against a configured token budget. Every assistant event carries usage (verified). Whether the subscription limiter weights these token classes identically is unverified; the estimator is labeled an approximation and never overrides fresher collector data.
3. **Circuit breaker (authoritative on trip)**: supervisors match the documented limit-hit message shapes ("You've hit your session limit · resets ...", weekly, and per-model variants) on headless agent output. A trip is treated as 100% regardless of the other layers. The exact machine-readable shape/exit code is not documented and cannot be verified without exhausting a window; the matcher ships docs-verified with a follow-up to confirm empirically.

Pacing behavior: `loop` consults the gate before each cycle, `exec` before each spawn and each restart. At or above `pace_max_percent`, the supervisor waits until the window's `resets_at` (plus jitter; configured fallback delay when unknown) and then continues. A pause is never an exit; a limit-hit mid-run is parked and relaunched after reset without consuming the restart budget. Every pacing decision is appended to the decision log.

## Hooks integration

Thin one-liners registered in each agent's own config:

| Agent config | Hook | Command |
|---|---|---|
| Claude `settings.json` Stop | turn end + score | `zirv ctx hook stop` |
| Claude `settings.json` UserPromptSubmit | marker instruction | `zirv ctx hook prompt` |
| Claude `settings.json` PreCompact | records compaction events, advisory only | `zirv ctx hook pre-compact` |

PreCompact cannot inject instructions into a compaction (its output honors only decision/reason/advisory fields), so compaction focus instructions are delivered exclusively by `wrap` as arguments to the injected `/compact <focus>` command.
| Codex `config.toml` notify | turn end + score | `zirv ctx hook notify` |

`zirv ctx hook stop` scores the session and (a) forwards the verdict to the owning wrap/exec socket when one exists, (b) otherwise emits a non-blocking advisory. It never blocks the agent's stop: blocking was the canary's misfire pain and is retired. The legacy `canary-check.sh` is removed from settings once `ctx` is installed.

## Configuration and state

- Global defaults: `~/.zirv/ctx.toml`. Per-repo overrides: `.zirv/ctx.toml`. Environment variables (`ZIRV_CTX_*`) on top, flags last. TOML parsing uses zirv's existing dependency.
- Tunables: window size, signal weights, verdict thresholds, token floor/ceiling, debounce, restart caps, marker text, distiller model.
- State dir: platform state directory (via `dirs`) under `zirv/ctx/`: handoffs, per-session sockets, supervisor logs. Never inside the repo.

## Error handling

- `wrap` treats the TUI as sacred. Every fallible path returns `Result`; no `unwrap`/`expect` on the hot path. zirv's release profile is `panic = "abort"`, so terminal raw-mode restore must happen in explicit error arms, not unwind-time `Drop`. On any supervision failure (scoring error, dead socket, parse failure) wrap degrades to pure passthrough: a wrapped session must never be worse than an unwrapped one.
- Injection is verified: after sending the compaction command, wrap confirms the transcript records a compaction event within a timeout; otherwise it logs and retreats to advisory. No blind keystroke retries.
- `loop`/`exec`: bounded restarts, exponential backoff, non-zero exits, `on_failure` command hook. Policy stays with callers.
- All supervisor decisions are logged (jsonl in the state dir) for post-hoc audit of any intervention.

## Testing strategy

TDD throughout, matching zirv's existing inline test style and `tempfile` usage.

1. **Rot engine**: pure unit tests over synthetic transcript fixtures. Port the canary's eight cases (healthy, young, warn, block, low-context override, never, stop-guard, boundary) and add per-signal cases: tool-failure spike, repetition loop, marker misses, token floor/ceiling gates, capability-gated marker absence.
2. **Adapters**: parser tests over recorded real transcript samples (claude and codex fixtures checked in, scrubbed).
3. **loop/exec**: integration tests driving a fake agent binary (a test script emitting transcript JSONL and exiting on cue), asserting fresh-session-per-cycle, kill/distill/restart, restart caps, timeout kills, exit codes, `on_failure` invocation.
4. **wrap**: PTY tests against a stub TUI asserting passthrough fidelity, idle detection, injection only at turn boundaries, compaction cooldown, and passthrough degradation when the socket is dead.
5. **handoff**: distiller invocation with a fake model binary; structural-fallback extraction tests.

## Migration and rollout

1. Ship `zirv ctx` (score, handoff, hooks) in zirv 2.5.0; register the Claude hooks; retire `canary-check.sh`.
2. Switch `.zirv/issue-loop.yaml` in zirv-fitness-tracking from launching `claude` + `/loop` to `zirv ctx loop`; wrap worker dispatch in `zirv ctx exec`.
3. Adopt `zirv ctx wrap` interactively via shell alias.

Each step is independently shippable and reversible. Dependencies added to zirv: `portable-pty`, `uuid`. Transcript watching is polling-based; no `notify` crate.

## Versioning

zirv 2.4.0 -> 2.5.0 (minor: new feature, no breaking changes to existing script-runner behavior).
