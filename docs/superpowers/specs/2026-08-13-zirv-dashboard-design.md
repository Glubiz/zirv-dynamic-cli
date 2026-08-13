# zirv dashboard — multiplexer TUI for `zirv chat`

**Date:** 2026-08-13
**Status:** approved design, pre-implementation
**Branch:** `feat/dashboard` (stacked on `feat/agent-coordination`, PR #20)

## Problem

`zirv chat` today supervises exactly one interactive session with a launch
banner, a one-line status bar, and the `zirv ▸` event channel. The user asked
for a real dashboard: see every managed session, the mailbox, the shared
memory bank, and header stats at once; switch between AI sessions; intervene
(nudge/send) from the TUI — while every capability stays available to AI
harnesses autonomously via CLI verbs.

Decisions taken with the user during brainstorming:

1. **Fully interactive attach to every session** (tmux-like), not
   read-only worker views.
2. **Dashboard spawns panes; `zirv ctx agent` requests a pane** when run
   inside a dashboard, and keeps its headless behavior outside one.
3. **Persistent sidebar layout** (sessions always visible), not a
   summonable overlay or tabs.
4. **Quit ends dashboard-owned sessions** (with confirm and handoff
   capture), **and sessions started through zirv must be resumable** by a
   later `zirv chat` — continuity without a daemon.
5. **Approach: ratatui + embedded vt100 emulation, fidelity spike first.**

## Why emulation is required

A persistent sidebar means the child's output cannot be passed through
raw: interactive harness TUIs use absolute cursor addressing and the
alternate screen, so raw passthrough into a sub-region would paint over
the sidebar. Like every real multiplexer, the dashboard must parse each
child's output into an in-memory screen grid and render that grid inside
the pane. This rules out extending the current DECSTBM scroll-region
chrome (rows-only) and rules in a terminal-emulation dependency.

## Architecture

### Process model

One `zirv` dashboard process owns N ConPTY children. Each child is an
interactive harness session (claude/codex) supervised with the same
machinery `wrap` uses today: registry `Record`, turn-signal socket, rot
events, usage pacing, mail rules, `scrub_supervision_env` +
dashboard-specific env (below). There is no background daemon; when the
dashboard exits, its children end (see Lifecycle).

### Module layout

New directory `src/commands/ctx/dash/`:

| Module | Responsibility |
|--------|----------------|
| `mod.rs` | App loop: event poll (input, PTY output, registry/mail/memory refresh, spawn requests), state machine, dispatch. |
| `pane.rs` | One session pane: ConPTY handle, vt100 screen model, supervision state (score, verdict, turn signals), reader thread → channel. |
| `ui.rs` | Pure ratatui renderers: sidebar, header, main pane (grid → widget), overlays (mail, memory, nudge, spawn, quit-confirm, restore). No I/O. |
| `spawnreq.rs` | Spawn-request file format, request-dir capability token, validation, ack files. |
| `roster.rs` | Quit-time roster write, startup roster read, resume-eligibility checks. |

`chat.rs` gains the dashboard as the default target for `zirv chat`/bare
`zirv`; `--simple` keeps today's single-session wrap chrome unchanged.
`wrap.rs` itself is untouched (`zirv ctx wrap` remains the single-session
supervisor).

### Rendering pipeline

```
child ConPTY output ──► vt100 parser (per pane, in-memory screen grid)
                              │
        active pane only      ▼
input ◄── prefix filter ── ratatui frame: header / sidebar / grid / overlay
```

- Child PTY is sized to the main-pane region (cols − sidebar width,
  rows − header − footer); resize events re-size every pane's PTY and
  vt100 screen.
- Only the active pane's grid renders; background panes still parse
  their output continuously (their state glyphs and "last line" preview
  in the sidebar come from the live grid).
- Zoom (`prefix,z`): the active pane's region becomes the full frame
  (still emulated). If the spike reveals fidelity gaps, a zoom-raw
  contingency (suspend ratatui, hand the terminal to the child raw via
  the proven wrap pipeline, resume on return) is the fallback — decided
  by the spike, not speculatively built.

### Keybindings

Children are full TUIs that consume nearly every key (claude uses Tab),
so all dashboard keys sit behind a tmux-style prefix, default `Ctrl+A`,
configurable via `[dash] prefix` in `ctx.toml` (operator layers only —
repo-forbidden). After the prefix:

| Key | Action |
|-----|--------|
| `1`–`9`, `Tab` | switch pane |
| `s` | spawn dialog (harness + prompt) |
| `n` | nudge dialog for the selected session |
| `m` | mailbox overlay (read; compose send/send --to-session) |
| `M` | memory overlay (recall list; remember/forget/verify) |
| `z` | zoom toggle |
| `q` | quit (confirm if work in flight) |
| `Ctrl+A` | send a literal Ctrl+A to the child |

Everything not prefixed passes to the active child untouched.

### Header and sidebar

Header: active harness, rot score, usage % (from `window`/`usage`),
unread mail (broadcast/direct split), memory entry count, session count.
Sidebar rows: state glyph (● working / ○ idle / ⏸ waiting-input /
✕ ended), agent name, registry short, one-line last-output preview.
Headless sessions started in other terminals appear from the registry as
view-only rows (no attach; nudge/send still work — they use the existing
headless paths).

## Intervention semantics

- **Attached pane nudge:** no kill/relaunch. The dashboard writes the
  guidance into the pane's PTY as a clearly labeled visible line
  (`[zirv ▸ nudge from operator] …`), submitted when the pane is idle
  (turn-signal idle or prompt detected). Transparent by construction —
  the user watches it land in the pane.
- **Attached pane mail:** same visible-injection mechanism, delivered
  when idle, consumed (moved to `read/`) only after successful injection
  — read-once preserved. The old trust concern (silent body injection
  into an interactive session) is resolved by visibility: the full text
  appears on screen in the pane.
- **Headless sessions elsewhere:** unchanged — restart-based nudge,
  composed-prompt mail, advisory counts.
- Every overlay action calls the same library functions as the CLI verbs
  (`mail::*`, `memory::*`, `sessions::run_nudge_with`, spawn request =
  same code path `zirv ctx agent` uses). No TUI-only capability, so AI
  harnesses can do everything the TUI can do, autonomously.

## Spawning and the capability token

The dashboard creates a per-run request directory
`<state>/dash/<dash-short>/requests/` with an unguessable component, and
exports its path to pane children as `ZIRV_CTX_DASH_REQUESTS`. This
variable is added to `SUPERVISION_ENV` scrubbing rules with dash-specific
handling: panes receive it; everything the *panes* spawn that goes
through `scrub_supervision_env` keeps it (a worker's subagents may
legitimately delegate), but `zirv ctx wrap`/`chat` nesting guards treat
it as nesting evidence.

`zirv ctx agent <name> <prompt>`:

1. If `ZIRV_CTX_DASH_REQUESTS` is set and the directory exists: validate
   the agent through `AgentGate` (unchanged), write
   `req-<nnn>.json` (`create_new`, `_NNN` collision suffix like mail)
   containing `{agent, prompt, cwd, requested_by (session short)}`, poll
   for `ack-<same>.json` (short timeout), print the new pane's registry
   short, exit 0. Timeout → fall back to headless with a stderr notice.
2. Otherwise: current headless behavior, byte-for-byte unchanged.

The dashboard polls the request dir in its event loop, re-validates the
gate itself (requests are data, not authority), spawns the pane with the
prompt injected via the composed-prompt machinery (Worker role, memory
and mail layers as today), and writes the ack.

## Lifecycle: quit and resume

**Quit:** if any pane is mid-turn, a confirm dialog lists them. On
confirm: capture handoffs best-effort, run the quit-child ladder per
pane, release registry records, write
`<state>/dash/roster-<repo_slug>.json`:

```json
{ "written": 1755100000,
  "panes": [ {"agent": "claude", "session_id": "…", "role": "worker",
               "short": "9f3e21aa", "handoff": "<path-or-null>"} ] }
```

**Restore:** on dashboard startup, if a roster exists and is fresh
(< configurable age, default 7 days), show a restore dialog with
per-session checkboxes. Each chosen session relaunches through its
adapter's resume mechanism (`claude --resume <session_id>`); sessions
whose transcript no longer resolves are shown greyed with the reason.
Roster is consumed (moved aside) once the dialog is answered — a stale
roster can never re-offer twice.

## Safety and fallbacks

- **The wrap invariant extends to the dashboard:** supervision or
  emulation failure in a pane degrades that pane (error banner + zoom
  offer), never the dashboard; dashboard-level failure restores the
  terminal (existing console-restore handlers + a panic hook that resets
  raw mode/alt screen) and reports plainly. No `unwrap`/`expect` on the
  event-loop hot path.
- **Nesting guard:** unchanged, plus `ZIRV_CTX_DASH_REQUESTS` as
  evidence. A pane child cannot start another dashboard.
- **Non-TTY / tiny terminal:** refuse the dashboard with a message
  pointing at `zirv ctx chat --simple` (which keeps today's behavior).
  `MIN_COLS`/`MIN_ROWS` rise to fit the sidebar (spike informs exact
  numbers).
- **Repo trust:** all new config keys (`dash.*`) are operator-only
  (`REPO_FORBIDDEN`).

## Dependencies

`ratatui`, `crossterm` (backend; coexists with `console` — dashboard
codepath only), `vt100` (screen model; if the spike shows fidelity gaps,
evaluate `wezterm-term`/`termwiz` before falling back to Approach 2).
`Technology Stack.md` updated when the Cargo.toml change lands.

## Testing

- Pure renderers in `ui.rs` via ratatui `TestBackend` snapshots.
- vt100 pane model against the recorded claude fixture
  (`scripts/record-claude-fixture.py` output) — the same bytes a real
  session emits.
- `spawnreq.rs`/`roster.rs`: pure parse/validate/format tests; spawn
  paths stubbed with `ZIRV_CTX_AGENT_BIN=/nonexistent/...` (standing
  rule: never disable a spawn-preventing guard to red-prove it).
- Interactive behavior (input forwarding, resize, prefix keys, restore
  dialog) manually verified in a **separate Windows Terminal window** —
  never inside a Claude Code session's terminal (standing safety rule).
- Windows test baseline: compare against the documented pre-existing
  os-193 failure set; Linux CI is the real gate.

## Delivery plan

Waves on `feat/dashboard`, commit per wave, adversarial review gate at
the end (rerun until clean):

- **W0 — spike (gate):** throwaway probe: portable-pty + vt100 renders a
  real interactive claude in a sub-region with input forwarding and
  resize. Output is a go/no-go note (+ exact MIN_COLS/MIN_ROWS and any
  fidelity caveats), committed under `docs/superpowers/notes/`. No-go →
  stop, re-plan with the user.
- **W1 — core multiplexer:** dash module skeleton, single pane through
  the full pipeline, prefix input filter, zoom, quit ladder, `--simple`
  fallback, chat.rs wiring.
- **W2 — dashboard surfaces:** sidebar + header stats, multi-pane
  switching, spawn dialog, mail/memory/nudge overlays, visible-injection
  intervention for attached panes.
- **W3 — autonomy + continuity:** spawn-request IPC in `zirv ctx agent`,
  roster write/restore, headless-session sidebar rows, docs (Ctx
  Supervisors, Ctx Subsystem, Technology Stack, Built-in Commands).

## Out of scope (explicitly)

- Background daemon / true detach-reattach of live processes.
- Split-screen simultaneous pane rendering (one active pane at a time in
  v1; the grid model makes side-by-side a later increment).
- Codex adapter completion (externally blocked on codex hooks contract);
  codex panes work to the extent the codex adapter already does.
- Mouse support.
