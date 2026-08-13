//! `zirv chat`'s session multiplexer: a dashboard process owning N
//! interactive ConPTY harness sessions, each rendered through its own
//! embedded `vt100` screen model.
//!
//! This module carries the event loop itself (`run_dashboard`) plus the pure
//! input filter (`filter_key`/`encode_key`) that decides, for every
//! keystroke, whether it goes straight to the active pane's child or gets
//! swallowed as a dashboard command behind the `Ctrl+A` prefix.
//! `chat.rs::run_with` calls `run_dashboard` once `chrome::dash_eligible`
//! says the terminal can carry it (Task 6); every ineligible terminal
//! (`--simple`, non-terminal stdio, too small, or the dashboard turned off
//! in config) still reaches today's `wrap::run_with` passthrough instead.

pub mod pane;
pub mod ui;

use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

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
use super::{mail, memory, sessions, window};

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

// Task 7: sidebar row assembly (dashboard panes + view-only registry rows)
// and the header's own live facts (mail, memory-bank size, usage, session
// count), both refreshed at most once per second.

/// One dashboard-owned pane's row inputs, decoupled from `Pane` itself so
/// `assemble_sidebar` stays pure and testable without a real spawn.
struct PaneRowMeta {
    short: String,
    title: String,
    glyph: char,
    preview: String,
}

/// The glyph a view-only registry row gets: this dashboard does not own that
/// session's turn-signal stream, so `PaneState`'s Working/Idle distinction is
/// genuinely unknown here -- only that the process is still alive
/// (`assemble_sidebar`'s caller has already filtered to `Liveness::Live`).
/// Deliberately distinct from every glyph `ui::glyph_for` produces, so a
/// view-only row never claims to know a pane state it cannot see.
const VIEW_ONLY_GLYPH: char = '\u{00b7}';

/// Combines this dashboard's own panes (attached, in pane order) with every
/// OTHER live session in the registry (view-only, `attached: false`), so the
/// sidebar shows every registered session, not only the ones this process
/// spawned. Deduped by short id -- a pane's own registry record is never
/// listed a second time as a view-only row. Dead/stale registry entries
/// (`Liveness::Stale`) are excluded outright: `sessions::list` already swept
/// them from disk, and a dashboard has nothing useful to attach to or nudge
/// there. `selected` indexes into the combined list this returns. Pure: no
/// I/O of its own -- `registry` is whatever the caller already read via
/// `sessions::list`.
fn assemble_sidebar(
    panes: &[PaneRowMeta],
    registry: &[(sessions::Record, sessions::Liveness)],
    selected: usize,
) -> Vec<ui::SidebarRow> {
    let own_shorts: HashSet<&str> = panes.iter().map(|p| p.short.as_str()).collect();

    let mut rows: Vec<ui::SidebarRow> = panes
        .iter()
        .map(|p| ui::SidebarRow {
            glyph: p.glyph,
            title: p.title.clone(),
            short: p.short.clone(),
            preview: p.preview.clone(),
            attached: true,
            selected: false,
        })
        .collect();

    for (record, liveness) in registry {
        if *liveness != sessions::Liveness::Live {
            continue;
        }
        if own_shorts.contains(record.short.as_str()) {
            continue;
        }
        rows.push(ui::SidebarRow {
            glyph: VIEW_ONLY_GLYPH,
            title: format!("{} {}", record.verb.as_str(), record.agent),
            short: record.short.clone(),
            preview: String::new(),
            attached: false,
            selected: false,
        });
    }

    if let Some(row) = rows.get_mut(selected) {
        row.selected = true;
    }
    rows
}

/// `window::max_used_percentage`'s `0.0..=100.0` reading, rounded to the
/// nearest whole percent for the header's compact `usage NN%` display and
/// clamped defensively to the documented range. Mirrors `chrome::status_bar`'s
/// own `{:.0}%` rounding so the dashboard header and wrap's status bar never
/// disagree about the same underlying number.
fn usage_pct_u8(percent: Option<f64>) -> Option<u8> {
    percent.map(|p| p.round().clamp(0.0, 100.0) as u8)
}

/// Pure: assembles `ui::HeaderFacts` from already-computed ingredients. Kept
/// separate from `FactsCache::refresh_if_due` (the impure disk-reading half)
/// so the header's own rendering rule -- a disabled/absent mail read renders
/// as no mail segment at all, never a hollow `mail 0+0` -- is exercised
/// without a state dir. Mirrors `ui`'s own `HeaderFacts` field order.
fn assemble_header_facts(
    harness: String,
    score: Option<u32>,
    usage_pct: Option<u8>,
    mail: Option<(usize, usize)>,
    memory_count: usize,
    sessions: usize,
) -> ui::HeaderFacts {
    let (mail_broadcast, mail_direct) = mail.unwrap_or((0, 0));
    ui::HeaderFacts {
        harness,
        score,
        usage_pct,
        mail_broadcast,
        mail_direct,
        memory_count,
        sessions,
    }
}

/// How often the header's own disk-backed facts (mail, memory-bank size,
/// usage) -- and the session registry the sidebar's view-only rows come
/// from -- are re-read. Mirrors wrap's own `BAR_THROTTLE`/`BarRuntime::
/// last_draw` pattern (`wrap.rs:1362`): the render loop polls every 50ms,
/// but nothing here needs a disk hit that often.
const FACTS_THROTTLE: Duration = Duration::from_secs(1);

/// The disk-backed part of the header's facts: everything `FactsCache::
/// refresh_if_due` re-reads on the throttle. Kept separate from
/// `ui::HeaderFacts` itself because the harness/error line and the live
/// session count are cheap, in-memory state recomputed fresh on every frame
/// regardless -- only these fields, plus the registry listing below, cost an
/// actual read.
#[derive(Default)]
struct DiskFacts {
    usage_pct: Option<u8>,
    mail: Option<(usize, usize)>,
    memory_count: usize,
}

