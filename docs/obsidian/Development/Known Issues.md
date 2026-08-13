---
last-verified: 2026-08-13
---

# Known Issues

Gotchas that have cost debugging time. Remove an entry once it's resolved — this
file tracks live traps, not history (use [[Decision Log]] or [[Work Journal]]
for that).

Each entry gets a changelog comment at the top of the file, newest first:

```
<!-- Updated YYYY-MM-DD (branch, state): what changed -->
```

<!-- Updated 2026-08-13 (feat/dashboard, docs sweep): dashboard panes carry no rot score yet -->
<!-- Updated 2026-08-13 (feat/agent-coordination, review round): markdown header absorption; registry short is a stable address; supervision env scrubbed on every spawn -->
<!-- Updated 2026-08-13 (feat/agent-coordination, console-safety round): portable-pty do_kill inversion; ConPTY control-byte broadcast; empty nudge prefixes -->

## A dashboard pane carries no rot score yet

`ui::HeaderFacts` has a `score: Option<u32>` field, and the header renders
`score NN` when it's `Some`, but the dashboard's real render loop
(`assemble_header_facts` in `run_dashboard`) always passes `None` -- no
[[Rot Engine]] transcript scoring is wired up for a pane yet, unlike `wrap`'s
own status bar. A pane still runs fully supervised otherwise (turn-signal env,
quit sequence, quit/restore roster), it just never advises/compacts/restarts
itself the way a plain `zirv ctx wrap` session does. Do not assume a pane
attached in the dashboard is rot-monitored just because it looks identical to
one running under `wrap` directly.

## A markdown header block ends at the first blank line

`mail::parse_markdown` and `memory::parse_markdown` both read a `## Message` /
`## Memory` header of `- key: value` bullets followed by a free-form body. The
header block ends at the **first blank line** -- the one `to_markdown` always
writes after the last bullet -- and everything after it is body, verbatim.

This is a trust boundary, not a formatting detail. Both bodies are
agent-authored text. When a blank line merely `continue`d (leaving the parser
in header mode), a body whose first line happened to be a `- key: value`
bullet was absorbed as header: a mail message could re-address itself
(`- To-session: victim`) or forge its sender, and a memory entry could rewrite
the `Key` it is filed under or promote itself from `handoff` to `explicit`. It
also silently ate any honest bulleted body (`- build: cargo build`), which is
how it was first noticed.

If either parser grows a new header field, keep the terminator rule intact.

## A supervisor's registry short id is a stable address, not its session id

`Record.short` is minted once at `SessionGuard::register` and deliberately
**not** rotated by `refresh_session`, even though `Record.session` is. It is
the address `resolve_prefix` hands a sender, what `send --to-session` and
`zirv ctx nudge` store on a message, and what `zirv ctx status` prints.

Rotating it (which is what `loop` did per cycle and `exec` per restart) made
every message addressed to a live session undeliverable the instant that
session was replaced -- the sender resolved a real address and the supervisor
then stopped answering to it. Every mail listing a supervisor performs on its
own behalf must therefore be scoped to the registry short, never to
`short_id(current session)`.

Consequences to preserve: `loop` filters on it too (passing `None` made a loop
swallow *and consume* mail addressed to other sessions), and `exec`'s nudge
marker is claimed under it (deriving it from `session` meant a nudge sent after
the first restart was never claimed).

## Anything spawned from a supervised session must have its supervision env scrubbed

`sessions::SUPERVISION_ENV` (`ZIRV_CTX_SESSION`, `ZIRV_CTX_SOCKET`,
`ZIRV_CTX_TRANSCRIPT`) has to be `env_remove`d from **every** child command
before the spawner sets whichever of it it owns -- `portable_pty::CommandBuilder::new`
and `std::process::Command` both inherit the parent environment, so "not set"
means "inherited", not "absent".

This is not limited to the supervisors' own agent children. It also covers
`handoff::run_model` (the distiller, and therefore `memory::harvest_from_handoff`,
which spawns through it) and `resume`'s hand-over launch. A distiller that
inherits its parent's session id posts turn signals into the parent's own rot
engine while the parent sits blocked waiting for that very call to return.
<!-- Updated 2026-08-12 (feat/obsidian-vault, seeded): initial gotchas pulled from repo CLAUDE.md -->

## portable-pty's Windows `do_kill` inverts its own success check

`WinChild::do_kill` in portable-pty 0.9.0 (`src/win/mod.rs`, lines 41–50) reads:

```rust
let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
let err = IoError::last_os_error();
if res != 0 { Err(err) } else { Ok(()) }
```

Win32 `TerminateProcess` returns **non-zero on success**, so this reports a
successful kill as an error and a failed one as success. `ChildKiller::kill`
then swallows the result with `.ok()` anyway, so zirv never learns a kill
failed: `child.kill()` in `wrap::quit_child` always looks like it worked.

Do **not** vendor or patch portable-pty for this. Treat `kill()` as
best-effort and never build logic on its return value — `try_wait()` /
`wait_for_exit` are the only trustworthy evidence a child is actually gone,
and `quit_child` already keys on those.

## Never write a control byte into a pty master

Writing `\x03` (or any console control byte) into the pty master is not a
signal to *that one child*. On Windows the master is a ConPTY and conhost
turns the byte into a console control event broadcast to **every** process
attached to the pseudoconsole; portable-pty 0.9.0 spawns without
`CREATE_NEW_PROCESS_GROUP`, so there is no process group to narrow it to. On
unix the line discipline delivers SIGINT to the whole foreground process
group of that pty — better, but still not one process.

This is why `wrap::quit_child`'s ladder is quit sequence → grace →
`child.kill()` with nothing in between, and why the removed Ctrl-C rung must
not come back. See the [[Decision Log]] entry for 2026-08-13.

## An empty or very short `zirv ctx nudge` prefix is refused

`sessions::resolve_prefix` accepts any *unique* prefix, including `""`
(`starts_with("")` is always true). That is fine for a read-only lookup but
not for a nudge, which wakes and — in `exec` — restarts the session it
resolves to: a single mistyped character can still be unique. `zirv ctx
nudge` therefore refuses any prefix shorter than four characters
(`sessions::MIN_NUDGE_PREFIX`) unless it exactly equals a live session's
whole short id.

Test helpers that used to lean on the empty prefix (`exec`'s
`nudge_live_session`, `wrap`'s interactive-nudge test) now resolve the live
short id from the registry and pass it whole. A test that still passes `""`
fails with `prefix too short`, and — if its cleanup runs after the call —
can leak `FAKE_AGENT_*` environment variables into every later test in the
same process.

## `ctx` shadows `.zirv/ctx.yaml`

`zirv ctx` is a built-in resolved in `main.rs` before YAML script lookup, so a
`.zirv/ctx.yaml` script named `ctx` is silently shadowed and never runs.
`.zirv/ctx.toml` is a different file — it's the ctx config, and it's excluded
from script listing in `help.rs`.

## Tests must run with `--test-threads=1`

`cargo test --verbose -- --test-threads=1` is required, not optional — tests
share state (state dir, fixtures) and will flake or corrupt each other under
the default parallel test runner.

## `wrap`'s hot path assumes `panic = "abort"`

The release profile is `panic = "abort"`, so a panic on `wrap`'s hot path
cannot unwind to a cleanup handler — raw-mode terminal restore must happen in
explicit arms, not in a `Drop` guard relying on unwind. No `unwrap`/`expect` on
that path; any supervision failure must degrade to pure passthrough instead of
leaving the terminal in raw mode.
