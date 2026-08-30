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
//!
//! Issue #202 phase 2b: the dashboard's own visual language -- one cyan
//! `zirv` brand chip, semantic colour reserved for state, rounded frames, a
//! live spinner for a working pane, two-tone key hints. Everything else in
//! this module (in particular [`render_grid`], which mirrors a live child
//! terminal's own colours verbatim) stays exactly as it was: the theme
//! migration is about the dashboard's own chrome, never about the harness
//! output it hosts.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph};

use crate::style;

use super::super::mail::Message;
use super::DashAction;
use super::pane::PaneState;

/// One enabled harness's cached subscription usage snapshot. No longer read
/// by the header itself (issue #202 phase 2b dropped the header's own usage
/// segment for width -- the header now has room only for the harness label,
/// the live count and the sticky error/notice line), but still filled by
/// `dash::mod`'s `FactsCache::refresh_if_due` each throttled tick and kept
/// here so that machinery -- and its own tests -- need no change; a future
/// surface (the errors overlay, a status line) can read it back without
/// re-deriving the read. `#[allow(dead_code)]`: every field is written by
/// `refresh_if_due` and read back by that machinery's own tests, but nothing
/// in the production render path reads one any more -- the same "landed
/// ahead of its next call site" situation `style.rs`'s own module-level
/// `#[allow(dead_code)]` documents for phase 1 of this issue.
#[derive(Clone)]
#[allow(dead_code)]
pub struct HarnessUsage {
    pub name: &'static str,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub credits: bool,
}

/// The header's live facts.
///
/// `harness` is the dashboard's own launch identity -- the agent plus any
/// `chat.model` disclosure (`chat.model` is repo-settable on the strength of
/// the choice staying visible; the dashboard's header is the one surface
/// that stays on screen for the whole session). It renders as the header's
/// one bold segment, standing in for the generic app label the rest of this
/// module's design otherwise uses.
///
/// `live`/`total` replace the old flat `sessions` count: `total` is every row
/// the sidebar draws (attached panes plus view-only registry rows this
/// dashboard owns), `live` is how many of those are not `Ended`/exited.
///
/// `error_count`/`latest_error` are the sticky `⚠` channel (`push_error`'s own
/// buffer); `notice` is the transient, auto-expiring informational channel
/// (`push_notice`/`live_notice`) and takes precedence over the error line
/// while it is fresh, exactly as it did before this phase.
pub struct HeaderFacts {
    pub harness: String,
    pub select_mode: bool,
    pub live: usize,
    pub total: usize,
    pub error_count: usize,
    pub latest_error: Option<String>,
    pub notice: Option<String>,
}

/// The sidebar/grid state a row's leading glyph column renders: [`render_
/// sidebar`] picks the actual glyph character (and colour) from this plus the
/// live spinner tick, so nothing above this module needs to know the spinner
/// frame set at all.
///
/// `Unknown` is a view-only registry row this dashboard did not spawn: its
/// `PaneState` (Working/Idle) genuinely is not observable from here, only
/// that the process is still alive -- so it gets its own neutral glyph
/// (`·`), distinct from every state a real pane can report, rather than
/// guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Working,
    Idle,
    Dead,
    Unknown,
}

