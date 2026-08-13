//! `zirv chat`'s session multiplexer: a dashboard process owning N
//! interactive ConPTY harness sessions, each rendered through its own
//! embedded `vt100` screen model.
//!
//! This module now carries the event loop itself (`run_dashboard`) plus the
//! pure input filter (`filter_key`/`encode_key`) that decides, for every
//! keystroke, whether it goes straight to the active pane's child or gets
//! swallowed as a dashboard command behind the `Ctrl+A` prefix. Nothing in
//! `main.rs`/`chat.rs` calls `run_dashboard` yet -- that wiring is Task 6 --
//! so the function (and anything only it reaches) carries an explicit
//! `#[allow(dead_code)]`, the same precedent `pane.rs`'s own module-level
//! attribute already set for this plan.

pub mod pane;
pub mod ui;

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use super::CtxResult;
use super::adapters;
use super::config::{CtxConfig, EnvLookup};
use super::event::{SessionId, SessionRef};
use super::state::StateDir;
use super::term;

pub(crate) use pane::{Pane, PaneSpec, PaneState};

/// The one dashboard prefix key, `Ctrl+A`. Not configurable in v1 (recorded
/// as a deliberate spec deviation in the plan's self-review: YAGNI).
pub const PREFIX: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::Char('a'));

/// What a filtered keystroke means for the dashboard's own loop to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputVerdict {
    /// Bytes to write into the active pane's pty as-is.
    ToChild(Vec<u8>),
    /// A dashboard command, already fully decoded.
    Dash(DashAction),
    /// The prefix key just armed; nothing to do until the next keystroke.
    Pending,
}

/// Every command the dashboard itself understands once the prefix is armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashAction {
    Switch(usize),
    NextPane,
    SelectUp,
    SelectDown,
    Spawn,
    Nudge,
    Mail,
    Memory,
    Zoom,
    Quit,
    /// The prefix key pressed again while armed: the operator meant to send
    /// the child a literal `Ctrl+A`, not invoke a dashboard command.
    LiteralPrefix,
}

/// Matches `PREFIX` in either shape a real terminal can deliver it in: the
/// classic `Char('a')` (or shifted `Char('A')`) plus a `CONTROL` modifier
/// flag, or -- per the vt100 spike's own finding
/// (`docs/superpowers/notes/2026-08-13-vt100-spike.md`) -- the raw control
/// byte `Char('\u{01}')` with no modifier flag at all, which is how Windows
/// can deliver `Ctrl+A` in VT input mode. Both `filter_key`'s own arming
/// check and its armed-and-pressed-again check reuse this.
fn is_prefix_key(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('\u{01}') => true,
        _ => false,
    }
}

/// Pure: decides what one keystroke means, given whether the prefix is
/// currently armed from the previous keystroke. Returns the *new* armed
/// state alongside the verdict -- every path disarms except successfully
/// arming on an unarmed prefix press, so a caller never has to reason about
/// the transition table itself, only apply the pair it gets back.
pub fn filter_key(prefix_armed: bool, key: KeyEvent) -> (bool, InputVerdict) {
    if !prefix_armed {
        if is_prefix_key(&key) {
            return (true, InputVerdict::Pending);
        }
        return (false, InputVerdict::ToChild(encode_key(key)));
    }

    if is_prefix_key(&key) {
        return (false, InputVerdict::Dash(DashAction::LiteralPrefix));
    }

    let action = match key.code {
        KeyCode::Tab => Some(DashAction::NextPane),
        KeyCode::Up => Some(DashAction::SelectUp),
        KeyCode::Down => Some(DashAction::SelectDown),
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            Some(DashAction::Switch((c as u8 - b'1') as usize))
        }
        KeyCode::Char('s') => Some(DashAction::Spawn),
        KeyCode::Char('n') => Some(DashAction::Nudge),
        KeyCode::Char('m') => Some(DashAction::Mail),
        KeyCode::Char('M') => Some(DashAction::Memory),
        KeyCode::Char('z') => Some(DashAction::Zoom),
        KeyCode::Char('q') => Some(DashAction::Quit),
        _ => None,
    };

    match action {
        Some(action) => (false, InputVerdict::Dash(action)),
        // An armed prefix followed by a key with no dashboard meaning
        // disarms and forwards nothing -- never leaks a stray keystroke to
        // the child that the operator only meant as a (failed) command.
        None => (false, InputVerdict::ToChild(Vec::new())),
    }
}

