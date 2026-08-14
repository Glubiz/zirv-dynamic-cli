---
last-verified: 2026-08-14
---

# Known Issues

Gotchas that have cost debugging time. Remove an entry once it's resolved — this
file tracks live traps, not history (use [[Decision Log]] or [[Work Journal]]
for that).

Each entry gets a changelog comment at the top of the file, newest first:

```
<!-- Updated YYYY-MM-DD (branch, state): what changed -->
```

<!-- Updated 2026-08-14 (feat/dashboard, security round): cmd.exe argv-reparse injection class recorded, with the two shipped defenses and the deferred file-preference hardening -->
<!-- Updated 2026-08-14 (feat/agent-coordination, mail trust round): two latent traps recorded -- exec/loop's mail gate keys off prompt composition; wrap's status bar paints without raw mode -->
<!-- Updated 2026-08-14 (feat/dashboard, review fixes): `Ord::clamp` panics on a zero-width rect -->
<!-- Updated 2026-08-13 (feat/dashboard, docs sweep): dashboard panes carry no rot score yet -->
<!-- Updated 2026-08-13 (feat/agent-coordination, review round): markdown header absorption; registry short is a stable address; supervision env scrubbed on every spawn -->
<!-- Updated 2026-08-13 (feat/agent-coordination, console-safety round): portable-pty do_kill inversion; ConPTY control-byte broadcast; empty nudge prefixes -->

## Windows `cmd.exe` argv reparse: repo config can reach a shell command line

On Windows, `adapters::resolve_program` rewrites an npm-installed `claude.cmd`
(or `.bat`) to `cmd.exe /c <shim>`. cmd.exe then **re-parses the whole
appended command line** before invoking the shim, so any downstream argv
element bearing a cmd.exe metacharacter (`& | < > ^ ( ) % ! "` newline) is
interpreted as a *command*, not passed through as a literal argument.
portable-pty and `std::process` both append no-whitespace metachar args RAW,
and an embedded `"` defeats any quoting they add (BatBadBut / CVE-2024-24576
quote-toggle). The approach is **keep untrusted content off the reparsed
argv** rather than try to quote around cmd.exe. Defenses that ship:

1. **`chat.model` charset validation** (`config::CtxConfig::load`): the one
   repo-settable string on this path is constrained to `[A-Za-z0-9-._:/@]`, so
   it cannot express a metacharacter (see [[Decision Log]] chat.model security
   amendment).
2. **Composed prompt via file form on the cmd shim** (FIX A,
   `prompt::injection_args_for_session`): the composed system prompt folds in
   repo-sourced text (repo `system-prompt.md`, repo CLAUDE.md via the
   command-line layer). When the launch resolves to the `cmd.exe /c <shim>`
   form (`adapters::launches_through_cmd_shim`), the file form
   (`--append-system-prompt-file <zirv-controlled-path>`) is *forced*
   regardless of the `--help` probe, so that text never reaches the reparsed
   argv at all; the inline `--append-system-prompt <text>` form is never used
   for composed text there, and if the file cannot be written the launch fails
   closed (an error) rather than degrading to inline. This closes the
   repo-config RCE at every launch seam at once (`wrap`, `exec`, `loop`,
   `resume`, `chat`, dash pane). A **non-shim** launch — a direct `.exe`, or an
   `sh <script>` — is not reparsed by any shell (CreateProcess hands argv to the
   target verbatim), so inline there is safe; the `--help` probe still gates the
   file form purely as an `ps`-visibility hardening, identical on every
   platform.
3. **Headless prompt via stdin on the shim form** (FIX B, `exec`/`loop`
   through `supervise::spawn_tapped`): on a `cmd.exe /c <shim>` launch the
   headless `-p` prompt — operator task text, plus any mail folded into a
   nudge/restart relaunch — is delivered on the child's **stdin** (the
   distiller's own mechanism, `AgentAdapter::headless_cmd_stdin`) rather than
   as an argv token, so a normal prompt containing `()`/`&` works instead of
   being refused, and cmd.exe never parses it. Gated on
   `AgentAdapter::launches_through_cmd_shim`, so off Windows and for a direct
   `.exe` the prompt stays on argv and every `sh`-based fake-agent test is
   byte-identical.
4. **`adapters::guard_cmd_shim_reparse`**: the fail-closed *backstop* at every
   spawn seam (`supervise::spawn_tapped` for `exec`/`loop`; the
   `CommandBuilder` assembly in `wrap` and `dash::pane` for the pty path; and
   `resume`'s own direct `command.status()` on Windows, added with FIX C). It
   rejects a launch whose program is the `cmd.exe /c <shim>` — or
   `powershell -File` (FIX D, defense-in-depth) — form and whose args carry a
   cmd.exe metacharacter. After FIX A/B the only free text still on a reparsed
   argv is an **interactive positional prompt** (a chat first message, a
   `resume` handoff prompt, a dash worker task) — operator/zirv-generated and
   rarely metachar-bearing. A no-op off Windows and for any non-shim program.

**Residual (usability, not security):** an *interactive* initial prompt that
contains a raw cmd.exe metacharacter is still refused by the backstop on a
Windows npm `.cmd` install (rephrase it). Headless is the common automation
path and is not subject to this (FIX B delivers it via stdin).

## `x.saturating_sub(n).clamp(1, x)` panics when `x` is 0

`Ord::clamp` asserts `min <= max` and panics otherwise, so the idiom
"shrink by a margin, but keep at least 1 and never exceed the area" is a live
panic whenever the area is zero -- which a real session reaches (a terminal
narrowed to at most `dash.sidebar_cols` makes `ui::layout`'s own main rect
zero-width, and `ZIRV_CTX_DASH_SIDEBAR_COLS` larger than the terminal does it
at startup). The release profile is `panic = "abort"`, so this is not a
recoverable error anywhere near a TUI. Use `.max(1).min(x.max(1))` instead
(`ui::dialog_width`), and guard whole renderers with `Rect::is_empty`.

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

## `exec`/`loop` gate mail on prompt composition, not on adapter capability

Mail is consumed as part of *composing* a worker's system prompt, so the
"was this message delivered?" decision is really "did we build a prompt?"
rather than a check of `adapter.capabilities().system_prompt`. With the two
adapters that ship today the two questions have the same answer, so nothing
is lost. A third adapter whose `ready()` returns ok but which has no
system-prompt support would turn this into a real message-eater: mail gets
`mail::consume`d on launch and then has nowhere to go. Latent — fix the gate
when a third adapter lands, not before.

## `wrap`'s status bar paints whenever stdout is a tty

Bar eligibility is decided on stdout being a terminal, but the bar reserves a
screen row and repaints assuming it owns the display, which really requires
raw mode on stdin. When stdout is a tty and stdin (or raw mode) is not, the
bar still reserves and paints, leaving reserved-row artifacts in scrollback.
Cosmetic only — nothing is lost and the session is unaffected — but it is why
a `wrap` run with redirected stdin can litter the terminal.

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