/// Pure: the sidebar row state a pane's own `PaneState` maps to. `Ended`
/// (any exit code) is always `Dead` -- the exit code itself is not part of
/// the glyph, exactly as the old `glyph_for` never encoded it either.
pub fn row_state_for(state: &PaneState) -> RowState {
    match state {
        PaneState::Working => RowState::Working,
        PaneState::Idle => RowState::Idle,
        PaneState::Ended(_) => RowState::Dead,
    }
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
/// `age_secs` is `None` when no registry record could be found for this row
/// (a race between a fresh spawn and its own registration, in practice) --
/// it renders as [`style::PLACEHOLDER`], never a fabricated `0s`.
pub struct SidebarRow {
    pub short: String,
    pub harness: String,
    pub age_secs: Option<u64>,
    pub state: RowState,
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

/// Issue #84: the handover picker's own state. `items` is precomputed by
/// `dash::mod` (every enabled+ready harness crossed with `handover::TIERS`,
/// each already tier-resolved to a concrete model label) as `(agent, tier,
/// resolved model)`; `target_short` names the pane the swap applies to,
/// captured once when the overlay opens the same way `NudgeTarget` is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoverDraft {
    pub items: Vec<(String, String, String)>,
    pub cursor: usize,
    pub target_short: String,
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

/// `Ctrl+A e`'s own state: the kept errors from `push_error`'s buffer
/// (`MAX_KEPT_ERRORS`), newest first -- built once when the overlay opens
/// (`dash::mod::build_errors_view`), not re-read live while it is up, the
/// same snapshot-on-open convention every other overlay here already uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorsView {
    pub items: Vec<String>,
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
    /// Issue #84: `Ctrl+A o` -- picks an enabled harness/tier to swap the
    /// target pane's model or harness to in place.
    Handover(HandoverDraft),
    /// The startup restore dialog, built once from the previous quit's
    /// roster (`dash::mod::run_dashboard`); never re-opened later in a
    /// session's life the way the other overlays are.
    Restore(RestoreView),
    /// `Ctrl+A ?`/`h`/`H`: the key-binding reference. No payload -- its
    /// content is the static [`HELP_BINDINGS`] table, not per-session state,
    /// and any key closes it.
    Help,
    /// `Ctrl+A e`: the kept-errors overlay.
    Errors(ErrorsView),
}

/// Pure: how many rows the header gets in an `area_height`-row frame.
///
/// Exactly one -- the focused instance's own summary, which [`render_
/// header`] guarantees fits any width.
///
/// `min` rather than arithmetic: `area.height` is genuinely `0` in real
/// frames, and the release profile is `panic = "abort"`.
pub(crate) fn header_rows(area_height: u16) -> u16 {
    1.min(area_height)
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

/// The header's fixed right-hand hint cluster -- always the same two chords,
/// always `style::tui::hint()` (dim, not the two-tone key/action treatment
/// the dialog footers use): discoverability for the errors overlay and the
/// help overlay itself, the two things a header has no other room to name.
const HEADER_HINTS: &str = "^A e errors  ^A ? help";

/// Pure: the header's spans, width-budgeted to `cols`.
///
/// Ordered exactly as the design calls for: the ` zirv ` chip, the harness
/// label (bold, standing in for the generic app name -- see [`HeaderFacts`]'s
/// own doc comment for why this is `harness` rather than a literal "dash"),
/// the live/total count (muted), select-mode's own reminder when it is on,
/// then the one flexible segment -- the sticky error line or a transient
/// notice -- and finally the hint cluster, always kept, always on the right.
///
/// Only the flexible middle ever loses a character to width pressure: the
/// fixed chrome (chip, harness, live count, hints) is reserved first, and
/// what's left over is the message's own ellipsis-truncation budget. A
/// terminal narrower than the fixed chrome alone still never panics --
/// `render_header` hands whatever this returns to a `Paragraph`, which clips
/// to `area` on its own -- but is not going to look polished either; that
/// floor is well below `chrome::MIN_DASH_COLS` and is accepted the same way
/// the old header's own extreme-width tests were.
fn header_spans(facts: &HeaderFacts, cols: u16) -> Vec<Span<'static>> {
    let cols = cols as usize;

    let chip_text = " zirv ".to_string();
    let harness_text = format!(" {}", facts.harness);
    let live_text = format!(" \u{b7} {}/{} live", facts.live, facts.total);
    let select_text = if facts.select_mode {
        Some("  SELECT".to_string())
    } else {
        None
    };

    let mut left: Vec<(String, Style)> = vec![
        (chip_text, style::tui::chip()),
        (harness_text, style::tui::title()),
        (live_text, style::tui::muted()),
    ];
    if let Some(text) = select_text {
        left.push((text, style::tui::muted()));
    }
    let left_w: usize = left.iter().map(|(t, _)| style::display_width(t)).sum();

    let hints_w = style::display_width(HEADER_HINTS);
    let gap_before_hints = 2usize;
    let reserved = left_w + hints_w + gap_before_hints;
    let middle_budget = cols.saturating_sub(reserved);

    let mut middle: Vec<(String, Style)> = Vec::new();
    if facts.error_count > 0 {
        let prefix = format!("  \u{26a0} {} ", facts.error_count);
        let prefix_w = style::display_width(&prefix);
        if prefix_w <= middle_budget {
            middle.push((prefix.clone(), style::tui::error()));
            if let Some(msg) = &facts.latest_error {
                let msg_budget = middle_budget - prefix_w;
                let shown = style::truncate_display_ellipsis(msg, msg_budget);
                if !shown.is_empty() {
                    middle.push((shown.into_owned(), Style::default().fg(Color::Red)));
                }
            }
        }
    } else if let Some(note) = &facts.notice {
        let prefix = "  ".to_string();
        let prefix_w = style::display_width(&prefix);
        if prefix_w <= middle_budget {
            let msg_budget = middle_budget - prefix_w;
            let shown = style::truncate_display_ellipsis(note, msg_budget);
            if !shown.is_empty() {
                middle.push((prefix, Style::default()));
                middle.push((shown.into_owned(), style::tui::muted()));
            }
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (text, style) in left.into_iter().chain(middle) {
        used += style::display_width(&text);
        spans.push(Span::styled(text, style));
    }
    let pad = cols.saturating_sub(used + hints_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(HEADER_HINTS.to_string(), style::tui::hint()));
    spans
}

pub fn render_header(f: &mut Frame, area: Rect, facts: &HeaderFacts) {
    if area.is_empty() {
        return;
    }
    let spans = header_spans(facts, area.width);
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );
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

/// The glyph character (never coloured on its own -- see [`glyph_style_for`])
/// a sidebar row's state renders as. A working pane's own glyph advances one
/// [`style::tui::SPINNER_FRAMES`] frame per render tick.
fn glyph_char_for(state: RowState, tick: usize) -> &'static str {
    match state {
        RowState::Working => style::tui::SPINNER_FRAMES[tick % style::tui::SPINNER_FRAMES.len()],
        RowState::Idle => "\u{25cf}",
        RowState::Dead => "\u{2717}",
        RowState::Unknown => "\u{00b7}",
    }
}

/// The glyph's own colour: cyan for a working pane's spinner, green for a
/// live-but-idle one, red for exited/dead, and no colour at all (default
/// monochrome, matching every view-only row before this phase) for a row
/// whose state cannot be observed from here.
fn glyph_style_for(state: RowState) -> Style {
    match state {
        RowState::Working => style::tui::accent(),
        RowState::Idle => style::tui::ok(),
        RowState::Dead => style::tui::error(),
        RowState::Unknown => Style::default(),
    }
}

/// Pure: a sidebar row split into its glyph and the rest of the line
/// (`{short:<8} {harness}`, with the age right-aligned against whatever
/// column budget is left), both already fitted to `cols` display columns as
/// one unit. Shared by [`sidebar_row_text`] (the plain-text test/measurement
/// surface) and [`render_sidebar`] (the styled renderer), so the two can
/// never disagree about layout.
fn sidebar_row_parts(row: &SidebarRow, tick: usize, cols: u16) -> (String, String) {
    let cols = cols as usize;
    let glyph = glyph_char_for(row.state, tick).to_string();
    if cols == 0 {
        return (String::new(), String::new());
    }
    let glyph_w = style::display_width(&glyph);
    if cols <= glyph_w {
        return (
            style::truncate_display(&glyph, cols).into_owned(),
            String::new(),
        );
    }

    let rest_cols = cols - glyph_w - 1; // one separating space
    let left = format!("{:<8} {}", row.short, row.harness);
    let age = row
        .age_secs
        .map(style::format_age)
        .unwrap_or_else(|| style::PLACEHOLDER.to_string());
    let left_w = style::display_width(&left);
    let age_w = style::display_width(&age);

    let rest = if left_w + 1 + age_w <= rest_cols {
        let pad = rest_cols - left_w - age_w;
        format!("{left}{}{age}", " ".repeat(pad))
    } else {
        let truncated = style::truncate_display(&left, rest_cols).into_owned();
        let fill = rest_cols.saturating_sub(style::display_width(&truncated));
        format!("{truncated}{}", " ".repeat(fill))
    };
    (glyph, rest)
}

/// Pure: one sidebar row's full plain text, exactly `cols` display columns
/// wide (or shorter only when `cols` itself has no room for a full row) --
/// `{glyph} {short:<8} {harness} {age}`, age right-aligned. Built from the
/// same [`sidebar_row_parts`] the styled renderer uses, so the two can never
/// disagree about layout; test-only, since nothing in the render path needs
/// the unstyled concatenation.
#[cfg(test)]
fn sidebar_row_text(row: &SidebarRow, tick: usize, cols: u16) -> String {
    let (glyph, rest) = sidebar_row_parts(row, tick, cols);
    if rest.is_empty() {
        glyph
    } else {
        format!("{glyph} {rest}")
    }
}

pub fn render_sidebar(f: &mut Frame, area: Rect, rows: &[SidebarRow], tick: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
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
            let (glyph, rest) = sidebar_row_parts(row, tick, inner.width);
            let mut base = Style::default();
            // A view-only row is a live session in the registry that this
            // dashboard did not spawn: the sidebar cursor can walk onto it,
            // but the keyboard cannot follow (`dash::apply_navigation`). Dim,
            // on top of the neutral glyph, so that reads as "not attached"
            // rather than as arrow navigation having silently failed -- which
            // is how it was reported.
            if !row.attached {
                base = base.add_modifier(Modifier::DIM);
            }
            // The keyboard-focus marker used to be a literal `*` prefix
            // character; folded into the row's own weight instead so the
            // compact `{glyph} {short} {harness} {age}` format never needs a
            // fifth column for it. Combined with `selected` (REVERSED) this
            // is exactly `style::tui::selected_strong()`'s own shape.
            if row.focused {
                base = base.add_modifier(Modifier::BOLD);
            }
            let mut glyph_style = glyph_style_for(row.state).patch(base);
            let mut rest_style = base;
            if row.selected {
                glyph_style = glyph_style.add_modifier(Modifier::REVERSED);
                rest_style = rest_style.add_modifier(Modifier::REVERSED);
            }
            let line = Line::from(vec![
                Span::styled(glyph, glyph_style),
                Span::styled(format!(" {rest}"), rest_style),
            ]);
            ListItem::new(line)
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

/// Pure: whether visible-grid cell `(row, col)` falls inside a selection
/// already normalized to `start <= end` in row-major (row, then col) order
/// (`dash::normalize_selection` does the normalizing; this only ever sees the
/// result). Mirrors exactly the span `vt100::Screen::contents_between(
/// start.0, start.1, end.0, end.1)` copies, so the highlighted cells and the
/// copied text always agree: the whole of every row strictly between the
/// two, the tail of the start row from `start.1` onward, and the head of the
/// end row up to but not including `end.1` -- the same "up until end_col"
/// vt100 itself documents on `contents_between`.
fn cell_in_selection(row: u16, col: u16, start: (u16, u16), end: (u16, u16)) -> bool {
    if row < start.0 || row > end.0 {
        return false;
    }
    if start.0 == end.0 {
        return col >= start.1 && col < end.1;
    }
    if row == start.0 {
        col >= start.1
    } else if row == end.0 {
        col < end.1
    } else {
        true
    }
}

/// Walks every `vt100` cell in `screen` into `area`'s buffer, cell for cell.
/// A wide cell's own contents are drawn once and the following column is
/// skipped, matching how `vt100` itself represents double-width glyphs (the
/// continuation cell carries no contents of its own).
///
/// `selection`, when given, is a normalized `(start, end)` pair of
/// visible-grid `(row, col)` cells (`dash::mod`'s own click-drag selection,
/// already ordered by `normalize_selection`) drawn with `Modifier::REVERSED`
/// layered on top of the cell's own style -- the same tmux-style highlight a
/// terminal's native selection would have shown, now that mouse reporting
/// has displaced it (see `term::dash_mouse_on_bytes`).
///
/// Deliberately outside this phase's theme migration: this mirrors a live
/// child terminal's own colours and attributes verbatim, which is not
/// "dashboard chrome" in the sense the rest of this module's design language
/// applies to.
pub fn render_grid(
    f: &mut Frame,
    area: Rect,
    screen: &vt100::Screen,
    selection: Option<((u16, u16), (u16, u16))>,
) {
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
            if let Some((start, end)) = selection
                && cell_in_selection(row, col, start, end)
            {
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
        Paragraph::new(marker).style(style::tui::selected_strong()),
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

/// Splits a draft/input string on `\n` into one dialog-line entry per visual
/// row, so a multi-line draft renders one dialog row per visual line: the
/// first row keeps the `> ` prompt prefix, every continuation row gets a
/// two-space prefix that lines up under it.
fn draft_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("> {line}")
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

/// The plain (non-list) dialog frame: rounded, one column of horizontal
/// interior padding, opaque (`Clear` first, or the pane grid underneath
/// bleeds through every cell neither the border nor the text reaches -- see
/// the regression test below). Used directly by the two overlays the list
/// primitive does not fit (Nudge's free-text target+body, and a mail/memory
/// compose-or-edit buffer) so every dialog in the dashboard shares the same
/// frame treatment even where [`render_list_dialog`] itself is the wrong
/// shape.
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
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Span::styled(title.to_string(), style::tui::accent()));
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
    let mut lines = draft_lines(input);
    for (i, item) in items.iter().enumerate() {
        let marker = if i == cursor { '>' } else { ' ' };
        lines.push(format!("{marker} {item}"));
    }
    render_dialog(f, area, title, &lines);
}

/// Truncates a body preview to a single-line, human-scanning length, on
/// display width and with an ellipsis -- the display-width-aware equivalent
/// of the old byte/char-counting local `preview()`. Shared by the mail and
/// memory dialogs so a long message or entry never blows out a dialog row.
fn preview(text: &str, max_cols: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    style::truncate_display_ellipsis(first_line, max_cols).into_owned()
}

/// One list-dialog row: plain text, an optional leading colour glyph (the
/// QuitConfirm spinner, in practice -- nothing else in this phase needs
/// one), and an optional checkbox (`Some(_)` shows `[x]`/`[ ]`, `None` shows
/// neither -- most dialogs have no checkbox at all).
pub struct ListDialogRow {
    pub text: String,
    pub checked: Option<bool>,
    pub glyph: Option<(String, Style)>,
}

impl ListDialogRow {
    fn plain(text: String) -> Self {
        Self {
            text,
            checked: None,
            glyph: None,
        }
    }
}

/// The shared list-dialog primitive's own input: everything [`render_list_
/// dialog`] needs to draw one dialog, decoupled from which real overlay it
/// is drawing -- QuitConfirm, mail/memory browsing, restore, handover, and
/// the help overlay all build one of these rather than each hand-rolling
/// its own `Block`/`Paragraph` pair.
pub struct ListDialogSpec<'a> {
    pub title: &'a str,
    pub count: Option<usize>,
    pub rows: Vec<ListDialogRow>,
    /// The row rendered REVERSED across the full row width. `None` when
    /// nothing in the dialog is cursor-addressable (the help overlay, an
    /// empty list).
    pub cursor: Option<usize>,
    /// `(key, action)` pairs, rendered two-tone (key bold, action dim,
    /// three spaces between pairs) on the dialog's own last row, one blank
    /// row below the last list row.
    pub footer: &'a [(&'a str, &'a str)],
    /// The warn variant: border and title render in `style::tui::warning()`
    /// (yellow) instead of `style::tui::accent()` (cyan).
    pub warn: bool,
    /// Shown, cursor-less, in place of `rows` when it is empty -- "(no
    /// mail)", "(nothing to restore)", and the like.
    pub empty_message: &'a str,
}

/// The shared list-dialog primitive: a rounded, opaque, `Clear`-first frame
/// with one column of horizontal interior padding, a title (accent, or
/// warning-yellow for the warn variant) with an optional muted count beside
/// it, one row per `spec.rows` (the cursor row REVERSED across the full row
/// width), a blank row, and a two-tone footer.
///
/// Every dialog in the dashboard except Nudge's free-text prompt and a
/// mail/memory compose-or-edit buffer (see [`render_dialog`]'s own doc
/// comment) is built through this.
pub fn render_list_dialog(f: &mut Frame, area: Rect, spec: &ListDialogSpec) {
    if area.is_empty() {
        return;
    }

    let title_style = if spec.warn {
        style::tui::warning()
    } else {
        style::tui::accent()
    };
    let border_style = if spec.warn {
        style::tui::warning()
    } else {
        Style::default()
    };
    let title_text = match spec.count {
        Some(n) => format!("{} ({n})", spec.title),
        None => spec.title.to_string(),
    };

    let content_rows = spec.rows.len().max(1);
    // +1 blank row above the footer, +1 the footer row itself, +2 for the
    // block's own top/bottom border.
    let h = (content_rows as u16 + 2 + 2).min(area.height);
    let w = dialog_width(area.width);
    let rect = centered(area, w, h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .padding(Padding::horizontal(1))
        .title(Span::styled(title_text, title_style));

    f.render_widget(Clear, rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.is_empty() {
        return;
    }
    let inner_width = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    if spec.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            spec.empty_message.to_string(),
            style::tui::muted(),
        )));
    } else {
        for (i, row) in spec.rows.iter().enumerate() {
            let is_cursor = spec.cursor == Some(i);
            let mut spans: Vec<Span> = Vec::new();
            if let Some((glyph, glyph_style)) = &row.glyph {
                spans.push(Span::styled(format!("{glyph} "), *glyph_style));
            }
            if let Some(checked) = row.checked {
                spans.push(Span::raw(if checked { "[x] " } else { "[ ] " }));
            }
            spans.push(Span::raw(row.text.clone()));

            if is_cursor {
                let raw_width: usize = spans
                    .iter()
                    .map(|s| style::display_width(s.content.as_ref()))
                    .sum();
                let mut reversed: Vec<Span> = spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.add_modifier(Modifier::REVERSED)))
                    .collect();
                let pad = inner_width.saturating_sub(raw_width);
                if pad > 0 {
                    reversed.push(Span::styled(
                        " ".repeat(pad),
                        Style::default().add_modifier(Modifier::REVERSED),
                    ));
                }
                lines.push(Line::from(reversed));
            } else {
                lines.push(Line::from(spans));
            }
        }
    }

    lines.push(Line::from(""));
    let mut footer_spans: Vec<Span> = Vec::new();
    for (i, (key, action)) in spec.footer.iter().enumerate() {
        if i > 0 {
            footer_spans.push(Span::raw("   "));
        }
        footer_spans.push(Span::styled(
            key.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        footer_spans.push(Span::raw(" "));
        footer_spans.push(Span::styled(action.to_string(), style::tui::hint()));
    }
    lines.push(Line::from(footer_spans));

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_mail_dialog(f: &mut Frame, area: Rect, view: &MailView) {
    if let Some(draft) = &view.compose {
        let mut lines = vec![format!(
            "compose to: {}",
            if draft.to.trim().is_empty() {
                "any"
            } else {
                draft.to.as_str()
            }
        )];
        lines.extend(draft_lines(&draft.body));
        lines.push("Enter to send, Esc to cancel".to_string());
        render_dialog(f, area, "mail", &lines);
        return;
    }

    let rows: Vec<ListDialogRow> = view
        .items
        .iter()
        .map(|(_, from, body)| ListDialogRow::plain(format!("{from}: {}", preview(body, 60))))
        .collect();
    let cursor = if rows.is_empty() {
        None
    } else {
        Some(view.cursor)
    };
    render_list_dialog(
        f,
        area,
        &ListDialogSpec {
            title: "mail",
            count: Some(view.items.len()),
            rows,
            cursor,
            footer: &[
                ("\u{23ce}", "read+consume"),
                ("c", "compose"),
                ("esc", "close"),
            ],
            warn: false,
            empty_message: "(no mail)",
        },
    );
}

fn render_memory_dialog(f: &mut Frame, area: Rect, view: &MemoryView) {
    if let Some(input) = &view.input {
        let mut lines = draft_lines(input);
        lines.push("Enter to save, Esc to cancel".to_string());
        render_dialog(f, area, "memory", &lines);
        return;
    }

    let rows: Vec<ListDialogRow> = view
        .entries
        .iter()
        .map(|(key, age, body)| {
            ListDialogRow::plain(format!("{key} ({age}) {}", preview(body, 40)))
        })
        .collect();
    let cursor = if rows.is_empty() {
        None
    } else {
        Some(view.cursor)
    };
    render_list_dialog(
        f,
        area,
        &ListDialogSpec {
            title: "memory",
            count: Some(view.entries.len()),
            rows,
            cursor,
            footer: &[
                ("r", "remember"),
                ("d", "forget"),
                ("v", "verify"),
                ("esc", "close"),
            ],
            warn: false,
            empty_message: "(no memory entries)",
        },
    );
}

fn render_restore_dialog(f: &mut Frame, area: Rect, view: &RestoreView) {
    let rows: Vec<ListDialogRow> = view
        .entries
        .iter()
        .map(|entry| ListDialogRow {
            text: entry.label.clone(),
            checked: Some(entry.checked),
            glyph: None,
        })
        .collect();
    let cursor = if rows.is_empty() {
        None
    } else {
        Some(view.cursor)
    };
    render_list_dialog(
        f,
        area,
        &ListDialogSpec {
            title: "restore",
            count: Some(view.entries.len()),
            rows,
            cursor,
            footer: &[
                ("space", "toggle"),
                ("\u{23ce}", "restore checked"),
                ("esc", "skip"),
            ],
            warn: false,
            empty_message: "(nothing to restore)",
        },
    );
}

/// Issue #84: `draft.items` is already fully resolved (agent/tier/model), so
/// this only ever formats and marks the cursor row -- no tier resolution or
/// config reads happen here, matching this module's own no-I/O contract.
/// The swap's target pane goes in the title (`handover -> pane {short}`)
/// rather than a trailing body row, so it stays visible even once the item
/// list scrolls.
fn render_handover_dialog(f: &mut Frame, area: Rect, draft: &HandoverDraft) {
    let title = format!("handover \u{2192} pane {}", draft.target_short);
    let rows: Vec<ListDialogRow> = draft
        .items
        .iter()
        .map(|(agent, tier, model)| ListDialogRow::plain(format!("{agent} / {tier} ({model})")))
        .collect();
    let cursor = if rows.is_empty() {
        None
    } else {
        Some(draft.cursor)
    };
    render_list_dialog(
        f,
        area,
        &ListDialogSpec {
            title: &title,
            count: None,
            rows,
            cursor,
            footer: &[("\u{23ce}", "swap"), ("esc", "cancel")],
            warn: false,
            empty_message: "no enabled, ready harness available to swap to",
        },
    );
}

fn render_nudge_dialog(f: &mut Frame, area: Rect, draft: &NudgeDraft) {
    let target = match &draft.target {
        NudgeTarget::AttachedPane(short) => format!("pane {short}"),
        NudgeTarget::ViewOnlySession(short) => format!("session {short} (headless)"),
        NudgeTarget::None => "(no target selected)".to_string(),
    };
    let mut lines = vec![format!("to: {target}")];
    lines.extend(draft_lines(&draft.input));
    lines.push("Enter to send, Esc to cancel".to_string());
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
        Paragraph::new(message.to_string()).style(style::tui::title()),
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

/// Which section of the help overlay a row belongs to -- `help_lines` groups
/// by this so the dialog says outright which keys need `Ctrl+A` first and
/// which don't, rather than one flat list that never mentions the prefix.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpSection {
    /// Needs the `Ctrl+A` prefix first.
    Prefixed,
    /// No prefix -- reaches the dashboard directly.
    Unprefixed,
    /// Not a keybinding: a closing reminder for whatever dialog is open.
    Note,
}

/// One row of the `Ctrl+A ?` help overlay: what is shown, and -- for a row
/// that is itself one or more real armed keystrokes -- the `filter_key`
/// outcome each one must produce. `checks` is the single source of truth a
/// sync test walks against the real dispatch, so this table can never drift
/// from what `Ctrl+A <key>` actually does; it is empty for a row that is not
/// one `filter_key` outcome (the unprefixed mouse wheel, or the closing note).
struct HelpBinding {
    label: &'static str,
    description: &'static str,
    section: HelpSection,
    // Only the sync tests below read this outside `#[cfg(test)]`, which the
    // plain bin target does not build.
    #[allow(dead_code)]
    checks: &'static [(KeyEvent, DashAction)],
}

/// The help overlay's content, in display order -- see [`HelpBinding`]. A
/// `static`, not a function rebuilding a `Vec` on every call: `render_overlay`
/// reaches this on every frame, which during the adaptive poll's hot window
/// (`input_poll_wait`) is up to ~100/s. `KeyEvent::new` is `const fn` in
/// crossterm 0.29, so the whole table is built once at compile time.
static HELP_BINDINGS: &[HelpBinding] = &[
    HelpBinding {
        label: "Ctrl+A",
        description: "send a literal Ctrl+A",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            DashAction::LiteralPrefix,
        )],
    },
    HelpBinding {
        label: "Tab",
        description: "next pane",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            DashAction::NextPane,
        )],
    },
    HelpBinding {
        label: "Up / Down",
        description: "select pane",
        section: HelpSection::Prefixed,
        checks: &[
            (
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                DashAction::SelectUp,
            ),
            (
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                DashAction::SelectDown,
            ),
        ],
    },
    HelpBinding {
        label: "1-9",
        description: "jump to pane",
        section: HelpSection::Prefixed,
        checks: &[
            (
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
                DashAction::Switch(0),
            ),
            (
                KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
                DashAction::Switch(8),
            ),
        ],
    },
    HelpBinding {
        label: "PageUp / PageDown",
        description: "scroll",
        section: HelpSection::Prefixed,
        checks: &[
            (
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                DashAction::ScrollPageUp,
            ),
            (
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                DashAction::ScrollPageDown,
            ),
        ],
    },
    HelpBinding {
        label: "Home / End",
        description: "scroll top / live",
        section: HelpSection::Prefixed,
        checks: &[
            (
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                DashAction::ScrollTop,
            ),
            (
                KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                DashAction::ScrollLive,
            ),
        ],
    },
    HelpBinding {
        label: "s",
        description: "spawn",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            DashAction::Spawn,
        )],
    },
    HelpBinding {
        label: "n",
        description: "nudge",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            DashAction::Nudge,
        )],
    },
    HelpBinding {
        label: "m",
        description: "mail",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            DashAction::Mail,
        )],
    },
    HelpBinding {
        label: "M",
        description: "memory",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE),
            DashAction::Memory,
        )],
    },
    HelpBinding {
        label: "o",
        description: "handover (swap model/harness)",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            DashAction::Handover,
        )],
    },
    HelpBinding {
        label: "e",
        description: "recent errors",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            DashAction::ShowErrors,
        )],
    },
    HelpBinding {
        label: "z",
        description: "zoom",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            DashAction::Zoom,
        )],
    },
    HelpBinding {
        label: "v",
        description: "toggle text selection",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            DashAction::ToggleSelectMode,
        )],
    },
    HelpBinding {
        label: "q",
        description: "quit",
        section: HelpSection::Prefixed,
        checks: &[(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            DashAction::Quit,
        )],
    },
    HelpBinding {
        label: "? / h",
        description: "this help screen",
        section: HelpSection::Prefixed,
        // A real terminal delivers SHIFT alongside both '?' (shift-slash on
        // most layouts) and 'H' itself; `filter_key` matches on `key.code`
        // alone, so all five must (and do) land on the same action.
        checks: &[
            (
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
                DashAction::Help,
            ),
            (
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                DashAction::Help,
            ),
            (
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
                DashAction::Help,
            ),
            (
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE),
                DashAction::Help,
            ),
            (
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
                DashAction::Help,
            ),
        ],
    },
    HelpBinding {
        label: "(mouse wheel)",
        description: "scroll the focused pane",
        section: HelpSection::Unprefixed,
        checks: &[],
    },
    HelpBinding {
        label: "",
        description: "Esc closes, Enter confirms",
        section: HelpSection::Note,
        checks: &[],
    },
];

