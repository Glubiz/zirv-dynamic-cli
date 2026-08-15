//! Pure renderers for the dashboard: every function in this module takes a
//! `&mut Frame` plus already-computed data and draws into it. No I/O, no
//! filesystem, no environment, no clock -- Task 5's event loop assembles the
//! facts (from panes, the session registry, mail/memory) and calls straight
//! through here, and every renderer is exercised with `ratatui::backend::
//! TestBackend` precisely because there is nothing else to stub.
//!
//! `SpawnDraft`/`NudgeDraft`/`MailView`/`MemoryView`/`RestoreView` carry
//! whatever shape their own overlay reducer needs (Tasks 8/9/12, in
//! `dash::mod`); `SpawnDraft` is still the Task 4 placeholder -- Spawn's own
//! reducer is out of this plan's scope.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use super::super::chrome::right_truncate;
use super::super::mail::Message;
use super::pane::PaneState;

/// One account's two usage windows, already reduced to whole percents by the
/// caller (`dash::mod`'s `usage_pct_u8`, the same rounding `chrome::status_bar`
/// uses, so the two surfaces never disagree about one number).
///
/// A `None` field is *unknown*: a real usage source that reports nothing for
/// that window. It is not zero, and it is not an error -- see [`AccountFacts`]
/// for the genuinely-no-source case, which is a different thing to tell an
/// operator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountWindows {
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
}

/// One provider account in the header: a provider slug (`"anthropic"`,
/// `"openai"`) plus its usage, when a usage source for it exists at all.
///
/// The outer `Option` is the whole point of `window::provider_summary`
/// returning one: `None` means **no usage source exists for this provider**,
/// which renders as words saying so and must never render as `0%` or as an
/// empty bar. `Some(AccountWindows::default())` is a source that exists and
/// reports neither window -- unknown, but real, and rendered as `--`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountFacts {
    pub provider: String,
    pub usage: Option<AccountWindows>,
}

/// The header's live facts. `mail_broadcast`/`mail_direct` render as
/// "mail B+D" only when at least one is non-zero, mirroring `wrap.rs`'s own
/// convention of omitting a segment entirely when mail is disabled rather
/// than showing a hollow "mail 0+0".
///
/// `harness`/`score`/`account` describe the **focused** pane specifically --
/// the harness whose grid is on screen, its own rot score, and its own
/// provider's usage. `accounts` is the whole-machine view: every provider the
/// adapter registry knows about, whether or not any of them has ever written a
/// reading. The two overlap deliberately: the operator needs the focused
/// session's own limit without having to find it in a list.
pub struct HeaderFacts {
    pub harness: String,
    pub score: Option<u32>,
    pub account: AccountFacts,
    pub mail_broadcast: usize,
    pub mail_direct: usize,
    pub memory_count: usize,
    pub sessions: usize,
    pub accounts: Vec<AccountFacts>,
}

/// One sidebar row: a dashboard pane (`attached: true`) or a view-only
/// registry session this dashboard did not spawn (`attached: false`).
///
/// `selected` and `focused` are deliberately two different things (F7).
/// `selected` is the sidebar cursor, which walks the *combined* row list --
/// view-only registry rows included, so a nudge can be aimed at a session
/// this dashboard does not own. `focused` is the pane whose grid is on
/// screen and whose child receives every un-prefixed keystroke, and it can
/// only ever be an attached pane. Before F7 the two were one index, so
/// selecting a view-only row blanked the grid and swallowed all typing.
///
/// `score` is that instance's cached rot score, and `None` there means
/// **unknown**, never healthy -- it renders as `rot --` (see [`score_text`]).
/// A view-only row carries one too: `score::cached_score` is keyed by session
/// id and repo, neither of which requires this dashboard to own the session.
pub struct SidebarRow {
    pub glyph: char,
    pub title: String,
    pub short: String,
    pub preview: String,
    pub score: Option<u32>,
    pub attached: bool,
    pub selected: bool,
    pub focused: bool,
}

/// A minimal draft/view struct shared by the overlay seams below. Only what
/// `render_overlay` needs to draw something today; Task 12 fills in richer
/// fields as it wires up the restore dialog's own reducer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnDraft {
    pub input: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

/// Where a submitted nudge goes: an attached pane this dashboard owns
/// (`AttachedPane` -- idle-gated visible injection, or queued if the pane is
/// still `Working`), or a session this dashboard did not spawn
/// (`ViewOnlySession`, routed through `sessions::run_nudge_with`'s existing
/// headless marker+mail semantics). `None` when nothing was selected at the
/// moment `prefix,n` was pressed.
///
/// D1: `AttachedPane` names the pane by its **registry short id**, exactly as
/// `ViewOnlySession` does, not by its index in the live `panes` vector. The
/// dialog stays open across as many ticks as the operator takes to type, and a
/// pane reaped (or spawned) in the meantime shifts every index after it -- so
/// a captured index quietly re-aimed the nudge at whichever pane had slid into
/// that slot. A short id either still names a live pane at Enter time or names
/// nothing at all, and "nothing" is a notice, never a misdelivery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NudgeTarget {
    #[default]
    None,
    AttachedPane(String),
    ViewOnlySession(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NudgeDraft {
    pub target: NudgeTarget,
    pub input: String,
}

/// One message-in-progress: `to` defaults to `"any"` when left blank (the
/// same default `mail::SendArgs` uses), `body` is what the operator types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeDraft {
    pub to: String,
    pub body: String,
}

/// The mail overlay's own state: every unread message visible to the
/// dashboard operator (`(path, from_agent, body_preview)`, oldest first --
/// the same order `mail::list` already returns), which one is selected, and
/// an in-progress compose draft when the operator is writing a new one
/// rather than browsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailView {
    pub items: Vec<(PathBuf, String, String)>,
    pub cursor: usize,
    pub compose: Option<ComposeDraft>,
}

/// What executing a `MailView` reducer's emitted action actually does to
/// storage -- executed by `dash::mod`'s own `apply_mail_effect`, never by the
/// reducer itself (the reducer stays pure). `Send`'s `Message` carries
/// placeholder `from_session`/`from_agent`/`sent` fields the executor
/// overwrites right before `mail::store`, keeping the reducer itself
/// identity- and clock-free, the same discipline `rot.rs` and `prompt.rs`
/// already hold themselves to.
#[derive(Debug, Clone, PartialEq)]
pub enum MailEffect {
    Consume(PathBuf),
    Send(Message),
}

/// The memory overlay's own state: every entry in this repo's bank
/// (`(key, age, body)`, oldest-written first -- the same order `memory::list`
/// already returns), which one is selected, and an in-progress edit buffer
/// when the operator is remembering new text for the selected key rather
/// than browsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryView {
    pub entries: Vec<(String, String, String)>,
    pub cursor: usize,
    pub input: Option<String>,
}

/// What executing a `MemoryView` reducer's emitted action does to storage --
/// executed by `dash::mod`'s own `apply_memory_effect`. `Remember` carries
/// only `key`/`body`: `written_by`/timestamps/`source` are filled in by the
/// executor the same way `run_remember_with` fills them for the CLI verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEffect {
    Remember { key: String, body: String },
    Forget(String),
    Verify(String),
}

/// One roster candidate offered in the restore dialog: a human-readable
/// `label` (title/agent/short, assembled by `dash::mod` from the roster
/// entry it mirrors) and whether the operator currently has it checked for
/// restore. Indices into `RestoreView::entries` line up 1:1 with the
/// `Vec<roster::RosterPane>` candidate list `dash::mod` keeps alongside the
/// view -- neither list is ever reordered, only toggled -- so an effect that
/// names indices is enough for the caller to find the roster data back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreEntry {
    pub label: String,
    pub checked: bool,
}

/// The startup restore dialog's own state: every worker pane offered back
/// from the previous quit's roster (the orchestrator is excluded before this
/// view is ever built -- see `dash::mod::run_dashboard`'s own doc comment),
/// each independently checked/unchecked, defaulting to checked so Enter
/// alone restores everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreView {
    pub entries: Vec<RestoreEntry>,
    pub cursor: usize,
}