/// Caches every disk read the header and sidebar need -- usage, mail,
/// memory-bank size, and the session registry itself -- refreshed at most
/// once per `FACTS_THROTTLE` rather than on the render loop's own 50ms poll.
struct FactsCache {
    disk: DiskFacts,
    registry: Vec<(sessions::Record, sessions::Liveness)>,
    last_refresh: Instant,
}

impl FactsCache {
    /// Never refreshed yet, so the very first check always reads through.
    /// `checked_sub` (rather than a bare subtraction) degrades to "refresh
    /// immediately" on a process uptime under a second, the same reasoning
    /// `BarRuntime::new` documents for its own `last_draw`.
    fn new(now: Instant) -> Self {
        Self {
            disk: DiskFacts::default(),
            registry: Vec::new(),
            last_refresh: now.checked_sub(FACTS_THROTTLE).unwrap_or(now),
        }
    }

    fn refresh_if_due(
        &mut self,
        cfg: &CtxConfig,
        state: &StateDir,
        repo: &Path,
        agent_name: &str,
        session_short: &str,
        now: Instant,
    ) {
        if now.duration_since(self.last_refresh) < FACTS_THROTTLE {
            return;
        }
        self.last_refresh = now;

        let windows = window::load(state);
        self.disk.usage_pct = usage_pct_u8(window::max_used_percentage(&windows));
        self.disk.mail =
            mail::unread_counts(state, repo, agent_name, session_short, cfg.mail.enabled);
        let slug = super::state::repo_slug(repo);
        self.disk.memory_count = memory::list(state, &slug).map(|v| v.len()).unwrap_or(0);
        self.registry = sessions::list(state);
    }
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

// Task 8: mail + memory overlays, driven by the same pure-reducer pattern
// `filter_key`/`encode_key` already established -- typing and navigation are
// pure functions from `(view, key)` to `(next view or close, effect)`; only
// the effect (a mail send/consume, a memory remember/forget/verify) touches
// disk, and only from `run_dashboard`'s own loop, through the exact same
// library functions the CLI verbs call.

/// Clamps a cursor into `0..len` (or `0` on an empty list) -- shared by every
/// browsing-mode reducer below so "move past the last row" and "the list
/// just shrank out from under the cursor" (an item consumed/forgotten while
/// selected) both land on a valid index.
fn clamp_cursor(cursor: usize, len: usize) -> usize {
    if len == 0 { 0 } else { cursor.min(len - 1) }
}

/// Pure: one keystroke against the mail overlay's current state. Returns the
/// overlay's next state (`None` closes it -- Esc while browsing) alongside
/// any effect the caller must execute against real storage. Esc while
/// composing cancels only the compose draft, not the whole overlay.
pub fn mail_overlay_reduce(
    mut view: ui::MailView,
    key: KeyEvent,
) -> (Option<ui::MailView>, Option<ui::MailEffect>) {
    if let Some(draft) = view.compose.as_mut() {
        return match key.code {
            KeyCode::Esc => {
                view.compose = None;
                (Some(view), None)
            }
            KeyCode::Enter => {
                if draft.body.trim().is_empty() {
                    return (Some(view), None);
                }
                let to = if draft.to.trim().is_empty() {
                    "any".to_string()
                } else {
                    draft.to.clone()
                };
                let body = draft.body.clone();
                view.compose = None;
                let msg = mail::Message {
                    // Placeholders: `apply_mail_effect` overwrites all three
                    // right before `mail::store` -- see `ui::MailEffect`'s
                    // own doc comment for why the reducer never touches
                    // identity or the clock itself.
                    from_session: String::new(),
                    from_agent: String::new(),
                    to,
                    to_session: None,
                    sent: 0,
                    body,
                };
                (Some(view), Some(ui::MailEffect::Send(msg)))
            }
            KeyCode::Backspace => {
                draft.body.pop();
                (Some(view), None)
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.body.push(c);
                (Some(view), None)
            }
            _ => (Some(view), None),
        };
    }

    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = clamp_cursor(view.cursor + 1, view.items.len());
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = view.cursor.saturating_sub(1);
            (Some(view), None)
        }
        KeyCode::Char('c') => {
            view.compose = Some(ui::ComposeDraft::default());
            (Some(view), None)
        }
        KeyCode::Enter => {
            if view.items.is_empty() {
                return (Some(view), None);
            }
            let (path, _, _) = view.items.remove(view.cursor);
            view.cursor = clamp_cursor(view.cursor, view.items.len());
            (Some(view), Some(ui::MailEffect::Consume(path)))
        }
        _ => (Some(view), None),
    }
}

/// Pure: the same shape as `mail_overlay_reduce`, for the memory bank. `r`
/// (remember) seeds the edit buffer with the selected entry's current body
/// so the operator edits rather than retypes; `d`/`v` (forget/verify) act on
/// the selected entry immediately, no confirmation dialog.
pub fn memory_overlay_reduce(
    mut view: ui::MemoryView,
    key: KeyEvent,
) -> (Option<ui::MemoryView>, Option<ui::MemoryEffect>) {
    if let Some(input) = view.input.as_mut() {
        return match key.code {
            KeyCode::Esc => {
                view.input = None;
                (Some(view), None)
            }
            KeyCode::Enter => {
                if input.trim().is_empty() {
                    return (Some(view), None);
                }
                let Some((key_name, _, _)) = view.entries.get(view.cursor).cloned() else {
                    view.input = None;
                    return (Some(view), None);
                };
                let body = input.clone();
                view.input = None;
                (
                    Some(view),
                    Some(ui::MemoryEffect::Remember {
                        key: key_name,
                        body,
                    }),
                )
            }
            KeyCode::Backspace => {
                input.pop();
                (Some(view), None)
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                (Some(view), None)
            }
            _ => (Some(view), None),
        };
    }

    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = clamp_cursor(view.cursor + 1, view.entries.len());
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = view.cursor.saturating_sub(1);
            (Some(view), None)
        }
        KeyCode::Char('r') => {
            if let Some((_, _, body)) = view.entries.get(view.cursor) {
                view.input = Some(body.clone());
            }
            (Some(view), None)
        }
        KeyCode::Char('d') => {
            if view.entries.is_empty() {
                return (Some(view), None);
            }
            let (key_name, _, _) = view.entries.remove(view.cursor);
            view.cursor = clamp_cursor(view.cursor, view.entries.len());
            (Some(view), Some(ui::MemoryEffect::Forget(key_name)))
        }
        KeyCode::Char('v') => match view.entries.get(view.cursor) {
            Some((key_name, _, _)) => {
                let effect = ui::MemoryEffect::Verify(key_name.clone());
                (Some(view), Some(effect))
            }
            None => (Some(view), None),
        },
        _ => (Some(view), None),
    }
}