/// Every other dialog's own footer hints, named once here and reused both to
/// build that dialog's [`ListDialogSpec`] and to list it in the help
/// overlay's own "dialogs:" section (`help_dialog_rows`) -- a static table
/// kept adjacent to the specs, per the phase's own design note, rather than
/// deriving it at runtime from a spec nothing keeps around between frames.
const QUIT_FOOTER: &[(&str, &str)] = &[("\u{23ce}", "quit and shut down"), ("esc", "stay")];
const MAIL_FOOTER: &[(&str, &str)] = &[
    ("\u{23ce}", "read+consume"),
    ("c", "compose"),
    ("esc", "close"),
];
const MEMORY_FOOTER: &[(&str, &str)] = &[
    ("r", "remember"),
    ("d", "forget"),
    ("v", "verify"),
    ("esc", "close"),
];
const RESTORE_FOOTER: &[(&str, &str)] = &[
    ("space", "toggle"),
    ("\u{23ce}", "restore checked"),
    ("esc", "skip"),
];
const HANDOVER_FOOTER: &[(&str, &str)] = &[("\u{23ce}", "swap"), ("esc", "cancel")];
const ERRORS_FOOTER: &[(&str, &str)] = &[("j/k", "scroll"), ("esc/q", "close")];

const DIALOG_FOOTERS: &[(&str, &[(&str, &str)])] = &[
    ("quit", QUIT_FOOTER),
    ("mail", MAIL_FOOTER),
    ("memory", MEMORY_FOOTER),
    ("restore", RESTORE_FOOTER),
    ("handover", HANDOVER_FOOTER),
    ("errors", ERRORS_FOOTER),
];