/// What, if anything, sits drawn on top of the grid right now. `QuitConfirm`
/// carries the titles of every pane still `Working`, so the confirmation
/// text can name what the operator is about to interrupt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    QuitConfirm(Vec<String>),
    Spawn(SpawnDraft),
    Nudge(NudgeDraft),
    Mail(MailView),
    Memory(MemoryView),
    /// The startup restore dialog, built once from the previous quit's
    /// roster (`dash::mod::run_dashboard`); never re-opened later in a
    /// session's life the way the other overlays are.
    Restore(RestoreView),
}

/// Pure: how many rows the header gets in an `area_height`-row frame.
///
/// One row always -- the focused instance's own summary, which
/// [`header_line`] guarantees fits any width. A second row, carrying the
/// full multi-provider account list, only once the terminal is tall enough to
/// spare it: the account list is a nice-to-have, and the pane grid is the
/// thing the operator is actually working in.
///
/// The threshold is `chrome::MIN_DASH_ROWS`, the dashboard's own documented
/// floor, rather than a number invented here. At or above the floor one row in
/// twenty is affordable chrome; below it -- a terminal resized under the
/// minimum mid-session, which the dashboard tolerates rather than refuses --
/// the grid keeps every row it has and the account list is dropped outright
/// rather than wrapped into a broken layout.
///
/// `min` throughout rather than arithmetic: `area.height` is genuinely `0` and
/// `1` in real frames, and the release profile is `panic = "abort"`.
pub(crate) fn header_rows(area_height: u16) -> u16 {
    if area_height >= super::super::chrome::MIN_DASH_ROWS {
        2.min(area_height)
    } else {
        1.min(area_height)
    }
}

/// Splits `area` into (header, sidebar, main): [`header_rows`] header rows, a
/// `sidebar_cols`-wide sidebar, a one-column separator, and everything else
/// as the active pane's grid.
pub fn layout(area: Rect, sidebar_cols: u16) -> (Rect, Rect, Rect) {
    let header_h = header_rows(area.height);
    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: header_h,
    };

    let body_y = area.y + header_h;
    let body_h = area.height.saturating_sub(header_h);
    let sidebar_w = sidebar_cols.min(area.width);
    let sidebar = Rect {
        x: area.x,
        y: body_y,
        width: sidebar_w,
        height: body_h,
    };

    let separator = if area.width > sidebar_w { 1 } else { 0 };
    let main = Rect {
        x: area.x + sidebar_w + separator,
        y: body_y,
        width: area.width.saturating_sub(sidebar_w + separator),
        height: body_h,
    };

    (header, sidebar, main)
}

/// Maps a `vt100` color to its `ratatui` equivalent. The first sixteen
/// indexed colors are translated to ratatui's named ANSI variants (`Idx(1)`
/// -> `Color::Red`, not `Color::Indexed(1)`) rather than left as `Indexed`:
/// `ratatui::style::Color`'s `PartialEq` is derived, so `Indexed(1)` and
/// `Red` never compare equal even though most terminals render the same
/// palette entry for both -- anything that compares a rendered cell's color
/// against a named `Color` (see `grid_renders_vt100_cells_with_colours`
/// below) needs the named variant. Indices 16-255 have no named equivalent
/// and stay `Indexed`.
fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        vt100::Color::Idx(i) => match i {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            15 => Color::White,
            other => Color::Indexed(other),
        },
    }
}

/// Pure: a rot score for display.
///
/// `None` is **unknown**, not healthy, and the two are opposite things to tell
/// an operator -- so it renders as `--`, exactly as `score::cached_score`'s own
/// doc comment requires and as `chrome::status_bar` already does for the same
/// number in a `wrap` session.
pub fn score_text(score: Option<u32>) -> String {
    match score {
        Some(score) => score.to_string(),
        None => "--".to_string(),
    }
}

/// Pure: one usage window's percentage, or `--` when that window is unknown.
fn window_text(pct: Option<u8>) -> String {
    match pct {
        Some(pct) => format!("{pct}%"),
        None => "--".to_string(),
    }
}

/// Pure: one account's usage reading.
///
/// The `None` case is the load-bearing one: no usage source exists for this
/// provider at all, which is said in words. Rendering it as `0%` would claim a
/// fresh, empty quota; rendering it as `100%` would claim an exhausted one;
/// both are inventions. A source that exists but reports neither window says
/// `5h -- 7d --`, which is unknown-but-real -- a different fact, and not an
/// error.
pub fn usage_text(usage: Option<&AccountWindows>) -> String {
    match usage {
        None => "no usage source".to_string(),
        Some(windows) => format!(
            "5h {} 7d {}",
            window_text(windows.five_hour),
            window_text(windows.seven_day)
        ),
    }
}

/// Pure: one provider's account row, `"<provider> <usage>"`.
pub fn account_text(account: &AccountFacts) -> String {
    format!(
        "{} {}",
        account.provider,
        usage_text(account.usage.as_ref())
    )
}

/// Pure: the header's always-present first line, right-truncated to `cols`.
///
/// Ordered by how badly the operator needs it if the line has to be cut: the
/// focused harness (which already carries any live notice or sticky error),
/// then its rot score, then its own provider's usage, then the incidental
/// counts. At `chrome::MIN_DASH_COLS` (80) everything through `sessions N`
/// fits for a bare harness label; a long `chat.model` disclosure pushes the
/// tail off the right, which is the same trade the line already made before
/// the score and usage segments existed.
pub fn header_line(facts: &HeaderFacts, cols: u16) -> String {
    let mut parts = vec![facts.harness.clone()];
    parts.push(format!("rot {}", score_text(facts.score)));
    parts.push(account_text(&facts.account));
    if facts.mail_broadcast > 0 || facts.mail_direct > 0 {
        parts.push(format!(
            "mail {}+{}",
            facts.mail_broadcast, facts.mail_direct
        ));
    }
    if facts.memory_count > 0 {
        parts.push(format!("mem {}", facts.memory_count));
    }
    parts.push(format!("sessions {}", facts.sessions));
    right_truncate(&parts.join("  "), cols as usize)
}

/// Pure: the header's second line -- every provider the adapter registry knows
/// about, each with its own reading or an honest "no usage source".
///
/// Right-truncated rather than wrapped: a third provider would otherwise push
/// this into two rows, and the header's height was decided (by
/// [`header_rows`]) before this string existed.
pub fn accounts_line(accounts: &[AccountFacts], cols: u16) -> String {
    if accounts.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = accounts.iter().map(account_text).collect();
    right_truncate(&format!("accounts  {}", listed.join("  ")), cols as usize)
}

pub fn render_header(f: &mut Frame, area: Rect, facts: &HeaderFacts) {
    if area.is_empty() {
        return;
    }
    f.render_widget(
        Paragraph::new(header_line(facts, area.width))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        Rect { height: 1, ..area },
    );
    // Only when `header_rows` actually gave us the row. Never squeezed into
    // the first line, and never drawn outside `area`.
    if area.height >= 2 {
        f.render_widget(
            Paragraph::new(accounts_line(&facts.accounts, area.width))
                .style(Style::default().add_modifier(Modifier::DIM)),
            Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            },
        );
    }
}

/// Pure: the first sidebar row drawn, given how many rows there are, how many
/// fit inside the block's border, and which one the cursor is on.
///
/// The sidebar used to draw from row `0` unconditionally, so once the combined
/// row count (panes plus view-only registry rows) outgrew the sidebar's height
/// the cursor could walk onto a row that was never drawn -- an invisible
/// selection, and with arrow navigation now moving focus too, a keyboard that
/// moved to a pane the operator could not see listed.
///
/// Stateless on purpose, rather than a `ListState` threaded through every
/// frame: the offset is a function of the three numbers it is derived from,
/// which keeps `render_sidebar` a pure function of its arguments the way every
/// other renderer in this module is, and keeps this testable without a frame.
/// The cost is that a scrolled window pins the selection to its bottom row
/// instead of remembering where it entered from; the benefit is that the
/// selection is *always* inside the window, from any starting state.
///
/// Every subtraction is guarded: `dash.sidebar_cols` and a two-row terminal
/// both genuinely reach here, and the release profile is `panic = "abort"`, so
/// an underflow would take the operator's terminal with it.
pub(crate) fn sidebar_offset(total: usize, visible: usize, selected: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    selected.saturating_sub(visible - 1).min(total - visible)
}

