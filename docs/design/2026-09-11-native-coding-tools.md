# Native coding tools and persistent processes

**Date:** 2026-09-11 · **Issue:** #474 · **Roadmap:** #469 N05

## Outcome

`src/commands/ctx/runtime/tools/` is Zirv's provider-neutral coding tool
service. It is callable by the native provider/agent loop planned in N07-N09;
it does not expose a second public CLI or change the still-default harness
backend.

The registry has twelve stable tools:

| Tool | Contract |
|---|---|
| `file_read` | bounded line range; text encoding/line ending/hash metadata; binary/image-safe fallback |
| `directory_list` | bounded sorted entries; recursive traversal never follows symlink directories |
| `glob_search` | relative `*`, `?`, and `**` patterns over a claimed root |
| `text_search` | fixed or regex search with include glob, Unicode paths, and binary skipping |
| `file_write` | atomic create/replace with idempotency and SHA-256 precondition |
| `apply_patch` | ordered exact-content replacements with full-file hash and occurrence preconditions |
| `process_start` | explicit argv or typed shell launch through N04 isolation |
| `process_poll` | non-blocking bounded output delta and status |
| `process_wait` | wait capped at 60 seconds |
| `process_write` | pipe or PTY input and explicit input close |
| `process_terminate` | process-tree cancellation and reap |
| `output_read` | bounded line/byte retrieval from an opaque existing output id |

Every definition carries a closed JSON schema, capability requirements,
execution mode, resource-claim kinds, cancellation behavior, retry policy,
and structured error vocabulary. Deserialization also uses
`deny_unknown_fields`; schema metadata is not mistaken for runtime
validation.

## Authorization boundary

The tool name and arguments are untrusted provider output. Execution order is:

1. Require one complete JSON object below the argument-size limit.
2. Resolve the stable name in the closed registry and deserialize its exact
   typed payload. Run semantic checks for paths, handles, ranges, counts,
   timeouts, and idempotency keys.
3. Build an N04 `ExecutionAction` and authorize it against the current seat,
   generation, policy, resource claims, approval, and writer permit.
4. Use only `Authorization::resolved_paths()` for filesystem access. Never
   reopen the provider's unresolved path spelling.
5. For a process, call `ExecutionBroker::prepare_process` immediately before
   spawn so policy/scope changes invalidate the prior authorization.
6. When journal identifiers are supplied, record `Prepared` and `Started`
   before the effect, then a bounded terminal receipt afterwards.

Process environment overrides were added to `ProcessInvocation` itself, so
they participate in the action and approval digest. The broker refuses NUL,
invalid names, configured provider secrets, common token/key/password
variables, SSH/askpass channels, and cloud credential selectors. The sandbox
receives the scrubbed inherited map plus validated overrides.

Process access flags are requests for sandbox bindings, not statements the
model is trusted to make. `read_only=true` leaves the worktree read-only;
omitting network, outside writes, or git metadata leaves those resources
unavailable. Exact argv git mutations are conservatively classified. Typed
shell mode conservatively requests git-metadata and destructive-git policy,
because an arbitrary script cannot be safely inferred as read-only.

## File safety

Reads are capped and distinguish UTF-8, UTF-8 BOM, UTF-16LE, UTF-16BE,
images, and other binary data. Large or binary content is written byte-for-
byte to the shared output store and represented inline only by bounded
metadata plus an opaque retrieval id.

Writes never truncate a target in place. The existing `state::write_atomic`
primitive now has a byte-preserving counterpart: write a sibling, retain an
existing regular file's permissions, close it, then atomically rename it over
the destination. Existing files require the hash observed by the caller;
repeating a completed write whose desired bytes already match reconciles as
`already_applied`. A patch requires the full prior hash and an exact expected
occurrence count for each replacement, so stale or ambiguous edits refuse
without changing the file. BOM/encoding and consistent CRLF/LF/CR style are
preserved.

Directory walking uses `symlink_metadata` and never descends into a symlink or
junction. N04 canonical resolution still runs before the traversal, so the
claimed root itself cannot be a path escape.

## Process lifecycle and output

Each successful spawn receives a random opaque handle retained by one
`ProcessManager`. A repeated idempotency key returns the existing handle
instead of spawning again. The manager enforces a live-process cap and uses
the existing heavy-command classifier and RAII heavy permit for builds,
tests, clippy, packaging, and operator-added patterns.

Non-interactive processes use piped stdin/stdout/stderr and an isolated
process group. Interactive processes alone use portable-pty (ConPTY on
Windows). Both paths register Zirv's existing `ChildGuard`; timeout,
termination, manager drop, and normal exit release guards and permits only
after the child is reaped. Output reader threads drain continuously, keeping
children responsive even when the caller polls slowly.

`StreamingCapture` reserves one file in the existing per-repository output
store before spawn. Reader chunks append raw bytes directly; no unbounded
runtime buffer and no second evidence store exist. On terminal state, the
capture is finalized once through the existing compaction classifier,
operator filters, metadata sidecar, and retention policy. Poll/wait responses
contain only a configured inline byte window; the terminal response includes
the full output id and bounded summary. Non-UTF-8 bytes remain exact on disk.

Arbitrary child processes cannot transparently survive a Zirv runtime crash.
The N03 recovery rule converts any journal execution left at `Started` to
`OutcomeUnknown`. Read-only calls are `Safe`; preconditioned writes and local
process controls are `Reconcile`; network/outside/destructive process starts
are `NeverAfterStart`. A durable-receipt failure after an effect similarly
returns `OutcomeUnknown` rather than claiming success.

## Platform evidence and limits

The focused `Native Tools` CI matrix compiles and runs the registry/file
contract on Ubuntu, macOS, and Windows. Unix runners additionally exercise
real long-running process polling, bounded wait, cancellation, idempotent
handles, and raw non-UTF-8 output. Cross-platform pure tests pin argv and
environment values without shell reparsing, Unicode paths, line-ending/BOM
preservation, malformed argument refusal, binary/image handling, and
symlink/junction non-traversal.

N04's honest platform gate still applies. Linux needs executable bubblewrap,
macOS needs Seatbelt, and Windows process execution stays unavailable until
the restricted-token/AppContainer helper is shipped. The registry and file
tools are portable now; this issue does not mislabel unavailable arbitrary
process containment as feature parity. PTY processes and opaque handles are
runtime-process-local, and the design does not claim they survive a crash.