/// Pure: the help overlay's rows, grouped by [`HelpSection`] so the dialog
/// reads as "here's what's behind Ctrl+A", then "here's what needs no
/// prefix", then which key closes/confirms whatever dialog is open, then a
/// "dialogs:" section listing every other dialog's own footer hints
/// ([`DIALOG_FOOTERS`]) -- see [`HELP_BINDINGS`].
fn help_lines() -> Vec<String> {
    let row = |b: &HelpBinding| format!("{:<18} {}", b.label, b.description);
    let mut lines = vec!["Ctrl+A, then:".to_string()];
    lines.extend(
        HELP_BINDINGS
            .iter()
            .filter(|b| b.section == HelpSection::Prefixed)
            .map(row),
    );
    lines.push(String::new());
    lines.push("no prefix:".to_string());
    lines.extend(
        HELP_BINDINGS
            .iter()
            .filter(|b| b.section == HelpSection::Unprefixed)
            .map(row),
    );
    lines.push(String::new());
    lines.extend(
        HELP_BINDINGS
            .iter()
            .filter(|b| b.section == HelpSection::Note)
            .map(|b| b.description.to_string()),
    );
    lines.push(String::new());
    lines.push("dialogs:".to_string());
    for (name, footer) in DIALOG_FOOTERS {
        let hints: Vec<String> = footer
            .iter()
            .map(|(key, action)| format!("{key} {action}"))
            .collect();
        // A single-space separator (rather than the dialogs' own two-space
        // footer spacing) to fit the 80-column budget every help line holds
        // itself to -- see `every_help_line_fits_an_eighty_column_terminal_
        // with_the_default_sidebar`.
        lines.push(format!("{:<8} {}", name, hints.join(" ")));
    }
    lines
}