/// Pure: one sidebar row's text, right-truncated to `cols`.
///
/// The rot score sits ahead of the title and the preview on purpose.
/// `dash.sidebar_cols` defaults to 24 -- 22 columns inside the border -- so
/// something has to lose; identity (`short`) and health (`rot`) are what the
/// column is for, and a title the operator already knows from the grid is what
/// can afford to be cut. `rot --` occupies exactly the width of `rot 42`, so a
/// column of rows stays aligned whether or not every session has been scored.
///
/// `cols` is `Block::inner`'s width, which is `0` for any sidebar under four
/// columns wide -- a real geometry, since `dash.sidebar_cols` is configurable
/// and a terminal can be narrower than it. `right_truncate` takes characters,
/// never bytes, so a multi-byte glyph is dropped whole rather than split.
pub fn sidebar_row_text(row: &SidebarRow, cols: u16) -> String {
    let attach_marker = if row.attached { ' ' } else { '~' };
    // The keyboard-focus marker is separate from the reversed-style
    // selection highlight: with F7 the two can sit on different rows, and
    // the operator has to be able to see which pane their typing is
    // actually reaching.
    let focus_marker = if row.focused { '*' } else { ' ' };
    let text = format!(
        "{}{} {}{:<8} rot {} {} {}",
        focus_marker,
        row.glyph,
        attach_marker,
        row.short,
        score_text(row.score),
        row.title,
        row.preview
    );
    right_truncate(&text, cols as usize)
}

pub fn render_sidebar(f: &mut Frame, area: Rect, rows: &[SidebarRow]) {
    let block = Block::default().borders(Borders::ALL);
    // The rows and columns the border actually leaves room for -- what the
    // offset and the row truncation are computed against, so the window and
    // the drawn area cannot disagree. `Block::inner` saturates, so a sidebar
    // narrower than its own border yields zero of both rather than underflow.
    let inner = block.inner(area);
    let visible = inner.height as usize;
    let selected = rows.iter().position(|r| r.selected).unwrap_or(0);
    let offset = sidebar_offset(rows.len(), visible, selected);

    let items: Vec<ListItem> = rows
        .iter()
        .skip(offset)
        .take(visible)
        .map(|row| {
            let text = sidebar_row_text(row, inner.width);
            let mut style = Style::default();
            // A view-only row is a live session in the registry that this
            // dashboard did not spawn: the sidebar cursor can walk onto it,
            // but the keyboard cannot follow (`dash::apply_navigation`). Dim,
            // on top of the `~` marker, so that reads as "not attached"
            // rather than as arrow navigation having silently failed -- which
            // is how it was reported.
            if !row.attached {
                style = style.add_modifier(Modifier::DIM);
            }
            if row.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            ListItem::new(text).style(style)
        })
        .collect();

    // The same affordance the grid's `SCROLL -N` marker gives a scrolled pane,
    // in the one place the sidebar has room for it: a window into a longer
    // list says so, so a truncated session list does not read as the whole of
    // it. A list that fits keeps the bare title.
    let title = if rows.len() > visible && visible > 0 {
        format!("panes {}-{}/{}", offset + 1, offset + visible, rows.len())
    } else {
        "panes".to_string()
    };
    f.render_widget(List::new(items).block(block.title(title)), area);
}

/// Walks every `vt100` cell in `screen` into `area`'s buffer, cell for cell.
/// A wide cell's own contents are drawn once and the following column is
/// skipped, matching how `vt100` itself represents double-width glyphs (the
/// continuation cell carries no contents of its own).
pub fn render_grid(f: &mut Frame, area: Rect, screen: &vt100::Screen) {
    let (rows, cols) = screen.size();
    let buf = f.buffer_mut();

    for row in 0..rows.min(area.height) {
        let mut skip_next = false;
        for col in 0..cols.min(area.width) {
            if skip_next {
                skip_next = false;
                continue;
            }
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };

            let x = area.x + col;
            let y = area.y + row;
            if x >= area.x + area.width || y >= area.y + area.height {
                continue;
            }
            let Some(target) = buf.cell_mut((x, y)) else {
                continue;
            };

            let symbol = cell.contents();
            target.set_symbol(if symbol.is_empty() { " " } else { symbol });

            let mut modifiers = Modifier::empty();
            if cell.bold() {
                modifiers |= Modifier::BOLD;
            }
            if cell.dim() {
                modifiers |= Modifier::DIM;
            }
            if cell.italic() {
                modifiers |= Modifier::ITALIC;
            }
            if cell.underline() {
                modifiers |= Modifier::UNDERLINED;
            }
            if cell.inverse() {
                modifiers |= Modifier::REVERSED;
            }

            target.set_style(
                Style::default()
                    .fg(map_color(cell.fgcolor()))
                    .bg(map_color(cell.bgcolor()))
                    .add_modifier(modifiers),
            );

            if cell.is_wide() {
                skip_next = true;
            }
        }
    }
}

/// Pure: the marker a pane's title carries while its viewport is scrolled
/// back, or `None` when it is live.
///
/// A scrolled-back pane looks exactly like a wedged one -- the child keeps
/// producing output and the grid does not move -- so the operator needs to be
/// told which of the two they are looking at. `-N` reads as "N rows behind
/// live", the same sign convention tmux's own copy-mode indicator uses.
pub fn scroll_marker(scrollback: usize) -> Option<String> {
    if scrollback == 0 {
        None
    } else {
        Some(format!("SCROLL -{scrollback}"))
    }
}

/// Draws [`scroll_marker`] over the top-right corner of a pane's grid, in
/// reverse video so it reads as chrome rather than as something the child
/// printed. A no-op for a live pane.
///
/// Deliberately on the grid rather than in the header or the sidebar: `Ctrl+A
/// z` hides both of those, and a zoomed pane is exactly the one an operator is
/// most likely to be scrolling through. Clipped to `area`, so a grid too
/// narrow to hold the marker simply does not get one.
pub fn render_scroll_marker(f: &mut Frame, area: Rect, scrollback: usize) {
    let Some(marker) = scroll_marker(scrollback) else {
        return;
    };
    let width = marker.chars().count() as u16;
    if area.is_empty() || area.width < width {
        return;
    }
    let rect = Rect {
        x: area.x + area.width - width,
        y: area.y,
        width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(marker).style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        ),
        rect,
    );
}

/// Pure: the absolute frame position of `screen`'s text cursor when it should
/// be shown, given the `area` its grid was rendered into. `render_grid` draws
/// cell `(row, col)` at `(area.x + col, area.y + row)` with no border of its
/// own, so the cursor translates the same way. Returns `None` when the cursor
/// is hidden (`vt100::Screen::hide_cursor`) or falls outside `area` -- a caller
/// that gets `None` leaves the frame cursor unset, which ratatui renders as no
/// caret at all.
///
/// Only the *focused* pane's screen is ever passed here: a non-focused pane's
/// grid is not on screen, so its cursor contributes nothing.
pub fn grid_cursor_position(area: Rect, screen: &vt100::Screen) -> Option<Position> {
    if screen.hide_cursor() {
        return None;
    }
    let (row, col) = screen.cursor_position();
    if col >= area.width || row >= area.height {
        return None;
    }
    Some(Position::new(area.x + col, area.y + row))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// The dialog box's own width inside `area`: four columns of margin when
/// there is room for them, never zero, and never wider than `area` itself.
///
/// Deliberately **not** `area.width.saturating_sub(4).clamp(1, area.width)`:
/// `Ord::clamp` asserts `min <= max` and panics when `area.width == 0`, which
/// a real session reaches (a terminal narrowed to at most the sidebar width
/// mid-session, or a `dash.sidebar_cols` wider than the terminal, both of
/// which make `ui::layout`'s own `main` rect zero-width) -- and the release
/// profile is `panic = "abort"`, so that takes the whole dashboard down with
/// it. Pure, so the degenerate widths are testable without a frame.
fn dialog_width(area_width: u16) -> u16 {
    area_width.saturating_sub(4).max(1).min(area_width.max(1))
}

fn render_dialog(f: &mut Frame, area: Rect, title: &str, lines: &[String]) {
    // Nothing to draw into: every renderer below would either be a no-op or
    // have to reason about a zero-sized rect. One guard, at the one place
    // every dialog funnels through.
    if area.is_empty() {
        return;
    }
    let h = (lines.len() as u16 + 2).min(area.height);
    let w = dialog_width(area.width);
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    // `Clear` first, or the dialog is transparent: a `Block` paints only its
    // border and a `Paragraph` only the cells its text reaches, so every
    // other cell inside `rect` keeps whatever the pane grid drew underneath.
    // Over a live harness screen that renders as a bordered box with the
    // child's output bleeding through it -- which is not merely ugly. An open
    // overlay swallows every keystroke (`filter_key` is not even called while
    // one is up), so a modal the operator cannot see is a dashboard that
    // appears to have stopped responding to `Ctrl+A` entirely. That is
    // exactly how this was reported from a real session.
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines.join("\n")).block(block), rect);
}

