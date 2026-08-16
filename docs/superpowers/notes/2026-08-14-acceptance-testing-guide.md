# zirv meta-harness — cross-OS/terminal acceptance testing guide

**Date:** 2026-08-14
**Scope:** PRs #17–#21 folded together on `feat/dashboard` — the whole zirv
meta-harness stack: `zirv chat`/dashboard multiplexer, worker panes, mail,
memory bank, sessions/nudge, chrome, and the cross-OS launch paths (ConPTY vs
unix PTY, the `cmd.exe` shim, raw mode, vt100 emulation).
**Audience:** the maintainer, running this by hand. Every command is
copy-pasteable; every step names the exact expected observation and a
PASS/FAIL check.

---

> ## ⛔ HARD SAFETY RULE — READ FIRST
>
> **NEVER launch an interactive `zirv chat`, `zirv ctx wrap`, bare `zirv`, or
> the dashboard from *inside* a Claude Code (or any other agent) session's
> terminal.** A nested interactive supervisor takes over the shared console
> (raw mode, alternate screen, ConPTY control-byte broadcast) and **can take
> the parent session down.** zirv itself refuses to nest (see Capability 5 /
> matrix row "nesting"), but do not rely on that as your safety net.
>
> - **All interactive tests** (anything that opens the dashboard, a chat, or a
>   wrap TUI) run in a **normal standalone terminal window** you opened
>   yourself — Windows Terminal, Terminal.app, gnome-terminal, etc. — never in
>   an editor-embedded agent shell, never inside `claude`/`codex`.
> - **Headless verbs are safe anywhere**, including inside an agent session:
>   `zirv ctx status`, `zirv ctx send`, `zirv ctx inbox`, `zirv ctx remember`,
>   `zirv ctx recall`, `zirv ctx forget`, `zirv ctx nudge`, `zirv ctx usage`,
>   `zirv ctx score`, `zirv ctx handoff`.
> - When a step says "in a standalone terminal", that is load-bearing.

---

## 1. Build & install per OS

The crate builds one binary named **`zirv`** (`Cargo.toml` `[package] name =
"zirv"`; release profile is `panic = "abort"`, so a panic on a hot path aborts
rather than unwinding — relevant to the terminal-restore checks below).

```bash
cargo build --release
cargo test --verbose -- --test-threads=1   # optional; see §5 for the Windows baseline
```

The binary lands at:

| OS | Path |
|----|------|
| Windows | `target\release\zirv.exe` |
| macOS / Linux | `target/release/zirv` |

### Put it on PATH (this session only)

**Windows PowerShell:**
```powershell
$env:Path = "$PWD\target\release;$env:Path"
zirv help    # sanity: prints the top-level help
```

**Windows cmd.exe:**
```cmd
set PATH=%CD%\target\release;%PATH%
zirv help
```

**macOS / Linux (sh/bash/zsh):**
```sh
export PATH="$PWD/target/release:$PATH"
zirv help
```

**PASS:** `zirv help` prints the command list. **FAIL:** "command not found".

### The npm-vs-native `claude` install note (Windows security-critical)

The dashboard launches the `claude` harness through the resolved adapter. **How
`claude` is installed changes the launch path on Windows only:**

- **npm-installed claude** resolves to `claude.cmd` (a `.cmd` shim). Windows
  `std::process::Command` cannot spawn a `.cmd` directly (CreateProcess error
  193), so `adapters::resolve_program` rewrites it to **`cmd.exe /c
  <shim> …>`**. `cmd.exe` then **re-parses the whole appended command line** —
  this is the security-critical path the shim defenses (FIX A–D) exist for.
- **Native `claude.exe`** (or `sh <script>` on unix) is handed to
  `CreateProcess`/`execve` verbatim — **no shell reparse**, so the shim-only
  defenses are inert there.

Check which you have on Windows:
```powershell
where.exe claude          # claude.cmd  → npm shim path (exercise the shim tests)
                          # claude.exe  → native path   (no reparse)
```

macOS/Linux: `which claude` — a native binary or an `sh` launcher, never a
`cmd.exe` reparse. The `cmd.exe`-shim security path **only exists for an
npm-installed `claude.cmd` on Windows.** Test that cell explicitly (matrix
§4, Windows rows).

---

## 2. Prerequisites

1. **An enabled, ready adapter (claude).** Verify:
   ```
   zirv ctx status
   ```
   The `agents:` block must show `claude enabled`, and the `chat:` line must
   name `claude`. (`codex` is *not implemented* — `--agent codex` always fails
   loudly; that is expected, not a bug.)
2. **A local `.zirv` directory** in the repo you test from. This repo
   (`zirv-dynamic-cli`) already has one. A bare `zirv` only opens a chat when a
   **local** `.zirv` exists — a global `~/.zirv` alone does not count.
3. **`[chat] model = "fable"`** is already committed in this repo's
   `.zirv/ctx.toml`. This is a repo-layer config (the untrusted layer);
   `chat.model` is one of the few keys a repo may set, because the choice is
   disclosed on screen and on the events channel. Confirm it is present:
   ```
   # .zirv/ctx.toml contains:
   # [chat]
   # model = "fable"
   ```
