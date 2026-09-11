# Native conversation journal and execution receipts (issue #472)

**Date:** 2026-09-11 · **Issue:** #472 (step N03 of the native-runtime roadmap, #469) · **Status:** decided; native provider loops consume this in N07–N09

## Context

The existing `NormalizedEvent` stream is deliberately lossy. It carries the
small hashes, sizes, timestamps, and text needed by rot scoring, while
external harnesses own their actual conversations. A Zirv-owned runtime cannot
recover, render, or safely continue from that projection. It needs one durable
record of acknowledged input, complete assistant blocks, requests, tool
effects, usage, task receipts, and checkpoints.

## Decision

`runtime::journal::Journal` stores native conversation facts in schema-v1
SQLite at `<state>/native-journal.sqlite`. WAL allows readers to inspect a
session while its runtime writes. `IMMEDIATE` transactions allocate a
monotonic per-session sequence and commit the event plus the new sequence
together. Every mutation checks the current seat generation, so a stale
runtime cannot append after rollover.

The schema keeps session, seat, generation, task, turn, request-attempt,
message, tool-call, execution, usage, checkpoint, and sequence identities
explicit. A session also records the exact N02 route, provider, endpoint,
account, billing pool, protocol, vendor, and model. Unknown schema versions,
unversioned tables, failed integrity checks, malformed event payloads, and
event-type mismatches fail closed; Zirv does not delete or silently repair a
database it cannot interpret.

### Committed facts and streaming drafts

Streaming frames live in a separate transient table. Each frame is capped at
64 KiB and a draft at 8 MiB. The completion barrier concatenates frames into
typed assistant blocks or parses a complete JSON-object tool call, appends one
committed event, and deletes the frames in the same transaction. Drafts never
appear in replay. Invalid or truncated tool arguments stay drafts and cannot
become executable calls.

Large tool results may be stored as SHA-256-addressed artifacts in the same
private database; their journal event carries a stable hash, exact byte count,
content fingerprint, and media type. Small results remain inline.

### Tool-effect recovery

Executions follow a checked state machine:

`prepared → started → completed | failed | cancelled | outcome_unknown`

Prepared intent is durable before an effect begins. A runtime recovering from
a crash explicitly converts every latest `started` execution to
`outcome_unknown` in one transaction. It may then complete/fail/cancel the
execution only after reconciliation or an idempotency guarantee; it cannot
move it back to `started`. Completed and failed states require an authoritative
result. No open/replay path repeats an effect.

### Portable and provider-specific state

Opaque provider continuation envelopes are stored in a separate table and are
returned only for an exact match of route, provider, endpoint, account,
protocol, vendor, and model. They are absent from portable replay and from the
scoring projection. Checkpoints contain provider-neutral JSON and identify
recovery, compaction, or handoff intent.

### Ownership

The native runtime service is the only writer for a native session. The
journal does not duplicate `sessions.rs`, `seat.rs`, `task.rs`, `mail.rs`,
workflow state, or policy state; it records their identifiers and immutable
receipts/provenance. N20 binds the writer lifecycle to the persistent service
and public protocol rather than introducing a competing lease here.

## Replay and compatibility

Replay reads committed events in sequence order and deterministically rebuilds
messages, usage, tool calls, latest execution states, task receipts,
checkpoints, generation, and completion. A second deterministic projection
emits the existing `NormalizedEvent` vocabulary for rot scoring. `rot.rs`
remains unchanged and pure.

Completed-session retention deletes the session and its event, draft, and
continuation rows through foreign-key cascades. Content-addressed artifacts
are shared and retained independently. Existing harness transcripts and
legacy state files are untouched; older Zirv binaries ignore the new private
database, so side-by-side operation and downgrade preserve both histories.

## Verification

Inline deterministic tests cover schema creation/refusal, corrupt and
malformed tails, sequence and generation fencing, replay equivalence,
execution crash reconciliation, completed-message barriers, incomplete tool
arguments, frame bounds, provider-envelope identity, content addressing,
concurrent WAL readers, normalized projection, owner-only file permissions,
and retention cascades. Live provider credentials are neither needed nor
read.