fn render_draft_dialog(
    f: &mut Frame,
    area: Rect,
    title: &str,
    input: &str,
    items: &[String],
    cursor: usize,
) {
    let mut lines = vec![format!("> {input}")];
    for (i, item) in items.iter().enumerate() {
        let marker = if i == cursor { '>' } else { ' ' };
        lines.push(format!("{marker} {item}"));
    }
    render_dialog(f, area, title, &lines);
}

/// Truncates a body preview to a single-line, human-scanning length. Shared
/// by the mail and memory dialogs so a long message or entry never blows out
/// the fixed-height dialog box `render_dialog` centres on screen.
fn preview(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    if first_line.chars().count() <= max_chars {
        first_line.to_string()
    } else {
        let mut truncated: String = first_line.chars().take(max_chars).collect();
        truncated.push('\u{2026}');
        truncated
    }
}

fn render_mail_dialog(f: &mut Frame, area: Rect, view: &MailView) {
    let lines = if let Some(draft) = &view.compose {
        vec![
            format!(
                "compose to: {}",
                if draft.to.trim().is_empty() {
                    "any"
                } else {
                    draft.to.as_str()
                }
            ),
            format!("> {}", draft.body),
            "Enter to send, Esc to cancel".to_string(),
        ]
    } else if view.items.is_empty() {
        vec!["(no mail)".to_string(), "c compose, Esc close".to_string()]
    } else {
        let mut lines: Vec<String> = view
            .items
            .iter()
            .enumerate()
            .map(|(i, (_, from, body))| {
                let marker = if i == view.cursor { '>' } else { ' ' };
                format!("{marker} {from}: {}", preview(body, 60))
            })
            .collect();
        lines.push("Enter read+consume, c compose, Esc close".to_string());
        lines
    };
    render_dialog(f, area, "mail", &lines);
}

fn render_memory_dialog(f: &mut Frame, area: Rect, view: &MemoryView) {
    let lines = if let Some(input) = &view.input {
        vec![
            format!("> {input}"),
            "Enter to save, Esc to cancel".to_string(),
        ]
    } else if view.entries.is_empty() {
        vec!["(no memory entries)".to_string(), "Esc close".to_string()]
    } else {
        let mut lines: Vec<String> = view
            .entries
            .iter()
            .enumerate()
            .map(|(i, (key, age, body))| {
                let marker = if i == view.cursor { '>' } else { ' ' };
                format!("{marker} {key} ({age}) {}", preview(body, 40))
            })
            .collect();
        lines.push("r remember, d forget, v verify, Esc close".to_string());
        lines
    };
    render_dialog(f, area, "memory", &lines);
}

fn render_restore_dialog(f: &mut Frame, area: Rect, view: &RestoreView) {
    let lines = if view.entries.is_empty() {
        vec!["(nothing to restore)".to_string(), "Esc close".to_string()]
    } else {
        let mut lines: Vec<String> = view
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let cursor = if i == view.cursor { '>' } else { ' ' };
                let check = if entry.checked { 'x' } else { ' ' };
                format!("{cursor} [{check}] {}", entry.label)
            })
            .collect();
        lines.push("space toggle, Enter restore checked, Esc skip".to_string());
        lines
    };
    render_dialog(f, area, "restore", &lines);
}

fn render_nudge_dialog(f: &mut Frame, area: Rect, draft: &NudgeDraft) {
    let target = match &draft.target {
        NudgeTarget::AttachedPane(short) => format!("pane {short}"),
        NudgeTarget::ViewOnlySession(short) => format!("session {short} (headless)"),
        NudgeTarget::None => "(no target selected)".to_string(),
    };
    let lines = vec![
        format!("to: {target}"),
        format!("> {}", draft.input),
        "Enter to send, Esc to cancel".to_string(),
    ];
    render_dialog(f, area, "nudge", &lines);
}

/// A single centered status line over `area`. Used by the dashboard to show
/// "shutting down N pane(s)…" the instant a quit begins, so the operator is
/// not left staring at a frozen alternate screen while each pane's quit grace
/// elapses (M9).
pub fn render_center_message(f: &mut Frame, area: Rect, message: &str) {
    if area.is_empty() {
        return;
    }
    let rect = centered(area, area.width, 1.min(area.height));
    f.render_widget(
        Paragraph::new(message.to_string()).style(Style::default().add_modifier(Modifier::BOLD)),
        rect,
    );
}

/// The smallest main rect that can host a dialog and have it read as one: a
/// bordered box needs two columns and two rows of border before a single cell
/// of text, and anything narrower than this is a box the operator cannot
/// recognise as a modal.
const MIN_OVERLAY_COLS: u16 = 8;
const MIN_OVERLAY_ROWS: u16 = 3;

/// Pure: where an open overlay is actually drawn.
///
/// Normally the main rect. But an open overlay swallows every keystroke --
/// `filter_key` is not even called while one is up -- so an overlay that
/// declines to draw is a dashboard that has stopped responding with nothing on
/// screen to say why. `main` genuinely goes to zero (a terminal narrowed to at
/// most `dash.sidebar_cols`, or a `dash.sidebar_cols` wider than the
/// terminal), and returning early there is the same class of bug as the
/// transparent dialog: the modal is invisible and still modal. So when the
/// main rect is too small to host a dialog, the whole frame is used instead --
/// over the header and sidebar, which is the right trade for a modal.
fn overlay_area(frame: Rect, main: Rect) -> Rect {
    if main.width >= MIN_OVERLAY_COLS && main.height >= MIN_OVERLAY_ROWS {
        main
    } else {
        frame
    }
}

pub fn render_overlay(f: &mut Frame, area: Rect, overlay: &Overlay) {
    // Never an early return on a too-small `area`: see `overlay_area`. Only a
    // frame with no cells at all leaves nothing to draw into, and
    // `render_dialog`'s own guard covers that.
    let area = overlay_area(f.area(), area);
    match overlay {
        Overlay::None => {}
        Overlay::QuitConfirm(working) => {
            let mut lines = vec!["quit dashboard? still working:".to_string()];
            lines.extend(working.iter().cloned());
            lines.push("Enter to confirm, Esc to cancel".to_string());
            render_dialog(f, area, "quit", &lines);
        }
        Overlay::Spawn(d) => render_draft_dialog(f, area, "spawn", &d.input, &d.items, d.cursor),
        Overlay::Nudge(d) => render_nudge_dialog(f, area, d),
        Overlay::Mail(d) => render_mail_dialog(f, area, d),
        Overlay::Memory(d) => render_memory_dialog(f, area, d),
        Overlay::Restore(d) => render_restore_dialog(f, area, d),
    }
}

