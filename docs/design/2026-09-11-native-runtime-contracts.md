# Native runtime: shared contracts, harness backend preserved (issue #470)

**Date:** 2026-09-11 · **Issue:** #470 (step N01 of the native-runtime roadmap, #469) · **Status:** decided, no behaviour change yet

## 1. Context

zirv's `ctx` supervisor drives every harness (Claude Code, Codex, and the rest
of `src/commands/ctx/adapters/`) the same way: build a `std::process::Command`
via the `AgentAdapter` trait (`headless_cmd`, `interactive_cmd`,
`distiller_cmd`, `dispatch_agent`, ...), spawn it through the
`supervise::spawn_tapped` chokepoint (or a PTY for `wrap`), and read its
transcript back off disk. That contract is *process-shaped*: it assumes the
thing being supervised is an external binary zirv can only watch from
outside.

Issues #353 and #352 describe a different execution mode zirv wants to grow
into: a **native** runtime that drives a model directly (its own provider
calls, its own tool executor, its own journal) rather than shelling out to a
vendor CLI. The wrong way to build that is to make "native" mean "zirv
re-execs itself and pretends to be another harness" -- a recursive subprocess
wearing an `AgentAdapter` costume would inherit every process-supervision
assumption (PTY sizing, transcript-file polling, exit-code contracts) that
the native path exists to remove, while adding a layer of self-impersonation
that makes failures harder to diagnose, not easier.

This step (N01) does not build the native runtime. It introduces the seam a
native backend will later plug into, and proves the seam is load-bearing by
routing 100% of today's behaviour through it unchanged. See
`docs/design/native-runtime-inventory.md` for the exhaustive per-command,
per-model-call-site ownership map this decision is checked against, and
`docs/benchmarks/native-runtime-baseline.md` for the quality baseline
recorded before anything is allowed to migrate off the harness backend.

## 2. Decision

### 2.1 `RuntimeBackend` trait and two implementations

A new module, `src/commands/ctx/runtime/`, defines:

```rust
trait RuntimeBackend: std::fmt::Debug {
    fn kind(&self) -> RuntimeKind;
    fn capabilities(&self) -> RuntimeCapabilities;
    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle>;
    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()>;
    fn steer(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()>;
    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()>;
    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle>;
    fn subscribe(&mut self, session: &SessionHandle, after_revision: u64) -> CtxResult<Vec<EventEnvelope>>;
}
```

`subscribe` returns an already-materialized `Vec<EventEnvelope>`, not a
stream type -- there is no daemon or long-lived connection in this step (see
2.3), so a caller polls with `after_revision` rather than holding a live
subscription open.

Two backends implement it:

- **`HarnessBackend`** -- a facade over the *existing* `AgentAdapter` trait
  and the `supervise::spawn_tapped` chokepoint. Nothing is rerouted by this
  step: `chat`, `wrap`, `exec`, `loop`, `agent`, and every `workflow`
  model-calling path keep launching through their original adapters exactly
  as they do today. `HarnessBackend` exists so those paths have a
  `RuntimeBackend`-shaped seam to be called through later, not so they
  behave differently now.
- **`FakeNativeBackend`** -- deterministic, no process spawn, no wall clock,
  no real model call. It exists for this step's own tests and for N02-N09 to
  develop against before a real provider call lands.

`runtime::select(RuntimeKind, adapter)` resolves which backend a
`SessionSpec` should use. `select(RuntimeKind::Native, ..)` returns a
`RuntimeError::Unsupported` naming roadmap #469's steps N02-N09 by number --
a clear, typed refusal, not a silent fallback to the harness backend -- until
those steps land a real native backend behind it.

### 2.2 Six separated axes on `SessionSpec`

Today's session identity conflates several independent facts. `SessionSpec`
splits them explicitly:

| Axis | Type | Example values |
|---|---|---|
| Execution backend | `RuntimeKind` | `Harness`, `Native` |
| Logical agent role | (existing role type) | orchestrator, reviewer, worker |
| Provider route | (new, N02) | anthropic, openai, ... |
| Model | `String` | as today |
| Session identity | (existing session id) | as today |
| UI surface | `UiSurface` | headless, PTY/wrap, dashboard pane, chat |

Separating these means a native coordinator driving wrapped (harness-backend)
workers is representable, and so is the reverse (a harness-backend
orchestrator delegating to a native worker once one exists) -- neither
combination requires its own bespoke session type, because backend, role,
and surface were never coupled in the type to begin with.

### 2.3 Versioned in-process protocol v1

`src/commands/ctx/runtime/protocol.rs` defines the wire shapes a
`RuntimeBackend` speaks, versioned from day one even though nothing crosses
a process boundary yet:

- `CommandEnvelope { version, id, command: RuntimeCommand }` and
  `RuntimeCommand` (start/submit/steer/interrupt/resume/subscribe, matching
  the trait).
- `EventEnvelope { version, revision, session_id, generation, event: RuntimeEvent }`
  on the wire (`version, revision, session, generation, event` as Rust field
  names -- `session` is `#[serde(rename = "session_id")]` only, because
  `RuntimeEvent::Started { session: SessionHandle }` flattens into the same
  JSON object and two flattened fields cannot share one wire key). `revision`
  increases monotonically per session; `generation` is stamped on every event
  so a replacement session (a rollover, a resumed conversation) can never
  satisfy an old subscriber expecting the prior generation's stream -- the
  same fencing `seat.rs`'s own `generation` already gives process-based
  sessions, reused rather than duplicated (see 2.5).