/// Executes a `MailEffect` against real storage -- the only place either
/// reducer's output actually touches disk. `from_session`/`from_agent` are
/// this dashboard's own identity (the orchestrator pane's short id and this
/// session's agent name), stamped onto a `Send` right before `mail::store`.
fn apply_mail_effect(
    effect: ui::MailEffect,
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    from_session: &str,
    from_agent: &str,
    errors: &mut Vec<String>,
) {
    let slug = super::state::repo_slug(repo);
    match effect {
        ui::MailEffect::Consume(path) => {
            if let Err(e) = mail::consume(state, &slug, &path) {
                push_error(errors, format!("mail consume: {e}"));
            }
        }
        ui::MailEffect::Send(mut msg) => {
            msg.from_session = from_session.to_string();
            msg.from_agent = from_agent.to_string();
            msg.sent = super::state::now_secs();
            if let Err(e) = mail::store(state, &slug, &msg, cfg) {
                push_error(errors, format!("mail send: {e}"));
            }
        }
    }
}

/// Executes a `MemoryEffect` against real storage. `written_by` is this
/// dashboard's own agent name, the same convention `run_remember_with` uses
/// for `AGENT_ENV`.
fn apply_memory_effect(
    effect: ui::MemoryEffect,
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    written_by: &str,
    errors: &mut Vec<String>,
) {
    let slug = super::state::repo_slug(repo);
    match effect {
        ui::MemoryEffect::Remember { key, body } => {
            let now = super::state::now_secs();
            let entry = memory::Entry {
                key,
                written_by: written_by.to_string(),
                written: now,
                verified: now,
                source: "explicit".to_string(),
                body,
            };
            if let Err(e) = memory::remember(state, &slug, &entry, cfg) {
                push_error(errors, format!("memory remember: {e}"));
            }
        }
        ui::MemoryEffect::Forget(key) => {
            if let Err(e) = memory::forget(state, &slug, &key) {
                push_error(errors, format!("memory forget: {e}"));
            }
        }
        ui::MemoryEffect::Verify(key) => {
            if let Err(e) = memory::verify(state, &slug, &key) {
                push_error(errors, format!("memory verify: {e}"));
            }
        }
    }
}

/// A single-line preview of a body: its first line, capped short enough to
/// fit the overlay dialog next to a `from`/`key` label.
fn mail_preview(body: &str) -> String {
    body.lines().next().unwrap_or("").chars().take(60).collect()
}

/// Builds a freshly-populated `MailView` from every message currently
/// visible to the dashboard operator -- `for_agent`/`for_session` both
/// `None`, the same broad "everything in this repo's mailbox" view
/// `zirv ctx inbox` gives a human, not the narrow per-session filter a
/// delivery seam applies. A read error degrades to an empty view rather than
/// failing the overlay open.
fn build_mail_view(state: &StateDir, repo: &Path) -> ui::MailView {
    let slug = super::state::repo_slug(repo);
    let items = mail::list(state, &slug, None, None)
        .unwrap_or_default()
        .into_iter()
        .map(|(path, msg)| (path, msg.from_agent, mail_preview(&msg.body)))
        .collect();
    ui::MailView {
        items,
        cursor: 0,
        compose: None,
    }
}

/// Builds a freshly-populated `MemoryView` from this repo's whole memory
/// bank. The age wording matches `memory::render_for_prompt`'s own
/// convention ("written Nd ago, verified Nd ago") so it reads the same
/// everywhere it appears.
fn build_memory_view(state: &StateDir, repo: &Path) -> ui::MemoryView {
    let slug = super::state::repo_slug(repo);
    let now = super::state::now_secs();
    let entries = memory::list(state, &slug)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, entry)| {
            let age = format!(
                "written {}d ago, verified {}d ago",
                now.saturating_sub(entry.written) / 86_400,
                now.saturating_sub(entry.verified) / 86_400,
            );
            (entry.key, age, entry.body)
        })
        .collect();
    ui::MemoryView {
        entries,
        cursor: 0,
        input: None,
    }
}

// Task 9: idle-gated visible intervention -- a per-pane nudge queue drained
// only once the pane is `Idle`, plus a once-per-tick mail sweep that injects
// swept mail the same visible way. Both share the same read-once discipline
// mail delivery already holds itself to elsewhere (`exec`/`loop`): a message
// is only ever marked consumed after it was actually shown to the agent.

/// Pure: whether a pane with `queued` nudges waiting should have the next one
/// delivered right now -- idle, and there is something to deliver.
pub fn deliverable_now(state: &PaneState, queued: usize) -> bool {
    matches!(state, PaneState::Idle) && queued > 0
}

/// Pops the next queued nudge for one pane if `deliverable_now` allows it;
/// otherwise leaves the queue untouched. Pure aside from the `VecDeque`
/// mutation -- no pane, no I/O -- so the FIFO-drain-on-idle rule is testable
/// without a real spawn.
fn next_deliverable(queue: &mut VecDeque<String>, state: &PaneState) -> Option<String> {
    if deliverable_now(state, queue.len()) {
        queue.pop_front()
    } else {
        None
    }
}