/// The sidebar/grid glyph for a pane's state: `●` Working, `○` Idle, `✕`
/// Ended (the exit code is not part of the glyph -- the sidebar preview text
/// carries it when it matters).
pub fn glyph_for(state: &PaneState) -> char {
    match state {
        PaneState::Working => '●',
        PaneState::Idle => '○',
        PaneState::Ended(_) => '✕',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A dialog must be opaque. `Block` paints only its border and
    /// `Paragraph` only the cells its text reaches, so without a `Clear` the
    /// pane grid underneath bleeds through every other cell -- and because an
    /// open overlay swallows every keystroke before `filter_key` is reached, a
    /// modal that reads as pane content is a dashboard that looks like it has
    /// stopped responding to `Ctrl+A`. Reported from a real session; this
    /// pins the fix by drawing a dialog over a screen full of `X`.
    #[test]
    fn a_dialog_is_opaque_and_never_lets_the_pane_bleed_through() {
        let mut parser = vt100::Parser::new(8, 40, 0);
        for _ in 0..8 {
            parser.process(&[b'X'; 40]);
        }
        let backend = TestBackend::new(40, 8);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_grid(f, area, parser.screen());
            render_dialog(f, area, "spawn", &["> prompt".to_string()]);
        })
        .expect("draw");

        let buf = term.backend().buffer();
        // The dialog is centred, so the row through its middle must contain
        // its border and text but no surviving grid character.
        let w = dialog_width(40);
        let rect = centered(Rect::new(0, 0, 40, 8), w, 3);
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "X",
                    "the pane grid bled through the dialog at ({x},{y})"
                );
            }
        }
        // Sanity: the grid is still painted outside the dialog, so the test
        // would fail for the right reason rather than because nothing drew.
        assert_eq!(buf[(0, 0)].symbol(), "X");
    }

    #[test]
    fn grid_renders_vt100_cells_with_colours() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m plain");
        let backend = TestBackend::new(20, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen()))
            .unwrap();
        let cell = &term.backend().buffer()[(0, 0)];
        assert_eq!(cell.symbol(), "r");
        assert_eq!(cell.fg, Color::Red);
    }

    #[test]
    fn grid_skips_the_continuation_cell_of_a_wide_glyph() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        // A CJK wide glyph occupies two terminal columns; vt100 marks the
        // first cell wide and leaves the second as a plain continuation.
        parser.process("\u{4e2d}ab".as_bytes());
        let backend = TestBackend::new(10, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen()))
            .unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "\u{4e2d}");
        // Column 1 is the wide glyph's continuation cell -- skipped by the
        // renderer, so it keeps the backend's blank default rather than
        // showing "a" a column early.
        assert_eq!(buf[(1, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }

    /// HIGH-1: the focused pane's cursor translates from a screen-relative
    /// `(row, col)` to an absolute frame position by adding the grid area's
    /// own origin, so a caret rendered at the pane's inner offset lands where
    /// the child actually put it.
    #[test]
    fn grid_cursor_translates_screen_position_into_the_grid_area() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        // Two lines then a partial third: the cursor sits at row 2, col 5.
        parser.process(b"line one\r\nline two\r\nabcde");
        let (row, col) = parser.screen().cursor_position();
        assert_eq!((row, col), (2, 5), "sanity: vt100 cursor is where we typed");

        let area = Rect::new(3, 1, 40, 10);
        let pos = grid_cursor_position(area, parser.screen()).expect("cursor is visible");
        assert_eq!(pos, Position::new(3 + 5, 1 + 2));
    }

    /// A hidden cursor contributes no caret, and a cursor past the grid area's
    /// own bounds is not drawn outside it.
    #[test]
    fn grid_cursor_is_none_when_hidden_or_out_of_bounds() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        // DECTCEM off: the harness has hidden its cursor.
        parser.process(b"\x1b[?25l");
        assert!(grid_cursor_position(Rect::new(0, 0, 40, 10), parser.screen()).is_none());

        let mut visible = vt100::Parser::new(10, 40, 0);
        visible.process(b"\x1b[?25h");
        // A grid area narrower/shorter than the cursor's own coordinates: the
        // caret would land outside the drawn cells, so it is suppressed.
        visible.process(b"\r\n\r\n\r\nx");
        assert!(grid_cursor_position(Rect::new(0, 0, 40, 2), visible.screen()).is_none());
    }

    /// A live pane carries no marker at all; a scrolled-back one says how far
    /// back it is, so "the grid is not updating" reads as a viewport the
    /// operator moved rather than as a wedged child.
    #[test]
    fn the_scroll_marker_appears_only_while_a_pane_is_scrolled_back() {
        assert_eq!(scroll_marker(0), None);
        assert_eq!(scroll_marker(1).as_deref(), Some("SCROLL -1"));
        assert_eq!(scroll_marker(42).as_deref(), Some("SCROLL -42"));
        assert_eq!(scroll_marker(1000).as_deref(), Some("SCROLL -1000"));
    }

    #[test]
    fn the_scroll_marker_renders_in_the_grids_top_right_corner() {
        let area = Rect::new(0, 0, 40, 6);
        let text = render_and_capture_text(area, |f, area| render_scroll_marker(f, area, 12));
        assert!(
            text.contains("SCROLL-12") || text.contains("SCROLL -12"),
            "got {text}"
        );
        // Top row, flush right: the last cell of row 0 is the marker's last
        // character, and nothing at all is drawn on any other row.
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_scroll_marker(f, area, 12)).unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(39, 0)].symbol(), "2");
        assert!(
            (0..40).all(|x| buf[(x, 1)].symbol() == " "),
            "the marker is one row tall"
        );
    }

    /// Never draws outside its area, and never panics on the degenerate
    /// geometries `layout` genuinely produces (a terminal narrowed to at most
    /// the sidebar width leaves a zero-sized main rect).
    #[test]
    fn the_scroll_marker_is_skipped_when_there_is_no_room_for_it() {
        let backend = TestBackend::new(20, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 20, 0),
            Rect::new(0, 0, 0, 4),
            Rect::new(0, 0, 5, 4),
            Rect::new(0, 0, 1, 1),
        ] {
            term.draw(|f| render_scroll_marker(f, area, 12))
                .expect("draw");
        }
        // A live pane draws nothing even with all the room in the world.
        let text = render_and_capture_text(Rect::new(0, 0, 20, 2), |f, area| {
            render_scroll_marker(f, area, 0)
        });
        assert!(text.trim().is_empty(), "got {text:?}");
    }

    #[test]
    fn layout_reserves_the_header_rows_and_the_sidebar() {
        let (h, s, m) = layout(Rect::new(0, 0, 100, 30), 24);
        assert_eq!(h.height, 2, "a tall terminal affords the accounts row");
        assert_eq!(s.width, 24);
        assert_eq!(m.width, 76 - 1);
        assert_eq!(m.height, 28);
    }

    #[test]
    fn layout_is_stable_on_a_tiny_area_without_underflow() {
        let (h, s, m) = layout(Rect::new(0, 0, 10, 2), 24);
        assert_eq!(h.height, 1);
        assert_eq!(s.width, 10);
        assert_eq!(m.width, 0);
        assert_eq!(m.height, 1);
    }

    /// The accounts row is chrome; the pane grid is the work. A terminal below
    /// the dashboard's own documented floor keeps every body row it has, and a
    /// zero/one-row frame never underflows on the way there (the release
    /// profile is `panic = "abort"`).
    #[test]
    fn the_accounts_row_is_only_taken_from_a_terminal_tall_enough_to_spare_it() {
        assert_eq!(header_rows(0), 0);
        assert_eq!(header_rows(1), 1);
        assert_eq!(header_rows(2), 1);
        assert_eq!(
            header_rows(super::super::super::chrome::MIN_DASH_ROWS - 1),
            1
        );
        assert_eq!(header_rows(super::super::super::chrome::MIN_DASH_ROWS), 2);
        assert_eq!(header_rows(200), 2);
        // And the body never loses more rows than the header gained.
        for height in 0..=64u16 {
            let (h, _, m) = layout(Rect::new(0, 0, 80, height), 24);
            assert_eq!(h.height + m.height, height, "height {height} lost a row");
        }
    }

    fn base_facts() -> HeaderFacts {
        HeaderFacts {
            harness: "claude".to_string(),
            score: None,
            account: AccountFacts {
                provider: "anthropic".to_string(),
                usage: None,
            },
            mail_broadcast: 0,
            mail_direct: 0,
            memory_count: 0,
            sessions: 1,
            accounts: Vec::new(),
        }
    }

    fn render_and_capture_text(area: Rect, draw: impl FnOnce(&mut Frame, Rect)) -> String {
        let backend = TestBackend::new(area.width, area.height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, area)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    }

    #[test]
    fn header_shows_the_broadcast_direct_mail_split() {
        let mut facts = base_facts();
        facts.mail_broadcast = 2;
        facts.mail_direct = 1;
        let area = Rect::new(0, 0, 60, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("mail 2+1"), "header text was: {text}");
    }

    #[test]
    fn header_omits_the_mail_segment_when_there_is_no_mail() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 60, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(!text.contains("mail"), "header text was: {text}");
    }

    /// "Unknown" and "healthy" are opposite things to tell an operator, so an
    /// unscored session shows `--` and never `0`. `score::cached_score`'s own
    /// doc comment states this as a requirement on its renderers.
    #[test]
    fn an_unknown_rot_score_reads_as_two_dashes_and_never_as_zero() {
        assert_eq!(score_text(None), "--");
        assert_eq!(score_text(Some(0)), "0");
        assert_eq!(score_text(Some(34)), "34");
        assert_eq!(score_text(Some(100)), "100");
    }

    /// The three usage outcomes that must stay distinguishable: no source at
    /// all, a source reporting nothing, and a source with readings. None of
    /// them may render as a bare `0%`.
    #[test]
    fn usage_text_separates_no_source_from_an_unknown_reading() {
        assert_eq!(usage_text(None), "no usage source");
        assert_eq!(usage_text(Some(&AccountWindows::default())), "5h -- 7d --");
        assert_eq!(
            usage_text(Some(&AccountWindows {
                five_hour: Some(42),
                seven_day: None,
            })),
            "5h 42% 7d --"
        );
        assert_eq!(
            usage_text(Some(&AccountWindows {
                five_hour: Some(0),
                seven_day: Some(13),
            })),
            "5h 0% 7d 13%",
            "a genuine zero reading is still a reading, and says so"
        );
    }

    #[test]
    fn an_account_row_names_its_provider_alongside_its_reading() {
        assert_eq!(
            account_text(&AccountFacts {
                provider: "openai".to_string(),
                usage: None,
            }),
            "openai no usage source"
        );
        assert_eq!(
            account_text(&AccountFacts {
                provider: "anthropic".to_string(),
                usage: Some(AccountWindows {
                    five_hour: Some(42),
                    seven_day: Some(13),
                }),
            }),
            "anthropic 5h 42% 7d 13%"
        );
    }

    fn two_accounts() -> Vec<AccountFacts> {
        vec![
            AccountFacts {
                provider: "anthropic".to_string(),
                usage: Some(AccountWindows {
                    five_hour: Some(42),
                    seven_day: Some(13),
                }),
            },
            AccountFacts {
                provider: "openai".to_string(),
                usage: None,
            },
        ]
    }

    /// The header's whole reason for existing: every provider the registry
    /// knows about is listed, including the one with nothing to show, which
    /// says so in words rather than being silently dropped.
    #[test]
    fn the_accounts_line_names_every_provider_including_the_ones_with_no_source() {
        let line = accounts_line(&two_accounts(), 200);
        assert_eq!(
            line,
            "accounts  anthropic 5h 42% 7d 13%  openai no usage source"
        );
        assert_eq!(accounts_line(&[], 200), "");
    }

    /// Both header lines have to fit `chrome::MIN_DASH_COLS` exactly, and
    /// never wrap: the header's height was decided before either string
    /// existed, so an over-long line steals a row it was not given.
    #[test]
    fn both_header_lines_fit_at_eighty_columns() {
        let cols = super::super::super::chrome::MIN_DASH_COLS;
        let mut facts = base_facts();
        facts.score = Some(34);
        facts.account = AccountFacts {
            provider: "anthropic".to_string(),
            usage: Some(AccountWindows {
                five_hour: Some(42),
                seven_day: Some(13),
            }),
        };
        facts.mail_broadcast = 2;
        facts.mail_direct = 1;
        facts.memory_count = 3;
        facts.sessions = 4;
        facts.accounts = two_accounts();

        let line = header_line(&facts, cols);
        assert_eq!(
            line, "claude  rot 34  anthropic 5h 42% 7d 13%  mail 2+1  mem 3  sessions 4",
            "nothing is cut at 80 columns for an ordinary header"
        );
        assert!(line.chars().count() <= cols as usize);
        assert!(!line.contains('\n'));
        assert!(accounts_line(&facts.accounts, cols).chars().count() <= cols as usize);

        // And a header that genuinely does not fit is cut, not wrapped -- at a
        // character boundary, for every width down to zero.
        facts.harness =
            "claude (a-very-long-model-name-disclosure)  \u{26a0} something went wrong \
                         while spawning a pane"
                .to_string();
        facts.accounts = vec![
            two_accounts()[0].clone(),
            two_accounts()[1].clone(),
            AccountFacts {
                provider: "third-party-provider".to_string(),
                usage: Some(AccountWindows::default()),
            },
        ];
        for w in 0..=cols {
            assert!(header_line(&facts, w).chars().count() <= w as usize);
            assert!(accounts_line(&facts.accounts, w).chars().count() <= w as usize);
        }
    }

    /// The accounts row is drawn only when `header_rows` gave the header two
    /// rows, and never outside `area` -- a one-row header must not paint over
    /// the sidebar's own top border.
    #[test]
    fn the_accounts_row_is_drawn_only_when_the_header_has_two_rows() {
        let mut facts = base_facts();
        facts.accounts = two_accounts();

        let one_row = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_header(f, area, &facts)
        });
        assert!(
            !one_row.contains("accounts"),
            "a one-row header has no account list: {one_row}"
        );

        let two_rows = render_and_capture_text(Rect::new(0, 0, 80, 2), |f, area| {
            render_header(f, area, &facts)
        });
        assert!(two_rows.contains("accounts"), "got {two_rows}");
        assert!(
            two_rows.contains("openai no usage source"),
            "the provider with no data is still named: {two_rows}"
        );
    }

    /// A very short terminal: the header renders its one line and nothing
    /// else, and every degenerate rect a resize can produce is a no-op rather
    /// than a panic.
    #[test]
    fn the_header_never_panics_on_a_degenerate_area() {
        let mut facts = base_facts();
        facts.accounts = two_accounts();
        let backend = TestBackend::new(80, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 80, 0),
            Rect::new(0, 0, 0, 4),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 3, 2),
            Rect::new(0, 0, 80, 1),
        ] {
            term.draw(|f| render_header(f, area, &facts)).expect("draw");
        }
        // A 80x4 frame: `header_rows` gives one row, so rows 1..4 stay blank
        // for the sidebar and grid that own them.
        let (header, _, _) = layout(Rect::new(0, 0, 80, 4), 24);
        assert_eq!(header.height, 1);
        term.draw(|f| render_header(f, header, &facts))
            .expect("draw");
        let buf = term.backend().buffer();
        assert!(
            (1..4).all(|y| (0..80).all(|x| buf[(x, y)].symbol() == " ")),
            "the header painted outside the row it was given"
        );
    }

    #[test]
    fn glyphs_match_the_spec() {
        assert_eq!(glyph_for(&PaneState::Working), '●');
        assert_eq!(glyph_for(&PaneState::Idle), '○');
        assert_eq!(glyph_for(&PaneState::Ended(0)), '✕');
    }

    #[test]
    fn render_overlay_none_draws_nothing_and_never_panics() {
        let area = Rect::new(0, 0, 40, 10);
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_overlay(f, area, &Overlay::None))
            .unwrap();
    }

    /// An open overlay swallows every keystroke before `filter_key` is
    /// reached, so an overlay that declines to draw is a dashboard that has
    /// stopped responding with nothing on screen to explain it. `layout`
    /// genuinely produces a zero-width main rect (a terminal narrowed to at
    /// most `dash.sidebar_cols`), where the old code returned early. It now
    /// falls back to the whole frame.
    #[test]
    fn an_overlay_with_no_room_in_the_main_rect_draws_over_the_whole_frame() {
        let overlay = Overlay::QuitConfirm(vec!["wrk claude".to_string()]);
        let frame = Rect::new(0, 0, 24, 8);
        for degenerate in [
            Rect::new(24, 0, 0, 8),
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 24, 1),
            Rect::new(0, 0, 3, 8),
        ] {
            let backend = TestBackend::new(frame.width, frame.height);
            let mut term = Terminal::new(backend).expect("terminal");
            term.draw(|f| render_overlay(f, degenerate, &overlay))
                .expect("draw");
            let buf = term.backend().buffer();
            let painted = (0..frame.height)
                .flat_map(|y| (0..frame.width).map(move |x| (x, y)))
                .any(|(x, y)| buf[(x, y)].symbol() != " ");
            assert!(
                painted,
                "an open modal must be visible somewhere in the frame, not silently \
                 skipped, for main rect {degenerate:?}"
            );
        }
    }

    /// And the ordinary case is unchanged: a main rect with room keeps the
    /// dialog inside it rather than over the sidebar.
    #[test]
    fn overlay_area_prefers_the_main_rect_when_it_has_room() {
        let frame = Rect::new(0, 0, 80, 24);
        let main = Rect::new(24, 1, 56, 23);
        assert_eq!(overlay_area(frame, main), main);
        assert_eq!(overlay_area(frame, Rect::new(24, 1, 0, 23)), frame);
        assert_eq!(overlay_area(frame, Rect::new(24, 1, 56, 2)), frame);
    }

    /// The sidebar drew from row 0 unconditionally, so a selection past the
    /// bottom of the column was simply never rendered. Pure, and every
    /// degenerate geometry a real terminal reaches is in here: the release
    /// profile is `panic = "abort"`, so an underflow takes the terminal down.
    #[test]
    fn the_sidebar_window_always_contains_the_selection() {
        // Everything fits: no scrolling at all.
        assert_eq!(sidebar_offset(3, 10, 0), 0);
        assert_eq!(sidebar_offset(3, 10, 2), 0);
        // A selection still inside the first window keeps it anchored at the
        // top rather than scrolling for no reason.
        assert_eq!(sidebar_offset(20, 5, 0), 0);
        assert_eq!(sidebar_offset(20, 5, 4), 0);
        // Past it, the window follows -- and the selection is inside it.
        for selected in 0..20 {
            let offset = sidebar_offset(20, 5, selected);
            assert!(
                (offset..offset + 5).contains(&selected),
                "row {selected} is outside the window starting at {offset}"
            );
            assert!(offset + 5 <= 20, "the window never runs past the last row");
        }
        // The last row is reachable and sits at the bottom of the window.
        assert_eq!(sidebar_offset(20, 5, 19), 15);
        // Degenerate geometries: no room, one row, an out-of-range selection.
        assert_eq!(sidebar_offset(20, 0, 19), 0);
        assert_eq!(sidebar_offset(0, 0, 0), 0);
        assert_eq!(sidebar_offset(1, 1, 0), 0);
        assert_eq!(sidebar_offset(20, 1, 19), 19);
        assert_eq!(
            sidebar_offset(3, 5, 99),
            0,
            "a selection past the end never scrolls a list that fits"
        );
    }

    #[test]
    fn a_long_session_list_scrolls_to_the_selected_row_and_says_it_is_a_window() {
        let row = |i: usize, selected: bool| SidebarRow {
            glyph: '\u{25cb}',
            title: String::new(),
            short: format!("sess{i:04}"),
            preview: String::new(),
            score: None,
            attached: true,
            selected,
            focused: false,
        };
        // Twelve rows into a six-row column: four fit inside the border.
        let rows: Vec<SidebarRow> = (0..12).map(|i| row(i, i == 10)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows)
        });
        assert!(
            text.contains("sess0010"),
            "the selected row must be on screen: {text}"
        );
        assert!(
            !text.contains("sess0000"),
            "and the window has scrolled off the top: {text}"
        );
        assert!(
            text.contains("8-11/12"),
            "the title says which window of the list this is: {text}"
        );

        // A selection at the top renders from the top, with no marker.
        let rows: Vec<SidebarRow> = (0..3).map(|i| row(i, i == 0)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows)
        });
        assert!(
            text.contains("sess0000") && text.contains("sess0002"),
            "{text}"
        );
        assert!(
            text.contains("panes") && !text.contains("/3"),
            "a list that fits keeps the bare title: {text}"
        );
    }

    /// A sidebar with no room for a single row -- a two-row terminal, or a
    /// `dash.sidebar_cols` wider than the terminal -- must draw nothing rather
    /// than underflow.
    #[test]
    fn the_sidebar_never_panics_on_a_column_with_no_room() {
        let rows: Vec<SidebarRow> = (0..5)
            .map(|i| SidebarRow {
                glyph: '\u{25cb}',
                title: "t".to_string(),
                short: format!("s{i}"),
                preview: String::new(),
                score: Some(42),
                attached: i % 2 == 0,
                selected: i == 4,
                focused: false,
            })
            .collect();
        let backend = TestBackend::new(30, 8);
        let mut term = Terminal::new(backend).expect("terminal");
        for area in [
            Rect::new(0, 0, 30, 0),
            Rect::new(0, 0, 30, 1),
            Rect::new(0, 0, 30, 2),
            Rect::new(0, 0, 30, 3),
            Rect::new(0, 0, 0, 8),
            Rect::new(0, 0, 1, 1),
        ] {
            term.draw(|f| render_sidebar(f, area, &rows)).expect("draw");
        }
        // And an empty list is fine too.
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &[]))
            .expect("draw");
    }

    fn scored_row(short: &str, score: Option<u32>) -> SidebarRow {
        SidebarRow {
            glyph: '\u{25cb}',
            title: "wrk claude".to_string(),
            short: short.to_string(),
            preview: "building".to_string(),
            score,
            attached: true,
            selected: false,
            focused: false,
        }
    }

    /// The per-instance readout: every row carries its own rot score, and an
    /// unscored one says `--` rather than claiming a healthy zero. `rot --` is
    /// the same width as `rot 42`, so the column stays aligned either way.
    #[test]
    fn a_sidebar_row_carries_its_own_rot_score() {
        let scored = sidebar_row_text(&scored_row("aaa11111", Some(34)), 200);
        assert_eq!(scored, " \u{25cb}  aaa11111 rot 34 wrk claude building");
        let unknown = sidebar_row_text(&scored_row("aaa11111", None), 200);
        assert_eq!(unknown, " \u{25cb}  aaa11111 rot -- wrk claude building");
        assert!(!unknown.contains("rot 0"), "unknown is not zero: {unknown}");
    }

    /// `dash.sidebar_cols` is configurable and a terminal can be narrower than
    /// it, so the row text has to survive any width -- including the 0..=3 that
    /// `Block::inner` saturates to nothing at. The release profile is
    /// `panic = "abort"`, so an underflow here takes the terminal with it.
    #[test]
    fn a_sidebar_row_degrades_gracefully_at_every_width() {
        // A multi-byte glyph at the front and a multi-byte title: truncation
        // is by character, so no width can split one.
        let mut row = scored_row("aaa11111", Some(7));
        row.title = "\u{4e2d}\u{6587} \u{2713} title".to_string();
        row.focused = true;
        for cols in 0..=64u16 {
            let text = sidebar_row_text(&row, cols);
            assert!(
                text.chars().count() <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
        assert_eq!(sidebar_row_text(&row, 0), "");
        assert_eq!(sidebar_row_text(&row, 1), "*");
        assert_eq!(sidebar_row_text(&row, 3), "*\u{25cb} ");
        // At the default `dash.sidebar_cols` of 24 (22 inside the border) the
        // identity and the score both survive; the title is what gets cut.
        // That ordering is the point of putting `rot` ahead of the title.
        let default_width = sidebar_row_text(&scored_row("aaa11111", Some(7)), 22);
        assert_eq!(default_width, " \u{25cb}  aaa11111 rot 7 wrk");
    }

    /// And the renderer itself never panics on those widths, with the
    /// scrolling window and the selection intact underneath.
    #[test]
    fn the_sidebar_renders_scores_at_every_column_width() {
        let mut rows: Vec<SidebarRow> = (0..8)
            .map(|i| scored_row(&format!("sess{i:04}"), (i % 2 == 0).then_some(i as u32)))
            .collect();
        rows[6].selected = true;
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        for width in 0..=40u16 {
            term.draw(|f| render_sidebar(f, Rect::new(0, 0, width, 6), &rows))
                .expect("draw");
        }
        // Wide enough to read: the scrolled-to selection is on screen with its
        // own score, and the window marker still says which slice this is.
        let text = render_and_capture_text(Rect::new(0, 0, 40, 6), |f, area| {
            render_sidebar(f, area, &rows)
        });
        assert!(text.contains("sess0006"), "got {text}");
        assert!(text.contains("rot 6"), "got {text}");
        assert!(text.contains("rot --"), "an unscored row shows --: {text}");
        assert!(text.contains("4-7/8"), "the scroll window marker: {text}");
    }

    /// F7: the sidebar cursor may walk onto a live session this dashboard did
    /// not spawn, but the keyboard cannot follow it there. Dimmed, so that
    /// reads as "not attached" instead of as arrow navigation having silently
    /// failed -- which is how it was reported.
    #[test]
    fn view_only_sidebar_rows_are_dimmed_so_an_unfocusable_row_looks_it() {
        let rows = vec![
            SidebarRow {
                glyph: '\u{25cb}',
                title: "chat claude".to_string(),
                short: "aaa11111".to_string(),
                preview: String::new(),
                score: Some(12),
                attached: true,
                selected: false,
                focused: true,
            },
            SidebarRow {
                glyph: '\u{25e6}',
                title: "wrap codex".to_string(),
                short: "bbb22222".to_string(),
                preview: String::new(),
                score: None,
                attached: false,
                selected: true,
                focused: false,
            },
        ];
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 6), &rows))
            .expect("draw");
        let buf = term.backend().buffer();
        // Row 0 of the list sits inside the block's own border.
        assert!(
            !buf[(2, 1)].modifier.contains(Modifier::DIM),
            "an attached pane row is drawn at full strength"
        );
        assert!(
            buf[(2, 2)].modifier.contains(Modifier::DIM),
            "a view-only row is dimmed"
        );
        assert!(
            buf[(2, 2)].modifier.contains(Modifier::REVERSED),
            "and still carries the selection highlight the cursor put on it"
        );
    }

    #[test]
    fn render_overlay_quit_confirm_lists_working_panes() {
        let overlay = Overlay::QuitConfirm(vec!["wrk claude".to_string(), "wrk codex".to_string()]);
        let area = Rect::new(0, 0, 40, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(text.contains("wrkclaude") || text.contains("wrk claude"));
    }

    #[test]
    fn mail_dialog_shows_the_selected_message_and_the_key_hints() {
        let view = MailView {
            items: vec![(
                PathBuf::from("/mail/1.md"),
                "claude".to_string(),
                "the webhook route moved".to_string(),
            )],
            cursor: 0,
            compose: None,
        };
        let overlay = Overlay::Mail(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(text.contains("claude"), "got {text}");
        assert!(
            text.contains("webhookroute") || text.contains("webhook route"),
            "got {text}"
        );
    }

    #[test]
    fn mail_dialog_shows_the_compose_draft_when_composing() {
        let view = MailView {
            items: Vec::new(),
            cursor: 0,
            compose: Some(ComposeDraft {
                to: String::new(),
                body: "heads up".to_string(),
            }),
        };
        let overlay = Overlay::Mail(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(
            text.contains("headsup") || text.contains("heads up"),
            "got {text}"
        );
    }

    #[test]
    fn memory_dialog_lists_entries_with_key_and_age() {
        let view = MemoryView {
            entries: vec![(
                "build-cmd".to_string(),
                "written 3d ago, verified 1d ago".to_string(),
                "cargo build --release".to_string(),
            )],
            cursor: 0,
            input: None,
        };
        let overlay = Overlay::Memory(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(text.contains("build-cmd"), "got {text}");
    }

    /// D1: the dialog names the pane by the same short id the nudge is
    /// resolved against at Enter time, not by a position that can shift.
    #[test]
    fn nudge_dialog_names_an_attached_pane_target() {
        let draft = NudgeDraft {
            target: NudgeTarget::AttachedPane("bbbb2222".to_string()),
            input: "hello".to_string(),
        };
        let overlay = Overlay::Nudge(draft);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(
            text.contains("panebbbb2222") || text.contains("pane bbbb2222"),
            "got {text}"
        );
    }

    #[test]
    fn restore_dialog_shows_checked_and_unchecked_entries() {
        let view = RestoreView {
            entries: vec![
                RestoreEntry {
                    label: "wrk claude (aaaa1111)".to_string(),
                    checked: true,
                },
                RestoreEntry {
                    label: "wrk codex (bbbb2222)".to_string(),
                    checked: false,
                },
            ],
            cursor: 0,
        };
        let overlay = Overlay::Restore(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(
            text.contains("[x]"),
            "checked entry missing its mark: {text}"
        );
        assert!(text.contains("[ ]"), "unchecked entry missing: {text}");
        assert!(
            text.contains("wrkclaude") || text.contains("wrk claude"),
            "got {text}"
        );
    }

    #[test]
    fn restore_dialog_on_an_empty_roster_says_so() {
        let overlay = Overlay::Restore(RestoreView::default());
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(
            text.contains("nothingtorestore") || text.contains("nothing to restore"),
            "got {text}"
        );
    }

    /// Every overlay variant, in a shape that has something to draw, so the
    /// degenerate-area tests below exercise the full rendering path rather
    /// than an early "nothing to show" branch.
    fn every_overlay() -> Vec<Overlay> {
        vec![
            Overlay::None,
            Overlay::QuitConfirm(vec!["wrk claude".to_string()]),
            Overlay::Spawn(SpawnDraft {
                input: "claude fix the tests".to_string(),
                items: vec!["claude".to_string()],
                cursor: 0,
            }),
            Overlay::Nudge(NudgeDraft {
                target: NudgeTarget::AttachedPane("aaaa1111".to_string()),
                input: "hello".to_string(),
            }),
            Overlay::Mail(MailView {
                items: vec![(
                    PathBuf::from("/mail/1.md"),
                    "claude".to_string(),
                    "a body".to_string(),
                )],
                cursor: 0,
                compose: Some(ComposeDraft {
                    to: "any".to_string(),
                    body: "drafting".to_string(),
                }),
            }),
            Overlay::Memory(MemoryView {
                entries: vec![("k".to_string(), "age".to_string(), "body".to_string())],
                cursor: 0,
                input: Some("typing".to_string()),
            }),
            Overlay::Restore(RestoreView {
                entries: vec![RestoreEntry {
                    label: "wrk claude (aaaa1111)".to_string(),
                    checked: true,
                }],
                cursor: 0,
            }),
        ]
    }

    /// F1: `dialog_width` used to be `saturating_sub(4).clamp(1, width)`,
    /// and `Ord::clamp` panics when `min > max` -- which is every zero-width
    /// area. A terminal narrowed to at most the sidebar width makes
    /// `layout`'s own main rect exactly that, and the release profile is
    /// `panic = "abort"`.
    #[test]
    fn dialog_width_never_panics_and_never_exceeds_the_area() {
        assert_eq!(dialog_width(0), 1);
        assert_eq!(dialog_width(1), 1);
        assert_eq!(dialog_width(4), 1);
        assert_eq!(dialog_width(5), 1);
        assert_eq!(dialog_width(40), 36);
        for w in 0..=64u16 {
            assert!(dialog_width(w) >= 1);
            assert!(dialog_width(w) <= w.max(1));
        }
    }

    #[test]
    fn every_overlay_renders_into_a_zero_sized_area_without_panicking() {
        let backend = TestBackend::new(20, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        for overlay in every_overlay() {
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 0, 0), &overlay))
                .expect("draw");
            // A zero-height-but-not-zero-width sliver, and its transpose:
            // both used to reach the same clamp.
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 20, 0), &overlay))
                .expect("draw");
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 0, 10), &overlay))
                .expect("draw");
        }
    }

    #[test]
    fn every_overlay_renders_into_a_one_by_one_area_without_panicking() {
        let backend = TestBackend::new(20, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        for overlay in every_overlay() {
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 1, 1), &overlay))
                .expect("draw");
        }
    }

    /// `tests/fixtures/claude-session.raw` is a gitignored capture of a real
    /// interactive session (see the vt100 spike,
    /// `docs/superpowers/notes/2026-08-13-vt100-spike.md`) -- present on the
    /// machine that recorded it, absent everywhere else including CI. Skips
    /// cleanly rather than failing when it is not there.
    #[test]
    fn renders_a_real_claude_session_fixture_without_panicking() {
        let path = std::path::Path::new("tests/fixtures/claude-session.raw");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!(
                "skipping renders_a_real_claude_session_fixture_without_panicking: {} not present",
                path.display()
            );
            return;
        };

        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(&bytes);

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen()))
            .unwrap();

        let buf = term.backend().buffer();
        let non_blank = (0..40).any(|y| (0..120).any(|x| buf[(x, y)].symbol() != " "));
        assert!(
            non_blank,
            "expected the real session fixture to render at least one non-blank cell"
        );
    }
}
