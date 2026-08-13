# zirv Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `zirv chat` into a session multiplexer: a ratatui dashboard owning N interactive ConPTY harness sessions rendered through embedded vt100 emulation, with sidebar, header stats, overlays (mail/memory/nudge/spawn), spawn-request IPC, and quit/restore roster.

**Architecture:** One dashboard process. Each pane = ConPTY child + per-pane vt100 screen model + reader thread + the same supervision primitives `wrap` uses (registry record, turn-signal server, env scrub). Only dashboard keys sit behind a `Ctrl+A` prefix; everything else passes to the active child. All TUI actions call the same library functions as CLI verbs.

**Tech Stack:** Rust edition 2024, `ratatui`, `crossterm`, `vt100`, existing `portable-pty 0.9`.

**Spec:** `docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md` — read it first; it records the user's binding decisions (interactive attach, persistent sidebar, spawn-via-verb, quit-ends+roster-resume).

## Global Constraints

- Branch `feat/dashboard` (stacked on `feat/agent-coordination`). Commit per task.
- Release profile is `panic = "abort"`: `Drop` is NOT guaranteed. Every guard needs an explicit idempotent `restore()`/`release()` call in explicit exit arms (see `RawGuard`, `SessionGuard` precedent).
- The wrap invariant extends: no `unwrap`/`expect` on the dashboard event-loop hot path; pane failure degrades the pane, never the dashboard; any dashboard failure restores the terminal.
- Never add a Ctrl-C rung to any quit ladder (ConPTY broadcasts console-wide; finding F1).
- **NEVER run the dashboard, `zirv chat`, or `zirv ctx wrap` inside a Claude Code session's terminal.** All interactive verification happens in a separate Windows Terminal window, by the human. Tests must never reach a real agent spawn: stub with `ZIRV_CTX_AGENT_BIN=Z:/nonexistent/agent-bin` — never disable a spawn-preventing guard.
- Tests: inline `#[cfg(test)] mod tests`, `cargo test --verbose -- --test-threads=1`. ~32–48 pre-existing Windows failures (os error 193 / path length) are baseline; compare against main, don't chase. `cargo fmt -- --check` and `cargo clippy --all-targets -- -D warnings` must pass per task.
- All new config keys live under `[dash]` and are operator-only (`REPO_FORBIDDEN`).
- Tests that set env vars use the existing `VarGuard`/`HomeGuard` patterns (see `sessions.rs` tests) — never raw `set_var` without restore.
- `CtxResult<T> = Result<T, Box<dyn Error>>`; verbs that already printed their error return `Ok(1)`, never `Err` (dispatch prints `Err` once).

---

## Wave 0 — fidelity spike (GO/NO-GO GATE)

### Task 1: vt100 spike — fixture test + interactive probe

**Files:**
- Modify: `Cargo.toml` (add `[dev-dependencies]` only: `vt100 = "0.16"`, `ratatui = "0.30"`, `crossterm = "0.29"`)
- Create: `examples/vt_spike.rs`
- Create: `docs/superpowers/notes/2026-08-13-vt100-spike.md` (go/no-go result)

**Interfaces:**
- Consumes: `tests/fixtures/` claude fixture (re-record with `scripts/record-claude-fixture.py` if absent for interactive mode).
- Produces: the go/no-go decision plus measured `MIN_DASH_COLS`/`MIN_DASH_ROWS` values used by Task 2. NO production code survives this task.

- [ ] **Step 1: Add the three crates as dev-dependencies only** (examples build against dev-deps; the release binary is untouched until the gate passes). Run `cargo build` to confirm the main binary is unaffected.