/// Thin seam over `Pane::inject_visible` so the mail sweep's "consume only
/// after a successful visible injection" rule can be exercised without a
/// real pty writer: `Pane` is the only production implementer; a test-only
/// double can force an `Err` to prove a failed write leaves the source
/// message file untouched (C7 discipline -- a message never actually shown
/// to the agent must not be marked read).
pub(crate) trait Injector {
    fn try_inject(&mut self, label: &str, body: &str) -> CtxResult<()>;
}

impl Injector for Pane {
    fn try_inject(&mut self, label: &str, body: &str) -> CtxResult<()> {
        self.inject_visible(label, body)
    }
}

/// Delivers one mail message visibly into `injector`, consuming the source
/// file (moving it to `read/`) ONLY if the injection itself returned `Ok`.
fn deliver_and_consume<I: Injector>(
    injector: &mut I,
    state: &StateDir,
    slug: &str,
    label: &str,
    path: &Path,
    body: &str,
) -> CtxResult<()> {
    injector.try_inject(label, body)?;
    mail::consume(state, slug, path)
}

/// Pure: whether a pane in `verb`/`state` is a valid mail-sweep target --
/// only an attached *worker* pane (`Verb::Dash`) that is currently `Idle`.
/// The orchestrator pane (`Verb::Chat`) is deliberately excluded here, not
/// just skipped by convention: it is never body-injected, only ever told a
/// one-line unread-count advisory (the header's own mail segment) -- the
/// same trust split every other mail delivery seam in this codebase already
/// holds for an interactive Orchestrator session.
fn is_delivery_eligible(verb: sessions::Verb, state: &PaneState) -> bool {
    verb == sessions::Verb::Dash && matches!(state, PaneState::Idle)
}

/// Once-per-tick mail sweep: every attached worker pane that is `Idle` gets
/// its own unread mail (the same per-session visibility `unread_counts`
/// already applies: addressed to its agent, and either undirected or
/// addressed to its own short id) injected visibly, one message at a time,
/// each consumed only after a successful injection.
fn mail_sweep(
    panes: &mut [Pane],
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    errors: &mut Vec<String>,
) {
    if !cfg.mail.enabled {
        return;
    }
    let slug = super::state::repo_slug(repo);
    for pane in panes.iter_mut() {
        if !is_delivery_eligible(pane.verb(), &pane.state()) {
            continue;
        }
        let agent = pane.agent().to_string();
        let short = pane.short().to_string();
        let messages = match mail::list(state, &slug, Some(&agent), Some(&short)) {
            Ok(m) => m,
            Err(e) => {
                push_error(errors, format!("mail sweep: {e}"));
                continue;
            }
        };
        for (path, msg) in messages {
            let label = format!(
                "mail from {}/{}",
                msg.from_agent,
                sessions::short_id(&msg.from_session)
            );
            if let Err(e) = deliver_and_consume(pane, state, &slug, &label, &path, &msg.body) {
                push_error(errors, format!("mail sweep: {e}"));
            }
        }
    }
}

/// Once-per-tick FIFO drain: for every pane whose queue has something
/// deliverable right now, injects exactly the next one (never the whole
/// queue at once -- one visible line per tick keeps the child's input
/// stream readable). `panes` and `queues` are kept the same length by every
/// caller that grows `panes` (today, only the initial spawn in
/// `run_dashboard`; a future spawn seam -- Tasks 10/11 -- must push a
/// matching `VecDeque::new()` here too).
fn deliver_queued_nudges(
    panes: &mut [Pane],
    queues: &mut [VecDeque<String>],
    errors: &mut Vec<String>,
) {
    for (pane, queue) in panes.iter_mut().zip(queues.iter_mut()) {
        if let Some(text) = next_deliverable(queue, &pane.state())
            && let Err(e) = pane.inject_visible("nudge from operator", &text)
        {
            push_error(errors, format!("nudge delivery: {e}"));
        }
    }
}

/// Handles a submitted `NudgeDraft`: an attached pane gets `inject_visible`
/// immediately if `Idle`, or is queued (FIFO, drained by
/// `deliver_queued_nudges` once it goes idle) if still `Working`; a
/// view-only row is routed through the existing headless
/// `sessions::run_nudge_with` (marker + mail + restart, unchanged). `target
/// == None` (nothing was selected when the dialog opened) is a no-op.
fn submit_nudge(
    target: ui::NudgeTarget,
    text: &str,
    panes: &mut [Pane],
    queues: &mut [VecDeque<String>],
    repo: &Path,
    env: EnvLookup<'_>,
    errors: &mut Vec<String>,
) {
    match target {
        ui::NudgeTarget::AttachedPane(i) => {
            let Some(pane) = panes.get_mut(i) else {
                return;
            };
            if matches!(pane.state(), PaneState::Idle) {
                if let Err(e) = pane.inject_visible("nudge from operator", text) {
                    push_error(errors, format!("nudge: {e}"));
                }
            } else if let Some(queue) = queues.get_mut(i) {
                queue.push_back(text.to_string());
                push_error(errors, "nudge queued -- delivers when idle".to_string());
            }
        }
        ui::NudgeTarget::ViewOnlySession(short) => {
            let args = sessions::NudgeArgs {
                prefix: short,
                message: Some(text.to_string()),
                message_file: None,
            };
            let mut sink = Vec::new();
            let mut stdin = std::io::empty();
            if let Err(e) = sessions::run_nudge_with(&args, &mut sink, repo, env, &mut stdin) {
                push_error(errors, format!("nudge: {e}"));
            }
        }
        ui::NudgeTarget::None => {}
    }
}

/// The `PaneRowMeta` list for every pane this dashboard currently owns, in
/// pane order -- shared by the pre-input (routing) and post-input
/// (rendering) calls to `assemble_sidebar` each tick.
fn build_pane_rows(panes: &[Pane]) -> Vec<PaneRowMeta> {
    panes
        .iter()
        .map(|pane| PaneRowMeta {
            short: pane.short().to_string(),
            title: pane.title().to_string(),
            glyph: ui::glyph_for(&pane.state()),
            preview: pane.last_line(),
        })
        .collect()
}