pub fn render_overlay(f: &mut Frame, area: Rect, overlay: &Overlay, tick: usize) {
    // Never an early return on a too-small `area`: see `overlay_area`. Only a
    // frame with no cells at all leaves nothing to draw into, and
    // `render_dialog`/`render_list_dialog`'s own guards cover that.
    let area = overlay_area(f.area(), area);
    match overlay {
        Overlay::None => {}
        Overlay::QuitConfirm(working) => {
            let rows: Vec<ListDialogRow> = working
                .iter()
                .map(|title| ListDialogRow {
                    text: title.clone(),
                    checked: None,
                    glyph: Some((
                        style::tui::SPINNER_FRAMES[tick % style::tui::SPINNER_FRAMES.len()]
                            .to_string(),
                        style::tui::accent(),
                    )),
                })
                .collect();
            // No cursor: nothing here is keyboard-navigable (there is no
            // j/k on this dialog, only Enter/Esc), so reversing a row would
            // read as a selection that does not exist.
            render_list_dialog(
                f,
                area,
                &ListDialogSpec {
                    title: "\u{26a0} quit zirv dash",
                    count: Some(working.len()),
                    rows,
                    cursor: None,
                    footer: QUIT_FOOTER,
                    warn: true,
                    empty_message: "nothing is still working",
                },
            );
        }
        Overlay::Spawn(d) => render_draft_dialog(f, area, "spawn", &d.input, &d.items, d.cursor),
        Overlay::Nudge(d) => render_nudge_dialog(f, area, d),
        Overlay::Handover(d) => render_handover_dialog(f, area, d),
        Overlay::Mail(d) => render_mail_dialog(f, area, d),
        Overlay::Memory(d) => render_memory_dialog(f, area, d),
        Overlay::Restore(d) => render_restore_dialog(f, area, d),
        Overlay::Help => {
            let rows: Vec<ListDialogRow> =
                help_lines().into_iter().map(ListDialogRow::plain).collect();
            render_list_dialog(
                f,
                area,
                &ListDialogSpec {
                    title: "help",
                    count: None,
                    rows,
                    cursor: None,
                    footer: &[("any key", "close")],
                    warn: false,
                    empty_message: "",
                },
            );
        }
        Overlay::Errors(view) => {
            let rows: Vec<ListDialogRow> = view
                .items
                .iter()
                .map(|msg| ListDialogRow::plain(format!("\u{26a0} {msg}")))
                .collect();
            let cursor = if rows.is_empty() {
                None
            } else {
                Some(view.cursor)
            };
            render_list_dialog(
                f,
                area,
                &ListDialogSpec {
                    title: "errors",
                    count: Some(view.items.len()),
                    rows,
                    cursor,
                    footer: ERRORS_FOOTER,
                    warn: false,
                    empty_message: "no recent errors",
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{InputVerdict, filter_key};
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
            render_grid(f, area, parser.screen(), None);
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

    /// The list-dialog primitive must be just as opaque as the plain one.
    #[test]
    fn a_list_dialog_is_opaque_and_never_lets_the_pane_bleed_through() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        for _ in 0..10 {
            parser.process(&[b'X'; 40]);
        }
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        let spec = ListDialogSpec {
            title: "mail",
            count: Some(1),
            rows: vec![ListDialogRow::plain("claude: hi".to_string())],
            cursor: Some(0),
            footer: MAIL_FOOTER,
            warn: false,
            empty_message: "(no mail)",
        };
        term.draw(|f| {
            let area = f.area();
            render_grid(f, area, parser.screen(), None);
            render_list_dialog(f, area, &spec);
        })
        .expect("draw");

        let buf = term.backend().buffer();
        let w = dialog_width(40);
        let rect = centered(Rect::new(0, 0, 40, 10), w, 5);
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "X",
                    "the pane grid bled through the dialog at ({x},{y})"
                );
            }
        }
        assert_eq!(buf[(0, 0)].symbol(), "X");
    }

    #[test]
    fn grid_renders_vt100_cells_with_colours() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m plain");
        let backend = TestBackend::new(20, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
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
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
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

    /// `cell_in_selection` mirrors `vt100::Screen::contents_between`'s own
    /// span exactly, cell for cell: the tail of the start row, every column
    /// of a middle row, and the head of the end row up to but not including
    /// `end.1`. Pinned against a multi-row selection first.
    #[test]
    fn cell_in_selection_spans_a_multi_row_selection_like_contents_between() {
        let start = (1, 5);
        let end = (3, 2);

        // Row above the selection: nothing is selected.
        assert!(!cell_in_selection(0, 0, start, end));
        assert!(!cell_in_selection(0, 40, start, end));

        // Start row: only from col 5 onward.
        assert!(!cell_in_selection(1, 4, start, end));
        assert!(cell_in_selection(1, 5, start, end));
        assert!(cell_in_selection(1, 999, start, end));

        // A middle row: every column, including 0.
        assert!(cell_in_selection(2, 0, start, end));
        assert!(cell_in_selection(2, 999, start, end));

        // End row: only up to (not including) col 2.
        assert!(cell_in_selection(3, 0, start, end));
        assert!(cell_in_selection(3, 1, start, end));
        assert!(!cell_in_selection(3, 2, start, end));
        assert!(!cell_in_selection(3, 40, start, end));

        // Row below the selection: nothing is selected.
        assert!(!cell_in_selection(4, 0, start, end));
    }

    /// A single-row selection is the half-open `[start.1, end.1)` range on
    /// that one row and nothing on any other row -- the `Ordering::Equal`
    /// branch `contents_between` itself takes.
    #[test]
    fn cell_in_selection_on_a_single_row_is_the_half_open_column_range() {
        let start = (2, 3);
        let end = (2, 7);

        assert!(!cell_in_selection(2, 2, start, end));
        assert!(cell_in_selection(2, 3, start, end));
        assert!(cell_in_selection(2, 6, start, end));
        assert!(!cell_in_selection(2, 7, start, end), "end col is exclusive");

        // Same row, but outside the selection's own row range.
        assert!(!cell_in_selection(1, 5, start, end));
        assert!(!cell_in_selection(3, 5, start, end));

        // A zero-width same-row "selection" (anchor == end, i.e. a click
        // rather than a drag) selects nothing at all.
        assert!(!cell_in_selection(2, 3, (2, 3), (2, 3)));
    }

    /// `render_grid` layers `Modifier::REVERSED` onto exactly the cells
    /// `cell_in_selection` reports, leaving every other cell's style alone.
    #[test]
    fn render_grid_highlights_only_the_selected_cells() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc");
        let backend = TestBackend::new(10, 3);
        let mut term = Terminal::new(backend).unwrap();
        // Row 1, cols 2..5 selected (single row).
        term.draw(|f| render_grid(f, f.area(), parser.screen(), Some(((1, 2), (1, 5)))))
            .unwrap();
        let buf = term.backend().buffer();
        assert!(!buf[(1, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(4, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(5, 1)].modifier.contains(Modifier::REVERSED));
        // Untouched rows carry no highlight at all.
        assert!(!buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(2, 2)].modifier.contains(Modifier::REVERSED));
    }

    /// No selection at all leaves every cell exactly as it was rendered
    /// before this feature existed -- the `None` default every other caller
    /// still passes.
    #[test]
    fn render_grid_with_no_selection_reverses_nothing_the_cell_itself_did_not_ask_for() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"plain text");
        let backend = TestBackend::new(10, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
            .unwrap();
        let buf = term.backend().buffer();
        for x in 0..10 {
            assert!(!buf[(x, 0)].modifier.contains(Modifier::REVERSED));
        }
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
        assert_eq!(h.height, 1, "one header row at every height");
        assert_eq!(s.width, 24);
        assert_eq!(m.width, 76 - 1);
        assert_eq!(m.height, 29);
    }

    #[test]
    fn layout_is_stable_on_a_tiny_area_without_underflow() {
        let (h, s, m) = layout(Rect::new(0, 0, 10, 2), 24);
        assert_eq!(h.height, 1);
        assert_eq!(s.width, 10);
        assert_eq!(m.width, 0);
        assert_eq!(m.height, 1);
    }

    /// The header is one row at every height. A zero/one-row frame never
    /// underflows on the way there (the release profile is `panic =
    /// "abort"`).
    #[test]
    fn the_header_is_one_row_at_every_terminal_height() {
        assert_eq!(header_rows(0), 0);
        assert_eq!(header_rows(1), 1);
        assert_eq!(header_rows(2), 1);
        assert_eq!(
            header_rows(super::super::super::chrome::MIN_DASH_ROWS - 1),
            1
        );
        assert_eq!(header_rows(super::super::super::chrome::MIN_DASH_ROWS), 1);
        assert_eq!(header_rows(200), 1);
        // And the body never loses more rows than the header gained.
        for height in 0..=64u16 {
            let (h, _, m) = layout(Rect::new(0, 0, 80, height), 24);
            assert_eq!(h.height + m.height, height, "height {height} lost a row");
        }
    }

    fn base_facts() -> HeaderFacts {
        HeaderFacts {
            harness: "claude".to_string(),
            select_mode: false,
            live: 1,
            total: 1,
            error_count: 0,
            latest_error: None,
            notice: None,
        }
    }

    #[test]
    fn header_shows_the_brand_chip_and_the_harness_label() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("zirv"), "chip text missing: {text}");
        assert!(text.contains("claude"), "harness label missing: {text}");
    }

    #[test]
    fn header_shows_the_live_over_total_count() {
        let mut facts = base_facts();
        facts.live = 2;
        facts.total = 5;
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(
            text.contains("2/5live") || text.contains("2/5 live"),
            "got {text}"
        );
    }

    #[test]
    fn header_shows_the_errors_hint_and_help_hint() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("errors"), "got {text}");
        assert!(text.contains("help"), "got {text}");
    }

    /// `Ctrl+A v`'s own state renders a visible reminder while it is on, since
    /// it changes how every click and wheel notch behaves until toggled back.
    #[test]
    fn header_shows_select_mode_only_while_active() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(!text.contains("SELECT"), "header text was: {text}");

        let mut facts = base_facts();
        facts.select_mode = true;
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("SELECT"), "header text was: {text}");
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

    /// When errors exist, the header shows the count and the latest message
    /// in red, and it takes precedence over a stale-but-not-yet-expired
    /// notice's own text no longer being relevant -- `HeaderFacts` itself
    /// encodes the precedence (the caller only ever fills one of the two).
    #[test]
    fn header_shows_the_error_count_and_latest_message() {
        let mut facts = base_facts();
        facts.error_count = 3;
        facts.latest_error = Some("mail send: disk full".to_string());
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("3"), "got {text}");
        assert!(
            text.contains("disksfull") || text.contains("disk full"),
            "got {text}"
        );
    }

    #[test]
    fn header_shows_no_error_segment_when_there_are_no_errors() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(!text.contains('\u{26a0}'), "got {text}");
    }

    /// A non-error notice shows muted, the same as before this phase.
    #[test]
    fn header_shows_a_notice_when_there_are_no_errors() {
        let mut facts = base_facts();
        facts.notice = Some("spawned claude as wrk-2".to_string());
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(
            text.contains("spawnedclaude") || text.contains("spawned claude"),
            "got {text}"
        );
    }

    /// A very short terminal: the header renders its one line and nothing
    /// else, and every degenerate rect a resize can produce is a no-op rather
    /// than a panic.
    #[test]
    fn the_header_never_panics_on_a_degenerate_area() {
        let facts = base_facts();
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
        // And every width from 0 to 200 columns, including the pathological
        // ones the flexible-middle budget cannot make room for.
        for w in 0..=200u16 {
            term.draw(|f| render_header(f, Rect::new(0, 0, w.min(80), 1), &facts))
                .expect("draw");
        }
    }

    /// The header draws its one line and nothing below it: a taller header
    /// rect (which `layout` no longer produces, but a caller could still pass)
    /// must not paint over the sidebar's own top border.
    #[test]
    fn the_header_draws_one_row_and_never_a_second() {
        let facts = base_facts();
        for height in [1u16, 2, 3] {
            let text = render_and_capture_text(Rect::new(0, 0, 80, height), |f, area| {
                render_header(f, area, &facts)
            });
            assert!(text.contains("zirv"), "got {text}");
            let below: String = text.chars().skip(80).collect();
            assert!(
                below.trim().is_empty(),
                "height {height} painted below its first row: {below:?}"
            );
        }
    }

    fn sidebar_row(short: &str, harness: &str, state: RowState) -> SidebarRow {
        SidebarRow {
            short: short.to_string(),
            harness: harness.to_string(),
            age_secs: Some(90),
            state,
            attached: true,
            selected: false,
            focused: false,
        }
    }

    #[test]
    fn row_state_matches_pane_state() {
        assert_eq!(row_state_for(&PaneState::Working), RowState::Working);
        assert_eq!(row_state_for(&PaneState::Idle), RowState::Idle);
        assert_eq!(row_state_for(&PaneState::Ended(0)), RowState::Dead);
        assert_eq!(row_state_for(&PaneState::Ended(1)), RowState::Dead);
    }

    #[test]
    fn glyph_char_matches_the_spec_per_state() {
        assert_eq!(glyph_char_for(RowState::Idle, 0), "\u{25cf}");
        assert_eq!(glyph_char_for(RowState::Dead, 0), "\u{2717}");
        assert_eq!(glyph_char_for(RowState::Unknown, 0), "\u{00b7}");
    }

    /// The working glyph advances one spinner frame per tick, wrapping back
    /// to the first frame once the cycle completes.
    #[test]
    fn spinner_glyph_advances_with_the_tick_and_wraps() {
        let frames = style::tui::SPINNER_FRAMES;
        for tick in 0..frames.len() * 2 {
            assert_eq!(
                glyph_char_for(RowState::Working, tick),
                frames[tick % frames.len()]
            );
        }
        assert_eq!(
            glyph_char_for(RowState::Working, 0),
            glyph_char_for(RowState::Working, frames.len())
        );
    }

    #[test]
    fn a_sidebar_row_renders_short_harness_and_age() {
        let row = sidebar_row("aaa11111", "claude", RowState::Idle);
        let text = sidebar_row_text(&row, 0, 200);
        assert!(text.contains("aaa11111"), "got {text}");
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("1m"), "got {text}");
        assert!(text.starts_with('\u{25cf}'), "got {text}");
    }

    /// Unknown age (no matching registry record at the moment this row was
    /// built) renders as the shared placeholder, never a fabricated `0s`.
    #[test]
    fn a_sidebar_row_with_unknown_age_shows_the_placeholder() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.age_secs = None;
        let text = sidebar_row_text(&row, 0, 200);
        assert!(text.contains(style::PLACEHOLDER), "got {text}");
    }

    /// `dash.sidebar_cols` is configurable and a terminal can be narrower than
    /// it, so the row text has to survive any width -- including the 0..=3
    /// that `Block::inner` saturates to nothing at. The release profile is
    /// `panic = "abort"`, so an underflow here takes the terminal with it.
    #[test]
    fn a_sidebar_row_degrades_gracefully_at_every_width() {
        let row = sidebar_row("aaa11111", "claude", RowState::Working);
        for cols in 0..=64u16 {
            let text = sidebar_row_text(&row, 3, cols);
            assert!(
                style::display_width(&text) <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
        assert_eq!(sidebar_row_text(&row, 0, 0), "");
    }

    /// And the renderer itself never panics on those widths, with the
    /// scrolling window and the selection intact underneath.
    #[test]
    fn the_sidebar_renders_rows_at_every_column_width() {
        let mut rows: Vec<SidebarRow> = (0..8)
            .map(|i| {
                sidebar_row(
                    &format!("sess{i:04}"),
                    "claude",
                    if i % 2 == 0 {
                        RowState::Working
                    } else {
                        RowState::Idle
                    },
                )
            })
            .collect();
        rows[6].selected = true;
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        for width in 0..=40u16 {
            term.draw(|f| render_sidebar(f, Rect::new(0, 0, width, 6), &rows, 0))
                .expect("draw");
        }
        let text = render_and_capture_text(Rect::new(0, 0, 40, 6), |f, area| {
            render_sidebar(f, area, &rows, 0)
        });
        assert!(text.contains("sess0006"), "got {text}");
    }

    /// F7: the sidebar cursor may walk onto a live session this dashboard did
    /// not spawn, but the keyboard cannot follow it there. Dimmed, so that
    /// reads as "not attached" instead of as arrow navigation having silently
    /// failed -- which is how it was reported.
    #[test]
    fn view_only_sidebar_rows_are_dimmed_so_an_unfocusable_row_looks_it() {
        let rows = vec![
            SidebarRow {
                short: "aaa11111".to_string(),
                harness: "claude".to_string(),
                age_secs: Some(5),
                state: RowState::Working,
                attached: true,
                selected: false,
                focused: true,
            },
            SidebarRow {
                short: "bbb22222".to_string(),
                harness: "codex".to_string(),
                age_secs: Some(5),
                state: RowState::Unknown,
                attached: false,
                selected: true,
                focused: false,
            },
        ];
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 6), &rows, 0))
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

    /// A long session list scrolls the window to keep the selected row on
    /// screen, and says which window of the list this is.
    #[test]
    fn a_long_session_list_scrolls_to_the_selected_row_and_says_it_is_a_window() {
        let row = |i: usize, selected: bool| SidebarRow {
            short: format!("sess{i:04}"),
            harness: String::new(),
            age_secs: None,
            state: RowState::Idle,
            attached: true,
            selected,
            focused: false,
        };
        let rows: Vec<SidebarRow> = (0..12).map(|i| row(i, i == 10)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows, 0)
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

        let rows: Vec<SidebarRow> = (0..3).map(|i| row(i, i == 0)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows, 0)
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
            .map(|i| sidebar_row(&format!("s{i}"), "t", RowState::Idle))
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
            term.draw(|f| render_sidebar(f, area, &rows, 0))
                .expect("draw");
        }
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &[], 0))
            .expect("draw");
    }

    #[test]
    fn render_overlay_quit_confirm_lists_working_panes() {
        let overlay = Overlay::QuitConfirm(vec!["wrk claude".to_string(), "wrk codex".to_string()]);
        let area = Rect::new(0, 0, 40, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
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
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
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
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
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
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
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
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
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
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
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
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("nothingtorestore") || text.contains("nothing to restore"),
            "got {text}"
        );
    }

    #[test]
    fn errors_overlay_lists_kept_errors_newest_first_with_a_warning_glyph() {
        let view = ErrorsView {
            items: vec![
                "mail send: disk full".to_string(),
                "handover: timed out".to_string(),
            ],
            cursor: 0,
        };
        let overlay = Overlay::Errors(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(text.contains('\u{26a0}'), "got {text}");
        assert!(
            text.contains("mailsend") || text.contains("mail send"),
            "got {text}"
        );
    }

    #[test]
    fn errors_overlay_on_no_errors_says_so() {
        let overlay = Overlay::Errors(ErrorsView::default());
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("norecenterrors") || text.contains("no recent errors"),
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
            Overlay::Help,
            Overlay::Errors(ErrorsView {
                items: vec!["an error".to_string()],
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
    fn draft_lines_splits_on_newline_with_a_two_space_continuation_prefix() {
        assert_eq!(
            draft_lines("line one\nline two"),
            vec!["> line one".to_string(), "  line two".to_string()]
        );
    }

    #[test]
    fn draft_lines_keeps_a_single_line_draft_as_one_row() {
        assert_eq!(draft_lines("hello"), vec!["> hello".to_string()]);
    }

    #[test]
    fn every_overlay_renders_into_a_zero_sized_area_without_panicking() {
        let backend = TestBackend::new(20, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        for overlay in every_overlay() {
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 0, 0), &overlay, 0))
                .expect("draw");
            // A zero-height-but-not-zero-width sliver, and its transpose:
            // both used to reach the same clamp.
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 20, 0), &overlay, 0))
                .expect("draw");
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 0, 10), &overlay, 0))
                .expect("draw");
        }
    }

    #[test]
    fn every_overlay_renders_into_a_one_by_one_area_without_panicking() {
        let backend = TestBackend::new(20, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        for overlay in every_overlay() {
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 1, 1), &overlay, 0))
                .expect("draw");
        }
    }

    /// The help table's own ground truth: every `checks` entry must produce
    /// exactly the `filter_key` outcome it claims, in the armed state, so the
    /// help overlay can never drift from what `Ctrl+A <key>` actually does.
    #[test]
    fn help_bindings_match_the_real_filter_key_dispatch() {
        for binding in HELP_BINDINGS {
            for &(event, expected) in binding.checks {
                let (_, verdict) = filter_key(true, event);
                assert_eq!(
                    verdict,
                    InputVerdict::Dash(expected),
                    "help row {:?}: filter_key({event:?}) disagreed",
                    binding.label
                );
            }
        }
    }

    /// One flag per [`DashAction`] variant, all false until the test below
    /// sees that action in some row's `checks`. The match against it (in the
    /// test) has no wildcard arm, so adding a `DashAction` variant without
    /// giving it a flag here -- and a help row that sets it -- is a compile
    /// error, not a silently-uncovered help row.
    #[derive(Default)]
    struct DashActionCoverage {
        switch: bool,
        next_pane: bool,
        select_up: bool,
        select_down: bool,
        spawn: bool,
        nudge: bool,
        mail: bool,
        memory: bool,
        handover: bool,
        show_errors: bool,
        zoom: bool,
        quit: bool,
        scroll_page_up: bool,
        scroll_page_down: bool,
        scroll_top: bool,
        scroll_live: bool,
        literal_prefix: bool,
        help: bool,
        toggle_select_mode: bool,
    }

    /// Completeness, not just correctness: the test above proves every
    /// claimed row is right, this one proves no `DashAction` variant is
    /// missing a row at all.
    #[test]
    fn help_bindings_cover_every_dash_action() {
        let mut cov = DashActionCoverage::default();
        for binding in HELP_BINDINGS {
            for &(_, action) in binding.checks {
                match action {
                    DashAction::Switch(_) => cov.switch = true,
                    DashAction::NextPane => cov.next_pane = true,
                    DashAction::SelectUp => cov.select_up = true,
                    DashAction::SelectDown => cov.select_down = true,
                    DashAction::Spawn => cov.spawn = true,
                    DashAction::Nudge => cov.nudge = true,
                    DashAction::Mail => cov.mail = true,
                    DashAction::Memory => cov.memory = true,
                    DashAction::Handover => cov.handover = true,
                    DashAction::ShowErrors => cov.show_errors = true,
                    DashAction::Zoom => cov.zoom = true,
                    DashAction::Quit => cov.quit = true,
                    DashAction::ScrollPageUp => cov.scroll_page_up = true,
                    DashAction::ScrollPageDown => cov.scroll_page_down = true,
                    DashAction::ScrollTop => cov.scroll_top = true,
                    DashAction::ScrollLive => cov.scroll_live = true,
                    DashAction::LiteralPrefix => cov.literal_prefix = true,
                    DashAction::Help => cov.help = true,
                    DashAction::ToggleSelectMode => cov.toggle_select_mode = true,
                }
            }
        }
        assert!(
            cov.switch
                && cov.next_pane
                && cov.select_up
                && cov.select_down
                && cov.spawn
                && cov.nudge
                && cov.mail
                && cov.memory
                && cov.handover
                && cov.show_errors
                && cov.zoom
                && cov.quit
                && cov.scroll_page_up
                && cov.scroll_page_down
                && cov.scroll_top
                && cov.scroll_live
                && cov.literal_prefix
                && cov.help
                && cov.toggle_select_mode,
            "help table is missing a row for at least one DashAction variant"
        );
    }

    #[test]
    fn help_lines_is_non_empty_and_documents_the_prefix() {
        let lines = help_lines();
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l == "Ctrl+A, then:"));
        assert!(lines.iter().any(|l| l == "no prefix:"));
        assert!(lines.iter().any(|l| l.contains("quit")));
        assert!(lines.iter().any(|l| l.contains("Esc closes")));
        assert!(lines.iter().any(|l| l == "dialogs:"));
        assert!(lines.iter().any(|l| l.contains("errors")));
    }

    #[test]
    fn every_help_line_fits_an_eighty_column_terminal_with_the_default_sidebar() {
        for line in help_lines() {
            assert!(
                line.chars().count() <= 49,
                "help line too long for an 80-col terminal: {line:?} ({} chars)",
                line.chars().count()
            );
        }
    }

    /// The old `render_dialog`-based help overlay fit a standard 24-row
    /// terminal with exactly zero rows of slack (`lines.len() + 2 == 23`,
    /// the main area's own height there). Issue #202 phase 2b's mandatory
    /// additions -- the `Ctrl+A e` binding and the new "dialogs:" section
    /// (item 7) -- grow it past that already-zero margin, so a bare 24-row
    /// terminal now clips the tail of the dialogs section. That is accepted
    /// rather than solved with scrolling (not in this phase's scope), the
    /// same trade-off already documented for terminals shorter than ~22
    /// rows; `render_list_dialog`'s own height clamp (`min(.., area.height)`)
    /// makes the clip safe, never a panic (see the degenerate-area overlay
    /// tests). This pins the current row count so a future content change
    /// that shrinks it back under budget is visible here, not silently lost.
    #[test]
    fn the_help_overlay_no_longer_fits_a_bare_24_row_terminal_and_clips_safely() {
        let content_rows = help_lines().len();
        // render_list_dialog's own height formula: content + 1 blank + 1
        // footer + 2 borders.
        let dialog_height = content_rows as u16 + 4;
        assert!(
            dialog_height > 23,
            "if this now fits a 24-row terminal again, restore the tighter \
             fit test instead of this one"
        );

        // And the clip itself is safe at every height down to a genuinely
        // tiny terminal -- no panic, some content still visible.
        for height in [1u16, 3, 8, 23, 24, 40] {
            let backend = TestBackend::new(80, height);
            let mut term = Terminal::new(backend).expect("terminal");
            term.draw(|f| {
                let area = f.area();
                render_overlay(f, area, &Overlay::Help, 0);
            })
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
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
            .unwrap();

        let buf = term.backend().buffer();
        let non_blank = (0..40).any(|y| (0..120).any(|x| buf[(x, y)].symbol() != " "));
        assert!(
            non_blank,
            "expected the real session fixture to render at least one non-blank cell"
        );
    }
}