- `ReplyEnvelope { version, id, reply: RuntimeReply }` and a structured
  `ErrorCode` enum: `version_mismatch`, `unknown_command`, `unsupported`,
  `unknown_session`, `busy`, `stale_generation`, `backend`, `unknown`.
  `protocol::dispatch` downcasts a backend's `Box<dyn Error>` to
  `runtime::RuntimeError { Unsupported, UnknownSession, Busy,
  StaleGeneration { expected, got } }` and maps each 1:1 to the
  same-named `ErrorCode`; any other error (a plain I/O failure, a spawn
  failure) maps to `ErrorCode::Backend`.
- Every enum (`RuntimeCommand`, `RuntimeEvent`, `RuntimeReply`, `ErrorCode`)
  carries an `unknown` fallback variant, and every envelope tolerates unknown
  fields on deserialize -- so a future protocol version can add variants
  without breaking an older reader, per issue #353's own compatibility
  requirement.

No daemon and no socket exist yet -- everything above is called in-process,
function to function. `tests/fixtures/runtime/v1/` freezes example wire
payloads for every envelope shape specifically so a later refactor cannot
silently change what v1 meant.

### 2.4 `SessionHandle`

```rust
struct SessionHandle {
    runtime: RuntimeKind,
    logical_id: String, // zirv's session uuid, as a string
    short: String,
    generation: u64,
    role: String,
    surface: UiSurface,
    conversation: Option<BackendConversationRef>,
}
```

`attached(surface)` produces a new handle with only the `surface` field
changed -- reattaching a dashboard pane to a chat window, or vice versa,
never touches `logical_id`, `role`, or `generation`. Identity and role
survive a UI change exactly because the type does not let a surface change
touch them.

### 2.5 Generation semantics now, seat binding later

Each backend tracks its own per-session generation on `SessionHandle` today
-- not `seat.rs`'s counter, since neither backend touches `seat` yet -- with
the same semantics `Seat.generation` already gives process-based sessions: a
`resume` bumps it (for `HarnessBackend`, only for a session it started in
this process; for a session it did not start, the common cross-process case
today, `resume` passes the handle's generation through unchanged), and a
command issued against a handle whose generation has since gone stale is
refused with `RuntimeError::StaleGeneration` / `ErrorCode::stale_generation`,
before anything else runs. Both `FakeNativeBackend` and `HarnessBackend`
enforce this. Cross-process enforcement arrives with the seat binding deferred
to N09/N16/N19. Binding a session's generation to the *persisted*
`seat::Seat.generation` (so the two counters are the same number, not just the
same shape) happens once a live caller actually routes through this facade --
the agent loop (N09, #478), a native coordinator (N16, #485), and rollover
(N19, #488) are each responsible for that binding at their own call site, not
introduced speculatively here.

### 2.6 Persisted, explicit runtime selection

`sessions::Record`, `seat::Seat`, and the `.conversation` marker
(`native_conversation()`/`record_native_conversation()` in `sessions.rs`)
each gain a `runtime` field, `#[serde(default)]`ing to `RuntimeKind::Harness`
so every record written by today's binary still parses unchanged.
`native_conversation()`'s match is extended to require agent, session, **and
runtime** to all agree before a resume is allowed to reuse a recorded
conversation -- a resume can never silently cross from a harness-backend
conversation to a native one (or back) just because the agent name and
session id happened to match. Recording the field is this step's job;
`sessions.rs`/`seat.rs` themselves are owned by a concurrent change landing
alongside this one and are not edited here.

### 2.7 One authority per mutable state domain

The native runtime introduces no parallel copies of task state, mail,
workflow state, context, or policy. `zirv ctx task`, `zirv ctx mail`,
`zirv workflow`, `zirv context`, and permission/sandbox policy keep exactly
one authority each, regardless of which `RuntimeBackend` a given session
uses. Where a later native step needs a shared entry point that today is
only reachable from harness-specific code, that entry point is extracted
from the existing owner at the point of need (N04 for permissions, N06 for
memory/skills compile, and so on) -- never forked.

### 2.8 Quality baseline recorded first

`docs/benchmarks/native-runtime-baseline.md` and its companion
`native-runtime-baseline.jsonl` define the representative-task protocol and
the non-regression rule a native run must clear before any default migrates
off the harness backend: no worse on first-pass gates or confirmed findings,
and honest token/wall-time reporting. The file is created empty by this
step; the orchestrator appends the first real recorded run.

## 3. Consequences

- Every existing chat/wrap/exec/loop/agent/workflow path keeps launching
  through its original `AgentAdapter` today -- this step changes no runtime
  behaviour, only adds the seam and the persisted `runtime` field (defaulted
  to `harness` everywhere).
- `docs/design/native-runtime-inventory.md` plus its enforcing check,
  `ZCHK-RUNTIME-INVENTORY` (`zirv verify --builtin`), give every future
  native step (N02-N23) a concrete list of which command verbs and which
  model-calling call sites it is responsible for migrating, and fail the
  build the moment a new command or model-calling call site lands without an
  owner.
- A resumed session can never silently switch `RuntimeKind` out from under
  an operator: the persisted field plus `native_conversation()`'s stricter
  match make that structurally impossible rather than a documented
  convention.
- Protocol v1's frozen fixtures mean a later step that touches
  `runtime/protocol.rs` gets a concrete, versioned diff to review rather
  than a "trust me, it's still compatible" claim.

## 4. Not decided here

- **Socket or named-pipe transport, and a daemon process** (#353/#352,
  step N20). Protocol v1 is in-process only; nothing here chooses a wire
  transport.
- **Native tool executor and sandbox** (N04/N05).
- **Provider credentials and routing** (N02).
- **Journal/receipts format** (N03).
- **A CLI flag for selecting `native`** -- deferred until a real native
  backend exists behind `RuntimeKind::Native`; exposing the flag before then
  would only ever produce the "not available yet" refusal.
