---
last-verified: 2026-08-12
---

# Context Management

> [!tip] Quick Reference
> - `zirv ctx` exists because long-running AI-agent sessions rot: as the context window fills, instruction-following slips, tools get called in loops, hallucinations appear — and built-in auto-compaction fires only after degradation has already started.
> - A pure, deterministic **rot engine** scores a normalized event stream and hands a verdict (`healthy`/`advise`/`compact`/`restart`) to one of three **supervisors** (`loop`, `exec`, `wrap`), which act on it — compacting, restarting with a distilled handoff, or leaving the session alone.
> - **Usage pacing** keeps autonomous work from dying mid-run when a subscription usage window runs dry.
> - This page is the *why*; for the *how* see [[Ctx Subsystem]] (verb tree/dispatch), [[Rot Engine]] (scoring internals), [[Ctx Supervisors]] (loop/exec/wrap mechanics), [[Ctx Adapters]] (agent-specific plumbing), and [[Usage and Pacing]].

> [!warning] If changed
> If the philosophy or verb set changes, re-check this page against the approved design docs (`docs/superpowers/specs/2026-07-31-zirv-ctx-context-management-design.md` and `2026-08-01-zirv-ctx-optimize-and-run-design.md`) and update [[Ctx Subsystem]] alongside it.

## The problem it replaces

Before `zirv ctx`, the mitigation was a shell Stop-hook "canary" watching for a fixed reply-prefix marker — a single noisy signal that sometimes misfired and, on trigger, could only warn; it had no way to clean up context itself. Recovery meant a manual `/compact` or session restart. Two use cases made this acute: a long-lived orchestrator session whose context grows every loop cycle, and headless workers that can rot mid-run on a single large task.

## The rot engine: score, don't guess

`zirv ctx score` is a pure function: transcript in, verdict out, same input always producing the same output — no clock, no filesystem, no environment reads inside the scoring logic itself (`rot.rs`). It combines several signals over a trailing window of turns:

- **Real context size** (a gate, not a vote): the most recent reported input-token count. Below a floor the verdict is always `healthy`; above a ceiling it's at least `compact`.
- **Tool-failure rate**: fraction of tool results that errored in the window.
- **Repetition loops**: the same `(tool, input)` pair repeating, a classic sign of a stuck agent.
- **Instruction-following marker** (optional, capability-gated): miss rate of a configured reply-prefix, when the agent supports the hook that emits it.

Determinism is what makes this testable and trustworthy: the canary's synthetic test transcripts port directly into unit tests. See [[Rot Engine]] for the exact thresholds and event model.

## Agent-agnostic by design

Everything agent-specific — how to launch a session, where its transcript lives, how to parse it into normalized events, how to inject a compaction command — sits behind an `AgentAdapter` trait, so the rot engine and supervisors never know which agent they're watching. Claude Code is implemented; Codex is not yet (tracked as issue #11, and `zirv ctx --help` says so explicitly rather than implying otherwise). See [[Ctx Adapters]].

## Supervisors: acting on a verdict without making things worse

Three supervisors turn a verdict into action, each suited to a different session shape:

- **`wrap`** babysits an *interactive* TUI session over a PTY, proxying keystrokes byte-for-byte. Intervention only happens at a verified turn boundary *and* while the user is idle, so the wrapped experience is indistinguishable from an unwrapped one until it actually intervenes. Its escalation ladder: `advise` prints a one-line note, `compact` injects the adapter's compaction command with focus instructions, `restart` distills a handoff, quits, and relaunches with that handoff as the opening prompt. The governing invariant — stated directly in project convention — is that a wrapped session must never be worse than an unwrapped one; any supervision failure degrades to plain passthrough.
- **`exec`** supervises one *headless* run: tails its transcript, scores periodically, and on a `restart` verdict or timeout kills, distills, and relaunches with a bounded restart budget before giving up and exiting non-zero for the caller's own policy to handle.
- **`loop`** replaces a long-lived orchestrator session outright: every cycle launches a *fresh* headless session with a clean context, so orchestrator-side rot becomes structurally impossible rather than something to detect and fix.

See [[Ctx Supervisors]] for the mechanics of each.

