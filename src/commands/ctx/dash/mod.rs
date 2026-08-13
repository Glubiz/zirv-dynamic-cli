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
pub mod roster;
pub mod spawnreq;
pub mod ui;

use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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
use super::{mail, memory, prompt, sessions, window};

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
/// there. `selected` indexes into the combined list this returns; `focused`
/// indexes into `panes` alone (see `ui::SidebarRow`'s own doc comment for
/// why the two are separate), and is simply not marked when it is out of
/// range -- an empty dashboard has nothing to focus. Pure: no I/O of its own
/// -- `registry` is whatever the caller already read via `sessions::list`.
fn assemble_sidebar(
    panes: &[PaneRowMeta],
    registry: &[(sessions::Record, sessions::Liveness)],
    selected: usize,
    focused: usize,
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
            focused: false,
        })
        .collect();

    if let Some(row) = rows.get_mut(focused) {
        row.focused = true;
    }

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
            focused: false,
        });
    }

    if let Some(row) = rows.get_mut(selected) {
        row.selected = true;
    }
    rows
}

/// Pure: one navigation action's effect on the `(selected, focused)` pair.
///
/// The split is the whole of F7. `selected` is the sidebar cursor over the
/// *combined* row list (panes plus view-only registry rows) and is what a
/// nudge is aimed at; `focused` is the pane whose grid is drawn and whose
/// child gets every un-prefixed keystroke, so it may only ever name a pane.
/// `prefix,Up`/`prefix,Down` therefore move `selected` alone -- walking onto
/// a view-only row leaves the focused pane exactly where it was, rather than
/// blanking the grid and swallowing all input the way a single shared index
/// did. `prefix,Tab` and `prefix,<digit>` address panes, so they move both.
///
/// Every index stays clamped to something addressable: an empty dashboard
/// (no panes at all) leaves both untouched.
fn apply_navigation(
    action: DashAction,
    selected: usize,
    focused: usize,
    pane_count: usize,
    total_rows: usize,
) -> (usize, usize) {
    match action {
        DashAction::Switch(i) => {
            if pane_count == 0 {
                (selected, focused)
            } else {
                let target = i.min(pane_count - 1);
                (target, target)
            }
        }
        DashAction::NextPane => {
            if pane_count == 0 {
                (selected, focused)
            } else {
                let target = (focused + 1) % pane_count;
                (target, target)
            }
        }
        DashAction::SelectUp => (selected.saturating_sub(1), focused),
        DashAction::SelectDown => {
            if total_rows == 0 {
                (selected, focused)
            } else {
                ((selected + 1).min(total_rows - 1), focused)
            }
        }
        _ => (selected, focused),
    }
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

/// Called on every quit path, before any pane is torn down (shutdown --
/// quit-sequence, registry release, socket unpublish -- happens in the
/// caller right after this returns). Two things happen here, both
/// best-effort (the dashboard is exiting either way, and there is nothing
/// left to report a failure to):
///
/// 1. Writes this repo's own restore roster (`roster::write_roster`) from
///    every pane still alive, orchestrator included -- `RosterPane::role`
///    records which is which (`roster::ROLE_ORCHESTRATOR`/`ROLE_WORKER`), so
///    a later startup restore can filter the orchestrator back out itself
///    rather than this write having to guess which pane index is safe to
///    keep.
/// 2. Removes the whole spawn-request directory this dashboard created at
///    startup (`requests_dir`'s own parent, `<dash_short>-<token>`, not just
///    the `requests` leaf, so no empty shell is left under `<state>/dash/`):
///    once this dashboard is gone, nothing should still be able to reach a
///    channel that nobody is polling any more.
fn on_quit(panes: &[Pane], requests_dir: &Path, state: &StateDir, repo: &Path) {
    let roster = roster::Roster {
        written: super::state::now_secs(),
        panes: panes
            .iter()
            .map(|pane| roster::RosterPane {
                agent: pane.agent().to_string(),
                session_id: pane.session_id().to_string(),
                role: if pane.verb() == sessions::Verb::Chat {
                    roster::ROLE_ORCHESTRATOR
                } else {
                    roster::ROLE_WORKER
                }
                .to_string(),
                short: pane.short().to_string(),
                title: pane.title().to_string(),
            })
            .collect(),
    };
    let slug = super::state::repo_slug(repo);
    let _ = roster::write_roster(state, &slug, &roster);

    let dir = requests_dir.parent().unwrap_or(requests_dir);
    let _ = std::fs::remove_dir_all(dir);
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

/// Restores the shared terminal on the way out of `run_dashboard`: disables
/// raw mode, then writes `term::dash_reset_bytes()` -- cursor shown, scroll
/// region un-fenced, alternate screen left -- to **stdout**, which is the
/// stream the alternate screen was entered on.
///
/// Showing the cursor is not optional and is not implied by leaving the
/// alternate screen: ratatui hides it on every frame it draws, and
/// `LeaveAlternateScreen` says nothing about cursor visibility, so before F4
/// every clean exit handed the operator a shell with an invisible cursor.
///
/// Idempotent, and called from every exit arm, matching the `RawGuard`/
/// `SessionGuard` precedent this plan's Global Constraints call for --
/// `panic = "abort"` in the release profile means `Drop` is not a safety
/// net here either.
fn teardown_terminal() {
    term::set_dash_active(false);
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = stdout.write_all(term::dash_reset_bytes());
    let _ = stdout.flush();
    // Belt and braces: crossterm's own sequence for the same thing, in case
    // a future crossterm emits something extra alongside `\x1b[?1049l`.
    // Leaving an alternate screen twice is a no-op.
    let _ = execute!(stdout, LeaveAlternateScreen);
}

/// Puts the terminal back before the default hook prints its message and the
/// process aborts. Three things the pre-F4 hook got wrong, all of which left
/// a panicking dashboard's operator with an unusable console:
///
/// 1. Raw mode was never disabled, so the shell that inherited the console
///    had no echo and no line editing.
/// 2. It wrote `term::emergency_reset_bytes(false)`, which is the **empty**
///    slice (see `term.rs`) -- so nothing was reset and the cursor was never
///    shown again.
/// 3. It wrote to stderr, but the alternate screen was entered on stdout.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = stdout.write_all(term::dash_reset_bytes());
        let _ = stdout.flush();
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

// Task 10: the spawn-request channel. A pane's own `zirv ctx agent`
// invocation (inheriting `DASH_REQUESTS_ENV` from its own turn_env, set up
// below) writes a `spawnreq::SpawnRequest` rather than running headless in
// the pane's own subshell; this dashboard fulfils it as a fresh worker pane
// using exactly the composed-prompt recipe `exec::run_with` uses for its own
// first launch (memory, then mail, then `with_mail_layer`), and answers with
// a `spawnreq::SpawnAck`.

/// A 16-hex-character capability token for this dashboard's own
/// spawn-request directory (`spawnreq::request_dir_for`). Freshly minted per
/// launch: unpredictable enough that a process never told this directory's
/// path cannot guess it, so only a pane that actually inherited
/// `DASH_REQUESTS_ENV` from this dashboard can reach its spawn channel.
fn spawn_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// `std::process::Command` -> the flat `program, arg, arg, ...` form
/// `PaneSpec::argv` wants, matching `chat::build_launch`'s own flattening of
/// `AgentAdapter::interactive_cmd`'s output exactly (duplicated rather than
/// shared: pulling in `chat` here for one helper would make `dash` and
/// `chat` depend on each other in both directions).
fn flatten_command(command: std::process::Command) -> Vec<String> {
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    argv
}

/// The refusal text a prompt that would be misread as a flag gets. A request
/// prompt is encoded *positionally* into `interactive_cmd`'s argv, so a
/// prompt like `--dangerously-skip-permissions` would reach the real harness
/// child as a flag rather than as the task text. Refused at the authority
/// side -- here, where the pane is actually spawned -- rather than only at
/// the requesting side, because a request is data, never authority.
///
/// Pure, so both ends of the channel (this one, and `agent.rs`'s own
/// defense-in-depth check before it ever writes a request) can assert the
/// same rule.
pub(crate) fn argv_unsafe_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with('-')
}

pub(crate) const ARGV_GUARD_REFUSAL: &str = "prompt must not begin with '-' (argv injection guard)";

/// Whether `a` and `b` name the same directory, canonicalising both when the
/// filesystem allows it (a request carries whatever `cwd` the requester wrote
/// down, which may be spelled differently from the dashboard's own repo path)
/// and falling back to a literal comparison when it does not.
fn same_directory(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Re-validates and fulfils one spawn request: the argv-safety guard, the
/// requesting repo, the pane cap, the agent gate and adapter resolution
/// first (a request is data, never authority -- the same checks an
/// operator-issued `zirv ctx agent` invocation goes through), then builds
/// a Worker pane's composed prompt and argv following `exec::run_with`'s own
/// recipe (`memory::render_for_prompt` -> `prompt::compose` -> mail listing
/// scoped to this fresh session's own short id -> `prompt::with_mail_layer`
/// -> `prompt::injection_args_for_session`), and spawns it. `Ok(short)` is
/// the freshly spawned pane's own registry short id; `Err(reason)` is
/// exactly the text `spawnreq::SpawnAck::reason` carries back to the
/// requester.
///
/// Pushes the new pane (and a matching empty nudge queue, keeping the two
/// vectors the same length -- see `deliver_queued_nudges`'s own doc comment)
/// on success. Delivered mail is consumed only after the pane has actually
/// spawned, mirroring `exec::run_with`'s own "consume right after the spawn
/// that carried it genuinely started" discipline.
#[allow(clippy::too_many_arguments)]
fn fulfill_spawn_request(
    req: &spawnreq::SpawnRequest,
    panes: &mut Vec<Pane>,
    nudge_queues: &mut Vec<VecDeque<String>>,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    size: (u16, u16),
    requests_dir: &Path,
    errors: &mut Vec<String>,
) -> Result<String, String> {
    // Every one of these is checked before anything is spawned, resolved or
    // written, in cheapest-and-most-hostile-first order.
    if argv_unsafe_prompt(&req.prompt) {
        return Err(ARGV_GUARD_REFUSAL.to_string());
    }
    // `cwd` used to be written by the requester and then never looked at.
    // Honouring it would mean this dashboard spawning panes into a directory
    // its operator never opened; ignoring it silently would mean a request
    // from another repo quietly running here instead. Refusing is the honest
    // contract, and it is the one the requester can see in the ack.
    if !same_directory(&req.cwd, repo) {
        return Err(format!(
            "this dashboard only spawns panes in its own repo ({}); the request named {}",
            repo.display(),
            req.cwd.display()
        ));
    }
    if panes.len() >= cfg.dash.max_panes {
        return Err(format!(
            "pane limit reached ({} panes, dash.max_panes = {})",
            panes.len(),
            cfg.dash.max_panes
        ));
    }
    if let Some(reason) = cfg.agents.refusal(&req.agent) {
        return Err(reason);
    }
    let adapter = adapters::select(Some(&req.agent), &[], cfg).map_err(|e| e.to_string())?;

    let session_id = SessionId::new_v4().to_string();
    let registry_short = sessions::short_id(&session_id);
    let slug = super::state::repo_slug(repo);
    let memory_entries = memory::render_for_prompt(state, &slug, cfg, super::state::now_secs());
    let composed = prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        false,
        &cfg.prompt,
        prompt::PromptRole::Worker,
        &memory_entries,
        cfg.memory.max_injected_bytes,
    );
    let mut mail_entries: Vec<(PathBuf, mail::Message)> = if composed.is_some() && cfg.mail.enabled
    {
        mail::list(
            state,
            &slug,
            Some(adapter.name()),
            sessions::delivery_filter(None, &registry_short),
        )
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mail_messages: Vec<mail::Message> = mail_entries.iter().map(|(_, m)| m.clone()).collect();
    let composed = prompt::with_mail_layer(composed, &mail_messages, cfg.mail.max_delivered_bytes);

    let prompt_args = prompt::injection_args_for_session(
        adapter.as_ref(),
        &[],
        composed.as_ref(),
        state,
        &session_id,
    );
    prompt::log_injection(
        state,
        "dash",
        &session_id,
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );

    let argv = flatten_command(adapter.interactive_cmd(Some(&req.prompt), &prompt_args));
    let spec = PaneSpec {
        agent_name: req.agent.clone(),
        argv,
        role: prompt::PromptRole::Worker,
        verb: sessions::Verb::Dash,
        session_id: session_id.clone(),
        title: format!("wrk {}", req.agent),
    };

    let (mut turn_env, turn_env_err) = build_turn_env(cfg, state, repo, &req.agent, &session_id);
    if let Some(e) = turn_env_err {
        push_error(errors, e);
    }
    turn_env.push((
        spawnreq::DASH_REQUESTS_ENV.to_string(),
        requests_dir.display().to_string(),
    ));

    let pane = Pane::spawn(spec, state, repo, size, &turn_env).map_err(|e| e.to_string())?;
    let short = pane.short().to_string();
    panes.push(pane);
    nudge_queues.push(VecDeque::new());

    for (path, _) in mail_entries.drain(..) {
        let _ = mail::consume(state, &slug, &path);
    }

    Ok(short)
}

/// Drains every request currently queued in `requests_dir` and answers each
/// one, in order. Called once per tick, alongside `mail_sweep`/
/// `deliver_queued_nudges`: a request is data sitting on disk, not something
/// that needs sub-tick latency.
#[allow(clippy::too_many_arguments)]
fn handle_spawn_requests(
    requests_dir: &Path,
    panes: &mut Vec<Pane>,
    nudge_queues: &mut Vec<VecDeque<String>>,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    size: (u16, u16),
    errors: &mut Vec<String>,
) {
    for (path, req) in spawnreq::take_requests(requests_dir) {
        let Some(stem) = spawnreq::request_stem(&path) else {
            continue;
        };
        // F10: claimed *before* fulfilment, not after. `take_requests` has
        // already deleted the request file, and fulfilment (adapter
        // resolution, a prompt file write, a real pty spawn) can easily
        // outlast the requester's own ack timeout -- at which point the
        // requester used to give up and run the same task headless as well,
        // so one `zirv ctx agent` became two live sessions. A claim on disk
        // is what lets that requester tell "nobody is listening" from "the
        // dashboard has this, the answer is just slow". Cleaned up with the
        // whole request directory on quit (`on_quit`), so a claim never
        // outlives the dashboard that made it.
        if let Err(e) = spawnreq::write_claim(requests_dir, &stem) {
            push_error(errors, format!("spawn claim: {e}"));
        }
        let ack = match fulfill_spawn_request(
            &req,
            panes,
            nudge_queues,
            cfg,
            state,
            repo,
            size,
            requests_dir,
            errors,
        ) {
            Ok(short) => spawnreq::SpawnAck {
                ok: true,
                short: Some(short),
                reason: None,
            },
            Err(reason) => spawnreq::SpawnAck {
                ok: false,
                short: None,
                reason: Some(reason),
            },
        };
        if let Err(e) = spawnreq::write_ack(requests_dir, &stem, &ack) {
            push_error(errors, format!("spawn ack: {e}"));
        }
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

/// What confirming the spawn dialog asks the caller to do. `Submit` has
/// already been split into its two required halves; `Notice` is a message for
/// the header, with the dialog left open so the operator can fix what they
/// typed rather than losing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnEffect {
    Submit { agent: String, prompt: String },
    Notice(String),
}

/// What a spawn line that cannot be used says. Both halves are required: an
/// agent with no task is not a request, and a task with no agent has nowhere
/// to go.
pub(crate) const SPAWN_USAGE_NOTICE: &str =
    "spawn: type <agent> <prompt>, e.g. `claude fix the failing tests`";

/// Pure: one keystroke against the spawn dialog (`Ctrl+A s`), the same
/// reducer shape every other overlay in this module already uses. `Enter`
/// splits the typed line at its first run of whitespace -- first token is the
/// agent name, the whole remainder is the prompt -- and closes the dialog;
/// anything missing a half keeps the dialog open with a notice. `Esc`
/// cancels outright.
///
/// Deliberately does **not** re-implement the argv guard, the pane cap or the
/// agent gate: a submitted draft is routed through the exact same
/// `fulfill_spawn_request` path a pane's own `zirv ctx agent` request takes,
/// so there is one place those rules live and one place they can be wrong.
pub fn spawn_overlay_reduce(
    mut draft: ui::SpawnDraft,
    key: KeyEvent,
) -> (Option<ui::SpawnDraft>, Option<SpawnEffect>) {
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Enter => {
            let line = draft.input.trim();
            let Some((agent, prompt)) = line.split_once(char::is_whitespace) else {
                return (
                    Some(draft),
                    Some(SpawnEffect::Notice(SPAWN_USAGE_NOTICE.to_string())),
                );
            };
            let prompt = prompt.trim();
            if agent.is_empty() || prompt.is_empty() {
                return (
                    Some(draft),
                    Some(SpawnEffect::Notice(SPAWN_USAGE_NOTICE.to_string())),
                );
            }
            (
                None,
                Some(SpawnEffect::Submit {
                    agent: agent.to_string(),
                    prompt: prompt.to_string(),
                }),
            )
        }
        KeyCode::Backspace => {
            draft.input.pop();
            (Some(draft), None)
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.input.push(c);
            (Some(draft), None)
        }
        _ => (Some(draft), None),
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

// Task 12: the startup restore dialog -- same pure-reducer shape as Task 8's
// mail/memory overlays. `restore_overlay_reduce` never touches a roster or a
// pane itself; it only tracks which checkboxes are on and, on Enter, reports
// back *which* entries were checked (by index) for the caller to act on.

/// What confirming the restore dialog (Enter) reports back: the indices,
/// into whatever candidate list the caller built the view's entries from in
/// the same order, that were checked at the moment of confirmation. `Esc`
/// (skip everything) yields no effect at all -- see `restore_overlay_reduce`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreEffect {
    Confirm(Vec<usize>),
}

/// Pure: one keystroke against the restore dialog's current state. `Space`
/// toggles the entry under the cursor; `Enter` closes the dialog and reports
/// every currently-checked index as a `Confirm` effect (an empty roster, or
/// everything unchecked, is still a valid confirm -- it simply restores
/// nothing); `Esc` closes the dialog with no effect, skipping the restore
/// entirely. Arrow keys and `j`/`k` move the cursor, clamped the same way
/// every other browsing-mode reducer in this module already is.
pub fn restore_overlay_reduce(
    mut view: ui::RestoreView,
    key: KeyEvent,
) -> (Option<ui::RestoreView>, Option<RestoreEffect>) {
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Enter => {
            let checked = view
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.checked)
                .map(|(i, _)| i)
                .collect();
            (None, Some(RestoreEffect::Confirm(checked)))
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = clamp_cursor(view.cursor + 1, view.entries.len());
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = view.cursor.saturating_sub(1);
            (Some(view), None)
        }
        KeyCode::Char(' ') => {
            if let Some(entry) = view.entries.get_mut(view.cursor) {
                entry.checked = !entry.checked;
            }
            (Some(view), None)
        }
        _ => (Some(view), None),
    }
}

/// Builds the restore dialog's own view from every roster candidate the
/// caller already filtered down to workers only (`run_dashboard`'s startup
/// path excludes `roster::ROLE_ORCHESTRATOR` before this is ever called).
/// Every entry defaults to checked, so a bare Enter restores the whole
/// roster -- the common case -- and unchecking is the exception the operator
/// opts into.
fn build_restore_view(candidates: &[roster::RosterPane]) -> ui::RestoreView {
    ui::RestoreView {
        entries: candidates
            .iter()
            .map(|pane| ui::RestoreEntry {
                label: format!("{} {} ({})", pane.title, pane.agent, pane.short),
                checked: true,
            })
            .collect(),
        cursor: 0,
    }
}

/// Spawns one roster candidate back as a fresh worker pane: resolves its
/// adapter (re-checked against the live gate, same "data, never authority"
/// discipline `fulfill_spawn_request` already holds a spawn request to --
/// an agent an operator disabled since the last quit must not come back just
/// because it was in the roster), builds its argv via `roster::restore_argv`,
/// and spawns it reusing the roster entry's own `session_id` (so its
/// registry short id, and the address mail/nudge reach it at, are the same
/// as before the quit -- restoring is continuing the same session, not
/// starting a new one with the old one's history).
#[allow(clippy::too_many_arguments)]
fn spawn_restored_pane(
    candidate: &roster::RosterPane,
    panes: &mut Vec<Pane>,
    nudge_queues: &mut Vec<VecDeque<String>>,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    size: (u16, u16),
    requests_dir: &Path,
    errors: &mut Vec<String>,
) {
    let adapter = match adapters::select(Some(&candidate.agent), &[], cfg) {
        Ok(adapter) => adapter,
        Err(e) => {
            push_error(errors, format!("restore {}: {e}", candidate.short));
            return;
        }
    };
    let argv = roster::restore_argv(adapter.as_ref(), candidate);
    let spec = PaneSpec {
        agent_name: candidate.agent.clone(),
        argv,
        role: prompt::PromptRole::Worker,
        verb: sessions::Verb::Dash,
        session_id: candidate.session_id.clone(),
        title: candidate.title.clone(),
    };

    let (mut turn_env, turn_env_err) =
        build_turn_env(cfg, state, repo, &candidate.agent, &candidate.session_id);
    if let Some(e) = turn_env_err {
        push_error(errors, e);
    }
    turn_env.push((
        spawnreq::DASH_REQUESTS_ENV.to_string(),
        requests_dir.display().to_string(),
    ));

    match Pane::spawn(spec, state, repo, size, &turn_env) {
        Ok(pane) => {
            panes.push(pane);
            nudge_queues.push(VecDeque::new());
        }
        Err(e) => push_error(errors, format!("restore {}: {e}", candidate.short)),
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

/// One pane's share of a mail sweep: **at most one** message, injected
/// visibly and consumed only if the injection itself succeeded. Returns
/// whether anything was delivered.
///
/// One per tick, not the whole mailbox (F8): the idle gate is evaluated once,
/// before the first injection, and injecting immediately puts the pane back
/// to work -- so the second and later messages of a batch used to be typed
/// into a session that was already mid-turn, which is exactly what the
/// idle gate exists to prevent. The remainder stays on disk, unread, and the
/// next tick's sweep sees it again once the pane is genuinely idle.
///
/// Takes an `Injector` rather than a `Pane` so the one-per-tick rule is
/// testable without a real pty, the same seam `deliver_and_consume` already
/// uses.
fn sweep_one_pane<I: Injector>(
    injector: &mut I,
    state: &StateDir,
    slug: &str,
    agent: &str,
    short: &str,
    errors: &mut Vec<String>,
) -> bool {
    let messages = match mail::list(state, slug, Some(agent), Some(short)) {
        Ok(m) => m,
        Err(e) => {
            push_error(errors, format!("mail sweep: {e}"));
            return false;
        }
    };
    let Some((path, msg)) = messages.into_iter().next() else {
        return false;
    };
    let label = format!(
        "mail from {}/{}",
        msg.from_agent,
        sessions::short_id(&msg.from_session)
    );
    match deliver_and_consume(injector, state, slug, &label, &path, &msg.body) {
        Ok(()) => true,
        Err(e) => {
            push_error(errors, format!("mail sweep: {e}"));
            false
        }
    }
}

/// Once-per-tick mail sweep: every attached worker pane that is `Idle` gets
/// the oldest of its own unread messages (the same per-session visibility
/// `unread_counts` already applies: addressed to its agent, and either
/// undirected or addressed to its own short id) injected visibly, and
/// consumed only after that injection succeeded.
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
        sweep_one_pane(pane, state, &slug, &agent, &short, errors);
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

    // Mutable, and kept current by the `Event::Resize` arm below (F6): the
    // zoom handler resizes every pane against `full`, so a `full` frozen at
    // startup would restore panes to the terminal's *launch* geometry after
    // any resize rather than to what it is now.
    let (mut term_cols, mut term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let sidebar_cols = cfg.dash.sidebar_cols;
    let mut full = Rect::new(0, 0, term_cols, term_rows);
    let main = effective_main(full, sidebar_cols, false);

    let agent_name = first.agent_name.clone();
    let session_id = first.session_id.clone();
    let (mut turn_env, turn_env_err) = build_turn_env(cfg, state, repo, &agent_name, &session_id);
    if let Some(e) = turn_env_err {
        push_error(&mut errors, e);
    }

    // Task 10: the spawn-request channel. `dashboard_short` is derivable
    // before any pane has actually spawned -- `Record::new`'s own `short`
    // field is exactly `sessions::short_id(session)` -- so even the very
    // first (orchestrator) pane's turn_env can already carry the request
    // directory, the same as every pane spawned later through a request.
    let dashboard_short = sessions::short_id(&session_id);
    let requests_token = spawn_token();
    let requests_dir = spawnreq::request_dir_for(state, &dashboard_short, &requests_token);
    if let Err(e) = super::state::create_private_dir_all(&requests_dir) {
        push_error(
            &mut errors,
            format!("dashboard: could not create the spawn-request directory: {e}"),
        );
    }
    turn_env.push((
        spawnreq::DASH_REQUESTS_ENV.to_string(),
        requests_dir.display().to_string(),
    ));

    let size = (main.width.max(1), main.height.max(1));
    let mut panes = vec![Pane::spawn(first, state, repo, size, &turn_env)?];
    // Task 9: one FIFO nudge queue per pane, kept the same length as `panes`.
    // Nothing in this task's scope ever grows `panes` after this point (a
    // future spawn seam -- Tasks 10/11 -- must push a matching
    // `VecDeque::new()` here too whenever it pushes a new pane).
    let mut nudge_queues: Vec<VecDeque<String>> = vec![VecDeque::new(); panes.len()];

    // Task 12: offer back whatever this repo's previous quit left behind,
    // once, before the terminal is ever touched -- a fresh roster within
    // `cfg.dash.roster_max_age_secs` becomes the startup restore dialog
    // below; anything else (absent, stale, already offered to some earlier
    // launch) leaves `overlay` at its usual `Overlay::None` start. The
    // orchestrator entry is filtered out here, not later: the `first` pane
    // spawned just above already *is* this dashboard's orchestrator, so
    // respawning a roster's own orchestrator entry would duplicate it.
    let repo_slug = super::state::repo_slug(repo);
    let restore_candidates: Vec<roster::RosterPane> = roster::take_roster(
        state,
        &repo_slug,
        super::state::now_secs(),
        cfg.dash.roster_max_age_secs,
    )
    .map(|taken| {
        taken
            .panes
            .into_iter()
            .filter(|pane| pane.role != roster::ROLE_ORCHESTRATOR)
            .collect()
    })
    .unwrap_or_default();

    install_panic_hook();
    // F4(c): an external kill (`taskkill`, a Ctrl-Break, a closed window)
    // reaches neither the panic hook nor any exit arm below. `RawGuard::
    // enter` arms this for `wrap`; the dashboard drives raw mode through
    // crossterm instead, so it has to stash the pre-raw console modes and
    // install the same handler itself. Both are write-once/idempotent, and
    // the return values are advisory only -- a process with no console of
    // its own stashes nothing and still starts.
    let _ = term::stash_current_console();
    let _ = term::install_console_restore_handler();
    if let Err(e) = enable_raw_mode() {
        let _ = std::panic::take_hook();
        return Err(format!("dashboard: enable_raw_mode failed: {e}").into());
    }
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        teardown_terminal();
        let _ = std::panic::take_hook();
        return Err(format!("dashboard: EnterAlternateScreen failed: {e}").into());
    }
    // From here on the emergency handler owes the terminal the alternate
    // screen back, not just the console modes. Cleared by `teardown_terminal`
    // on every exit arm below.
    term::set_dash_active(true);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            teardown_terminal();
            let _ = std::panic::take_hook();
            return Err(format!("dashboard: could not attach to the terminal: {e}").into());
        }
    };

    // Two indices, not one (F7): `selected` walks the combined sidebar
    // (panes plus view-only registry rows) and is what a nudge is aimed at;
    // `focused` is the pane on screen and under the keyboard, and only ever
    // names a pane. See `apply_navigation`.
    let mut selected: usize = 0;
    let mut focused: usize = 0;
    let mut zoomed = false;
    let mut prefix_armed = false;
    let mut overlay = if restore_candidates.is_empty() {
        ui::Overlay::None
    } else {
        ui::Overlay::Restore(build_restore_view(&restore_candidates))
    };
    let mut facts_cache = FactsCache::new(Instant::now());

    let exit_code: i32 = loop {
        for pane in panes.iter_mut() {
            pane.drain();
            pane.on_turn_signal();
        }

        mail_sweep(&mut panes, cfg, state, repo, &mut errors);
        deliver_queued_nudges(&mut panes, &mut nudge_queues, &mut errors);

        // The geometry any pane spawned during this tick gets -- the terminal
        // as it is now, at this tick's zoom level. Shared by the request
        // channel below and the `Ctrl+A s` spawn dialog.
        let pane_size = {
            let now_size = crossterm::terminal::size().unwrap_or((term_cols, term_rows));
            let m = effective_main(
                Rect::new(0, 0, now_size.0, now_size.1),
                sidebar_cols,
                zoomed,
            );
            (m.width.max(1), m.height.max(1))
        };
        handle_spawn_requests(
            &requests_dir,
            &mut panes,
            &mut nudge_queues,
            cfg,
            state,
            repo,
            pane_size,
            &mut errors,
        );

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
        // A pane can have gone away (or arrived) since the last tick, so both
        // indices are re-clamped before anything reads them.
        focused = focused.min(panes.len().saturating_sub(1));
        let rows = assemble_sidebar(
            &build_pane_rows(&panes),
            &facts_cache.registry,
            selected,
            focused,
        );
        let total_rows = rows.len();
        selected = selected.min(total_rows.saturating_sub(1));

        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if !matches!(overlay, ui::Overlay::None) {
                        let current = std::mem::take(&mut overlay);
                        match current {
                            ui::Overlay::None => {}
                            ui::Overlay::QuitConfirm(working) => match key.code {
                                KeyCode::Enter => {
                                    on_quit(&panes, &requests_dir, state, repo);
                                    shutdown_all(&mut panes, cfg, &mut errors);
                                    break 0;
                                }
                                KeyCode::Esc => {}
                                _ => overlay = ui::Overlay::QuitConfirm(working),
                            },
                            ui::Overlay::Spawn(draft) => {
                                let (next, effect) = spawn_overlay_reduce(draft, key);
                                overlay = match next {
                                    Some(d) => ui::Overlay::Spawn(d),
                                    None => ui::Overlay::None,
                                };
                                match effect {
                                    Some(SpawnEffect::Notice(note)) => {
                                        push_error(&mut errors, note)
                                    }
                                    // Straight through the same validation
                                    // and spawn path a pane's own `zirv ctx
                                    // agent` request takes -- argv guard,
                                    // repo check, pane cap, agent gate --
                                    // rather than a second, parallel one.
                                    Some(SpawnEffect::Submit { agent, prompt }) => {
                                        let req = spawnreq::SpawnRequest {
                                            agent,
                                            prompt,
                                            cwd: repo.to_path_buf(),
                                            requested_by: dashboard_short.clone(),
                                        };
                                        match fulfill_spawn_request(
                                            &req,
                                            &mut panes,
                                            &mut nudge_queues,
                                            cfg,
                                            state,
                                            repo,
                                            pane_size,
                                            &requests_dir,
                                            &mut errors,
                                        ) {
                                            Ok(short) => push_error(
                                                &mut errors,
                                                format!("spawned {} as {short}", req.agent),
                                            ),
                                            Err(reason) => push_error(&mut errors, reason),
                                        }
                                    }
                                    None => {}
                                }
                            }
                            ui::Overlay::Restore(view) => {
                                let (next, effect) = restore_overlay_reduce(view, key);
                                overlay = match next {
                                    Some(v) => ui::Overlay::Restore(v),
                                    None => ui::Overlay::None,
                                };
                                if let Some(RestoreEffect::Confirm(indices)) = effect {
                                    for idx in indices {
                                        if let Some(candidate) = restore_candidates.get(idx) {
                                            spawn_restored_pane(
                                                candidate,
                                                &mut panes,
                                                &mut nudge_queues,
                                                cfg,
                                                state,
                                                repo,
                                                pane_size,
                                                &requests_dir,
                                                &mut errors,
                                            );
                                        }
                                    }
                                }
                            }
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
                            // Typing always reaches the *focused* pane, never
                            // the merely selected sidebar row (F7): walking
                            // the sidebar onto a view-only session must not
                            // swallow the operator's keystrokes.
                            InputVerdict::ToChild(bytes) => {
                                if !bytes.is_empty()
                                    && let Some(pane) = panes.get_mut(focused)
                                    && let Err(e) = pane.write_input(&bytes)
                                {
                                    push_error(&mut errors, format!("write_input: {e}"));
                                }
                            }
                            InputVerdict::Dash(DashAction::LiteralPrefix) => {
                                if let Some(pane) = panes.get_mut(focused)
                                    && let Err(e) = pane.write_input(&literal_prefix_bytes())
                                {
                                    push_error(&mut errors, format!("write_input: {e}"));
                                }
                            }
                            // Switch/NextPane address panes, so they move
                            // both indices; SelectUp/SelectDown only walk the
                            // combined sidebar. All four in one pure place.
                            InputVerdict::Dash(
                                action @ (DashAction::Switch(_)
                                | DashAction::NextPane
                                | DashAction::SelectUp
                                | DashAction::SelectDown),
                            ) => {
                                (selected, focused) = apply_navigation(
                                    action,
                                    selected,
                                    focused,
                                    panes.len(),
                                    total_rows,
                                );
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
                                    on_quit(&panes, &requests_dir, state, repo);
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
                    // F6: the loop's own idea of the terminal is updated
                    // here, not just used locally. The zoom handler and the
                    // fallback size of every `crossterm::terminal::size`
                    // call below read these, so leaving them at the startup
                    // geometry made un-zooming after a resize restore panes
                    // to the size the terminal had at launch.
                    term_cols = cols;
                    term_rows = term_h;
                    full = Rect::new(0, 0, cols, term_h);
                    let m = effective_main(full, sidebar_cols, zoomed);
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
        let (header_area, sidebar_area, _) = ui::layout(frame_area, sidebar_cols);
        // F5: the grid and any overlay are drawn into the *effective* main
        // rect, which is the whole frame while zoomed. Before this, zoom
        // resized the pty (so the child re-laid itself out for a full-width
        // terminal) but kept drawing into the un-zoomed `main` rect and left
        // the header/sidebar columns blank -- the one thing zoom is for.
        let main_area = effective_main(frame_area, sidebar_cols, zoomed);

        // Recomputed (facts_cache itself was already refreshed above, before
        // input handling) so the sidebar's own `.selected` highlight
        // reflects any selection change the keystroke just made -- cheap and
        // pure, unlike the disk-backed facts refresh it does not repeat.
        let rows = assemble_sidebar(
            &build_pane_rows(&panes),
            &facts_cache.registry,
            selected,
            focused,
        );

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
            if let Some(pane) = panes.get(focused) {
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

    /// F5: the draw target itself, not just the pty resize. Zoom used to
    /// resize every pane's pty to the full terminal and then keep drawing
    /// into the un-zoomed `main` rect, leaving the header and sidebar
    /// columns blank and the grid clipped to a fraction of what the child
    /// had just re-laid itself out for.
    #[test]
    fn the_zoomed_draw_target_is_the_whole_frame_not_the_sidebar_inset() {
        let frame = Rect::new(0, 0, 100, 30);
        let sidebar_cols = 24;

        let zoomed_target = effective_main(frame, sidebar_cols, true);
        assert_eq!(zoomed_target, frame, "zoom draws into the whole frame");

        let plain_target = effective_main(frame, sidebar_cols, false);
        assert_ne!(
            plain_target, zoomed_target,
            "and that is genuinely different from the un-zoomed rect"
        );
        assert_eq!(plain_target, ui::layout(frame, sidebar_cols).2);
    }

    // F7: focus (the pane on screen and under the keyboard) versus selection
    // (the sidebar cursor, which may sit on a view-only session).

    #[test]
    fn digits_and_tab_move_both_the_selection_and_the_focus() {
        // Three panes, five combined rows (two view-only sessions).
        assert_eq!(apply_navigation(DashAction::Switch(2), 0, 0, 3, 5), (2, 2));
        // A digit past the last pane clamps to the last pane, not to a
        // view-only row: digits address panes.
        assert_eq!(apply_navigation(DashAction::Switch(8), 0, 0, 3, 5), (2, 2));
        assert_eq!(apply_navigation(DashAction::NextPane, 4, 2, 3, 5), (0, 0));
        assert_eq!(apply_navigation(DashAction::NextPane, 0, 0, 3, 5), (1, 1));
    }

    #[test]
    fn focus_stays_on_a_pane_when_selection_walks_into_view_only_rows() {
        // One pane, three combined rows: rows 1 and 2 are view-only
        // sessions this dashboard does not own.
        let (mut selected, mut focused) = (0usize, 0usize);
        for _ in 0..5 {
            (selected, focused) = apply_navigation(DashAction::SelectDown, selected, focused, 1, 3);
        }
        assert_eq!(selected, 2, "the sidebar cursor reaches the last row");
        assert_eq!(
            focused, 0,
            "but the focused pane -- the one being drawn and typed into -- never moves"
        );

        // And walking back up leaves focus alone too.
        (selected, focused) = apply_navigation(DashAction::SelectUp, selected, focused, 1, 3);
        assert_eq!((selected, focused), (1, 0));
    }

    #[test]
    fn navigation_on_an_empty_dashboard_moves_nothing() {
        assert_eq!(apply_navigation(DashAction::Switch(3), 0, 0, 0, 0), (0, 0));
        assert_eq!(apply_navigation(DashAction::NextPane, 0, 0, 0, 0), (0, 0));
        assert_eq!(apply_navigation(DashAction::SelectDown, 0, 0, 0, 0), (0, 0));
        assert_eq!(apply_navigation(DashAction::SelectUp, 0, 0, 0, 0), (0, 0));
    }

    #[test]
    fn assemble_sidebar_marks_the_focused_pane_separately_from_the_selection() {
        let panes = vec![pane_row("aaa11111", "orch"), pane_row("bbb22222", "wrk")];
        let registry = vec![(
            registry_record("ccc33333", "codex"),
            sessions::Liveness::Live,
        )];
        // Selection has walked onto the view-only row; focus is still pane 1.
        let rows = assemble_sidebar(&panes, &registry, 2, 1);
        assert!(
            rows[2].selected,
            "the sidebar cursor is on the view-only row"
        );
        assert!(!rows[2].focused, "a view-only row can never be focused");
        assert!(rows[1].focused, "the focused pane is still marked as such");
        assert!(!rows[1].selected);
        assert!(!rows[0].focused && !rows[0].selected);
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
        let rows = assemble_sidebar(&panes, &[], 0, 0);
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
        let rows = assemble_sidebar(&panes, &registry, 0, 0);
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
        let rows = assemble_sidebar(&panes, &registry, 0, 0);
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
        let rows = assemble_sidebar(&[], &registry, 0, 0);
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
        let rows = assemble_sidebar(&panes, &registry, 1, 0);
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

    // F8: one mail message per pane per tick.

    /// The idle gate is checked once, before the first injection, and an
    /// injection puts the pane straight back to work -- so a whole mailbox
    /// delivered in one sweep typed messages two..N into a session that was
    /// already mid-turn, which is precisely what the gate exists to prevent.
    #[test]
    fn a_sweep_delivers_exactly_one_message_per_pane_per_tick() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = "-work-repo";
        for body in ["first", "second", "third"] {
            mail::store(
                &state,
                slug,
                &mail::Message {
                    from_session: "s1".to_string(),
                    from_agent: "claude".to_string(),
                    to: "claude".to_string(),
                    to_session: None,
                    sent: 1,
                    body: body.to_string(),
                },
                &cfg,
            )
            .expect("store");
        }

        let mut injector = SucceedingInjector { calls: Vec::new() };
        let mut errors = Vec::new();
        let delivered = sweep_one_pane(
            &mut injector,
            &state,
            slug,
            "claude",
            "pane1234",
            &mut errors,
        );

        assert!(delivered);
        assert!(errors.is_empty(), "got errors: {errors:?}");
        assert_eq!(
            injector.calls.len(),
            1,
            "exactly one visible injection per tick, got {:?}",
            injector.calls
        );
        assert_eq!(injector.calls[0].1, "first", "oldest first");
        assert_eq!(
            mail::list(&state, slug, Some("claude"), Some("pane1234"))
                .expect("list")
                .len(),
            2,
            "the rest stay unread for a later tick"
        );
    }

    #[test]
    fn a_sweep_of_an_empty_mailbox_delivers_nothing_and_reports_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut injector = SucceedingInjector { calls: Vec::new() };
        let mut errors = Vec::new();
        assert!(!sweep_one_pane(
            &mut injector,
            &state,
            "-work-repo",
            "claude",
            "pane1234",
            &mut errors
        ));
        assert!(injector.calls.is_empty());
        assert!(errors.is_empty());
    }

    /// C7 again, through the sweep itself rather than `deliver_and_consume`
    /// alone: a failed injection is reported and consumes nothing.
    #[test]
    fn a_sweep_whose_injection_fails_consumes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = "-work-repo";
        mail::store(
            &state,
            slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "claude".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let mut errors = Vec::new();
        assert!(!sweep_one_pane(
            &mut FailingInjector,
            &state,
            slug,
            "claude",
            "pane1234",
            &mut errors
        ));
        assert_eq!(errors.len(), 1, "the failure is reported to the header");
        assert_eq!(
            mail::list(&state, slug, Some("claude"), Some("pane1234"))
                .expect("list")
                .len(),
            1,
            "a message never shown to the agent stays unread"
        );
    }

    // F2/F9/F13: what `fulfill_spawn_request` refuses before anything is
    // spawned. A request is data, never authority.

    fn spawn_request(prompt: &str, cwd: &Path) -> spawnreq::SpawnRequest {
        spawnreq::SpawnRequest {
            agent: "claude".to_string(),
            prompt: prompt.to_string(),
            cwd: cwd.to_path_buf(),
            requested_by: "aaaa1111".to_string(),
        }
    }

    /// Runs `fulfill_spawn_request` against an empty pane list. Every
    /// assertion below is on a refusal that happens *before* adapter
    /// resolution or any spawn, so no agent -- real or fake -- is ever
    /// launched.
    fn refusal_for(req: &spawnreq::SpawnRequest, cfg: &CtxConfig, repo: &Path) -> String {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut panes: Vec<Pane> = Vec::new();
        let mut queues: Vec<VecDeque<String>> = Vec::new();
        let mut errors = Vec::new();
        fulfill_spawn_request(
            req,
            &mut panes,
            &mut queues,
            cfg,
            &state,
            repo,
            (80, 24),
            &tmp.path().join("requests"),
            &mut errors,
        )
        .expect_err("must refuse")
    }

    #[test]
    fn argv_unsafe_prompt_flags_anything_that_would_be_read_as_a_flag() {
        assert!(argv_unsafe_prompt("--dangerously-skip-permissions"));
        assert!(argv_unsafe_prompt("  -p"));
        assert!(argv_unsafe_prompt("-"));
        assert!(!argv_unsafe_prompt("fix the failing tests"));
        assert!(!argv_unsafe_prompt("re-run the -x flag investigation"));
        assert!(!argv_unsafe_prompt(""));
    }

    /// F2 at the authority side: the request's prompt is encoded
    /// positionally into `interactive_cmd`'s argv, so a prompt shaped like a
    /// flag would reach the real harness child as one.
    #[test]
    fn fulfill_spawn_request_refuses_a_prompt_that_would_land_as_a_flag() {
        let repo = std::env::current_dir().expect("cwd");
        let cfg = CtxConfig::default();
        let reason = refusal_for(
            &spawn_request("--dangerously-skip-permissions", &repo),
            &cfg,
            &repo,
        );
        assert_eq!(reason, ARGV_GUARD_REFUSAL, "got {reason}");
    }

    /// F9: `cwd` used to be written by the requester and never looked at.
    /// Refusing is the honest contract -- this dashboard's panes live in this
    /// dashboard's repo.
    #[test]
    fn fulfill_spawn_request_refuses_a_request_naming_another_repo() {
        let repo = std::env::current_dir().expect("cwd");
        let cfg = CtxConfig::default();
        let elsewhere = repo.join("definitely-not-this-repo");
        let reason = refusal_for(&spawn_request("do the work", &elsewhere), &cfg, &repo);
        assert!(
            reason.contains("only spawns panes in its own repo"),
            "got {reason}"
        );
        assert!(reason.contains("definitely-not-this-repo"), "got {reason}");
    }

    /// F13: the cap is enforced where a pane is created by something other
    /// than the operator's own launch, so a pane child cannot fork-bomb its
    /// own dashboard. `max_panes = 0` proves the refusal without spawning a
    /// single real process.
    #[test]
    fn fulfill_spawn_request_refuses_once_the_pane_cap_is_reached() {
        let repo = std::env::current_dir().expect("cwd");
        let mut cfg = CtxConfig::default();
        cfg.dash.max_panes = 0;
        let reason = refusal_for(&spawn_request("do the work", &repo), &cfg, &repo);
        assert!(reason.contains("pane limit reached"), "got {reason}");
        assert!(reason.contains("dash.max_panes"), "got {reason}");
    }

    // F11: the spawn dialog's own reducer.

    fn type_line(line: &str) -> ui::SpawnDraft {
        let mut draft = ui::SpawnDraft::default();
        for c in line.chars() {
            let (next, effect) = spawn_overlay_reduce(draft, press(c));
            assert!(effect.is_none(), "typing emits no effect");
            draft = next.expect("typing keeps the dialog open");
        }
        draft
    }

    #[test]
    fn spawn_dialog_enter_splits_the_agent_from_the_prompt() {
        let draft = type_line("claude fix the failing tests");
        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none(), "a submitted dialog closes");
        assert_eq!(
            effect,
            Some(SpawnEffect::Submit {
                agent: "claude".to_string(),
                prompt: "fix the failing tests".to_string(),
            })
        );
    }

    #[test]
    fn spawn_dialog_needs_both_an_agent_and_a_prompt() {
        for line in ["", "   ", "claude", "claude   "] {
            let draft = type_line(line);
            let (next, effect) =
                spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
            assert!(
                next.is_some(),
                "the dialog stays open so the typed text is not lost: {line:?}"
            );
            assert_eq!(
                effect,
                Some(SpawnEffect::Notice(SPAWN_USAGE_NOTICE.to_string())),
                "got no notice for {line:?}"
            );
        }
    }

    #[test]
    fn spawn_dialog_backspace_edits_and_esc_cancels() {
        let draft = type_line("claudex");
        let (next, _) = spawn_overlay_reduce(draft, key(KeyCode::Backspace, KeyModifiers::NONE));
        let draft = next.expect("stays open");
        assert_eq!(draft.input, "claude");

        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none(), "Esc closes the dialog");
        assert!(effect.is_none(), "and asks for nothing");
    }

    /// The dialog does not re-implement the argv guard, the pane cap or the
    /// agent gate: it submits, and the shared `fulfill_spawn_request` path
    /// refuses. This pins that a flag-shaped prompt does reach that path
    /// intact (rather than being silently mangled or split into flags here).
    #[test]
    fn spawn_dialog_submits_a_flag_shaped_prompt_for_the_shared_guard_to_refuse() {
        let draft = type_line("claude --dangerously-skip-permissions");
        let (_, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
        match effect {
            Some(SpawnEffect::Submit { prompt, .. }) => {
                assert_eq!(prompt, "--dangerously-skip-permissions");
                assert!(
                    argv_unsafe_prompt(&prompt),
                    "and the shared guard is what refuses it"
                );
            }
            other => panic!("expected a Submit, got {other:?}"),
        }
    }

    // Task 12: the startup restore dialog's pure reducer, `build_restore_view`,
    // and `on_quit`'s own roster write.

    fn restore_entry(label: &str, checked: bool) -> ui::RestoreEntry {
        ui::RestoreEntry {
            label: label.to_string(),
            checked,
        }
    }

    #[test]
    fn restore_overlay_esc_skips_with_no_effect() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", true)],
            cursor: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn restore_overlay_space_toggles_the_entry_under_the_cursor() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", true), restore_entry("b", true)],
            cursor: 1,
        };
        let (next, effect) = restore_overlay_reduce(view, press(' '));
        let next = next.expect("stays open");
        assert!(next.entries[0].checked, "untouched entry stays checked");
        assert!(
            !next.entries[1].checked,
            "entry under the cursor toggles off"
        );
        assert!(effect.is_none());
    }

    #[test]
    fn restore_overlay_enter_confirms_only_the_checked_indices() {
        let view = ui::RestoreView {
            entries: vec![
                restore_entry("a", true),
                restore_entry("b", false),
                restore_entry("c", true),
            ],
            cursor: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none(), "confirming closes the dialog");
        assert_eq!(effect, Some(RestoreEffect::Confirm(vec![0, 2])));
    }

    #[test]
    fn restore_overlay_enter_with_nothing_checked_still_confirms_an_empty_set() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", false)],
            cursor: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none());
        assert_eq!(effect, Some(RestoreEffect::Confirm(Vec::new())));
    }

    #[test]
    fn restore_overlay_cursor_clamps_within_bounds() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", true), restore_entry("b", true)],
            cursor: 0,
        };
        let (next, _) = restore_overlay_reduce(view, key(KeyCode::Down, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.cursor, 1);

        let (next, _) = restore_overlay_reduce(next, key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            next.expect("stays open").cursor,
            1,
            "past the last row clamps rather than overflowing"
        );
    }

    #[test]
    fn build_restore_view_defaults_every_entry_to_checked_and_labels_it() {
        let candidates = vec![roster::RosterPane {
            agent: "claude".to_string(),
            session_id: "sess-1".to_string(),
            role: roster::ROLE_WORKER.to_string(),
            short: "aaaa1111".to_string(),
            title: "wrk claude".to_string(),
        }];
        let view = build_restore_view(&candidates);
        assert_eq!(view.entries.len(), 1);
        assert!(view.entries[0].checked);
        assert!(
            view.entries[0].label.contains("aaaa1111"),
            "got {}",
            view.entries[0].label
        );
    }

    #[cfg(windows)]
    fn trivial_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ]
    }

    #[cfg(unix)]
    fn trivial_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()]
    }

    /// A trivial, immediately-exiting child -- never a real agent (the
    /// ABSOLUTE rule this plan spells out) -- just enough of a process for
    /// `Pane::spawn` to have something real to supervise, matching
    /// `pane.rs`'s own test pattern.
    #[test]
    fn on_quit_writes_this_repos_roster_before_removing_the_requests_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            title: "wrk test".to_string(),
        };
        let mut panes = vec![Pane::spawn(spec, &state, &repo, (80, 24), &[]).expect("spawn")];

        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        on_quit(&panes, &requests_dir, &state, &repo);

        assert!(
            !requests_dir
                .parent()
                .expect("requests dir has a parent")
                .exists(),
            "the whole capability-token directory is removed on quit"
        );

        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("on_quit must have written a roster");
        assert_eq!(written.panes.len(), 1);
        assert_eq!(written.panes[0].agent, "test-agent");
        assert_eq!(written.panes[0].short, panes[0].short());
        assert_eq!(written.panes[0].role, roster::ROLE_WORKER);

        for pane in panes.iter_mut() {
            let _ = pane.shutdown("");
        }
    }
}