4. A real terminal at least **80×20** for any dashboard test (this is
   `MIN_DASH_COLS`×`MIN_DASH_ROWS`; below it the dashboard refuses and falls
   back to a plain wrap session).

**Environment variables you will use** (all operator-controlled; a repo cannot
set the `REPO_FORBIDDEN` ones):

| Variable | Effect |
|----------|--------|
| `ZIRV_CTX_STATE_DIR` | Override the state dir (handy to inspect roster/mail/logs) |
| `ZIRV_CTX_DASH=false` | Disable the dashboard (falls back to wrap) |
| `ZIRV_CTX_DASH_MAX_PANES=N` | Pane cap (default 9) |
| `ZIRV_CTX_DASH_SIDEBAR_COLS=N` | Sidebar width (default 24) |
| `ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS=N` | Restore-roster freshness (default 604800 = 7d) |
| `ZIRV_CTX_DASH_MOUSE=false` | Disable dashboard mouse reporting (default on) — trades pane wheel-scroll for the terminal's own native text selection |
| `ZIRV_CTX_DASH_KEYLOG=<path>` | Opt-in, append-only diagnostic log of every dashboard input event, overlay/`filter_key` verdict, and scroll decision (not `REPO_FORBIDDEN`-gated; there is no `ctx.toml` key, env-only) |
| `ZIRV_CTX_CHAT_MODEL=<m>` | Orchestrator model (overrides `[chat] model`) |
| `ZIRV_CTX_QUIET=true` | Silence the `zirv ▸` announcement channel |
| `ZIRV_ALLOW_NESTED=true` | Allow an interactive verb to start inside a session |
| `NO_COLOR=1` | Suppress colour |

**State dir location** (where roster/mail/memory/logs live), per OS:

| OS | State dir root |
|----|----------------|
| Windows | `%LOCALAPPDATA%\zirv\ctx` |
| macOS | `~/Library/Application Support/zirv/ctx` |
| Linux | `~/.local/state/zirv/ctx` |

(Or wherever `ZIRV_CTX_STATE_DIR` points.) Sub-paths used below: `dash/`
(roster + spawn-request dirs), `sessions/` (`<short8>.json` registry),
`logs/decisions.jsonl` (the injection/decision log), and the repo-scoped
mailbox/memory banks.

---

## 3. Capability checklist

Seven capabilities. Each is numbered steps with exact commands and the concrete
expected observation. **Every interactive step runs in a standalone terminal.**

### Capability 1 — Dashboard launch, `--simple` fallback, tiny-terminal refusal

**1a. Bare `zirv` opens the dashboard.**
In a standalone terminal, `cd` into this repo and run:
```
zirv
```
- **Expect:** the launch banner prints, then the screen switches to the
  alternate-screen dashboard:
  - Banner line 1: `zirv chat claude (configured as the default agent)`
  - `session <uuid>`
  - `harnesses: claude, codex (disabled)`
  - `model: fable`
  - Then a full-screen TUI: a **header** row (harness [+ focused pane title] /
    rot score / mail counts / memory count / session count — one row at every
    terminal height; usage was removed from the header, see §5), a
    **persistent left sidebar** listing sessions, and a **main pane** rendering
    the claude orchestrator session.
- **PASS:** dashboard opens with sidebar + header + one `orch` pane.
- **FAIL:** a plain scrolling claude with no sidebar (that is the wrap
  fallback — check terminal size and `dash_eligible` axes), or exit code 2
  ("bare `zirv` is a usage error" is the *old* clap behavior; the alias
  overrides it).

Quit for now: `Ctrl+A` then `q` (confirm with `Enter` if prompted).

**1b. `zirv chat` opens the same dashboard.**
```
zirv chat
```
Same result as 1a. (`zirv chat` is a top-level alias for `zirv ctx chat`,
intercepted from raw argv before clap runs. Unlike bare `zirv`, an explicit
`zirv chat` is **not** subject to the TTY rule — see Capability-matrix "piped"
row.)

**1c. `--simple` falls back to single-session chrome.**
```
zirv chat --simple
```
- **Expect:** **no** dashboard, **no** sidebar. A plain `wrap` passthrough
  session: the claude TUI takes the whole terminal, with zirv's injected
  prompt **skipped** (that is what `--simple` promises). The launch banner does
  not print (chrome is off under `--simple`).
- **PASS:** single-session, full-terminal claude, no sidebar. **FAIL:**
  sidebar appears.

**1d. Tiny terminal (<80×20) refuses the dashboard with a size notice.**
Resize the standalone terminal to, say, 70 columns (or 15 rows), then:
```
zirv chat
```
- **Expect on stderr, before falling through to a plain wrap session:**
  ```
  the terminal is too small for the dashboard (need at least 80x20, got 70x24);
  falling back to a plain session. Pass --simple to silence this.
  ```
  (Exact numbers reflect your terminal.) The session then runs as plain wrap.