/// Runs the dashboard until the operator quits, owning `first` (the
/// orchestrator pane the caller already built via `build_launch`) plus
/// whatever additional panes get spawned along the way. Nesting is the
/// caller's job (`chat.rs::run_with` checks `sessions::nesting_refusal`
/// before calling this at all).
pub fn run_dashboard(
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    state: &StateDir,
    first: PaneSpec,
) -> CtxResult<i32> {
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
    // Task 9: one FIFO nudge queue per pane, kept the same length as `panes`.
    // Nothing in this task's scope ever grows `panes` after this point (a
    // future spawn seam -- Tasks 10/11 -- must push a matching
    // `VecDeque::new()` here too whenever it pushes a new pane).
    let mut nudge_queues: Vec<VecDeque<String>> = vec![VecDeque::new(); panes.len()];

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
    let mut facts_cache = FactsCache::new(Instant::now());

    let exit_code: i32 = loop {
        for pane in panes.iter_mut() {
            pane.drain();
            pane.on_turn_signal();
        }

        mail_sweep(&mut panes, cfg, state, repo, &mut errors);
        deliver_queued_nudges(&mut panes, &mut nudge_queues, &mut errors);

        // Facts + sidebar rows, computed BEFORE input handling: the Nudge
        // dialog's attached-vs-view-only routing and the SelectUp/SelectDown
        // clamp both need this iteration's row layout, not a rendering-only
        // snapshot taken after the keystroke that needs it.
        let dashboard_short = panes
            .first()
            .map(|p| p.short().to_string())
            .unwrap_or_default();
        facts_cache.refresh_if_due(
            cfg,
            state,
            repo,
            &agent_name,
            &dashboard_short,
            Instant::now(),
        );
        let rows = assemble_sidebar(&build_pane_rows(&panes), &facts_cache.registry, selected);
        let total_rows = rows.len();

        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if !matches!(overlay, ui::Overlay::None) {
                        let current = std::mem::take(&mut overlay);
                        match current {
                            ui::Overlay::None => {}
                            ui::Overlay::QuitConfirm(working) => match key.code {
                                KeyCode::Enter => {
                                    on_quit(&mut panes);
                                    shutdown_all(&mut panes, cfg, &mut errors);
                                    break 0;
                                }
                                KeyCode::Esc => {}
                                _ => overlay = ui::Overlay::QuitConfirm(working),
                            },
                            ui::Overlay::Spawn(d) => match key.code {
                                KeyCode::Esc => {}
                                _ => overlay = ui::Overlay::Spawn(d),
                            },
                            ui::Overlay::Restore(d) => match key.code {
                                KeyCode::Esc => {}
                                _ => overlay = ui::Overlay::Restore(d),
                            },
                            ui::Overlay::Mail(view) => {
                                let (next, effect) = mail_overlay_reduce(view, key);
                                overlay = match next {
                                    Some(v) => ui::Overlay::Mail(v),
                                    None => ui::Overlay::None,
                                };
                                if let Some(effect) = effect {
                                    apply_mail_effect(
                                        effect,
                                        state,
                                        repo,
                                        cfg,
                                        &dashboard_short,
                                        &agent_name,
                                        &mut errors,
                                    );
                                }
                            }
                            ui::Overlay::Memory(view) => {
                                let (next, effect) = memory_overlay_reduce(view, key);
                                overlay = match next {
                                    Some(v) => ui::Overlay::Memory(v),
                                    None => ui::Overlay::None,
                                };
                                if let Some(effect) = effect {
                                    apply_memory_effect(
                                        effect,
                                        state,
                                        repo,
                                        cfg,
                                        &agent_name,
                                        &mut errors,
                                    );
                                }
                            }
                            ui::Overlay::Nudge(mut draft) => match key.code {
                                KeyCode::Esc => {}
                                KeyCode::Enter => {
                                    let text = draft.input.trim().to_string();
                                    if !text.is_empty() {
                                        submit_nudge(
                                            draft.target,
                                            &text,
                                            &mut panes,
                                            &mut nudge_queues,
                                            repo,
                                            env,
                                            &mut errors,
                                        );
                                    }
                                }
                                KeyCode::Backspace => {
                                    draft.input.pop();
                                    overlay = ui::Overlay::Nudge(draft);
                                }
                                KeyCode::Char(c)
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    draft.input.push(c);
                                    overlay = ui::Overlay::Nudge(draft);
                                }
                                _ => overlay = ui::Overlay::Nudge(draft),
                            },
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
                                // Clamped against the *combined* row count
                                // (attached panes plus view-only registry
                                // rows), not just `panes.len()`: Task 9's
                                // Nudge dialog has to be able to select a
                                // view-only row to route a nudge through
                                // `sessions::run_nudge_with`.
                                if total_rows > 0 {
                                    selected = (selected + 1).min(total_rows - 1);
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
                            // defined. Spawn's own reducer is Task 10/11's;
                            // Esc (handled in the overlay-active branch
                            // above) closes it in the meantime.
                            InputVerdict::Dash(DashAction::Spawn) => {
                                overlay = ui::Overlay::Spawn(ui::SpawnDraft::default());
                            }
                            InputVerdict::Dash(DashAction::Nudge) => {
                                // `selected` addresses the combined row list
                                // (`rows`, this iteration's): an index below
                                // `panes.len()` is an attached pane, at or
                                // above it is a view-only registry row named
                                // by that same index in `rows`.
                                let target = if selected < panes.len() {
                                    ui::NudgeTarget::AttachedPane(selected)
                                } else {
                                    rows.get(selected)
                                        .map(|row| {
                                            ui::NudgeTarget::ViewOnlySession(row.short.clone())
                                        })
                                        .unwrap_or(ui::NudgeTarget::None)
                                };
                                overlay = ui::Overlay::Nudge(ui::NudgeDraft {
                                    target,
                                    input: String::new(),
                                });
                            }
                            InputVerdict::Dash(DashAction::Mail) => {
                                overlay = ui::Overlay::Mail(build_mail_view(state, repo));
                            }
                            InputVerdict::Dash(DashAction::Memory) => {
                                overlay = ui::Overlay::Memory(build_memory_view(state, repo));
                            }
                        }
                    }
                }
                Ok(Event::Resize(cols, term_h)) => {
                    let new_full = Rect::new(0, 0, cols, term_h);
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

        // Recomputed (facts_cache itself was already refreshed above, before
        // input handling) so the sidebar's own `.selected` highlight
        // reflects any selection change the keystroke just made -- cheap and
        // pure, unlike the disk-backed facts refresh it does not repeat.
        let rows = assemble_sidebar(&build_pane_rows(&panes), &facts_cache.registry, selected);

        let harness = if let Some(last) = errors.last() {
            format!("{agent_name}  \u{26a0} {last}")
        } else {
            agent_name.clone()
        };
        let facts = assemble_header_facts(
            harness,
            None,
            facts_cache.disk.usage_pct,
            facts_cache.disk.mail,
            facts_cache.disk.memory_count,
            rows.len(),
        );

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
    use std::path::PathBuf;

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

    // Task 7: sidebar row assembly + header facts.

    fn pane_row(short: &str, title: &str) -> PaneRowMeta {
        PaneRowMeta {
            short: short.to_string(),
            title: title.to_string(),
            glyph: '\u{25cf}',
            preview: "hi".to_string(),
        }
    }

    fn registry_record(short: &str, agent: &str) -> sessions::Record {
        sessions::Record {
            session: format!("session-{short}"),
            short: short.to_string(),
            agent: agent.to_string(),
            repo: std::path::PathBuf::from("/repo"),
            repo_slug: "-repo".to_string(),
            verb: sessions::Verb::Wrap,
            pid: 1,
            started_at: 0,
            reachable: true,
        }
    }

    #[test]
    fn assemble_sidebar_lists_dashboard_panes_first_in_pane_order() {
        let panes = vec![
            pane_row("aaa11111", "orch"),
            pane_row("bbb22222", "wrk claude"),
        ];
        let rows = assemble_sidebar(&panes, &[], 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].short, "aaa11111");
        assert!(rows[0].attached);
        assert_eq!(rows[1].short, "bbb22222");
        assert!(rows[1].attached);
    }

    #[test]
    fn assemble_sidebar_appends_view_only_registry_rows_not_owned_by_this_dashboard() {
        let panes = vec![pane_row("aaa11111", "orch")];
        let registry = vec![(
            registry_record("ccc33333", "codex"),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].short, "ccc33333");
        assert!(!rows[1].attached, "a registry-only row is never attached");
    }

    #[test]
    fn assemble_sidebar_dedupes_a_panes_own_registry_record() {
        let panes = vec![pane_row("aaa11111", "orch")];
        let registry = vec![(
            registry_record("aaa11111", "claude"),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, 0);
        assert_eq!(
            rows.len(),
            1,
            "the pane's own registry record must not appear a second time"
        );
        assert!(rows[0].attached);
    }

    #[test]
    fn assemble_sidebar_excludes_stale_registry_entries() {
        let registry = vec![(
            registry_record("ddd44444", "codex"),
            sessions::Liveness::Stale,
        )];
        let rows = assemble_sidebar(&[], &registry, 0);
        assert!(
            rows.is_empty(),
            "a dead session must not appear as a view-only row"
        );
    }

    #[test]
    fn assemble_sidebar_marks_the_selected_index_in_the_combined_list() {
        let panes = vec![pane_row("aaa11111", "orch")];
        let registry = vec![(
            registry_record("ccc33333", "codex"),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, 1);
        assert!(!rows[0].selected);
        assert!(rows[1].selected);
    }

    #[test]
    fn assemble_header_facts_omits_mail_when_none() {
        let facts = assemble_header_facts("claude".to_string(), None, Some(42), None, 3, 5);
        assert_eq!(facts.mail_broadcast, 0);
        assert_eq!(facts.mail_direct, 0);
        assert_eq!(facts.usage_pct, Some(42));
        assert_eq!(facts.memory_count, 3);
        assert_eq!(facts.sessions, 5);
    }

    #[test]
    fn assemble_header_facts_carries_the_broadcast_direct_split_through() {
        let facts = assemble_header_facts("claude".to_string(), Some(12), None, Some((2, 1)), 0, 1);
        assert_eq!(facts.mail_broadcast, 2);
        assert_eq!(facts.mail_direct, 1);
        assert_eq!(facts.score, Some(12));
    }

    #[test]
    fn usage_pct_u8_rounds_and_clamps() {
        assert_eq!(usage_pct_u8(None), None);
        assert_eq!(usage_pct_u8(Some(63.4)), Some(63));
        assert_eq!(usage_pct_u8(Some(63.5)), Some(64));
        assert_eq!(usage_pct_u8(Some(150.0)), Some(100));
        assert_eq!(usage_pct_u8(Some(-5.0)), Some(0));
    }

    #[test]
    fn facts_cache_refreshes_immediately_then_honors_the_throttle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();

        let mut cache = FactsCache::new(now);
        cache.refresh_if_due(&cfg, &state, &repo, "claude", "sess0000", now);
        assert_eq!(
            cache.disk.mail,
            Some((0, 0)),
            "the first check must refresh even though `last_refresh` was just set"
        );

        // A message stored right after that first refresh must not be seen
        // again until the throttle elapses.
        let slug = super::super::state::repo_slug(&repo);
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "other".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        cache.refresh_if_due(&cfg, &state, &repo, "claude", "sess0000", now);
        assert_eq!(
            cache.disk.mail,
            Some((0, 0)),
            "within the throttle window, the cached facts must not change"
        );

        let later = now + FACTS_THROTTLE;
        cache.refresh_if_due(&cfg, &state, &repo, "claude", "sess0000", later);
        assert_eq!(
            cache.disk.mail,
            Some((1, 0)),
            "once the throttle elapses, the disk-backed facts refresh"
        );
    }

    // Task 8: mail + memory overlay reducers -- pure, no I/O.

    fn press(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn mail_overlay_esc_while_browsing_closes_the_overlay() {
        let (next, effect) = mail_overlay_reduce(
            ui::MailView::default(),
            key(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn mail_overlay_cursor_clamps_within_bounds() {
        let view = ui::MailView {
            items: vec![
                (PathBuf::from("/a"), "claude".to_string(), "one".to_string()),
                (PathBuf::from("/b"), "codex".to_string(), "two".to_string()),
            ],
            cursor: 0,
            compose: None,
        };

        let (next, _) = mail_overlay_reduce(view.clone(), key(KeyCode::Down, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.cursor, 1);

        // Past the last row: clamps rather than overflowing.
        let (next, _) = mail_overlay_reduce(next, key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(next.expect("stays open").cursor, 1);

        // Up from row 0 saturates at 0.
        let (next, _) = mail_overlay_reduce(view, key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(next.expect("stays open").cursor, 0);
    }

    #[test]
    fn mail_overlay_c_opens_compose_and_typing_accumulates_the_draft() {
        let (next, effect) = mail_overlay_reduce(ui::MailView::default(), press('c'));
        let next = next.expect("stays open");
        assert!(next.compose.is_some(), "c opens the compose draft");
        assert!(effect.is_none());

        let (next, _) = mail_overlay_reduce(next, press('h'));
        let (next, _) = mail_overlay_reduce(next.expect("stays open"), press('i'));
        let draft = next.expect("stays open").compose.expect("still composing");
        assert_eq!(draft.body, "hi");
    }

    #[test]
    fn mail_overlay_backspace_edits_the_compose_draft() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "hix".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, _) = mail_overlay_reduce(view, key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            next.expect("stays open")
                .compose
                .expect("still composing")
                .body,
            "hi"
        );
    }

    #[test]
    fn mail_overlay_esc_while_composing_cancels_only_the_draft() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "half-written".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        let next = next.expect("overlay stays open; only the draft is cancelled");
        assert!(next.compose.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn mail_overlay_enter_on_an_empty_compose_body_is_a_noop() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft::default()),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert!(next.compose.is_some(), "still composing, nothing was sent");
        assert!(effect.is_none());
    }

    #[test]
    fn mail_overlay_enter_while_composing_emits_a_send_effect() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "heads up".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("overlay stays open");
        assert!(next.compose.is_none(), "compose closes on submit");
        match effect {
            Some(ui::MailEffect::Send(msg)) => {
                assert_eq!(msg.to, "any");
                assert_eq!(msg.body, "heads up");
            }
            other => panic!("expected a Send effect, got {other:?}"),
        }
    }

    #[test]
    fn mail_overlay_enter_on_an_item_emits_consume_and_removes_it_from_the_list() {
        let view = ui::MailView {
            items: vec![(PathBuf::from("/a"), "claude".to_string(), "one".to_string())],
            cursor: 0,
            compose: None,
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert!(
            next.items.is_empty(),
            "the read item is removed from the view"
        );
        assert_eq!(effect, Some(ui::MailEffect::Consume(PathBuf::from("/a"))));
    }

    #[test]
    fn mail_overlay_enter_on_an_empty_list_is_a_noop() {
        let (next, effect) = mail_overlay_reduce(
            ui::MailView::default(),
            key(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(next.is_some());
        assert!(effect.is_none());
    }

    fn memory_view(entries: Vec<(&str, &str, &str)>) -> ui::MemoryView {
        ui::MemoryView {
            entries: entries
                .into_iter()
                .map(|(k, a, b)| (k.to_string(), a.to_string(), b.to_string()))
                .collect(),
            cursor: 0,
            input: None,
        }
    }

    #[test]
    fn memory_overlay_esc_while_browsing_closes_the_overlay() {
        let (next, effect) = memory_overlay_reduce(
            ui::MemoryView::default(),
            key(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn memory_overlay_cursor_clamps_on_an_empty_list() {
        let (next, _) = memory_overlay_reduce(
            ui::MemoryView::default(),
            key(KeyCode::Down, KeyModifiers::NONE),
        );
        assert_eq!(next.expect("stays open").cursor, 0);
    }

    #[test]
    fn memory_overlay_r_prefills_input_from_the_selected_entrys_body() {
        let view = memory_view(vec![(
            "build-cmd",
            "written 1d ago, verified 1d ago",
            "cargo build",
        )]);
        let (next, effect) = memory_overlay_reduce(view, press('r'));
        let next = next.expect("stays open");
        assert_eq!(next.input, Some("cargo build".to_string()));
        assert!(effect.is_none());
    }

    #[test]
    fn memory_overlay_esc_while_editing_cancels_only_the_edit() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("half-typed".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        let next = next.expect("overlay stays open");
        assert!(next.input.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn memory_overlay_enter_while_editing_emits_remember_and_exits_edit_mode() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("new body".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert!(next.input.is_none());
        assert_eq!(
            effect,
            Some(ui::MemoryEffect::Remember {
                key: "build-cmd".to_string(),
                body: "new body".to_string(),
            })
        );
    }

    #[test]
    fn memory_overlay_d_emits_forget_and_removes_the_entry_locally() {
        let view = memory_view(vec![("drop-me", "age", "body")]);
        let (next, effect) = memory_overlay_reduce(view, press('d'));
        let next = next.expect("stays open");
        assert!(next.entries.is_empty());
        assert_eq!(
            effect,
            Some(ui::MemoryEffect::Forget("drop-me".to_string()))
        );
    }

    #[test]
    fn memory_overlay_v_emits_verify_without_changing_the_list() {
        let view = memory_view(vec![("build-cmd", "age", "body")]);
        let (next, effect) = memory_overlay_reduce(view, press('v'));
        let next = next.expect("stays open");
        assert_eq!(next.entries.len(), 1, "verify does not remove the entry");
        assert_eq!(
            effect,
            Some(ui::MemoryEffect::Verify("build-cmd".to_string()))
        );
    }

    // The disk-reading half: `build_mail_view`/`build_memory_view`.

    #[test]
    fn build_mail_view_lists_every_message_visible_to_the_operator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "hello world".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let view = build_mail_view(&state, &repo);
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].1, "claude");
        assert_eq!(view.items[0].2, "hello world");
    }

    #[test]
    fn build_memory_view_lists_every_entry_with_its_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);
        let now = super::super::state::now_secs();
        memory::remember(
            &state,
            &slug,
            &memory::Entry {
                key: "build-cmd".to_string(),
                written_by: "claude".to_string(),
                written: now,
                verified: now,
                source: "explicit".to_string(),
                body: "cargo build".to_string(),
            },
            &cfg,
        )
        .expect("remember");

        let view = build_memory_view(&state, &repo);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].0, "build-cmd");
        assert_eq!(view.entries[0].2, "cargo build");
        assert!(view.entries[0].1.contains("written"));
    }

    // The executor half: `apply_mail_effect`/`apply_memory_effect`.

    #[test]
    fn apply_mail_effect_send_stamps_identity_and_stores_the_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let mut errors = Vec::new();

        let msg = mail::Message {
            from_session: String::new(),
            from_agent: String::new(),
            to: "any".to_string(),
            to_session: None,
            sent: 0,
            body: "heads up".to_string(),
        };
        apply_mail_effect(
            ui::MailEffect::Send(msg),
            &state,
            &repo,
            &cfg,
            "orch1234",
            "claude",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");

        let slug = super::super::state::repo_slug(&repo);
        let listed = mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.from_session, "orch1234");
        assert_eq!(listed[0].1.from_agent, "claude");
    }

    #[test]
    fn apply_mail_effect_consume_moves_the_message_to_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);
        let path = mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let mut errors = Vec::new();
        apply_mail_effect(
            ui::MailEffect::Consume(path.clone()),
            &state,
            &repo,
            &cfg,
            "orch1234",
            "claude",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");
        assert!(!path.exists());
        assert!(
            mail::list(&state, &slug, None, None)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn apply_memory_effect_remember_writes_an_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let mut errors = Vec::new();

        apply_memory_effect(
            ui::MemoryEffect::Remember {
                key: "build-cmd".to_string(),
                body: "cargo build".to_string(),
            },
            &state,
            &repo,
            &cfg,
            "claude",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");

        let slug = super::super::state::repo_slug(&repo);
        let listed = memory::list(&state, &slug).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.key, "build-cmd");
        assert_eq!(listed[0].1.written_by, "claude");
    }

    // Task 9: idle-gated visible intervention.

    #[test]
    fn deliverable_now_truth_table() {
        assert!(!deliverable_now(&PaneState::Working, 1));
        assert!(!deliverable_now(&PaneState::Idle, 0));
        assert!(deliverable_now(&PaneState::Idle, 1));
        assert!(!deliverable_now(&PaneState::Ended(0), 1));
        assert!(!deliverable_now(&PaneState::WaitingInput, 1));
    }

    #[test]
    fn queue_drains_fifo_only_while_idle() {
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back("first".to_string());
        queue.push_back("second".to_string());

        assert_eq!(next_deliverable(&mut queue, &PaneState::Working), None);
        assert_eq!(queue.len(), 2, "nothing is popped while working");
        assert_eq!(
            next_deliverable(&mut queue, &PaneState::Idle),
            Some("first".to_string())
        );
        assert_eq!(
            next_deliverable(&mut queue, &PaneState::Idle),
            Some("second".to_string())
        );
        assert_eq!(next_deliverable(&mut queue, &PaneState::Idle), None);
    }

    #[test]
    fn orchestrator_pane_is_excluded_from_mail_delivery() {
        assert!(!is_delivery_eligible(
            sessions::Verb::Chat,
            &PaneState::Idle
        ));
        assert!(is_delivery_eligible(sessions::Verb::Dash, &PaneState::Idle));
        assert!(!is_delivery_eligible(
            sessions::Verb::Dash,
            &PaneState::Working
        ));
    }

    struct FailingInjector;
    impl Injector for FailingInjector {
        fn try_inject(&mut self, _label: &str, _body: &str) -> CtxResult<()> {
            Err("simulated injection failure".into())
        }
    }

    struct SucceedingInjector {
        calls: Vec<(String, String)>,
    }
    impl Injector for SucceedingInjector {
        fn try_inject(&mut self, label: &str, body: &str) -> CtxResult<()> {
            self.calls.push((label.to_string(), body.to_string()));
            Ok(())
        }
    }

    /// C7 discipline: a message that was never actually shown to the agent
    /// (the injection failed) must not be marked read.
    #[test]
    fn a_failed_injection_leaves_the_mail_file_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = "-work-repo";
        let path = mail::store(
            &state,
            slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let mut injector = FailingInjector;
        let result = deliver_and_consume(&mut injector, &state, slug, "label", &path, "note");

        assert!(result.is_err());
        assert!(
            path.exists(),
            "the message file must be untouched after a failed injection"
        );
        assert_eq!(mail::list(&state, slug, None, None).expect("list").len(), 1);
    }

    #[test]
    fn a_successful_injection_consumes_the_mail_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = "-work-repo";
        let path = mail::store(
            &state,
            slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let mut injector = SucceedingInjector { calls: Vec::new() };
        let result = deliver_and_consume(&mut injector, &state, slug, "label", &path, "note");

        assert!(result.is_ok());
        assert!(!path.exists(), "consumed on a successful injection");
        assert_eq!(
            injector.calls,
            vec![("label".to_string(), "note".to_string())]
        );
    }
}