- [ ] **Step 2: Write the automated fixture test inside the example file** (`examples/vt_spike.rs`, `#[cfg(test)]` doesn't run in examples, so make it a `--check` mode):

```rust
// examples/vt_spike.rs — THROWAWAY spike, deleted or left unshipped per gate outcome.
// Modes:
//   cargo run --example vt_spike -- --check            (automated: fixture -> vt100 -> assertions)
//   cargo run --example vt_spike -- claude [args...]   (manual: real child in a sub-region)
use std::io::{Read, Write};

fn check_fixture() -> Result<(), String> {
    let bytes = std::fs::read("tests/fixtures/claude-session.raw")
        .map_err(|e| format!("fixture missing: {e} (re-record with scripts/record-claude-fixture.py)"))?;
    let mut parser = vt100::Parser::new(40, 120, 0);
    parser.process(&bytes);
    let screen = parser.screen();
    // Assertions: no panic (already proven by getting here), cursor in bounds,
    // at least one non-blank cell, and the grid round-trips through resize.
    let (rows, cols) = screen.size();
    assert_eq!((rows, cols), (40, 120));
    let non_blank = (0..rows).any(|r| (0..cols).any(|c| {
        screen.cell(r, c).map(|cell| !cell.contents().trim().is_empty()).unwrap_or(false)
    }));
    if !non_blank { return Err("fixture rendered an entirely blank screen".into()); }
    parser.set_size(20, 80);
    parser.process(b"after-resize");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--check") {
        match check_fixture() {
            Ok(()) => println!("SPIKE CHECK: PASS"),
            Err(e) => { eprintln!("SPIKE CHECK: FAIL — {e}"); std::process::exit(1); }
        }
        return;
    }
    // Manual mode: spawn args as a child in a ConPTY sized to (cols-26, rows-3),
    // feed output through vt100, render the grid via ratatui with a dummy sidebar,
    // forward all keys except Ctrl+Q (quit). Follow wrap.rs patterns exactly:
    //   - native_pty_system().openpty(PtySize { rows, cols, .. })
    //   - write b"\x1b[1;1R" to the writer BEFORE spawn_command (conhost cursor-probe
    //     deadlock, wrap.rs:52)
    //   - take_writer() exactly once
    //   - reader thread with 8192-byte buffer -> mpsc channel
    // Render loop: crossterm::event::poll(50ms); ratatui draw of vt100 grid cells
    // (contents + fg/bg mapped to ratatui::style::Color); resize re-sizes pty AND parser.
    todo!("manual probe body — implementer writes this; it is throwaway code")
}
```

Note: the `todo!` above is the ONE permitted stub in this plan — it is inside a throwaway example whose manual half only matters when a human runs it; the implementer fills it in during this task using the listed wrap.rs patterns (~120 lines).

- [ ] **Step 3: Implement the manual-mode body** per the comment block (PTY spawn, vt100 feed, ratatui render of the grid at an offset with a 24-col dummy sidebar, key forwarding, Ctrl+Q exit that restores the terminal).

- [ ] **Step 4: Run the automated check**: `cargo run --example vt_spike -- --check`. Expected: `SPIKE CHECK: PASS`. If the fixture file doesn't exist, ask the user to re-record it (`python scripts/record-claude-fixture.py`) — do not fake one.

- [ ] **Step 5: HAND TO THE HUMAN.** Ask the user to run, in a **separate Windows Terminal window**:
  `cargo run --example vt_spike -- claude`
  and report: (a) does the Claude TUI render legibly in the sub-region? (b) does typing work, including Tab/arrows/Enter? (c) does resizing the window stay coherent? (d) colors sane?

- [ ] **Step 6: Write the go/no-go note** to `docs/superpowers/notes/2026-08-13-vt100-spike.md`: verdict, observed glitches, and the minimum usable region → set `MIN_DASH_COLS` (expect ~80) and `MIN_DASH_ROWS` (expect ~20) for Task 2. **If NO-GO: STOP THE PLAN — report to the user and re-plan (spec's Approach 2).**

- [ ] **Step 7: Commit** — `git add -A && git commit -m "spike: vt100 fidelity probe for the dashboard (gate)"`

---

## Wave 1 — core multiplexer

### Task 2: dependencies, DashConfig, Verb::Dash

**Files:**
- Modify: `Cargo.toml` (move `ratatui`, `crossterm`, `vt100` to `[dependencies]`)
- Modify: `src/commands/ctx/config.rs` (DashConfig struct ~line 300, `CtxConfig` field ~line 305, `ENV_MAP` ~line 339, `REPO_FORBIDDEN` ~line 568)
- Modify: `src/commands/ctx/sessions.rs:154` (`Verb::Dash`)
- Test: inline in `config.rs` / `sessions.rs` tests modules

**Interfaces:**
- Produces: `pub struct DashConfig { pub enabled: bool, pub sidebar_cols: u16, pub roster_max_age_secs: u64 }` with `Default { enabled: true, sidebar_cols: 24, roster_max_age_secs: 604_800 }`; `CtxConfig.dash: DashConfig`; `Verb::Dash` serializing as `"dash"`.
- ALSO produces: `pub struct ChatConfig { pub model: Option<String> }` (`Default: None`), `CtxConfig.chat: ChatConfig`, ENV_MAP entry `("ZIRV_CTX_CHAT_MODEL", &["chat","model"], EnvKind::String)`. **`chat.model` is deliberately NOT in REPO_FORBIDDEN** — the spec's "Orchestrator model" section records the rationale (interactive, operator-launched, displayed on screen; unlike the background `handoff.model`/`optimize.model`). Add a test proving the repo layer CAN set `chat.model` and a comment in REPO_FORBIDDEN pointing at the spec section so nobody "fixes" it later. And: `AgentAdapter::model_args(&self, model: &str) -> Vec<String>` default `vec!["--model".into(), model.into()]`-style per adapter — claude `["--model", m]`; codex: check `codex --help` for its flag (`-m`/`--model`), and if codex has none return `vec![]` with a doc-comment.
- Note: the prefix key is NOT configurable in v1 (YAGNI — constant `Ctrl+A` in Task 5). If the spike note demanded different minimums, use those.

- [ ] **Step 1: Write failing tests** in `config.rs` tests module (follow the existing MailConfig test shapes):

```rust
#[test]
fn dash_defaults_are_on_with_a_24_col_sidebar() {
    let cfg = CtxConfig::default();
    assert!(cfg.dash.enabled);
    assert_eq!(cfg.dash.sidebar_cols, 24);
    assert_eq!(cfg.dash.roster_max_age_secs, 604_800);
}

#[test]
fn repo_layer_cannot_touch_dash_keys() {
    // Follow the existing reject_untrusted_keys test pattern for mail.enabled:
    // a repo ctx.toml containing [dash] enabled=false / sidebar_cols=80 /
    // roster_max_age_secs=1 must be rejected/ignored for each key.
    let rejected = reject_untrusted_keys(toml::toml! { [dash] enabled = false }.into());
    assert!(/* dash.enabled absent from the accepted table */);
}

#[test]
fn env_can_disable_the_dashboard() {
    // ZIRV_CTX_DASH=false -> cfg.dash.enabled == false (EnvKind::Bool via ENV_MAP)
}
```

And in `sessions.rs`: `verb_dash_serializes_lowercase` asserting `Verb::Dash.as_str() == "dash"` and serde round-trip.

- [ ] **Step 2: Run to verify failure** — `cargo test dash_ verb_dash -- --test-threads=1`. Expected: compile errors (DashConfig absent).

- [ ] **Step 3: Implement**: `DashConfig` with manual `impl Default` (non-zero defaults — codebase rule), `#[serde(default, deny_unknown_fields)]`; add `dash` field to `CtxConfig`; ENV_MAP entries `("ZIRV_CTX_DASH", &["dash","enabled"], EnvKind::Bool)`; REPO_FORBIDDEN entries for `dash.enabled`, `dash.sidebar_cols`, `dash.roster_max_age_secs`; `Verb::Dash` variant + `as_str` arm (check every existing `match` on `Verb` — the compiler will list them; dashboard worker panes use `Dash`, orchestrator pane stays `Chat`).

- [ ] **Step 4: Run tests green**, then full `cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`.

- [ ] **Step 5: Commit** — `feat: dash config section, Verb::Dash, TUI dependencies`

### Task 3: `dash/pane.rs` — the session pane

**Files:**
- Create: `src/commands/ctx/dash/mod.rs` (module shell only: `pub mod pane;` + `pub(crate) use` re-exports; the app loop arrives in Task 5)
- Create: `src/commands/ctx/dash/pane.rs`
- Modify: `src/commands/ctx/mod.rs` (add `pub mod dash;`)
- Modify: `src/commands/ctx/wrap.rs` (make `answer_inherit_cursor_probe` and `spawn_output_thread`'s pattern reusable: change `answer_inherit_cursor_probe` at wrap.rs:52 to `pub(in crate::commands::ctx)`)

**Interfaces:**
- Consumes: `portable_pty` (as `wrap.rs:1044` does), `vt100::Parser`, `sessions::{Record, SessionGuard, Verb, scrub_supervision_env}`, `signal::SignalServer`, `state::StateDir::socket_for`, `wrap::{publish_socket_path, unpublish_socket_path, quit_child}`, `adapters::AgentAdapter`.
- Produces (Task 5/9 rely on these exact signatures):

```rust
pub enum PaneState { Working, Idle, WaitingInput, Ended(i32) }

pub struct PaneSpec {
    pub agent_name: String,
    pub argv: Vec<String>,          // program + args, from adapter interactive_cmd/build_launch
    pub role: super::super::prompt::PromptRole,
    pub verb: super::super::sessions::Verb,
    pub session_id: String,         // uuid, caller-generated
    pub title: String,              // sidebar label ("orch", "wrk codex", ...)
}

pub struct Pane { /* private */ }

impl Pane {
    pub fn spawn(spec: PaneSpec, state: &StateDir, repo: &Path, size: (u16, u16),
                 turn_env: &[(String, String)]) -> CtxResult<Pane>;
    pub fn drain(&mut self) -> bool;              // pump reader channel into vt100; true if new bytes
    pub fn screen(&self) -> &vt100::Screen;       // for ui.rs rendering
    pub fn write_input(&mut self, bytes: &[u8]) -> CtxResult<()>;
    pub fn resize(&mut self, rows: u16, cols: u16) -> CtxResult<()>;   // pty AND parser
    pub fn state(&self) -> PaneState;             // from turn signals + child try_wait
    pub fn on_turn_signal(&mut self);             // poll SignalServer::try_recv, update state
    pub fn short(&self) -> &str;                  // registry short (nudge/mail address)
    pub fn title(&self) -> &str;
    pub fn agent(&self) -> &str;
    pub fn last_line(&self) -> String;            // bottom-most non-blank row, for sidebar preview
    pub fn shutdown(&mut self, quit_sequence: &str) -> CtxResult<()>;  // idempotent: quit ladder,
                                                  // release guard, unpublish socket
}
```

- [ ] **Step 1: Write failing tests** (pure parts only — no real spawns; where a child is needed use the platform echo trick the wrap tests use, and mark spawn-reaching tests to expect the os-193 baseline pattern on Windows by using the same fake-agent helper wrap.rs tests use):

```rust
#[test]
fn pane_state_maps_turn_signals_to_glyph_states() {
    // pure fn: state_from(signal_seen_recently, child_exit) -> PaneState
    assert!(matches!(state_from(false, None), PaneState::Working));
    assert!(matches!(state_from(true,  None), PaneState::Idle));
    assert!(matches!(state_from(true,  Some(0)), PaneState::Ended(0)));
}

#[test]
fn last_line_returns_bottom_most_non_blank_row() {
    let mut p = vt100::Parser::new(4, 10, 0);
    p.process(b"hello\r\nworld\r\n");
    assert_eq!(last_line_of(p.screen()), "world");
}

#[test]
fn shutdown_is_idempotent() { /* second call returns Ok without touching anything */ }
```

- [ ] **Step 2: Red** — `cargo test pane_ -- --test-threads=1` fails to compile.

- [ ] **Step 3: Implement `Pane`**. Spawn follows `wrap.rs:1044-1106` faithfully: `native_pty_system().openpty(PtySize{rows, cols, ..})`; build `CommandBuilder` from `spec.argv` with `.cwd(repo)`; `scrub_supervision_env(&mut builder)` then apply `turn_env`; **write the cursor-probe answer `b"\x1b[1;1R"` before `spawn_command`**; `take_writer()` once into `Arc<Mutex<Box<dyn Write + Send>>>`; reader thread (8192 buffer) → `mpsc::Sender<Vec<u8>>` (dash variant: bytes go to the channel ONLY — never to stdout; the vt100 parser is the sole consumer, so no stdout lock and no generation counter needed — one pty per pane, panes never relaunch in place). Bind `SignalServer` at `state.socket_for(&session_id)`, `publish_socket_path`. Register `SessionGuard` with `Record::new(...)` for the pane's verb. `state()` derives from last turn signal + `child.try_wait()`. `shutdown()` = `quit_child(sink, child, quit_sequence, QUIT_GRACE)` with the pane's writer as sink, then `unpublish_socket_path` + `guard.release()`, guarded by a `done: bool`.

- [ ] **Step 4: Green + clippy + fmt.**

- [ ] **Step 5: Commit** — `feat: dashboard pane — supervised ConPTY child behind a vt100 screen`

### Task 4: `dash/ui.rs` — pure renderers

**Files:**
- Create: `src/commands/ctx/dash/ui.rs`
- Modify: `src/commands/ctx/dash/mod.rs` (`pub mod ui;`)

**Interfaces:**
- Consumes: `ratatui` widgets, `vt100::Screen`, `PaneState`.
- Produces (Task 5 renders exclusively through these — all pure, no I/O):

```rust
pub struct HeaderFacts { pub harness: String, pub score: Option<u32>, pub usage_pct: Option<u8>,
    pub mail_broadcast: usize, pub mail_direct: usize, pub memory_count: usize, pub sessions: usize }
pub struct SidebarRow { pub glyph: char, pub title: String, pub short: String,
    pub preview: String, pub attached: bool, pub selected: bool }
pub enum Overlay { None, QuitConfirm(Vec<String>), Spawn(SpawnDraft), Nudge(NudgeDraft),
    Mail(MailView), Memory(MemoryView), Restore(RestoreView) }

pub fn layout(area: Rect, sidebar_cols: u16) -> (Rect, Rect, Rect);  // header, sidebar, main
pub fn render_header(f: &mut Frame, area: Rect, facts: &HeaderFacts);
pub fn render_sidebar(f: &mut Frame, area: Rect, rows: &[SidebarRow]);
pub fn render_grid(f: &mut Frame, area: Rect, screen: &vt100::Screen);  // vt100 cells -> Buffer,
                                                                        // fg/bg/bold/inverse mapped
pub fn render_overlay(f: &mut Frame, area: Rect, overlay: &Overlay);
pub fn glyph_for(state: &PaneState) -> char;  // ● Working  ○ Idle  ⏸ WaitingInput  ✕ Ended
```

(`SpawnDraft`/`NudgeDraft`/`MailView`/`MemoryView`/`RestoreView` are plain field structs defined here in Task 4 with only what rendering needs — `input: String`, `items: Vec<String>`, `cursor: usize` — Tasks 8/9/12 fill them.)

- [ ] **Step 1: Write failing TestBackend snapshot tests:**

```rust
#[test]
fn grid_renders_vt100_cells_with_colours() {
    let mut parser = vt100::Parser::new(4, 20, 0);
    parser.process(b"\x1b[31mred\x1b[0m plain");
    let backend = ratatui::backend::TestBackend::new(20, 4);
    let mut term = ratatui::Terminal::new(backend).unwrap();
    term.draw(|f| render_grid(f, f.area(), parser.screen())).unwrap();
    let cell = &term.backend().buffer()[(0, 0)];
    assert_eq!(cell.symbol(), "r");
    assert_eq!(cell.fg, ratatui::style::Color::Red);
}

#[test]
fn layout_reserves_one_header_row_and_the_sidebar() {
    let (h, s, m) = layout(Rect::new(0, 0, 100, 30), 24);
    assert_eq!(h.height, 1); assert_eq!(s.width, 24);
    assert_eq!(m.width, 76 - 1 /* separator */); assert_eq!(m.height, 29);
}

#[test]
fn header_shows_the_broadcast_direct_mail_split() { /* "mail 2+1" appears in buffer text */ }

#[test]
fn glyphs_match_the_spec() {
    assert_eq!(glyph_for(&PaneState::Working), '●');
    assert_eq!(glyph_for(&PaneState::Ended(0)), '✕');
}
```

- [ ] **Step 2: Red.** **Step 3: Implement** — `render_grid` walks `screen.cell(r,c)`, maps `contents()`, `fgcolor()/bgcolor()` (`vt100::Color::{Default,Idx,Rgb}` → ratatui `Color::{Reset,Indexed,Rgb}`), bold/italic/inverse to modifiers; wide cells: skip the following cell when `cell.is_wide()`. **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: dashboard pure renderers (header, sidebar, grid, overlays)`

### Task 5: `dash/mod.rs` — event loop, prefix keys, zoom, quit

**Files:**
- Modify: `src/commands/ctx/dash/mod.rs`
- Test: inline

**Interfaces:**
- Consumes: Tasks 3–4 interfaces; `term::{RawGuard...}` is NOT used here — crossterm's raw mode + alternate screen manage the dashboard terminal; `sessions::nesting_refusal`.
- Produces: `pub fn run_dashboard(cfg: &CtxConfig, repo: &Path, env: EnvLookup<'_>, state: &StateDir, first: pane::PaneSpec) -> CtxResult<i32>` (Task 6 calls this) and the pure input filter:

```rust
pub const PREFIX: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::Char('a'));
pub enum InputVerdict { ToChild(Vec<u8>), Dash(DashAction), Pending /* prefix armed */ }
pub enum DashAction { Switch(usize), NextPane, SelectUp, SelectDown, Spawn, Nudge, Mail, Memory, Zoom, Quit, LiteralPrefix }
pub fn filter_key(prefix_armed: bool, key: KeyEvent) -> (bool, InputVerdict);
pub fn encode_key(key: KeyEvent) -> Vec<u8>;  // crossterm KeyEvent -> child bytes (incl. arrows,
                                              // Tab, Enter, Ctrl-<x>, Alt-<x>, plain chars)
```

- [ ] **Step 1: Failing tests for the pure input filter** (this is the highest-risk pure logic — be thorough):

```rust
#[test]
fn plain_keys_pass_to_the_child() {
    let (armed, v) = filter_key(false, key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(!armed); assert!(matches!(v, InputVerdict::ToChild(b) if b == b"x"));
}
#[test]
fn tab_passes_to_the_child_unprefixed() { /* claude uses Tab — must NOT switch panes */ }
#[test]
fn prefix_arms_and_swallows() {
    let (armed, v) = filter_key(false, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(armed); assert!(matches!(v, InputVerdict::Pending));
}
#[test]
fn armed_tab_switches_and_disarms() { /* filter_key(true, Tab) -> (false, Dash(NextPane)) */ }
#[test]
fn armed_digit_switches_to_that_pane() { /* '1' -> Switch(0), '9' -> Switch(8) */ }
#[test]
fn armed_ctrl_a_sends_a_literal_ctrl_a() { /* -> ToChild(vec![0x01]) via LiteralPrefix */ }
#[test]
fn armed_arrows_move_the_pane_selection() {
    /* filter_key(true, Up) -> (false, Dash(SelectUp)); Down -> SelectDown.
       UNARMED arrows still pass to the child (claude uses them). */
}
#[test]
fn prefix_matches_the_raw_control_byte_shape_too() {
    /* Spike finding (docs/superpowers/notes/2026-08-13-vt100-spike.md): Windows can
       deliver Ctrl+A as Char('\u{01}') with NO modifier flag. filter_key must arm
       on both Char('a')+CONTROL and Char('\u{01}'). Same for any Ctrl-<x> in
       encode_key. */
}
#[test]
fn armed_unknown_key_disarms_and_forwards_nothing() { /* no stray bytes to the child */ }
#[test]
fn encode_key_covers_the_terminal_basics() {
    assert_eq!(encode_key(key(KeyCode::Enter, KeyModifiers::NONE)), b"\r");
    assert_eq!(encode_key(key(KeyCode::Up, KeyModifiers::NONE)), b"\x1b[A");
    assert_eq!(encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), vec![0x03]);
    assert_eq!(encode_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)), b"\x1b[Z");
}
```

- [ ] **Step 2: Red.** **Step 3: Implement** `filter_key`/`encode_key` (pure), then `run_dashboard`:
  - Setup: `nesting_refusal("chat", env, allow_nested)` already ran in the caller (Task 6); enable crossterm raw mode + `EnterAlternateScreen`; install a scopeguard-style explicit teardown in EVERY exit arm (`LeaveAlternateScreen`, disable raw) — plus a `std::panic::hook` that emits `term::emergency_reset_bytes(false)` + leave-alt-screen bytes to stderr before the abort.
  - Loop (50ms `event::poll`): drain each pane (`pane.drain()`, `pane.on_turn_signal()`); on input, `filter_key` → child bytes to active pane / DashAction; on `Resize`, recompute `ui::layout` and `pane.resize(main.height, main.width)` for every pane; draw frame (header facts, sidebar rows, active grid or overlay). Zoom: a `zoomed: bool` that makes `layout` return the full area as main (header/sidebar skipped).
  - Quit: if any pane `Working` → `Overlay::QuitConfirm(list)`; on confirm `pane.shutdown(adapter.quit_sequence())` for each, exit 0. Roster write is Task 12 — leave a `// roster: Task 12` seam function `fn on_quit(panes: &mut [Pane])` that Task 12 extends.
  - Errors on the hot path: log to a `Vec<String>` shown in the header area, never propagate.

- [ ] **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: dashboard event loop, prefix keys, zoom, quit confirm`

### Task 6: chat.rs wiring + `--simple` fallback

**Files:**
- Modify: `src/commands/ctx/chat.rs` (branch inside `run_with` ~line 216 after `ChromeCaps::probe`)
- Modify: `src/commands/ctx/chrome.rs` (add `pub const MIN_DASH_COLS: u16` / `MIN_DASH_ROWS: u16` from the spike note; `pub fn dash_eligible(stdout_tty: bool, stdin_tty: bool, vt_ok: bool, size: (u16,u16), cfg: &DashConfig, simple: bool) -> bool` — pure)
- Test: inline in both

**Interfaces:**
- Consumes: `run_dashboard` (Task 5), `build_launch` (chat.rs:79), `resolve_adapter` (chat.rs:107).
- Produces: `zirv chat` → dashboard when `dash_eligible`; `zirv chat --simple` or ineligible terminal → today's `wrap::run_with` path unchanged; refusal message points at `--simple`.

- [ ] **Step 1: Failing tests:** `dash_eligible` truth table (tty/vt/size/enabled/simple axes — 6 cases incl. exactly-at-minimum and one-below-minimum); `chat` run_with test with `simple: true` still reaching the wrap path (existing test shape); non-tty stdin → `Help`/wrap unchanged; `orchestrator_argv_carries_the_configured_model` (`cfg.chat.model = Some("opus")` → `build_launch`/PaneSpec argv contains `adapter.model_args("opus")` right after the launch prefix; `None` → argv unchanged); banner/header shows the model when set.
- [ ] **Step 2: Red.** **Step 3: Implement**: in `chat.rs::run_with`, after adapter resolution and nesting guard, when `dash_eligible(...)` build the orchestrator `PaneSpec` from `build_launch` (`role: Orchestrator`, `verb: Verb::Chat`, fresh uuid, title `"orch"`) and call `dash::run_dashboard`; else fall through to the existing wrap delegation untouched. The composed prompt for the orchestrator pane goes through the same `wrap`-style injection the pane spawn provides via `turn_env`/argv (reuse `chat.rs`'s existing prompt flow — the argv from `build_launch` already carries it).
- [ ] **Step 4: Green + clippy + fmt; run the FULL suite** and compare failures against the documented baseline. **Step 5: Commit** — `feat: zirv chat opens the dashboard when the terminal can carry it`
- [ ] **Step 6: Create this repo's own `.zirv/ctx.toml`** (dogfooding — the worked example of repo-layer configuration; remember `.zirv/ctx.toml` is excluded from script listing in help.rs, nothing else needed):

```toml
# Repo-layer zirv configuration (untrusted layer: operator config and env win;
# keys listed in REPO_FORBIDDEN are ignored here by design).

[chat]
# Model for the interactive orchestrator session (displayed in the banner/header).
model = "opus"
```

- [ ] **Step 7: HAND TO THE HUMAN** — separate Windows Terminal: `cargo run -- chat` (dashboard appears, orchestrator pane interactive AND running the configured model, prefix+z zoom, prefix+q quit, prefix+↑/↓ selection) and `cargo run -- chat --simple` (today's behavior). Report before continuing.

---

## Wave 2 — dashboard surfaces

### Task 7: multi-pane sidebar, registry rows, header stats

**Files:**
- Modify: `src/commands/ctx/dash/mod.rs` (facts assembly in the loop)
- Modify: `src/commands/ctx/mail.rs` (move `unread_mail_counts` from wrap.rs:478 here as `pub fn unread_counts(state, repo, agent, session_short, mail_enabled) -> Option<(usize, usize)>`; wrap.rs re-exports/calls it — keep wrap's tests passing)
- Modify: `src/commands/ctx/dash/ui.rs` (only if a renderer gap appears; renderers are already built)
- Test: inline

**Interfaces:**
- Consumes: `sessions::list(state)` (sessions.rs:387) for non-pane rows; `memory::list` count; `mail::unread_counts`; usage/score: `window`/`usage` helpers already used by `wrap.rs:1436`'s bar redraw — mirror exactly what `redraw_bar_if_due` reads.
- Produces: sidebar = dashboard panes first (attached), then registry sessions not owned by this dashboard (view-only rows, `attached: false`); header fully populated.

- [ ] **Step 1: Failing tests:** pure `assemble_sidebar(panes_meta, registry_records, selected) -> Vec<SidebarRow>` — dedupe by short (a pane's own registry record must not appear twice), ordering (panes first), view-only marking; `HeaderFacts` assembly from stubbed inputs (mail disabled → counts 0 and no `mail` segment rendered — reuse wrap's `None` convention).
- [ ] **Step 2: Red.** **Step 3: Implement** (facts refresh at most once per second — mirror `BAR_THROTTLE`). **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: dashboard sidebar shows every registered session; header carries live stats`

### Task 8: mail + memory overlays

**Files:**
- Modify: `src/commands/ctx/dash/mod.rs` (overlay state machine + key handling inside overlays)
- Modify: `src/commands/ctx/dash/ui.rs` (fill `MailView { items: Vec<(String, String)>, cursor: usize, compose: Option<ComposeDraft> }`, `MemoryView { entries: Vec<(String,String,String)>, cursor: usize, input: Option<String> }` rendering detail)
- Test: inline

**Interfaces:**
- Consumes: `mail::{list, consume, store, Message}` (mail.rs:248/294/203), `memory::{list, remember, forget, verify, Entry}` (memory.rs:203/258/303/326) — the SAME functions the CLI verbs call; no new I/O paths.
- Produces: `prefix,m` opens mail (j/k move, Enter read + consume-on-read for messages addressed to the dashboard operator view, c compose → `store`), `prefix,M` memory (Enter view, r remember dialog, d forget, v verify). Esc closes any overlay.

- [ ] **Step 1: Failing tests** for the pure overlay reducers: `mail_overlay_reduce(view, key) -> (view, Option<MailEffect>)` where `MailEffect::{Consume(PathBuf), Send(Message)}` — cursor clamping, Esc-closes, compose draft accumulation; same shape for memory (`MemoryEffect::{Remember{key,body}, Forget(String), Verify(String)}`). Effects are EXECUTED in the loop by calling the mail/memory functions — reducers stay pure and fully tested.
- [ ] **Step 2: Red.** **Step 3: Implement** reducers + effect execution + rendering detail. **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: mailbox and memory-bank overlays driven by the same code as the CLI verbs`

### Task 9: intervention — nudge dialog + visible injection + attached-pane mail

**Files:**
- Modify: `src/commands/ctx/dash/pane.rs` (idle-gated visible injection)
- Modify: `src/commands/ctx/dash/mod.rs` (nudge overlay + delivery sweep in the loop)
- Test: inline

**Interfaces:**
- Consumes: `PaneState::Idle`, `pane.write_input`, `mail::{list, consume}`, `sessions::run_nudge_with` (sessions.rs:652) for NON-attached targets only.
- Produces:

```rust
// pane.rs
pub fn inject_visible(&mut self, label: &str, body: &str) -> CtxResult<()>
// writes: "\r\n[zirv ▸ {label}] {body}\r\n" to the child pty followed by "\r" submit;
// caller must have checked state() == Idle.

// mod.rs — pure
pub fn deliverable_now(state: &PaneState, queued: usize) -> bool  // Idle && queued > 0
```

- Behavior: `prefix,n` on a selected ATTACHED pane opens `NudgeDraft`; submit → if pane Idle, `inject_visible("nudge from operator", text)`; if Working, queue it (pane-local `VecDeque<String>`) and show "queued — delivers when idle" in the header. For a selected VIEW-ONLY row, the same dialog routes to the existing headless path: `sessions::run_nudge_with` (unchanged semantics: marker + mail + restart). Mail sweep: once per loop tick, for each attached WORKER pane (verb Dash) that is Idle, `mail::list(state, slug, Some(agent), Some(short))` → `inject_visible("mail from {from_agent}/{from_short}", body)` → `mail::consume` ONLY after `inject_visible` returned Ok (read-once, C7 discipline). Orchestrator pane gets NO body injection — counts in the header only (trust split preserved).

- [ ] **Step 1: Failing tests:** `deliverable_now` truth table; queue drains FIFO on idle; consume-only-after-successful-injection (simulate a writer that errors → message file untouched — use a `Pane` test double via a trait-thin seam `fn try_inject(&mut self, ...) -> CtxResult<()>` if needed); orchestrator pane excluded from body delivery.
- [ ] **Step 2: Red.** **Step 3: Implement.** **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: idle-gated visible nudge and mail injection for attached panes`
- [ ] **Step 6: HAND TO THE HUMAN** — separate terminal: spawn two panes, nudge the idle one (text visibly lands), nudge the busy one (queues, then lands), send mail from a third terminal via `zirv ctx send --to-session <short>` and watch it arrive visibly.

---

## Wave 3 — autonomy + continuity

### Task 10: `dash/spawnreq.rs` — request/ack files

**Files:**
- Create: `src/commands/ctx/dash/spawnreq.rs`
- Modify: `src/commands/ctx/state.rs` (add `pub fn dash(&self) -> PathBuf` → `<state>/dash`)
- Modify: `src/commands/ctx/dash/mod.rs` (poll + handle requests in the loop; export env to panes)
- Test: inline

**Interfaces:**
- Produces:

```rust
pub const DASH_REQUESTS_ENV: &str = "ZIRV_CTX_DASH_REQUESTS";

#[derive(Serialize, Deserialize)]
pub struct SpawnRequest { pub agent: String, pub prompt: String, pub cwd: PathBuf,
                          pub requested_by: String }
#[derive(Serialize, Deserialize)]
pub struct SpawnAck { pub ok: bool, pub short: Option<String>, pub reason: Option<String> }

pub fn request_dir_for(state: &StateDir, dash_short: &str, token: &str) -> PathBuf;
    // <state>/dash/<dash_short>-<token>/requests ; token = 16 hex chars from uuid v4
pub fn write_request(dir: &Path, req: &SpawnRequest) -> CtxResult<PathBuf>;
    // create_new "req-<uuid>.json"; 0600 via state::write_private
pub fn take_requests(dir: &Path) -> Vec<(PathBuf, SpawnRequest)>;  // read+delete, malformed -> skip+delete
pub fn write_ack(dir: &Path, request_stem: &str, ack: &SpawnAck) -> CtxResult<()>;  // "ack-<stem>.json"
pub fn wait_for_ack(dir: &Path, request_stem: &str, timeout: Duration) -> Option<SpawnAck>;  // 100ms poll
```

- Dashboard side (mod.rs): create the dir at startup, pass `(DASH_REQUESTS_ENV, dir)` inside every pane's `turn_env` (pane spawn already applies env after scrub — order guarantees it survives); each tick `take_requests` → re-validate `cfg.agents.refusal(&req.agent)` (requests are data, not authority) and `adapters::select` → on pass, build a Worker `PaneSpec` (composed prompt via `prompt::compose(home, repo, false, &cfg.prompt, PromptRole::Worker, &memory_lines, cap)` + `with_mail_layer`, exactly the `exec.rs` recipe) and spawn the pane; `write_ack`. On refusal: `write_ack(ok:false, reason)`. Delete the whole dir in every quit arm.
- Nesting: add `DASH_REQUESTS_ENV` to the evidence list in `sessions::nested_session_evidence` (a pane child must not start another dashboard) but NOT to `SUPERVISION_ENV` scrubbing (children legitimately inherit it; scrubbing would break `zirv ctx agent` inside panes' own subshells).

- [ ] **Step 1: Failing tests:** round-trip request/ack; `take_requests` skips+removes malformed json; `wait_for_ack` times out `None`; gate-refused request acks `ok:false` with the refusal text; `nested_session_evidence` fires on the new env var.
- [ ] **Step 2: Red.** **Step 3: Implement.** **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: dashboard spawn-request channel with capability-token directory`

### Task 11: `zirv ctx agent` requests a pane when inside a dashboard

**Files:**
- Modify: `src/commands/ctx/agent.rs` (branch at the top of `run_with`, agent.rs:84)
- Test: inline

**Interfaces:**
- Consumes: `spawnreq::{DASH_REQUESTS_ENV, SpawnRequest, write_request, wait_for_ack}`.
- Produces: inside a dashboard (`env(DASH_REQUESTS_ENV)` set AND the dir exists): write the request, wait up to 5s for the ack; `ok:true` → print `spawned in dashboard as {short}` and return `Ok(0)`; `ok:false` → print the reason, return `Ok(1)`; timeout → stderr notice `dashboard did not answer; running headless` and FALL THROUGH to the existing headless path unchanged. Outside a dashboard: byte-for-byte current behavior.

- [ ] **Step 1: Failing tests:** env set + dir present + ack written by the test → `Ok(0)` and the request file contains the prompt as data; env set + no dir → headless fallback reached (assert via the existing fake-agent-bin pattern, `ZIRV_CTX_AGENT_BIN=Z:/nonexistent/agent-bin` so the spawn fails fast and provably); env unset → current tests still green; gate refusal ack → `Ok(1)` with reason printed.
- [ ] **Step 2: Red.** **Step 3: Implement.** **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: zirv ctx agent joins the dashboard as a pane when one is running`

### Task 12: roster — quit capture and startup restore

**Files:**
- Create: `src/commands/ctx/dash/roster.rs`
- Modify: `src/commands/ctx/adapters/mod.rs` (trait: `fn resume_args(&self, session_id: &str) -> Option<Vec<String>> { None }`)
- Modify: `src/commands/ctx/adapters/claude.rs` (`Some(vec!["--resume".into(), session_id.into()])`)
- Modify: `src/commands/ctx/dash/mod.rs` (extend `on_quit`; restore dialog before first draw)
- Test: inline

**Interfaces:**
- Produces:

```rust
#[derive(Serialize, Deserialize)]
pub struct RosterPane { pub agent: String, pub session_id: String, pub role: String,
                        pub short: String, pub title: String }
#[derive(Serialize, Deserialize)]
pub struct Roster { pub written: u64, pub panes: Vec<RosterPane> }

pub fn roster_path(state: &StateDir, repo_slug: &str) -> PathBuf; // <state>/dash/roster-<slug>.json
pub fn write_roster(state: &StateDir, slug: &str, roster: &Roster) -> CtxResult<()>;
pub fn take_roster(state: &StateDir, slug: &str, now: u64, max_age: u64) -> Option<Roster>;
    // read + rename to roster-<slug>.consumed.json (never re-offers); None if stale/absent
pub fn restore_argv(adapter: &dyn AgentAdapter, pane: &RosterPane) -> Vec<String>;
    // resume_args -> interactive_cmd(None, resume_args) flattened;
    // adapters returning None -> interactive_cmd(Some(resume-note prompt), &[]) using the
    // handoff-based resume_prompt convention (resume.rs:67) with a one-line
    // "resuming after dashboard restart" note when no handoff exists.
```

- Quit (`on_quit`): before shutdowns, write the roster from live panes (orchestrator included, role recorded). Restore: at `run_dashboard` start, `take_roster(...)` fresh within `cfg.dash.roster_max_age_secs` → `Overlay::Restore` listing panes with checkboxes (space toggles, Enter confirms, Esc skips); selected panes spawn via `restore_argv` BEFORE the first orchestrator pane spawns only if the roster contained one — never spawn a duplicate orchestrator.

- [ ] **Step 1: Failing tests:** roster round-trip; `take_roster` consumes (second call `None`) and rejects stale; `restore_argv` for claude yields `["claude", "--resume", "<id>"]` (through `interactive_cmd` flattening with `launch_prefix_len` respected); codex (no `resume_args`) falls back to a prompt-carrying argv; restore-dialog reducer (same pure-reducer pattern as Task 8) toggles and confirms.
- [ ] **Step 2: Red.** **Step 3: Implement.** **Step 4: Green + clippy + fmt.** **Step 5: Commit** — `feat: dashboard roster — quit captures sessions, next launch offers restore`
- [ ] **Step 6: HAND TO THE HUMAN** — separate terminal: spawn a worker, quit (confirm), relaunch `cargo run -- chat`, restore the worker, verify the conversation resumed.

### Task 13: docs, reserved surface, full gate

**Files:**
- Modify: `docs/obsidian/Modules/Ctx Supervisors.md` (dashboard section: pane model, prefix keys, injection semantics)
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md` (dash module row, spawnreq/roster)
- Modify: `docs/obsidian/Modules/Built-in Commands.md` (chat behavior change)
- Modify: `docs/obsidian/Architecture/Technology Stack.md` (ratatui/crossterm/vt100)
- Modify: `docs/obsidian/Development/{Active Work.md, Work Journal.md, Decision Log.md}` (per CLAUDE.md contract; Decision Log: prefix-key choice + emulation-over-passthrough rationale, ≤15 lines)
- Modify: `CLAUDE.md` module map (dash/)
- Modify: `scripts/check-doc-coverage.sh` (pair: `^src/commands/ctx/dash/` → `docs/obsidian/Modules/Ctx Supervisors.md`)

- [ ] **Step 1:** Update every page above; bump `last-verified`.
- [ ] **Step 2:** `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo test --verbose -- --test-threads=1` — full suite, compare failures to baseline.
- [ ] **Step 3: Commit** — `docs: dashboard documentation sweep` — and push the branch.
- [ ] **Step 4: Review gate** — full-diff adversarial review per orchestrator conventions (dedicated review subagent(s), findings triaged, fixed, re-run until clean). Then open the PR (base: `feat/agent-coordination`).

---

## Self-review record

- Spec coverage: emulation pipeline (T3/T4), prefix keys (T5), sidebar/header (T7), overlays (T8), visible-injection intervention + trust split (T9), spawn IPC + capability token + gate re-check (T10/T11), roster + resume (T12), `--simple` fallback + eligibility minimums (T6), REPO_FORBIDDEN (T2), docs (T13), spike gate (T1). Deferred per spec's out-of-scope list: detach daemon, split-screen, mouse.
- Spec deviation (recorded): configurable prefix key dropped from v1 (YAGNI); constant `Ctrl+A`. Spec's `[dash] prefix` key therefore NOT added to config — REPO_FORBIDDEN list shrinks accordingly. Update the spec if the user objects.
- Type consistency: `PaneSpec`/`Pane` consumed by T5/T6/T9/T10; `Overlay` variants defined T4, filled T8/T9/T12; `SpawnRequest`/`SpawnAck` shared T10/T11; `resume_args` T12 only.