/// `crossterm::event::KeyEvent` -> bytes to write to the active pane's pty.
/// Covers the terminal basics: `Enter`, arrows, `Tab`/`BackTab`, navigation
/// keys, function keys, `Alt-<x>`, `Ctrl-<x>`, and plain/UTF-8 characters.
///
/// Deliberately makes no special case for a raw control byte arriving as
/// `KeyCode::Char('\u{01}')` (or any other `Char('\u{0N}')`) with no
/// modifier flag: those code points already encode to the same single byte
/// through plain UTF-8 (ASCII control characters are their own UTF-8
/// encoding), so the fallback `Char(c)` arm below reproduces the raw byte
/// unchanged without needing to detect the shape at all.
pub fn encode_key(key: KeyEvent) -> Vec<u8> {
    if key.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = key.code
    {
        let mut buf = [0u8; 4];
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return bytes;
    }
    match key.code {
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => Vec::new(),
        },
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                vec![(upper as u8) & 0x1f]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        _ => Vec::new(),
    }
}

/// The literal bytes `Ctrl+A` itself encodes to, reused for
/// `DashAction::LiteralPrefix` rather than hard-coded a second time: single
/// source of truth with `PREFIX`.
fn literal_prefix_bytes() -> Vec<u8> {
    encode_key(KeyEvent::new(PREFIX.1, PREFIX.0))
}

/// Task 12's own extension point: on quit, before any pane is torn down,
/// persist a roster of live panes so the next launch can offer to restore
/// them. A no-op today -- shutdown (quit-sequence, registry release, socket
/// unpublish) happens in the caller right after this returns.
// roster: Task 12
fn on_quit(panes: &mut [Pane]) {
    let _ = panes;
}

/// Caps how many hot-path error strings the header keeps around: the header
/// is one line, so anything beyond the most recent handful is never going
/// to be shown anyway.
const MAX_KEPT_ERRORS: usize = 5;

fn push_error(errors: &mut Vec<String>, message: String) {
    errors.push(message);
    if errors.len() > MAX_KEPT_ERRORS {
        let drop = errors.len() - MAX_KEPT_ERRORS;
        errors.drain(0..drop);
    }
}

/// Restores the shared terminal: leaves the alternate screen and disables
/// raw mode. Idempotent (both crossterm calls are themselves idempotent) and
/// called from every exit arm of `run_dashboard`, matching the `RawGuard`/
/// `SessionGuard` precedent this plan's Global Constraints call for --
/// `panic = "abort"` in the release profile means `Drop` is not a safety
/// net here either.
fn teardown_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Bytes the panic hook writes directly to stderr before the process
/// aborts: `\x1b[?1049l` leaves the alternate screen (the `LeaveAlternate
/// Screen` sequence crossterm itself would emit), paired with `term::
/// emergency_reset_bytes(false)` for the cursor-visibility/scroll-region
/// reset every other supervisor's own emergency handler already writes.
/// `false` because the dashboard never draws the `wrap`-style reserved
/// status bar `bar_active()` tracks -- there is no bar to account for here.
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stderr = io::stderr();
        let _ = stderr.write_all(term::emergency_reset_bytes(false));
        let _ = stderr.write_all(LEAVE_ALT_SCREEN);
        let _ = stderr.flush();
        default_hook(info);
    }));
}

/// The area a pane's grid actually renders into this frame: the full
/// terminal when `zoomed` (header and sidebar skipped entirely), otherwise
/// `ui::layout`'s own `main` rect.
fn effective_main(area: Rect, sidebar_cols: u16, zoomed: bool) -> Rect {
    if zoomed {
        area
    } else {
        ui::layout(area, sidebar_cols).2
    }
}

/// Builds the env a freshly spawned pane's child needs to report its own
/// turn boundaries: `adapter.register_turn_signal` against the pane's own
/// deterministic socket path (`state.socket_for`, the same derivation
/// `Pane::spawn` binds to internally), plus `AGENT_ENV` so a nested `zirv
/// ctx ...` call inside the pane's own children defaults to this pane's own
/// harness. Mirrors `wrap.rs`'s own `turn_env` assembly
/// (`wrap.rs:1072-1086`) faithfully.
///
/// A resolution failure degrades to `AGENT_ENV` alone (the pane still
/// spawns, still gets its own socket bound by `Pane::spawn`, but the child
/// is never told where to post -- exactly the same "unsupervised, never
/// supervised by somebody else" degrade every other supervisor in this
/// codebase already accepts) and is reported back as an error string for the
/// header rather than failing the spawn.
fn build_turn_env(
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    agent_name: &str,
    session_id: &str,
) -> (Vec<(String, String)>, Option<String>) {
    match adapters::select(Some(agent_name), &[], cfg) {
        Ok(adapter) => {
            let socket = state.socket_for(session_id);
            let setup = adapter.register_turn_signal(
                &SessionRef {
                    id: SessionId::parse(session_id),
                    cwd: repo.to_path_buf(),
                },
                &socket,
            );
            let mut env = setup.env;
            env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
            (env, None)
        }
        Err(e) => (
            vec![(adapters::AGENT_ENV.to_string(), agent_name.to_string())],
            Some(format!(
                "dashboard: could not resolve adapter '{agent_name}' for turn signals: {e}"
            )),
        ),
    }
}