- **PASS:** the "too small … need at least 80x20" notice prints and a plain
  session runs. **FAIL:** dashboard opens anyway, or a hard error.

**1e. Disabled dashboard falls back silently.**
```
ZIRV_CTX_DASH=false zirv chat        # PowerShell: $env:ZIRV_CTX_DASH="false"; zirv chat
```
- **Expect:** plain wrap session, no sidebar, **no** size notice (disabling is
  deliberate, not a degradation). **PASS/FAIL** accordingly.

---

### Capability 2 — Prefix keymap (`Ctrl+A`)

Open the dashboard (`zirv chat`) in a standalone terminal. The one prefix is
**`Ctrl+A`** (not configurable in v1). After the prefix, one key decides. Test
each:

| Sequence | Expected observation |
|----------|----------------------|
| `Ctrl+A` then `1`…`9` | Switch to (and focus) pane N. A digit **beyond the current pane count is a no-op** — nothing moves. |
| `Ctrl+A` then `Tab` | Cycle to the next pane (focus + selection both move). |
| `Ctrl+A` then `↑` / `↓` | Move the sidebar selection up/down; landing on an **attached pane row** moves the keyboard there too (focus follows selection onto any pane row — fixed 2026-08-15, previously arrows only highlighted a row that could not actually be switched to). Landing on a **view-only** row (dimmed) moves the selection only — focus stays on the last pane, since a view-only row cannot take the keyboard. |
| `Ctrl+A` then `s` | Spawn overlay opens (type `<agent> <prompt>`). |
| `Ctrl+A` then `n` | Nudge overlay opens for the selected session. |
| `Ctrl+A` then `m` | Mail overlay opens (read; compose with `c`). |
| `Ctrl+A` then `M` (shift+m) | Memory overlay opens (recall list; remember/forget/verify). |
| `Ctrl+A` then `z` | Zoom toggle — the focused pane's grid fills the whole frame (header + sidebar hidden); `Ctrl+A z` again restores. |
| `Ctrl+A` then `q` | Quit (with a confirm dialog if any pane is mid-turn). |
| `Ctrl+A` then `Ctrl+A` | Sends **one literal `Ctrl+A` byte** to the focused child (not a dashboard command). |
| `Ctrl+A` then an unmapped key | Disarms and forwards **nothing** — no stray keystroke leaks to the child. |

**Plain (un-prefixed) keys reach the child:** press plain `Tab` and the plain
arrow keys — they must reach the claude TUI (claude uses Tab and arrows
itself), **not** switch panes. This is the entire reason the prefix exists.

- **PASS:** every prefixed key does the table action; plain Tab/arrows reach
  the child. **FAIL:** a plain arrow switches panes, or `Ctrl+A <digit>` past
  the pane count jumps the keyboard somewhere.

**Sidebar glyphs** (verify while testing): `●` working, `○` idle, `✕` ended; a
**view-only** registry row (a session this dashboard did not spawn) shows a
middot `·` instead, and the **focused** row is marked with a leading `*`
separate from the selection highlight. (`PaneState::WaitingInput` and its `⏸`
glyph were **removed** 2026-08-14 — they had no producer and never rendered.
Do not expect to see a `⏸` anywhere; if you do, that's a regression, not an
untested reservation. A real "waiting on input" indicator would need a new
turn-signal kind end-to-end.)

**Windows control-byte note:** on Windows in VT input mode, `Ctrl+A` may arrive
as the raw control byte `\x01` with no modifier flag. The matcher accepts both
shapes, so the prefix must work identically on Windows Terminal, conhost, and
VS Code — confirm in each (matrix §4).

**Key encoding into a pane's child (fixed 2026-08-14 — verify, don't assume):**