## Handoff and resume: never ask a rotted session to summarize itself

Distillation always runs as a **fresh** headless model call over the transcript tail — never the rotting session itself — using a cheap model and a versioned prompt template, producing a Task/Done/Remaining/Next-step/Files-touched/Gotchas markdown handoff. If that call fails, a structural fallback extracts a handoff mechanically (recent user messages plus files touched), so a restart always has *something* to stand on. `zirv ctx resume` starts a clean interactive session with the latest handoff for the current repo injected as the opening prompt — the seamless-handoff experience the score/handoff/resume/status verbs exist to support: a human or an automation can inspect state (`status`), force a restart with continuity preserved (`handoff` + `resume`), without losing task context across the boundary.

## Usage pacing

Autonomous loops must not die mid-run because a subscription usage window (a rolling 5-hour or 7-day limit) ran dry. `zirv ctx usage` layers three sources of truth — a server-authoritative collector (fed by tee-ing the statusline), a token-count estimator fallback, and an output-matching circuit breaker as the last resort — and supervisors pace work so a window reaches at most a configured percentage before waiting out the reset. See [[Usage and Pacing]].

## A consistent session prompt

Every session zirv starts (`wrap`, `exec`, `loop`, `resume`) gets a small injected system prompt so behavior is consistent run to run: a shipped default floor, optionally extended by a user layer and a repo layer. `--simple` skips all of it. The repo layer crosses a trust boundary worth its own page — see [[Untrusted Configuration]].

## zirv as a meta-harness: chat, delegation, and a mailbox

`zirv ctx chat` (also reachable as the top-level alias `zirv chat`, or as bare `zirv` in a repo with a `.zirv` directory and a real terminal attached — see [[Built-in Commands]]) is the interactive seat: a `wrap`-supervised session tagged `PromptRole::Orchestrator`, which is what makes it — and only it — eligible for the harness-teaching prompt layer (`HARNESS_PROMPT` in `prompt.rs`). That layer is what turns zirv from "a supervisor watching one session" into a small meta-harness the orchestrator session is told about directly: it names `zirv ctx agent` for delegating a task to another enabled harness, `zirv ctx send`/`zirv ctx inbox` for leaving and reading short notes between sessions, and `zirv ctx status` for checking which harnesses are enabled and whether mail is waiting.

- **Delegation.** `zirv ctx agent <name> <prompt>` (also `zirv agent`) is a one-shot dispatch to a supervised headless worker on another enabled harness, reusing `exec`'s full pacing/rot/restart machinery. A delegated run is always tagged `PromptRole::Worker`, never `Orchestrator` — it gets the shipped default and the adapter's own base layer, but never the harness-teaching layer, so a worker cannot itself be taught to delegate further. This is the same "don't make things worse" instinct as `wrap`'s own safety contract, applied to recursion instead of raw-mode terminals.
- **Mailbox.** `zirv ctx send`/`zirv ctx inbox` let agent sessions on the same machine leave each other short markdown notes, scoped to the current repo. Full message bodies are delivered into a headless **Worker** session's composed prompt as its own layer (after the repo layer, before the command-line layer) — `zirv ctx exec` (including a delegated `zirv ctx agent` run) once at launch, `zirv ctx loop` fresh every cycle — labeled as information from another session, not as an instruction, and consumed right after so it is not redelivered (see [[Untrusted Configuration]]). An interactive **Orchestrator** session (`zirv ctx chat`, via `wrap`) never gets message bodies in its own prompt at all: it gets a one-line unread-count advisory on the announcement channel instead, since the orchestrator seat is where a human decides what to go read.
- **Discoverability.** `zirv ctx status` (see [[Ctx Subsystem]]) reports both halves of this story without a human having to remember the verbs: a `chat:` line naming the adapter `zirv ctx chat` would actually launch and the rule that picked it, and a `mail: N unread` count for the current repo.

See [[Ctx Subsystem]] for the `chat`/`agent`/`send`/`inbox` verbs themselves, and [[Ctx Supervisors]] for how the role parameter, mail advisory, and terminal chrome (banner, status bar, `zirv ▸` event channel) fit into the `wrap` pump loop that actually drives an interactive session.