/// Runs the dashboard until the operator quits, owning `first` (the
/// orchestrator pane the caller already built via `build_launch`) plus
/// whatever additional panes get spawned along the way. Nesting is the
/// caller's job (Task 6 checks `sessions::nesting_refusal` before calling
/// this at all) -- `env` is accepted for interface completeness with the
/// rest of the launch pipeline and for the roster/spawn-request env lookups
/// Tasks 10-12 add, but this task's own body does not read it.
#[allow(dead_code)]
pub fn run_dashboard(
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    state: &StateDir,
    first: PaneSpec,
) -> CtxResult<i32> {
    let _ = env;

    let mut errors: Vec<String> = Vec::new();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let sidebar_cols = cfg.dash.sidebar_cols;
    let full = Rect::new(0, 0, term_cols, term_rows);
    let main = effective_main(full, sidebar_cols, false);

    let agent_name = first.agent_name.clone();
    let session_id = first.session_id.clone();
    let (turn_env, turn_env_err) = build_turn_env(cfg, state, repo, &agent_name, &session_id);
    if let Some(e) = turn_env_err {
        push_error(&mut errors, e);
    }

    let size = (main.width.max(1), main.height.max(1));
    let mut panes = vec![Pane::spawn(first, state, repo, size, &turn_env)?];

    install_panic_hook();
    if let Err(e) = enable_raw_mode() {
        let _ = std::panic::take_hook();
        return Err(format!("dashboard: enable_raw_mode failed: {e}").into());
    }
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        teardown_terminal();
        let _ = std::panic::take_hook();
        return Err(format!("dashboard: EnterAlternateScreen failed: {e}").into());
    }

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            teardown_terminal();
            let _ = std::panic::take_hook();
            return Err(format!("dashboard: could not attach to the terminal: {e}").into());
        }
    };

    let mut selected: usize = 0;
    let mut zoomed = false;
    let mut prefix_armed = false;
    let mut overlay = ui::Overlay::None;

    let exit_code: i32 = loop {
        for pane in panes.iter_mut() {
            pane.drain();
            pane.on_turn_signal();
        }

        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if !matches!(overlay, ui::Overlay::None) {
                        match key.code {
                            KeyCode::Enter if matches!(overlay, ui::Overlay::QuitConfirm(_)) => {
                                on_quit(&mut panes);
                                shutdown_all(&mut panes, cfg, &mut errors);
                                break 0;
                            }
                            KeyCode::Esc => overlay = ui::Overlay::None,
                            _ => {}
                        }
                    } else {
                        let (armed, verdict) = filter_key(prefix_armed, key);
                        prefix_armed = armed;
                        match verdict {
                            InputVerdict::Pending => {}
                            InputVerdict::ToChild(bytes) => {
                                if !bytes.is_empty()
                                    && let Some(pane) = panes.get_mut(selected)
                                    && let Err(e) = pane.write_input(&bytes)
                                {
                                    push_error(&mut errors, format!("write_input: {e}"));
                                }
                            }
                            InputVerdict::Dash(DashAction::LiteralPrefix) => {
                                if let Some(pane) = panes.get_mut(selected)
                                    && let Err(e) = pane.write_input(&literal_prefix_bytes())
                                {
                                    push_error(&mut errors, format!("write_input: {e}"));
                                }
                            }
                            InputVerdict::Dash(DashAction::Switch(i)) => {
                                if !panes.is_empty() {
                                    selected = i.min(panes.len() - 1);
                                }
                            }
                            InputVerdict::Dash(DashAction::NextPane) => {
                                if !panes.is_empty() {
                                    selected = (selected + 1) % panes.len();
                                }
                            }
                            InputVerdict::Dash(DashAction::SelectUp) => {
                                selected = selected.saturating_sub(1);
                            }
                            InputVerdict::Dash(DashAction::SelectDown) => {
                                if !panes.is_empty() {
                                    selected = (selected + 1).min(panes.len() - 1);
                                }
                            }
                            InputVerdict::Dash(DashAction::Zoom) => {
                                zoomed = !zoomed;
                                let m = effective_main(full, sidebar_cols, zoomed);
                                for pane in panes.iter_mut() {
                                    if let Err(e) = pane.resize(m.height.max(1), m.width.max(1)) {
                                        push_error(&mut errors, format!("resize: {e}"));
                                    }
                                }
                            }
                            InputVerdict::Dash(DashAction::Quit) => {
                                let working: Vec<String> = panes
                                    .iter()
                                    .filter(|p| matches!(p.state(), PaneState::Working))
                                    .map(|p| p.title().to_string())
                                    .collect();
                                if working.is_empty() {
                                    on_quit(&mut panes);
                                    shutdown_all(&mut panes, cfg, &mut errors);
                                    break 0;
                                }
                                overlay = ui::Overlay::QuitConfirm(working);
                            }
                            // Opens the corresponding overlay seam Task 4
                            // defined; Tasks 8/9/10 own the reducer that
                            // actually drives it (compose, cursor movement,
                            // submit). Esc (handled in the overlay-active
                            // branch above) closes any of these today.
                            InputVerdict::Dash(DashAction::Spawn) => {
                                overlay = ui::Overlay::Spawn(ui::SpawnDraft::default());
                            }
                            InputVerdict::Dash(DashAction::Nudge) => {
                                overlay = ui::Overlay::Nudge(ui::NudgeDraft::default());
                            }
                            InputVerdict::Dash(DashAction::Mail) => {
                                overlay = ui::Overlay::Mail(ui::MailView::default());
                            }
                            InputVerdict::Dash(DashAction::Memory) => {
                                overlay = ui::Overlay::Memory(ui::MemoryView::default());
                            }
                        }
                    }
                }
                Ok(Event::Resize(cols, rows)) => {
                    let new_full = Rect::new(0, 0, cols, rows);
                    let m = effective_main(new_full, sidebar_cols, zoomed);
                    for pane in panes.iter_mut() {
                        if let Err(e) = pane.resize(m.height.max(1), m.width.max(1)) {
                            push_error(&mut errors, format!("resize: {e}"));
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => push_error(&mut errors, format!("event read: {e}")),
            },
            Ok(false) => {}
            Err(e) => push_error(&mut errors, format!("event poll: {e}")),
        }

        let term_size = crossterm::terminal::size().unwrap_or((term_cols, term_rows));
        let frame_area = Rect::new(0, 0, term_size.0, term_size.1);
        let (header_area, sidebar_area, main_area) = ui::layout(frame_area, sidebar_cols);

        let harness = if let Some(last) = errors.last() {
            format!("{agent_name}  \u{26a0} {last}")
        } else {
            agent_name.clone()
        };
        let facts = ui::HeaderFacts {
            harness,
            score: None,
            usage_pct: None,
            mail_broadcast: 0,
            mail_direct: 0,
            memory_count: 0,
            sessions: panes.len(),
        };
        let rows: Vec<ui::SidebarRow> = panes
            .iter()
            .enumerate()
            .map(|(i, pane)| ui::SidebarRow {
                glyph: ui::glyph_for(&pane.state()),
                title: pane.title().to_string(),
                short: pane.short().to_string(),
                preview: pane.last_line(),
                attached: true,
                selected: i == selected,
            })
            .collect();

        let draw = terminal.draw(|f| {
            if !zoomed {
                ui::render_header(f, header_area, &facts);
                ui::render_sidebar(f, sidebar_area, &rows);
            }
            if let Some(pane) = panes.get(selected) {
                ui::render_grid(f, main_area, pane.screen());
            }
            ui::render_overlay(f, main_area, &overlay);
        });
        if let Err(e) = draw {
            push_error(&mut errors, format!("draw: {e}"));
        }
    };

    teardown_terminal();
    let _ = std::panic::take_hook();
    Ok(exit_code)
}