| Keystroke (in a focused pane, not prefixed) | Expected observation |
|---|---|
| `Ctrl+Left` / `Ctrl+Right` in claude's editor | Moves **word-wise**, not one character. (Special keys now carry the xterm modifier parameter; a build that regresses this sends the bare unmodified arrow instead.) |
| `Ctrl+Space` | Sends a true NUL (`0x00`), not a literal space character. |
| `Ctrl+\`, `Ctrl+]`, `Ctrl+^`, `Ctrl+_` | Each sends its real control byte (`0x1c`/`0x1d`/`0x1e`/`0x1f`), not the literal printable character. |
| `Shift+Enter` | Inserts a newline in the prompt and does **not** submit. Bare `Enter` still submits as `\r`. |

- **PASS:** all four rows behave as described. **FAIL:** any of them types the
  literal character instead of sending the control byte, or `Ctrl`+arrow moves
  one character instead of by word.

**Pane scrolling (fixed 2026-08-15 — verify, don't assume):** Claude Code
spends the session on the alternate screen and enables its own mouse
reporting, so the dashboard forwards the mouse wheel to it instead of trying
to maintain its own scrollback for a full-screen child (vt100 structurally
cannot keep scrollback on the alternate screen, in two independent ways).

| Action | Expected observation |
|---|---|
| Mouse wheel over the focused (claude, full-screen) pane | The wheel scrolls **claude's own** history (forwarded to the child, SGR-encoded, pane-local coordinates) — not a zirv-drawn scrollback view. |
| `Ctrl+A` then `PageUp`/`Home`/`End` on that same pane | **No-op by design**, with a transient notice that scrolling belongs to the app — there is no synthesised wheel event to send a full-screen child. This is expected, not a bug. |
| Unprefixed `PageUp` | Reaches the child directly (claude's own binding, if it has one). |
| Mouse wheel over a pane running a plain-screen program (not full-screen) | Scrolls zirv's own vt100 scrollback view; a `SCROLL -N` notice appears. |

- **PASS:** wheel scrolling over the claude pane visibly scrolls claude's own
  output, and `Ctrl+A PageUp` on it produces the "scrolling belongs to the
  app" notice rather than doing nothing silently. **FAIL:** the wheel does
  nothing over the claude pane, or scrolling desyncs/corrupts the pane's
  display.

**Cursor visibility (fixed 2026-08-14):** while typing into the focused pane, a
visible terminal cursor tracks the pane's own insertion point (translated from
the child's `vt100` cursor into frame coordinates). It disappears while an
overlay (spawn/nudge/mail/memory/quit-confirm) owns input, and reappears when
the overlay closes.
- **PASS:** a caret is visible in the focused pane while typing. **FAIL:** no
  cursor is visible anywhere in the dashboard.

---

### Capability 3 — Orchestrator model = `fable`, disclosed and un-hideable

**3a. Banner + header show the model.**
Open `zirv chat`. The banner includes `model: fable`, and the dashboard
**header** renders the orchestrator harness as **`claude (fable)`** (the model
is folded into the harness label when `cfg.chat.model` is set).
- **PASS:** the banner shows `model: fable` and the header shows `claude
  (fable)`.

**3b. The `zirv ▸` events channel discloses it too.**
On launch, stderr carries a `zirv ▸` line:
```
[HH:MM:SS] zirv ▸ chat model 'fable' (from config)
```
- **PASS:** the events line appears on stderr at launch.

**3c. A repo cannot hide the disclosure even with the banner off.**
The events channel (`chrome.events`) is `REPO_FORBIDDEN`; the banner
(`chrome.banner`) is not. Temporarily add `[chrome]\nbanner = false` to
`.zirv/ctx.toml` (keep `[chat] model = "fable"`), then `zirv chat`:
- **Expect:** the banner is gone, **but** the `zirv ▸ chat model 'fable' (from
  config)` line still prints on stderr, and the header still shows the model.
- Now try to silence the channel *from the repo* — add `[chrome]\nevents =
  false` to `.zirv/ctx.toml` and run any ctx verb (e.g. `zirv ctx status`):
  ```
  <path>/.zirv/ctx.toml: `chrome.events` may not be set by a repository config…
  ```
  The config load **fails loudly** naming the key.
- **Revert** both edits when done.
- **PASS:** repo can hide the banner but not the disclosure; `chrome.events`
  in a repo file is refused. **FAIL:** the disclosure vanishes when the banner
  is off.

**3d. The operator (only) can silence it.**
```
zirv chat --quiet
# or: ZIRV_CTX_QUIET=true zirv chat
```
- **Expect:** no `zirv ▸ chat model …` line on stderr (the banner/header still
  show the model). **PASS/FAIL** accordingly.

---

### Capability 4 — Spawn a worker pane and confirm report-back

**4a. Spawn from inside the dashboard via `zirv ctx agent`.**
Open `zirv chat` in a standalone terminal. In the **orchestrator pane** (talk to
claude, or drop to its shell if you prefer), run — from a shell that inherited
the pane's environment:
```
zirv ctx agent claude "write the string DONE to a file called proof.txt then stop"
```
Because this process is a dashboard pane child (`ZIRV_CTX_DASH_REQUESTS` is set
and its directory is live), `zirv ctx agent` **does not run headless** — it
writes a spawn request and the dashboard fulfils it as a **new attached pane**.
- **Expect:** stdout prints `spawned in dashboard as <short8>`, and a new pane
  (`wrk claude`, glyph `●` then `○`) appears in the sidebar. Switch to it with
  `Ctrl+A 2` (or `Ctrl+A Tab`).
- **PASS:** a second, attached worker pane appears and reports the short id.
  **FAIL:** it runs headless inside the orchestrator's subshell (no new pane) —
  that means the request channel was not reached.

**4b. The worker reports back by mail when done.**
The worker pane is told (via its composed prompt's report-back layer) to run
`zirv ctx send --to-session <requested_by> …` with its summary when finished.
Once the worker completes, from any terminal in this repo:
```
zirv ctx inbox
```
- **Expect:** a message from the worker addressed to the orchestrator's short
  id, with the worker's summary in the body.
- **PASS:** the report-back message is in the inbox. **FAIL:** empty inbox
  after the worker clearly finished (note: report-back only happens when
  `mail.enabled` is on, which is the default).

**4c. Pane cap.**
Set `ZIRV_CTX_DASH_MAX_PANES=2` before launching the dashboard, then try to
spawn a second worker (giving 3 panes total incl. orchestrator):
```
zirv ctx agent claude "task exceeding the cap"
```
- **Expect:** a refusal reason naming the cap, e.g. `pane limit reached (2 live
  panes, dash.max_panes = 2)`, and **no** new pane. **PASS/FAIL** accordingly.

**4d. Argv-guard refusal (prompt shaped like a flag).**
```
zirv ctx agent claude "--dangerously-skip-permissions"
```
- **Expect:** the request is **not** written as a pane spawn; it falls back to
  the safe headless path with a stderr notice ("a prompt beginning with '-'
  cannot be spawned as a dashboard pane; running headless"). No pane appears.
- **PASS:** no pane, notice printed.

---

### Capability 5 — Intervention (status / send / nudge / visible injection)

These use headless verbs and are **safe from any terminal**, though the visible
injection (5d) needs a live dashboard to watch.

**5a. Find session shorts.**
```
zirv ctx status
```
- **Expect:** a `sessions:` block, one line per live session:
  ```
  <short8> <agent> <verb> pid <pid> <age> live <repo_slug>
  ```
  where `<verb>` is `chat` for the orchestrator pane and `dash` for a worker
  pane. Note the short ids. An `unreachable` session (no turn-signal socket) is
  labelled as such, not hidden.
- **PASS:** shorts visible with correct verbs.

**5b. Send to an agent, and to a specific session.**
```
zirv ctx send --to claude --message "note to every claude session"
zirv ctx send --to-session <short8> --message "note to one live session"
```
(`--message` may be replaced by `--message-file <path>` or piped stdin. An
unknown/ambiguous `--to-session` prefix is refused with a candidate list.)
- **PASS:** both succeed; `zirv ctx inbox` shows them (the directed one visible
  only to the addressed session).

**5c. Nudge by short-id prefix (≥4 chars, or the whole short id).**
```
zirv ctx nudge <first4+> --message "look at the failing test"
```
- **Expect:** accepted when the prefix is ≥4 chars and unique. A prefix under 4
  characters is **refused** (`prefix too short`) unless it equals a whole live
  short id. An unreachable target is refused and points at `zirv ctx send`.
- **PASS:** short prefix refused, ≥4-char unique prefix accepted.

**5d. Visible, idle-gated injection into an attached pane.**
With the dashboard open and a worker pane **idle** (glyph `○`), nudge that
pane's short:
```
zirv ctx nudge <workerShort> --message "check the build output"
```
- **Expect:** within a tick, a **visible** line appears typed into that pane:
  ```
  [zirv ▸ nudge from operator] check the build output
  ```
  submitted with a single carriage return. If the pane is **working** (`●`) or
  you are mid-typing in it, the nudge **queues** and lands only once the pane
  reports its next turn boundary (it must never submit a half-composed prompt).
- **PASS:** the labelled line lands visibly, only when idle. **FAIL:** it
  interleaves with the agent's own output, or submits your half-typed text.

**5e. Unreachable-target refusal.**
A `--no-supervise` wrap session (or any session with no bound socket) shows as
`unreachable` in status; `zirv ctx nudge <thatShort>` must refuse with the
reason and suggest `zirv ctx send`.
- **PASS:** refused with a clear reason.

**5f. Mail/memory overlays from inside the dashboard.**
`Ctrl+A m` opens the mailbox overlay (arrow/`j`/`k` to move, `Enter` to consume,
`c` to compose, `Esc` to close). `Ctrl+A M` opens the memory overlay. Both call
the **same library functions** as the CLI verbs — a message consumed in the
overlay is gone from `zirv ctx inbox`, and vice versa.
- **PASS:** overlay actions and CLI verbs agree on state.

---

### Capability 6 — Memory bank

Repo-scoped, one file per key. Safe from any terminal.

**6a. Remember / recall / forget.**
```
zirv ctx remember --key staging-db --text "staging DB is at db.staging.internal:5432"
zirv ctx recall
zirv ctx recall --key staging-db
zirv ctx recall --json
zirv ctx forget staging-db
```
- **Expect:** `remember` stores (replacing any prior entry for the same key);
  `recall` lists entries with a human age ("written Nd ago, verified Nd ago");
  `--key` filters to one; `--json` emits one JSON object per line; `forget`
  removes the key. `zirv ctx forget --all` clears the whole repo bank.
- **PASS:** the store/list/remove round-trips cleanly.

**6b. `--verify` refreshes the stamp without new text.**
```
zirv ctx remember --key staging-db --text "…"     # seed
zirv ctx remember --key staging-db --verify        # no --text: bumps Verified only
zirv ctx recall --key staging-db                   # "verified 0d ago"
zirv ctx recall --stale 7                           # entries not verified in 7+ days
```
- **PASS:** `--verify` alone updates the verified stamp; `--stale N` filters by it.

**6c. Memory is injected into a worker's prompt.**
Seed a distinctive fact, then spawn a worker (Capability 4) and ask it to repeat
what it knows from its zirv memory layer:
```
zirv ctx remember --key canary --text "CANARY-MEMORY-9F3E"
```
Spawn a worker and confirm it can see `CANARY-MEMORY-9F3E` in its context (the
Memory layer is folded into a **Worker** pane's composed prompt; the interactive
**orchestrator** gets memory too, but never mail bodies).
- **PASS:** the worker can recall the seeded fact.

**6d. Harvest is opt-in (off by default).**
`cfg.memory.harvest` defaults **false**; a repo cannot turn it on
(`REPO_FORBIDDEN`). Only the operator enables it:
```
ZIRV_CTX_MEMORY_HARVEST=true zirv ctx …
```
Confirm that with harvest off, a rot-restart handoff does **not** silently add
memory entries; with it on (operator), distilled facts may be harvested.
- **PASS:** default off; only the operator env/home layer flips it.

---

### Capability 7 — Quit / restore roster

**7a. Quit with a live worker → roster written.**
Open `zirv chat`, spawn a worker (Capability 4), leave it running. Press
`Ctrl+A q`.
- **Expect:** because a pane is mid-turn, a **QuitConfirm** overlay lists the
  working pane(s). Confirm. The dashboard captures handoffs best-effort, runs
  the quit ladder per pane, restores the terminal, and writes:
  ```
  <state>/dash/roster-<repo_slug>.json
  ```
  Inspect it (set `ZIRV_CTX_STATE_DIR` to a known dir first if you want it
  local): it lists each surviving pane with `agent`, `session_id`, `role`
  (`orchestrator`/`worker`), `short`, and a handoff path-or-null.
- **PASS:** confirm dialog names the working pane; roster file exists with the
  worker entry.

**7b. Relaunch → restore dialog.**
Within the roster freshness window (default 7 days) relaunch:
```
zirv chat
```
- **Expect:** a **Restore** overlay with a per-session checkbox, every
  candidate **defaulting to checked**. The orchestrator itself is filtered out
  of the offer (a fresh orchestrator is always launched). Sessions whose
  transcript no longer resolves are shown greyed with the reason. `Enter`
  restores the checked ones; `Esc` declines. The roster is **consumed** on
  answer (renamed to `.consumed.json`) so it can never re-offer twice.
- **PASS:** restore dialog appears with the worker checked.

**7c. `claude --resume` continuity.**
Confirm the restored worker pane resumes its **prior conversation** rather than
starting fresh: it comes back with its earlier context (the pane was launched
pinned to zirv's own uuid via `--session-id`, so `claude --resume <uuid>`
actually finds the conversation instead of failing "no conversation found").
The restored pane keeps the **same registry short id** as before the quit, so
its mail/nudge address is unchanged.
- **PASS:** restored pane resumes with context and the same short id.
- **Codex note:** codex has no verified resume flag, so a codex pane restores
  via a plain prompt-carrying relaunch with a "resuming after a dashboard
  restart" note — not a true resume. (Codex is not a supported adapter yet, so
  this is informational.)

---

## 4. OS × terminal MATRIX (critical)

Run the Capability checklist (§3) in each cell below. The matrix has **three OS
families** and **fourteen terminal/host environments** (Windows 5, macOS 4,
Linux 5), plus **six cross-cutting rows** that must pass on every platform.
Where behavior differs by OS it is called out inline.

**Legend for the per-cell mini-run** (do at minimum these, in a standalone
terminal): **[L]** launch dashboard (Cap 1) · **[K]** prefix keymap incl. plain
Tab/arrows reach child (Cap 2) · **[R]** resize the window while the dashboard
is open, and again while **zoomed** (`Ctrl+A z`), and confirm the pane re-lays
out without corrupting the sidebar/header · **[Q]** quit + restore (Cap 7) ·
**[U]** paste a wide-glyph/UTF-8 string into a pane and confirm it renders (Cap
cross-cutting).

### Windows

| Environment | Launch path | What to watch |
|-------------|-------------|---------------|
| **Windows Terminal (ConPTY)** | ConPTY PTY | The primary target. `[L][K][R][Q][U]`. Watch the **status-bar / dashboard corner under ConPTY on resize** — ConPTY can lag a redraw; confirm no stale reserved-row artifact in the bottom-right after a resize (see §5). |
| **Legacy conhost.exe** | ConPTY (Win10+) | Same run. Older conhost is more likely to show resize/redraw lag; still must lay out correctly and restore the terminal cleanly on quit. |
| **VS Code integrated terminal** | ConPTY | Run in a **standalone VS Code window's terminal that is NOT an agent session** (never the Claude Code panel). `[L][K][R][Q]`. Confirm `Ctrl+A` arrives (VS Code keybindings can intercept it; if so, note it). |
| **PowerShell host vs cmd.exe host** | ConPTY either way | Launch `zirv chat` from PowerShell and from cmd.exe. Behavior must be identical; the host shell only matters for the PATH setup (§1) and for the shim path below. |
| **npm `claude.cmd` vs native `claude.exe`** | shim reparse vs direct | **The security-critical cell.** See below. |

**Windows ConPTY status-bar / resize check (explicit):** with the dashboard
open, shrink and grow the window several times, including while zoomed. The
header and sidebar must re-lay-out on each resize; the very bottom-right corner
must not keep a fenced/reserved-row artifact. If you see a leftover row,
record it (it is the known ConPTY corner caveat, §5) — it is cosmetic, not a
crash.

**Windows `cmd.exe`-shim security path (explicit):**
1. Determine your install: `where.exe claude` → `.cmd` (npm shim) or `.exe`
   (native).
2. **On an npm `claude.cmd` install**, the composed system prompt — which folds
   in **repo-sourced** text (`.zirv/system-prompt.md`, repo `CLAUDE.md`) — is
   forced to the **file form** `--append-system-prompt-file <zirv-controlled
   path>`, never inline, so repo text never reaches the reparsed `cmd.exe`
   command line. **Sanity-check it:**
   - Put a benign cmd-metacharacter into a repo prompt file to prove it does
     **not** break the launch (it would, if it reached the reparsed argv):
     ```
     # .zirv/system-prompt.md  (temporary)
     Reminder: build with A & B (this ampersand must be harmless)
     ```
     Launch `zirv chat`. **Expect:** it launches normally — the `&` never
     reached `cmd.exe`. Confirm the injection happened via the decision log:
     ```
     zirv ctx status --decisions 20
     ```
     look for an `action: prompt-injected` entry for the `chat` verb. **Remove
     the temporary file when done.**
   - **Residual, expected:** an **interactive positional prompt** (a chat first
     message, a resume handoff, a dash worker task) that itself contains a raw
     `cmd.exe` metacharacter is **refused** by the backstop on a Windows npm
     `.cmd` install — rephrase it. This is a usability limit, not a bug (§5).
     Headless (`zirv ctx agent`) is not subject to it (its prompt travels on
     stdin).
3. **On a native `claude.exe`**, there is no reparse; inline delivery is safe.
   The file-form is still used as a `ps`-visibility hardening but the
   metacharacter test above is not security-relevant there. Note which install
   you tested.

### macOS

| Environment | Launch path | What to watch |
|-------------|-------------|---------------|
| **Terminal.app** | unix PTY | `[L][K][R][Q][U]`. Baseline unix path. |
| **iTerm2** | unix PTY | Same; confirm wide-glyph rendering `[U]` and colour. |
| **tmux** | unix PTY inside tmux | `Ctrl+A` is tmux's own default prefix — either re-bind tmux's prefix, or use a tmux config where it is not `C-a`, so `Ctrl+A` reaches zirv. Confirm resize propagates from tmux to the pane. |
| **VS Code integrated terminal** | unix PTY | Standalone window, not an agent panel. `[L][K][Q]`. |

### Linux

| Environment | Launch path | What to watch |
|-------------|-------------|---------------|
| **gnome-terminal / konsole** | unix PTY | Baseline. `[L][K][R][Q][U]`. |
| **xterm** | unix PTY | Minimal terminal; confirm VT/colour and that the dashboard still lays out. |
| **tmux / screen** | unix PTY inside mux | As macOS/tmux: free up `Ctrl+A` (screen also defaults to `C-a`). Confirm resize propagation. |
| **plain Linux TTY (Ctrl+Alt+F3)** | unix PTY | No compositor; confirm raw mode + alternate screen restore cleanly on quit, and wide glyphs degrade gracefully. |
| **over ssh** | unix PTY (remote) | Run in an ssh session to a Linux host. Confirm resize (SIGWINCH) propagates and the terminal restores on disconnect/quit. |

### Cross-cutting rows (every platform must pass)

| Row | Command / action | Expected |
|-----|------------------|----------|
| **Non-TTY / piped** | `zirv \| cat` (bash) or `zirv | cat` | Must **NOT** open a chat/dashboard. Prints `zirv help` (bare invocation with a non-terminal stdout ⇒ Help). Likewise `echo hi \| zirv` (piped **stdin**) ⇒ help. **Explicit** `zirv chat \| cat` is a different path — the alias is not subject to the TTY rule, so it launches; but never rely on piping the bare form. |
| **Narrow terminal fallback** | resize <80×20, `zirv chat` | The "too small (need at least 80x20, got NxM); falling back to a plain session. Pass --simple to silence this." notice, then a plain wrap session (Cap 1d). |
| **`--allow-nested` / nesting refusal** | From inside a `zirv chat`/`wrap` session's own pane, run `zirv chat` again | **Refused**: prints a nesting-refusal message and exits 1 (`ZIRV_CTX_SESSION`/`DASH_REQUESTS` evidence). `zirv chat --allow-nested` (or `ZIRV_ALLOW_NESTED=true`) overrides it. Headless verbs (`agent`, `exec`, `loop`, and all read-only verbs) are **never** gated — delegating from inside a session is what they are for. |
| **Resize while zoomed** | `Ctrl+A z`, then resize the window | The zoomed pane re-fills the new full frame (header/sidebar stay hidden); `Ctrl+A z` again restores the normal layout at the new size. No panic, no corruption. (A terminal narrowed to ≤ `dash.sidebar_cols` is the zero-width-rect case that was a known panic — it must now degrade, not crash.) |
| **UTF-8 / wide glyphs in a pane** | paste e.g. `日本語 ▸ café 🚀` into a pane | Renders through the vt100 emulation without breaking the grid or the sidebar preview. |
| **Colour vs no-colour** | `zirv chat` then `NO_COLOR=1 zirv chat` | Colour on by default (banner/header styled); with `NO_COLOR=1` the same content renders **plain** (no escape codes), identical text. On Windows, colour additionally requires VT processing — a terminal that cannot do VT gets a plain banner and no styling. |

---

## 5. Known limitations — expected, not bugs

These are drawn from `docs/obsidian/Development/Known Issues.md`. Do **not**
file them as failures; confirm they behave as described.

1. **A dashboard pane still does not advise/compact/restart itself.** Fixed
   2026-08-15: the header and every sidebar row now show a real rot score
   (`score::cached_score`, `rot NN` or `rot --` for unknown/unscorable — never
   `0`). What's still true: a pane runs fully supervised (turn-signal env,
   quit sequence, quit/restore roster) but nothing acts on that score the way
   `exec`/`wrap`/`run_loop` do for their own single session — this closes the
   *display* gap only, not a supervision gap. Also note: usage is **no
   longer** in the header at all (removed 2026-08-15 — every figure read "no
   usage source" in practice); the header is harness/rot/mail/memory/session
   only, one row at every terminal height.
2. **Windows interactive positional prompt with a raw shell metacharacter is
   refused** on an npm `claude.cmd` install (the `cmd.exe` reparse backstop).
   Rephrase the prompt. Headless (`zirv ctx agent`, whose prompt goes via
   stdin) is not affected. Expected residual, not a regression.
3. **The Windows os-193 test baseline.** On the maintainer's Windows machine a
   set of tests fail from `os error 193` (fake-agent-bin can't be spawned on
   Windows) plus temp-path-length issues — the documented pre-existing Windows
   baseline (the task brief cites ~44; the machine memory note cites 32 — treat
   the exact count as approximate and **compare against `main`, don't chase
   individual failures**). **Linux CI is the real gate.** Run
   `cargo test --verbose -- --test-threads=1` on Linux for a clean signal.
4. **Status-bar / dashboard ConPTY corner needs a manual resize check.** ConPTY
   can leave a stale reserved-row artifact in the bottom-right after a resize.
   It is cosmetic (nothing is lost, the session is unaffected) — the explicit
   Windows resize check (§4) is how you confirm it, not a failure.
5. **`wrap`'s status bar paints whenever stdout is a tty** even if stdin/raw
   mode is not — can leave reserved-row artifacts in scrollback with redirected
   stdin. Cosmetic only.
6. **A zero-width main rect used to panic** (`clamp` on a terminal narrowed to ≤
   `dash.sidebar_cols`, or an oversized `ZIRV_CTX_DASH_SIDEBAR_COLS`). Fixed —
   it must now degrade, not crash; the "resize while zoomed" and narrow-terminal
   rows exercise it.

---

## 6. Record results

Fill one row per (capability × environment). Mark **P**ass / **F**ail / **N/A**,
with a note for anything non-obvious.

### Capabilities × OS/terminal

| Capability | Win: WT (ConPTY) | Win: conhost | Win: VS Code | Win: PS/cmd host | Win: npm .cmd | Win: native .exe | mac: Terminal | mac: iTerm2 | mac: tmux | mac: VS Code | lin: gnome/konsole | lin: xterm | lin: tmux/screen | lin: plain TTY | lin: ssh |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 Launch / `--simple` / tiny-refuse |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| 2 Prefix keymap + plain keys |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| 3 Model disclosure (fable) |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| 4 Spawn worker + report-back |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| 5 Intervention (status/send/nudge/inject) |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| 6 Memory bank |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| 7 Quit / restore roster |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |

### Cross-cutting × OS

| Cross-cutting check | Windows | macOS | Linux | Notes |
|---|---|---|---|---|
| Non-TTY / piped ⇒ help (never a chat) |  |  |  |  |
| Narrow-terminal fallback + notice |  |  |  |  |
| Nesting refusal + `--allow-nested` override |  |  |  |  |
| Resize while zoomed (no panic/corruption) |  |  |  |  |
| UTF-8 / wide-glyph rendering |  |  |  |  |
| Colour vs `NO_COLOR` |  |  |  |  |
| (Windows only) `cmd.exe`-shim file-form sanity |  | N/A | N/A |  |

**Tester:** ____________  **Build (`git rev-parse --short HEAD`):** ____________
**Date:** ____________  **claude install (npm `.cmd` / native `.exe` / unix):**
____________