/// Shuts down every remaining pane with its own adapter's quit sequence,
/// best-effort: a shutdown failure is logged into `errors`, never
/// propagated -- the dashboard is exiting either way.
fn shutdown_all(panes: &mut [Pane], cfg: &CtxConfig, errors: &mut Vec<String>) {
    for pane in panes.iter_mut() {
        let quit_sequence = adapters::select(Some(pane.agent()), &[], cfg)
            .map(|adapter| adapter.quit_sequence())
            .unwrap_or("");
        if let Err(e) = pane.shutdown(quit_sequence) {
            push_error(errors, format!("shutdown {}: {e}", pane.short()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_keys_pass_to_the_child() {
        let (armed, v) = filter_key(false, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b == b"x"));
    }

    #[test]
    fn tab_passes_to_the_child_unprefixed() {
        let (armed, v) = filter_key(false, key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b == b"\t"));
    }

    #[test]
    fn prefix_arms_and_swallows() {
        let (armed, v) = filter_key(false, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(armed);
        assert!(matches!(v, InputVerdict::Pending));
    }

    #[test]
    fn armed_tab_switches_and_disarms() {
        let (armed, v) = filter_key(true, key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::NextPane)));
    }

    #[test]
    fn armed_digit_switches_to_that_pane() {
        let (_, v) = filter_key(true, key(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(matches!(v, InputVerdict::Dash(DashAction::Switch(0))));
        let (_, v) = filter_key(true, key(KeyCode::Char('9'), KeyModifiers::NONE));
        assert!(matches!(v, InputVerdict::Dash(DashAction::Switch(8))));
    }

    #[test]
    fn armed_ctrl_a_sends_a_literal_ctrl_a() {
        let (armed, v) = filter_key(true, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::LiteralPrefix)));
        assert_eq!(literal_prefix_bytes(), vec![0x01]);
    }

    #[test]
    fn armed_arrows_move_the_pane_selection() {
        let (armed, v) = filter_key(true, key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::SelectUp)));

        let (armed, v) = filter_key(true, key(KeyCode::Down, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::SelectDown)));

        // Unarmed arrows still pass to the child (claude uses them).
        let (armed, v) = filter_key(false, key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b == b"\x1b[A"));
    }

    #[test]
    fn prefix_matches_the_raw_control_byte_shape_too() {
        // Windows can deliver Ctrl+A as the raw SOH byte with no modifier
        // flag (docs/superpowers/notes/2026-08-13-vt100-spike.md).
        let (armed, v) = filter_key(false, key(KeyCode::Char('\u{01}'), KeyModifiers::NONE));
        assert!(armed);
        assert!(matches!(v, InputVerdict::Pending));

        let (armed, v) = filter_key(true, key(KeyCode::Char('\u{01}'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::LiteralPrefix)));
    }

    #[test]
    fn armed_unknown_key_disarms_and_forwards_nothing() {
        let (armed, v) = filter_key(true, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b.is_empty()));
    }

    #[test]
    fn encode_key_covers_the_terminal_basics() {
        assert_eq!(encode_key(key(KeyCode::Enter, KeyModifiers::NONE)), b"\r");
        assert_eq!(encode_key(key(KeyCode::Up, KeyModifiers::NONE)), b"\x1b[A");
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        assert_eq!(
            encode_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            b"\x1b[Z"
        );
    }

    #[test]
    fn encode_key_passes_a_raw_control_byte_through_unchanged() {
        // No CONTROL modifier flag -- the raw-control-byte shape from the
        // spike note -- still round-trips to the same single byte, since
        // ASCII control characters are their own UTF-8 encoding.
        assert_eq!(
            encode_key(key(KeyCode::Char('\u{01}'), KeyModifiers::NONE)),
            vec![0x01]
        );
    }

    #[test]
    fn armed_spawn_nudge_mail_memory_keys_are_recognised() {
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('s'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Spawn)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('n'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Nudge)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('m'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Mail)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('M'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Memory)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('z'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Zoom)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('q'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Quit)
        ));
    }

    #[test]
    fn effective_main_returns_the_full_area_when_zoomed() {
        let area = Rect::new(0, 0, 100, 30);
        let zoomed = effective_main(area, 24, true);
        assert_eq!(zoomed, area);
        let unzoomed = effective_main(area, 24, false);
        assert_eq!(unzoomed, ui::layout(area, 24).2);
    }

    #[test]
    fn push_error_keeps_only_the_most_recent_handful() {
        let mut errors = Vec::new();
        for i in 0..10 {
            push_error(&mut errors, format!("err{i}"));
        }
        assert_eq!(errors.len(), MAX_KEPT_ERRORS);
        assert_eq!(errors.last().unwrap(), "err9");
        assert_eq!(errors.first().unwrap(), "err5");
    }
}
