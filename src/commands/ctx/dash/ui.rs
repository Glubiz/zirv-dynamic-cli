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
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph};

use crate::style;

use super::super::mail::Message;
use super::super::price;
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

/// Issue #264: where an aggregate-row cell's value came from -- currently
/// only ever `Live` (read fresh this frame, or fresh as of the last
/// throttled `delegations.jsonl` read). Kept as an explicit enum rather than
/// folding straight into a bare `Option<T>` so a future cached/stale
/// distinction (mirroring `price::PriceTable::is_stale`) has somewhere to go
/// without changing every call site's shape again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Live,
}

/// One aggregate-row cell: `Some((value, source, age))` when a live source
/// produced it, `None` when none exists yet. [`render_aggregate_row`] renders
/// a `None` cell as `--`, never a default number -- Ruflo's own `statusline/
/// index.ts:517` hard-codes `patternsLearned: 156` as a literal, and this
/// shape is what makes the equivalent bug impossible to write here: there is
/// no code path that can hand a bare value to the renderer with no source
/// behind it.
pub type AggregateCell<T> = Option<(T, Source, Duration)>;

/// The dashboard's own aggregate row, drawn above the roster
/// (`dash::mod::run_dashboard`'s own draw closure carves one row off the top
/// of the sidebar for it, issue #264). `workers_running` is cheap in-memory
/// state (`total_live`) recomputed fresh every frame, the same discipline
/// `HeaderFacts::live` already holds; `workers_failed`/`spend_micros` come
/// from a throttled `delegations.jsonl` read (`dash::mod::DiskFacts::spend`)
/// and are `None` until at least one delegation has ever completed;
/// `five_hour_pct` reuses the same per-harness usage snapshot the header/
/// footer already read (`DiskFacts::usage`).
pub struct AggregateFacts {
    pub workers_running: AggregateCell<u64>,
    pub workers_failed: AggregateCell<u64>,
    pub spend_micros: AggregateCell<u64>,
    pub five_hour_pct: AggregateCell<f64>,
}

fn aggregate_cell_text<T: Copy>(cell: &AggregateCell<T>, render: impl Fn(T) -> String) -> String {
    match cell {
        Some((value, _, _)) => render(*value),
        None => "--".to_string(),
    }
}

/// Pure: the aggregate row's own text. A cell with no live source renders
/// `--` in its place -- never a guessed or default number (see
/// [`AggregateCell`]'s own doc comment for why that is structurally, not
/// just conventionally, true).
pub fn render_aggregate_row(facts: &AggregateFacts) -> String {
    format!(
        "workers {} running \u{b7} {} failed \u{b7} {} \u{b7} five_hour {}",
        aggregate_cell_text(&facts.workers_running, |v: u64| v.to_string()),
        aggregate_cell_text(&facts.workers_failed, |v: u64| v.to_string()),
        aggregate_cell_text(&facts.spend_micros, |v: u64| price::format_usd(v, false)),
        aggregate_cell_text(&facts.five_hour_pct, |v: f64| format!("{v:.0}%")),
    )
}

/// Draws [`render_aggregate_row`]'s text into `area`'s first row, dim -- this
/// is ambient summary an operator glances at, not a state they act on the
/// way a working/idle glyph is.
pub fn render_aggregate(f: &mut Frame, area: Rect, facts: &AggregateFacts) {
    if area.is_empty() {
        return;
    }
    let text = render_aggregate_row(facts);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        ))),
        Rect { height: 1, ..area },
    );
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
///
/// `score` (issue #209/v3 §C, restored after #207 dropped it) is this row's
/// cached rot score -- `score::cached_score`'s own `None` means *unknown*,
/// never *healthy*, and renders the same dim placeholder a dead row's score
/// always does regardless of what is cached for it (a dead pane's last
/// reading is stale, not a live verdict).
pub struct SidebarRow {
    pub short: String,
    pub harness: String,
    pub age_secs: Option<u64>,
    pub score: Option<u32>,
    pub state: RowState,
    pub attached: bool,
    pub selected: bool,
    pub focused: bool,
    /// Issue #209/v3 codex review finding 5: `Pane::reachable()` for an
    /// attached row (whether this pane's own turn-signal socket bound at
    /// spawn time); `true` for a view-only registry row, which has no
    /// `Pane` of its own to ask and can never be `focused` anyway (see
    /// `focused`'s own doc comment) -- the footer is the only reader, and
    /// it only ever reads this off the focused row.
    pub supervised: bool,
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

/// Pure: how many rows each of the frame's fixed (non-body) chrome pieces
/// get, in `(header, rule_top, rule_bottom, footer)` order -- issue #209/v3
/// §A4/§D replaces the sidebar's own full rounded box with a full-width rule
/// above and below the body plus a new footer row, mirroring the header.
///
/// Reserved in priority order, each capped at 1.min(remaining): the header
/// first (unchanged from before this phase), then the footer (the new
/// signal row this phase adds -- kept as close to the header's own
/// guarantee as the remaining height allows), then the two rules last,
/// since they are pure decoration and a terminal too short for all four
/// should lose the decoration before it loses either signal row. Every
/// subtraction is guarded (`saturating_sub`/`.min`), so a zero- or one-row
/// frame degrades to all-zero chrome rather than underflowing -- the
/// release profile is `panic = "abort"`.
pub(crate) fn chrome_rows(area_height: u16) -> (u16, u16, u16, u16) {
    let header_h = header_rows(area_height);
    let remaining = area_height.saturating_sub(header_h);
    let footer_h = 1.min(remaining);
    let remaining = remaining.saturating_sub(footer_h);
    let rule_top_h = 1.min(remaining);
    let remaining = remaining.saturating_sub(rule_top_h);
    let rule_bottom_h = 1.min(remaining);
    (header_h, rule_top_h, rule_bottom_h, footer_h)
}

/// Every rect [`layout`] hands the render loop, named rather than
/// positional: `header`/`sidebar`/`main`/`footer` are drawn into directly;
/// `rule_top`/`rule_bottom` are the full-width flat rules that replace the
/// sidebar's old box border (issue #209/v3 §A4), each zero-height on a frame
/// too short to afford it (see [`chrome_rows`]).
pub struct DashLayout {
    pub header: Rect,
    pub rule_top: Rect,
    pub sidebar: Rect,
    pub main: Rect,
    pub rule_bottom: Rect,
    pub footer: Rect,
}

/// Splits `area` into every chrome rect a v3 frame draws: one header row, a
/// full-width top rule, a `sidebar_cols`-wide sidebar with a one-column
/// divider before the grid, a bottom rule mirroring the top one, and one
/// footer row (§D) -- see [`DashLayout`] and [`chrome_rows`] for how each
/// piece's height is decided.
pub fn layout(area: Rect, sidebar_cols: u16) -> DashLayout {
    let (header_h, rule_top_h, rule_bottom_h, footer_h) = chrome_rows(area.height);
    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: header_h,
    };

    let rule_top = Rect {
        x: area.x,
        y: area.y + header_h,
        width: area.width,
        height: rule_top_h,
    };

    let body_y = area.y + header_h + rule_top_h;
    let body_h = area
        .height
        .saturating_sub(header_h + rule_top_h + rule_bottom_h + footer_h);
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

    let rule_bottom = Rect {
        x: area.x,
        y: body_y + body_h,
        width: area.width,
        height: rule_bottom_h,
    };

    let footer = Rect {
        x: area.x,
        y: body_y + body_h + rule_bottom_h,
        width: area.width,
        height: footer_h,
    };

    DashLayout {
        header,
        rule_top,
        sidebar,
        main,
        rule_bottom,
        footer,
    }
}

/// Issue #209/v3 §A4/§D: the full-width flat rule that replaces the
/// sidebar's old box border, drawn once above the body (mirroring the
/// header) and once below it (mirroring the footer). `divider_col` is the
/// sidebar's own width -- the rule draws a `┬`/`┴` junction there against
/// the sidebar/grid divider, `top` selects which junction glyph. Dim,
/// matching `--t-line` in the approved mock, same as
/// [`render_sidebar_divider`].
pub fn render_rule(f: &mut Frame, area: Rect, divider_col: u16, top: bool) {
    if area.is_empty() {
        return;
    }
    let cols = area.width as usize;
    let junction_at = (divider_col as usize).min(cols.saturating_sub(1));
    let junction = if top { '\u{252c}' } else { '\u{2534}' };
    let mut line = String::with_capacity(cols);
    for col in 0..cols {
        line.push(if col == junction_at && divider_col < area.width {
            junction
        } else {
            '\u{2500}'
        });
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, style::tui::muted()))),
        area,
    );
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

    // The chip and the hint cluster always show, and the live count and
    // SELECT marker are short and fixed-shape; the harness/model label is
    // the one variable-length piece of the left side, so it is the one that
    // gives up room when the row is narrow. Ellipsis-truncate it to
    // whatever is left after those fixed pieces, rather than drawing it at
    // full length: a long-but-valid label would otherwise consume the row
    // and push the hint cluster past the Paragraph's own right edge, where
    // ratatui clips it off-screen with no ellipsis and no warning.
    let hints_w = style::display_width(HEADER_HINTS);
    let gap_before_hints = 2usize;
    let chip_w = style::display_width(&chip_text);
    let live_w = style::display_width(&live_text);
    let select_w = select_text
        .as_deref()
        .map(style::display_width)
        .unwrap_or(0);
    let fixed_w = chip_w + live_w + select_w;
    let harness_budget = cols.saturating_sub(fixed_w + hints_w + gap_before_hints);
    let harness_text = style::truncate_display_ellipsis(&harness_text, harness_budget).into_owned();

    let mut left: Vec<(String, Style)> = vec![
        (chip_text, style::tui::chip()),
        (harness_text, style::tui::title()),
        (live_text, style::tui::muted()),
    ];
    if let Some(text) = select_text {
        left.push((text, style::tui::muted()));
    }
    let left_w: usize = left.iter().map(|(t, _)| style::display_width(t)).sum();

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

/// Issue #209/v3 §D: the focused session's active `zirv workflow` position,
/// as much as the footer needs -- the rest of `workflow::ActiveWorkflowSummary`
/// (attempts, artifacts, review evidence, ...) has no footer segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterWorkflow {
    /// This session's repo has no active workflow at all.
    None,
    Active {
        kind: String,
        step: String,
        /// The current step is gated on the operator's own approval
        /// (`WorkflowStatus::AwaitingApproval`) -- Q4: escalates to
        /// yellow-bold, the same weight unread mail already gets.
        gated: bool,
    },
}

/// The footer's own facts for the focused pane (Q1: focused-only, never one
/// row per live harness) -- issue #209/v3 §D's new signal row, one below
/// the grid. `None` when nothing is focused at all (an empty dashboard);
/// nothing draws.
pub enum FooterFacts {
    None,
    Alive(FooterAliveFacts),
    Dead(FooterDeadFacts),
}

/// The healthy/attention footer shapes (mock §04's first two examples) --
/// they differ only in *values*, not in which fields exist.
pub struct FooterAliveFacts {
    pub harness: String,
    /// `None` when no cached score exists yet for this session -- renders
    /// the same `✻ –` unknown placeholder the wrap bar's own `BarState`
    /// uses for the identical case.
    pub score: Option<u32>,
    pub usage_five_hour: Option<f64>,
    pub usage_seven_day: Option<f64>,
    /// Total unread mail (broadcast + direct) for this session. The mock's
    /// footer shows one unlabeled number, unlike the wrap bar's own
    /// broadcast/direct `+`-split -- `0` renders the dim placeholder.
    pub unread_mail: usize,
    pub workflow: FooterWorkflow,
    /// `Pane::reachable()` for the focused pane (issue #209/v3 codex review
    /// finding 5): whether its own turn-signal socket bound successfully at
    /// spawn time. `false` is the same "degrades to unsupervised" case
    /// `Pane::spawn`'s own doc comment describes for a failed bind -- a
    /// dashboard pane that cannot act on a wake-up is still legitimate and
    /// visible, but the footer must say so rather than assume every alive
    /// pane is fine. Not the same signal as `wrap`'s own in-process
    /// `chrome::BarState::degraded` (there is no dash-observable analogue
    /// of THAT one -- a supervising *loop* going bad mid-session, as
    /// opposed to never having bound in the first place), which stays out
    /// of this issue's scope (§E).
    pub supervised: bool,
    /// Issue #310: this pane's stall latch is currently armed
    /// (`sessions::stall_marker`, via `DiskFacts::stalled`) -- overrides the
    /// supervision segment with a `stalled` badge instead of `supervised`/
    /// `unsupervised` while it holds, and reverts the instant the latch
    /// clears (observed progress, or the session ended). Takes priority over
    /// `supervised`: a session can be both reachable and stalled at once,
    /// and the operator needs to see the more urgent fact.
    pub stalled: bool,
}

/// The dead-pane-focused footer shape (mock §04's third example): a
/// different message entirely, not just different values -- there is no
/// verdict, usage or mail segment for a session that has already exited.
pub struct FooterDeadFacts {
    pub harness: String,
    pub exited_age_secs: Option<u64>,
    pub workflow: FooterWorkflow,
}

/// One footer segment, already styled per-piece (a segment is sometimes
/// more than one span -- the verdict's glyph+word and its dim score number,
/// for instance) but not yet joined to its neighbours. Named to keep every
/// footer-assembly signature below readable, the same reason `chrome.rs`
/// names its own `Segments` alias.
type FooterSeg = Vec<(String, Style)>;

/// Pure: `workflow`'s own footer text, in its full and width-compressed
/// forms (§D's drop order compresses the workflow segment before dropping
/// it outright) -- `(full, compressed)`, both already styled. `None` has
/// only the one dim `▸ –` form; a gated step's compressed form gains the
/// `!` suffix mentioned nowhere but the mock's own 44-column example.
fn footer_workflow_spans(workflow: &FooterWorkflow) -> (FooterSeg, FooterSeg) {
    match workflow {
        FooterWorkflow::None => {
            let dim = vec![(
                format!("\u{25b8} {}", style::PLACEHOLDER),
                style::tui::muted(),
            )];
            (dim.clone(), dim)
        }
        FooterWorkflow::Active { kind, step, gated } => {
            if *gated {
                let style = style::tui::warning().add_modifier(Modifier::BOLD);
                let full = vec![(format!("\u{25b8} {step} awaits approval"), style)];
                let compressed = vec![(format!("\u{25b8} {step}!"), style)];
                (full, compressed)
            } else {
                let style = style::tui::muted();
                let full = vec![(format!("\u{25b8} {kind} \u{b7} {step}"), style)];
                let compressed = vec![(format!("\u{25b8} {step}"), style)];
                (full, compressed)
            }
        }
    }
}

/// Three plain spaces between top-level footer segments, mirroring
/// `chrome::BAR_SEGMENT_GAP` -- the footer reuses the wrap bar's own
/// grammar verbatim (minus the chip, per the mock's own note).
const FOOTER_SEGMENT_GAP: &str = "   ";

/// Pure: the alive-pane footer's spans, width-budgeted to `cols`, applying
/// §D's own drop order: usage first, then the verdict's score number, then
/// the workflow segment compresses (full form to `step!`), then the
/// harness label drops, then -- last resort -- the now-harness-less
/// workflow segment drops too. Verdict word/glyph, mail and supervision are
/// never dropped.
fn footer_alive_spans(
    facts: &FooterAliveFacts,
    advise_at: u32,
    compact_at: u32,
    cols: u16,
) -> Vec<Span<'static>> {
    let cols = cols as usize;

    let harness: FooterSeg = vec![(facts.harness.clone(), Style::default())];

    let (verdict_full, verdict_reduced): (FooterSeg, FooterSeg) = match facts.score {
        Some(score) => {
            let band = rot_band_for(score, advise_at, compact_at);
            let word = match band {
                RotBand::Fresh => "fresh",
                RotBand::Warming => "warming",
                RotBand::Rotting => "rotting",
            };
            let style = footer_rot_style(band);
            let full = vec![
                (format!("{ROT_GLYPH} {word}"), style),
                (format!(" {score}"), style::tui::muted()),
            ];
            let reduced = vec![(format!("{ROT_GLYPH} {word}"), style)];
            (full, reduced)
        }
        None => {
            let unknown = vec![(
                format!("{ROT_GLYPH} {}", style::PLACEHOLDER),
                style::tui::muted(),
            )];
            (unknown.clone(), unknown)
        }
    };

    let usage: FooterSeg = {
        let five = facts
            .usage_five_hour
            .map(style::format_pct)
            .unwrap_or_else(|| style::PLACEHOLDER.to_string());
        let seven = facts
            .usage_seven_day
            .map(style::format_pct)
            .unwrap_or_else(|| style::PLACEHOLDER.to_string());
        vec![(format!("\u{25d4} {five}\u{b7}{seven}"), style::tui::muted())]
    };

    let mail: FooterSeg = if facts.unread_mail == 0 {
        vec![(
            format!("\u{2709} {}", style::PLACEHOLDER),
            style::tui::muted(),
        )]
    } else {
        let style = style::tui::warning().add_modifier(Modifier::BOLD);
        vec![(format!("\u{2709} {}", facts.unread_mail), style)]
    };

    let (workflow_full, workflow_compressed) = footer_workflow_spans(&facts.workflow);

    // Issue #310: a stalled latch takes priority over the ordinary
    // supervised/unsupervised segment -- see `FooterAliveFacts::stalled`'s
    // own doc comment.
    let supervision: FooterSeg = if facts.stalled {
        vec![(
            "\u{25c6} stalled".to_string(),
            style::tui::warning().add_modifier(Modifier::BOLD),
        )]
    } else if facts.supervised {
        vec![
            ("\u{25cf} ".to_string(), style::tui::ok()),
            ("supervised".to_string(), Style::default()),
        ]
    } else {
        vec![("\u{25b2} unsupervised".to_string(), style::tui::error())]
    };

    // §D's own drop order, most to least generous -- usage, then the
    // verdict's score number, then the workflow segment compresses (long
    // form to `step!`), then the harness label drops (the mock's own
    // 44-column example: `▸ spec!` survives with no harness at all), and
    // only as the very last resort before the irreducible core does the
    // now-harness-less workflow segment drop too. The verdict word/glyph,
    // mail and supervision segments are never dropped. Issue #209/v3 codex
    // review finding 4: harness must drop *before* the workflow segment is
    // removed outright, not after -- the original tier order dropped
    // workflow to nothing while still holding onto the harness, which
    // never matches the mock at 44 columns. Mirrors `chrome::status_bar`'s
    // own tiered-candidate shape.
    let tiers = [
        join_footer_segments(&[
            &harness,
            &verdict_full,
            &usage,
            &mail,
            &workflow_full,
            &supervision,
        ]),
        join_footer_segments(&[&harness, &verdict_full, &mail, &workflow_full, &supervision]),
        join_footer_segments(&[
            &harness,
            &verdict_reduced,
            &mail,
            &workflow_full,
            &supervision,
        ]),
        join_footer_segments(&[
            &harness,
            &verdict_reduced,
            &mail,
            &workflow_compressed,
            &supervision,
        ]),
        join_footer_segments(&[&verdict_reduced, &mail, &workflow_compressed, &supervision]),
        join_footer_segments(&[&verdict_reduced, &mail, &supervision]),
    ];
    choose_footer_tier(&tiers, cols)
}

/// Pure: joins `segments` with [`FOOTER_SEGMENT_GAP`] between each one
/// present, in order.
fn join_footer_segments(segments: &[&FooterSeg]) -> FooterSeg {
    let mut out = FooterSeg::new();
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            out.push((FOOTER_SEGMENT_GAP.to_string(), Style::default()));
        }
        out.extend(seg.iter().cloned());
    }
    out
}

/// Pure: total display width of a joined footer segment.
fn footer_seg_width(pieces: &[(String, Style)]) -> usize {
    pieces.iter().map(|(t, _)| style::display_width(t)).sum()
}

/// Pure: the first `tiers` entry (most generous first) that fits `cols`, or
/// -- if even the narrowest tier overflows -- that narrowest tier hard-
/// truncated to `cols` with no ellipsis, exactly `chrome::status_bar`'s own
/// last resort. `Paragraph` would clip silently past `area`'s own width
/// regardless; this keeps the pure function itself honest about `cols` for
/// its own tests.
fn choose_footer_tier(tiers: &[FooterSeg], cols: usize) -> Vec<Span<'static>> {
    let chosen = match tiers
        .iter()
        .find(|tier| footer_seg_width(tier) <= cols)
        .or_else(|| tiers.last())
    {
        Some(tier) => tier,
        // An empty tier list renders as an empty row rather than panicking
        // the render loop; every current caller passes a non-empty literal.
        None => return Vec::new(),
    };

    if footer_seg_width(chosen) <= cols {
        return chosen
            .iter()
            .map(|(text, style)| Span::styled(text.clone(), *style))
            .collect();
    }
    let plain: String = chosen.iter().map(|(t, _)| t.as_str()).collect();
    vec![Span::raw(
        style::truncate_display(&plain, cols).into_owned(),
    )]
}

/// Pure: the dead-pane-focused footer's spans (mock §04's third example) --
/// a different message entirely, not the alive shape with blanks. The
/// exited notice and the restore hint are never dropped; `harness` and the
/// workflow segment are the only droppable pieces, in that order (the
/// restore hint is the one actionable thing this state exists to tell the
/// operator, so it survives longest).
fn footer_dead_spans(facts: &FooterDeadFacts, cols: u16) -> Vec<Span<'static>> {
    let cols = cols as usize;

    let harness: FooterSeg = vec![(facts.harness.clone(), Style::default())];

    let exited: FooterSeg = {
        let age = facts
            .exited_age_secs
            .map(style::format_age)
            .unwrap_or_else(|| style::PLACEHOLDER.to_string());
        vec![
            ("\u{2717} exited".to_string(), style::tui::error()),
            (format!(" {age} ago"), style::tui::muted()),
        ]
    };

    let (workflow_full, _) = footer_workflow_spans(&facts.workflow);

    let restore_hint: FooterSeg = vec![
        ("\u{21ba} ".to_string(), style::tui::accent()),
        (
            "^A r".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        (" restore".to_string(), style::tui::hint()),
    ];

    let tiers = [
        join_footer_segments(&[&harness, &exited, &workflow_full, &restore_hint]),
        join_footer_segments(&[&harness, &exited, &restore_hint]),
        join_footer_segments(&[&exited, &restore_hint]),
    ];
    choose_footer_tier(&tiers, cols)
}

/// Issue #209/v3 §D: the footer signal row, describing whichever pane is
/// focused. `advise_at`/`compact_at` are `rot::ScoreConfig`'s own
/// thresholds, threaded through exactly as [`render_sidebar`] takes them.
pub fn render_footer(
    f: &mut Frame,
    area: Rect,
    facts: &FooterFacts,
    advise_at: u32,
    compact_at: u32,
) {
    if area.is_empty() {
        return;
    }
    let spans = match facts {
        FooterFacts::None => return,
        FooterFacts::Alive(alive) => footer_alive_spans(alive, advise_at, compact_at, area.width),
        FooterFacts::Dead(dead) => footer_dead_spans(dead, area.width),
    };
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

/// The colour band a rot score falls into, mirroring the same three-band
/// collapse `chrome::verdict_paint` draws for the wrap status bar (healthy
/// vs. advise vs. compact-or-restart) but keyed on the score alone: the
/// dashboard does not track each pane's live context-token count the way
/// `wrap` does, so there is no token-gate escalation (`rot::verdict_for`) to
/// fold in here -- see [`rot_band_for`]'s own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotBand {
    Fresh,
    Warming,
    Rotting,
}

/// Pure: which [`RotBand`] a raw rot `score` falls into, given the same
/// `advise_at`/`compact_at` thresholds `rot::ScoreConfig` already carries
/// (passed through rather than re-read, so this stays a plain function of
/// its arguments). `restart_at` collapses into `Rotting` alongside
/// `compact_at`, matching `chrome::verdict_paint`'s own three-colour band.
pub fn rot_band_for(score: u32, advise_at: u32, compact_at: u32) -> RotBand {
    if score >= compact_at {
        RotBand::Rotting
    } else if score >= advise_at {
        RotBand::Warming
    } else {
        RotBand::Fresh
    }
}

/// The sidebar's own rot-glyph style: warming is yellow, rotting is
/// red-bold, and -- per the approved mock (§03) -- fresh gets no colour of
/// its own at all, inheriting whatever tone the row it sits in already
/// carries. Never call this for a selected row: see `render_sidebar`'s own
/// selected-row handling, which drops every glyph's own colour outright
/// (§B) rather than composing it with `Modifier::REVERSED`.
fn sidebar_rot_style(band: RotBand) -> Style {
    match band {
        RotBand::Fresh => Style::default(),
        RotBand::Warming => style::tui::warning(),
        RotBand::Rotting => style::tui::error(),
    }
}

/// The footer's own rot-verdict style (§D): unlike the sidebar, fresh gets
/// an explicit green here -- the footer has no surrounding row tone to
/// inherit the way a sidebar row does, so it colours all three bands
/// outright, exactly `chrome::verdict_paint` does for the wrap bar's own
/// verdict segment.
fn footer_rot_style(band: RotBand) -> Style {
    match band {
        RotBand::Fresh => style::tui::ok(),
        RotBand::Warming => style::tui::warning(),
        RotBand::Rotting => style::tui::error(),
    }
}

/// The rot glyph itself -- the same star `chrome::status_bar` already draws
/// for the wrap bar's own verdict segment, so a session reads identically
/// whether it is shown from inside (wrap) or from the dash.
const ROT_GLYPH: &str = "\u{273b}";

/// Pure: a sidebar row's rot column text, colourless -- [`render_sidebar`]
/// paints it. `None` (dead pane, or no cached score at all) is always the
/// shared placeholder, never a fabricated reading: a dead pane's last cached
/// score is stale, not a live verdict, so it renders the same as "unknown"
/// regardless of what happens to still be cached for it.
fn rot_text(row: &SidebarRow) -> String {
    if row.state == RowState::Dead {
        return style::PLACEHOLDER.to_string();
    }
    match row.score {
        Some(score) => format!("{ROT_GLYPH}{score}"),
        None => style::PLACEHOLDER.to_string(),
    }
}

/// One sidebar row's pieces, already fitted to `cols` display columns as one
/// unit. Shared by [`sidebar_row_text`] (the plain-text test/measurement
/// surface) and [`render_sidebar`] (the styled renderer), so the two can
/// never disagree about layout.
///
/// Width pressure drops fields in a fixed order (§C): the age column goes
/// first, then the rot score's own digits -- its glyph survives alone
/// (`rot` becomes just [`ROT_GLYPH`]), so the verdict's colour never
/// disappears outright, only the number attached to it. `short`/`harness`
/// are the last resort, hard-truncated exactly as before this phase.
struct SidebarLayout {
    glyph: String,
    left: String,
    rot: String,
    age: String,
}

/// Pure: lays a sidebar row out into [`SidebarLayout`]'s pieces, applying
/// the §C width-degradation order above. See [`sidebar_row_parts`]'s own
/// former doc comment (kept here) for the original `{short:<8} {harness}`
/// shape this still produces for `left`.
fn sidebar_row_parts(row: &SidebarRow, tick: usize, cols: u16) -> SidebarLayout {
    let cols = cols as usize;
    let glyph = glyph_char_for(row.state, tick).to_string();
    if cols == 0 {
        return SidebarLayout {
            glyph: String::new(),
            left: String::new(),
            rot: String::new(),
            age: String::new(),
        };
    }
    let glyph_w = style::display_width(&glyph);
    if cols <= glyph_w {
        return SidebarLayout {
            glyph: style::truncate_display(&glyph, cols).into_owned(),
            left: String::new(),
            rot: String::new(),
            age: String::new(),
        };
    }

    let rest_cols = cols - glyph_w - 1; // one separating space
    let left = format!("{:<8} {}", row.short, row.harness);
    let left_w = style::display_width(&left);
    let age = row
        .age_secs
        .map(style::format_age)
        .unwrap_or_else(|| style::PLACEHOLDER.to_string());
    let age_w = style::display_width(&age);
    let rot_full = rot_text(row);
    let rot_full_w = style::display_width(&rot_full);
    let rot_glyph_only = ROT_GLYPH.to_string();
    let rot_glyph_w = style::display_width(&rot_glyph_only);
    let pad = |n: usize| " ".repeat(n);

    // §C's drop order, most to least generous: full rot number + age, then
    // the rot number reduced to just its coloured glyph (age dropped), then
    // no rot column at all (age dropped too), then a hard truncation of
    // `left` itself. Whichever tier fits gets the *entire* leftover slack as
    // trailing padding on its last surviving field (age right-aligned when
    // it survives, otherwise the rot glyph or `left` itself) -- exactly the
    // single-tier version's own `pad`/`fill` did -- so a selected row's
    // REVERSED background always reaches the full `cols` width, never just
    // the text.
    if left_w + 1 + rot_full_w + 1 + age_w <= rest_cols {
        let slack = rest_cols - (left_w + 1 + rot_full_w + 1 + age_w);
        return SidebarLayout {
            glyph,
            left,
            rot: rot_full,
            age: format!("{}{age}", pad(slack)),
        };
    }
    if left_w + 1 + rot_full_w <= rest_cols {
        let slack = rest_cols - (left_w + 1 + rot_full_w);
        return SidebarLayout {
            glyph,
            left,
            rot: format!("{rot_full}{}", pad(slack)),
            age: String::new(),
        };
    }
    if left_w + 1 + rot_glyph_w <= rest_cols {
        let slack = rest_cols - (left_w + 1 + rot_glyph_w);
        return SidebarLayout {
            glyph,
            left,
            rot: format!("{rot_glyph_only}{}", pad(slack)),
            age: String::new(),
        };
    }
    if left_w <= rest_cols {
        let slack = rest_cols - left_w;
        return SidebarLayout {
            glyph,
            left: format!("{left}{}", pad(slack)),
            rot: String::new(),
            age: String::new(),
        };
    }
    // Nothing fit even bare `left`: hard-truncate it, exactly as before this
    // phase.
    let truncated = style::truncate_display(&left, rest_cols).into_owned();
    let fill = rest_cols.saturating_sub(style::display_width(&truncated));
    SidebarLayout {
        glyph,
        left: format!("{truncated}{}", pad(fill)),
        rot: String::new(),
        age: String::new(),
    }
}

/// Pure: one sidebar row's full plain text, exactly `cols` display columns
/// wide (or shorter only when `cols` itself has no room for a full row) --
/// `{glyph} {short:<8} {harness} {rot} {age}`, age right-aligned, padded to
/// fill whatever room the dropped fields left behind. Built from the same
/// [`sidebar_row_parts`] the styled renderer uses, so the two can never
/// disagree about layout; test-only, since nothing in the render path needs
/// the unstyled concatenation.
#[cfg(test)]
fn sidebar_row_text(row: &SidebarRow, tick: usize, cols: u16) -> String {
    let layout = sidebar_row_parts(row, tick, cols);
    let mut rest = layout.left;
    for part in [layout.rot, layout.age] {
        if !part.is_empty() {
            rest.push(' ');
            rest.push_str(&part);
        }
    }
    if rest.is_empty() {
        layout.glyph
    } else {
        format!("{} {rest}", layout.glyph)
    }
}

/// Issue #209/v3 §A4: the sidebar's own flat divider column -- what used to
/// be the right edge of its full rounded `Block::bordered()` -- against the
/// grid. Dim, matching `--t-line` in the approved mock; a full box is now
/// reserved for the banner and overlays only.
pub fn render_sidebar_divider(f: &mut Frame, area: Rect) {
    if area.is_empty() {
        return;
    }
    let line = "\u{2502}".repeat(area.height as usize);
    let lines: Vec<Line> = line
        .chars()
        .map(|c| Line::from(Span::styled(c.to_string(), style::tui::muted())))
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// `advise_at`/`compact_at` are `rot::ScoreConfig`'s own thresholds, passed
/// through by the caller (`dash::mod` already has the loaded `CtxConfig`)
/// rather than duplicated here as defaults -- an operator's configured
/// thresholds drive the sidebar's rot colour exactly as they drive
/// `rot::verdict_for`'s own bands.
pub fn render_sidebar(
    f: &mut Frame,
    area: Rect,
    rows: &[SidebarRow],
    tick: usize,
    advise_at: u32,
    compact_at: u32,
) {
    // Issue #209/v3 §A4: the sidebar no longer draws its own full rounded
    // box -- that framing is reserved for the banner and overlays now, per
    // the approved mock. What used to be `Block::inner` is simply `area`
    // itself; the flat divider that replaces the box's right border is
    // drawn separately by `render_sidebar_divider`, called alongside this
    // from the same layout the removed block used to own.
    let inner = area;
    let visible = inner.height as usize;
    let selected = rows.iter().position(|r| r.selected).unwrap_or(0);
    let offset = sidebar_offset(rows.len(), visible, selected);

    let items: Vec<ListItem> = rows
        .iter()
        .skip(offset)
        .take(visible)
        .map(|row| {
            let layout = sidebar_row_parts(row, tick, inner.width);
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
            // compact `{glyph} {short} {harness} {rot} {age}` format never
            // needs a sixth column for it. Combined with `selected`
            // (REVERSED) this is exactly `style::tui::selected_strong()`'s
            // own shape.
            if row.focused {
                base = base.add_modifier(Modifier::BOLD);
            }
            let rot_band = row
                .score
                .filter(|_| row.state != RowState::Dead)
                .map(|score| rot_band_for(score, advise_at, compact_at));
            // Bug fix (issue #209/v3 §B, operator-reported): a selected row
            // used to patch REVERSED onto the glyph's *own* fg colour
            // (cyan/green/red from `glyph_style_for`) -- ratatui's REVERSED
            // swaps fg and bg at render time, so that colour became the
            // row's background while the glyph itself rendered in whatever
            // the terminal treats as the default foreground: a colored block
            // with a background-colored glyph, not a uniformly reversed row.
            // On a selected row every glyph (state and rot alike) drops its
            // own colour entirely and joins the row's plain `base` style
            // before REVERSED is applied, exactly as the mock's focused row
            // shows -- the state is still legible from the glyph *character*
            // (spinner/●/✗/·), just not from an extra colour layered under
            // the reversal.
            let (glyph_style, rot_style, plain_style) = if row.selected {
                let reversed = base.add_modifier(Modifier::REVERSED);
                (reversed, reversed, reversed)
            } else {
                // `None` (dead pane, or nothing cached) renders the same
                // muted placeholder every other unknown value in this row
                // already does -- not `base`, which would draw the en dash
                // at full strength.
                let rot_style = match rot_band {
                    Some(band) => sidebar_rot_style(band).patch(base),
                    None => style::tui::muted().patch(base),
                };
                (glyph_style_for(row.state).patch(base), rot_style, base)
            };
            let mut spans = vec![
                Span::styled(layout.glyph, glyph_style),
                Span::styled(format!(" {}", layout.left), plain_style),
            ];
            if !layout.rot.is_empty() {
                spans.push(Span::styled(" ".to_string(), plain_style));
                spans.push(Span::styled(layout.rot, rot_style));
            }
            if !layout.age.is_empty() {
                spans.push(Span::styled(format!(" {}", layout.age), plain_style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    f.render_widget(List::new(items), area);
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

/// A dialog's own row count (content rows plus `extra_rows` of chrome --
/// border/footer/blank lines) as a `u16`, clamped rather than cast: `rows`
/// comes from caller-controlled data (mail, memory, restore lists) with no
/// upper bound, so a plain `rows as u16` would silently wrap on a five-digit
/// row count, and `rows as u16 + extra_rows` (`+` on `u16`, not
/// `saturating_add`) can then overflow -- which panics in every debug/test
/// build (this profile has no `overflow-checks` override, so the default
/// `true` for dev/test applies) and silently wraps in release. Saturating
/// both the cast and the add means an enormous row count degrades to
/// "as tall as it can possibly be", clamped again by the caller's own
/// `.min(area.height)`, instead of corrupting the layout or aborting the
/// whole dashboard.
fn dialog_row_count(rows: usize, extra_rows: u16) -> u16 {
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_add(extra_rows)
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

/// The border colour every dialog frame draws in (except the warn variant,
/// which keeps yellow): dim cyan, per the approved v3 mock (§A2). Shared by
/// [`render_dialog`] and [`render_list_dialog`] so the two frame primitives
/// can never drift from each other.
fn dialog_border_style() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
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
    let h = dialog_row_count(lines.len(), 2).min(area.height);
    let w = dialog_width(area.width);
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        // Issue #209/v3 §A2: the shared dialog frame's border was colourless
        // (the terminal default) -- the approved mock gives every dialog a
        // dim-cyan border, matching the list-dialog primitive right below.
        // Not `tui::accent()` itself: that is now bold (§A1), and a bold
        // border would out-shout the title it is meant to frame.
        .border_style(dialog_border_style())
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
        // Issue #209/v3 §A2: dim-cyan, not the terminal-default colourless
        // border every dialog drew before.
        dialog_border_style()
    };
    // Issue #209/v3 §A3: `{title} · {n}` -- the title keeps its own accent
    // (or warning) weight and the count is a separate, dim span, replacing
    // the old single-span `{title} (n)` where the count read at the same
    // weight as the title itself.
    let title_spans: Vec<Span<'static>> = match spec.count {
        Some(n) => vec![
            Span::styled(spec.title.to_string(), title_style),
            Span::styled(format!(" \u{b7} {n}"), style::tui::muted()),
        ],
        None => vec![Span::styled(spec.title.to_string(), title_style)],
    };

    let content_rows = spec.rows.len().max(1);
    // +1 blank row above the footer, +1 the footer row itself, +2 for the
    // block's own top/bottom border.
    let h = dialog_row_count(content_rows, 2 + 2).min(area.height);
    let w = dialog_width(area.width);
    let rect = centered(area, w, h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .padding(Padding::horizontal(1))
        .title(Line::from(title_spans));

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
                // Bug fix (issue #209/v3 §B): the same defect as the
                // sidebar's selected row (see `render_sidebar`'s own doc
                // comment) -- patching REVERSED onto a span's own colour
                // (the QuitConfirm spinner glyph, in practice) swapped that
                // colour into the background and left the glyph rendered in
                // the terminal's default foreground instead of reversing
                // uniformly. Every span on the cursor row drops its own
                // style and takes the same plain reversed one.
                let reversed_style = Style::default().add_modifier(Modifier::REVERSED);
                let mut reversed: Vec<Span> = spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, reversed_style))
                    .collect();
                let pad = inner_width.saturating_sub(raw_width);
                if pad > 0 {
                    reversed.push(Span::styled(" ".repeat(pad), reversed_style));
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
            // Issue #209/v3 §A5: shares `MAIL_FOOTER` with the help overlay's
            // own "dialogs:" listing rather than a second, easily-drifting
            // copy of the same four hints.
            footer: MAIL_FOOTER,
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
    // Issue #209/v3 §A5: `j/k` already moved the cursor (shared with every
    // other list dialog's up/down handling); this only documents it, per
    // the approved mock's footer.
    ("j/k", "move"),
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

    fn no_live_source() -> AggregateFacts {
        AggregateFacts {
            workers_running: None,
            workers_failed: None,
            spend_micros: None,
            five_hour_pct: None,
        }
    }

    /// Issue #264, the render-path contract: with no live source at all, the
    /// aggregate row must contain no digit whatsoever -- proof there is no
    /// hard-coded metric literal anywhere in `render_aggregate_row` (Ruflo's
    /// own `statusline/index.ts:517` hard-codes `patternsLearned: 156`,
    /// exactly the bug class this rules out). A digit could only reach the
    /// output through a `Some` cell, and every cell here is `None`.
    #[test]
    fn render_aggregate_row_with_no_live_source_contains_no_digit_literal() {
        let text = render_aggregate_row(&no_live_source());
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "no live source means no cell may render a number at all: {text}"
        );
        assert!(text.contains("--"), "got {text}");
    }

    /// Every cell backed by a live source renders its real value, in the
    /// design's own worked shape: `workers N running · M failed · $x ·
    /// five_hour P%`.
    #[test]
    fn render_aggregate_row_renders_every_live_cell() {
        let facts = AggregateFacts {
            workers_running: Some((3, Source::Live, Duration::ZERO)),
            workers_failed: Some((1, Source::Live, Duration::ZERO)),
            spend_micros: Some((4_200_000, Source::Live, Duration::ZERO)),
            five_hour_pct: Some((41.0, Source::Live, Duration::ZERO)),
        };
        let text = render_aggregate_row(&facts);
        assert_eq!(
            text,
            "workers 3 running \u{b7} 1 failed \u{b7} $4.20 \u{b7} five_hour 41%"
        );
    }

    /// A mix of live and absent cells renders each independently -- `--`
    /// never leaks into a cell that DOES have a live source, and vice versa.
    #[test]
    fn render_aggregate_row_mixes_live_and_absent_cells_independently() {
        let facts = AggregateFacts {
            workers_running: Some((2, Source::Live, Duration::ZERO)),
            workers_failed: None,
            spend_micros: None,
            five_hour_pct: Some((10.0, Source::Live, Duration::ZERO)),
        };
        let text = render_aggregate_row(&facts);
        assert!(text.contains("workers 2 running"), "got {text}");
        assert!(text.contains("-- failed"), "got {text}");
        assert!(text.contains("\u{b7} -- \u{b7}"), "got {text}");
        assert!(text.contains("five_hour 10%"), "got {text}");
    }

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

    /// Issue #209/v3 §A4/§D: the sidebar's own box border becomes a
    /// full-width top rule and bottom rule around the body, plus a new
    /// footer row -- one of each on a frame with room for all four.
    #[test]
    fn layout_reserves_the_header_rules_footer_and_the_sidebar() {
        let l = layout(Rect::new(0, 0, 100, 30), 24);
        assert_eq!(l.header.height, 1, "one header row at every height");
        assert_eq!(l.rule_top.height, 1);
        assert_eq!(l.rule_bottom.height, 1);
        assert_eq!(l.footer.height, 1);
        assert_eq!(l.sidebar.width, 24);
        assert_eq!(l.main.width, 100 - 24 - 1);
        assert_eq!(l.sidebar.height, 30 - 1 - 1 - 1 - 1);
        assert_eq!(l.main.height, l.sidebar.height);
    }

    #[test]
    fn layout_is_stable_on_a_tiny_area_without_underflow() {
        let l = layout(Rect::new(0, 0, 10, 2), 24);
        assert_eq!(l.header.height, 1);
        // Only the header and the footer fit in a two-row frame -- the
        // rules are the first chrome dropped (`chrome_rows`'s own priority
        // order), and the body has nothing left at all.
        assert_eq!(l.footer.height, 1);
        assert_eq!(l.rule_top.height, 0);
        assert_eq!(l.rule_bottom.height, 0);
        assert_eq!(l.sidebar.width, 10);
        assert_eq!(l.main.width, 0);
        assert_eq!(l.main.height, 0);
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
        // And the body never loses more rows than the fixed chrome gained --
        // every row of `height` is accounted for by exactly one of the six
        // rects `layout` hands back.
        for height in 0..=64u16 {
            let l = layout(Rect::new(0, 0, 80, height), 24);
            assert_eq!(
                l.header.height
                    + l.rule_top.height
                    + l.main.height
                    + l.rule_bottom.height
                    + l.footer.height,
                height,
                "height {height} lost a row"
            );
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

    /// A long-but-valid harness/model label must not consume the whole row:
    /// the hint cluster (`^A e errors  ^A ? help`) has priority over the
    /// label and must still be visible at a typical terminal width, even
    /// though only the label itself -- not the hints -- gets ellipsis-
    /// truncated to make room.
    #[test]
    fn header_keeps_the_hint_cluster_visible_behind_an_absurdly_long_model_name() {
        let mut facts = base_facts();
        facts.harness =
            "claude-".to_string() + &"opus-4-1-20260830-preview-extra-long-alias".repeat(4);
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("errors"), "hints missing: {text}");
        assert!(text.contains("help"), "hints missing: {text}");
        // The chip stays intact even though the label had to give up room.
        assert!(text.contains("zirv"), "chip missing: {text}");
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
            score: None,
            state,
            attached: true,
            selected: false,
            focused: false,
            supervised: true,
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
            term.draw(|f| render_sidebar(f, Rect::new(0, 0, width, 6), &rows, 0, 40, 60))
                .expect("draw");
        }
        let text = render_and_capture_text(Rect::new(0, 0, 40, 6), |f, area| {
            render_sidebar(f, area, &rows, 0, 40, 60)
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
                score: None,
                state: RowState::Working,
                attached: true,
                selected: false,
                focused: true,
                supervised: true,
            },
            SidebarRow {
                short: "bbb22222".to_string(),
                harness: "codex".to_string(),
                age_secs: Some(5),
                score: None,
                state: RowState::Unknown,
                attached: false,
                selected: true,
                focused: false,
                supervised: true,
            },
        ];
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 6), &rows, 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        // Issue #209/v3 §A4: the sidebar no longer draws its own box, so row
        // 0 is the frame's own first row (`y = 0`), not one row in from a
        // border that no longer exists.
        assert!(
            !buf[(2, 0)].modifier.contains(Modifier::DIM),
            "an attached pane row is drawn at full strength"
        );
        assert!(
            buf[(2, 1)].modifier.contains(Modifier::DIM),
            "a view-only row is dimmed"
        );
        assert!(
            buf[(2, 1)].modifier.contains(Modifier::REVERSED),
            "and still carries the selection highlight the cursor put on it"
        );
    }

    /// A long session list scrolls the window to keep the selected row on
    /// screen. Issue #209/v3 §A4: the sidebar no longer has a title to say
    /// which window this is (the box that title lived in is gone), so this
    /// only exercises the scrolling itself now.
    #[test]
    fn a_long_session_list_scrolls_to_keep_the_selected_row_on_screen() {
        let row = |i: usize, selected: bool| SidebarRow {
            short: format!("sess{i:04}"),
            harness: String::new(),
            age_secs: None,
            score: None,
            state: RowState::Idle,
            attached: true,
            selected,
            focused: false,
            supervised: true,
        };
        let rows: Vec<SidebarRow> = (0..12).map(|i| row(i, i == 10)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows, 0, 40, 60)
        });
        assert!(
            text.contains("sess0010"),
            "the selected row must be on screen: {text}"
        );
        assert!(
            !text.contains("sess0000"),
            "and the window has scrolled off the top: {text}"
        );

        let rows: Vec<SidebarRow> = (0..3).map(|i| row(i, i == 0)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows, 0, 40, 60)
        });
        assert!(
            text.contains("sess0000") && text.contains("sess0002"),
            "{text}"
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
            term.draw(|f| render_sidebar(f, area, &rows, 0, 40, 60))
                .expect("draw");
        }
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &[], 0, 40, 60))
            .expect("draw");
    }

    // Issue #209/v3 §B: the selected-row REVERSED-over-colored-glyph bug fix.

    /// A selected row's own state glyph (cyan for `Working`, in this case)
    /// must drop its colour entirely and join the row's uniform reversed
    /// style -- not layer REVERSED on top of its own fg, which used to
    /// render a colored block with a background-colored glyph.
    #[test]
    fn a_selected_rows_state_glyph_carries_no_explicit_fg() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Working);
        row.selected = true;
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        let glyph_cell = &buf[(0, 0)];
        assert_eq!(
            glyph_cell.fg,
            Color::Reset,
            "a selected row's glyph must carry no explicit fg colour, got {:?}",
            glyph_cell.fg
        );
        assert!(
            glyph_cell.modifier.contains(Modifier::REVERSED),
            "and still reverses uniformly with the rest of the row"
        );
    }

    /// The same fix, for a selected row whose rot glyph is coloured
    /// (rotting, red-bold): it must drop that colour too when selected.
    #[test]
    fn a_selected_rows_rot_glyph_carries_no_explicit_fg_either() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(90); // well past any default compact_at threshold
        row.selected = true;
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        for x in 0..40u16 {
            let cell = &buf[(x, 0)];
            assert_eq!(
                cell.fg,
                Color::Reset,
                "no cell in a selected row may carry an explicit fg colour, got {:?} at column {x}",
                cell.fg
            );
        }
    }

    /// The list-dialog primitive's own cursor row has the identical defect
    /// with `ListDialogRow::glyph` (the QuitConfirm spinner, in practice):
    /// its glyph must not carry its own colour once reversed either.
    #[test]
    fn a_list_dialog_cursor_rows_glyph_carries_no_explicit_fg() {
        let spec = ListDialogSpec {
            title: "quit",
            count: None,
            rows: vec![ListDialogRow {
                text: "wrk claude".to_string(),
                checked: None,
                glyph: Some(("\u{2807}".to_string(), style::tui::accent())),
            }],
            cursor: Some(0),
            footer: &[],
            warn: false,
            empty_message: "",
        };
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_list_dialog(f, f.area(), &spec))
            .expect("draw");
        let buf = term.backend().buffer();
        let rect = centered(Rect::new(0, 0, 40, 10), dialog_width(40), 5);
        // The cursor row is the first content row inside the frame's border
        // and horizontal padding.
        let row_y = rect.y + 1;
        let glyph_x = rect.x + 2;
        let cell = &buf[(glyph_x, row_y)];
        assert_eq!(
            cell.fg,
            Color::Reset,
            "the cursor row's glyph must carry no explicit fg colour, got {:?}",
            cell.fg
        );
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    // Issue #209/v3 §C: the sidebar rot column.

    #[test]
    fn rot_band_thresholds_match_advise_and_compact_at() {
        assert_eq!(rot_band_for(0, 40, 60), RotBand::Fresh);
        assert_eq!(rot_band_for(39, 40, 60), RotBand::Fresh);
        assert_eq!(rot_band_for(40, 40, 60), RotBand::Warming);
        assert_eq!(rot_band_for(59, 40, 60), RotBand::Warming);
        assert_eq!(rot_band_for(60, 40, 60), RotBand::Rotting);
        assert_eq!(rot_band_for(100, 40, 60), RotBand::Rotting);
    }

    /// A fresh score renders in the sidebar with no colour of its own --
    /// it inherits the row's tone (here: none, since the row is neither
    /// dim nor focused nor selected).
    #[test]
    fn a_fresh_sidebar_score_inherits_the_row_tone() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(12);
        let text = sidebar_row_text(&row, 0, 40);
        assert!(text.contains("\u{273b}12"), "got {text:?}");
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        // `⠹ aaa11111 claude ✻12  1m` -- the rot glyph sits right after the
        // harness; scanning the row for it directly keeps this independent
        // of exact column math.
        let rot_col = (0..40u16)
            .find(|&x| buf[(x, 0)].symbol() == "\u{273b}")
            .expect("rot glyph must be drawn somewhere on the row");
        assert_eq!(
            buf[(rot_col, 0)].fg,
            Color::Reset,
            "fresh must carry no colour of its own"
        );
    }

    #[test]
    fn a_warming_sidebar_score_is_yellow() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(47);
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        let rot_col = (0..40u16)
            .find(|&x| buf[(x, 0)].symbol() == "\u{273b}")
            .expect("rot glyph must be drawn");
        assert_eq!(buf[(rot_col, 0)].fg, Color::Yellow);
    }

    #[test]
    fn a_rotting_sidebar_score_is_red_and_bold() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(81);
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        let rot_col = (0..40u16)
            .find(|&x| buf[(x, 0)].symbol() == "\u{273b}")
            .expect("rot glyph must be drawn");
        let cell = &buf[(rot_col, 0)];
        assert_eq!(cell.fg, Color::Red);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    /// A dead pane always shows the placeholder, even with a stale score
    /// still cached from before it exited -- a dead pane's last reading is
    /// stale, not a live verdict.
    #[test]
    fn a_dead_pane_shows_the_placeholder_regardless_of_a_cached_score() {
        let mut row = sidebar_row("aaa11111", "codex", RowState::Dead);
        row.score = Some(12);
        let text = sidebar_row_text(&row, 0, 40);
        assert!(
            text.contains(style::PLACEHOLDER),
            "dead pane must show the placeholder: {text:?}"
        );
        assert!(
            !text.contains('\u{273b}'),
            "and never the rot glyph: {text:?}"
        );
    }

    /// No cached score at all (an idle/working pane whose transcript has not
    /// resolved yet) also shows the placeholder, never a fabricated zero.
    #[test]
    fn no_cached_score_shows_the_placeholder() {
        let row = sidebar_row("aaa11111", "claude", RowState::Idle);
        let text = sidebar_row_text(&row, 0, 40);
        assert!(text.contains(style::PLACEHOLDER), "got {text:?}");
    }

    /// §C's own width-degradation order: the age column drops first, then
    /// the rot score's own digits (the glyph survives alone), and only then
    /// does anything about `short`/`harness` give way.
    #[test]
    fn sidebar_rot_column_degrades_age_first_then_the_score_number() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(47);
        // Plenty of room: age and the full score both show.
        let wide = sidebar_row_text(&row, 0, 40);
        assert!(wide.contains("\u{273b}47"), "got {wide:?}");
        assert!(wide.contains("1m"), "got {wide:?}");

        // Narrower: age drops, the score number survives.
        let mut cols = 40u16;
        let mut lost_age_at = None;
        while cols > 0 {
            let text = sidebar_row_text(&row, 0, cols);
            if !text.contains("1m") && text.contains("\u{273b}47") {
                lost_age_at = Some(cols);
                break;
            }
            cols -= 1;
        }
        assert!(
            lost_age_at.is_some(),
            "age must drop while the full score number still fits"
        );

        // Narrower still: the score number drops too, but the coloured
        // glyph survives alone.
        let mut lost_number_at = None;
        while cols > 0 {
            let text = sidebar_row_text(&row, 0, cols);
            if !text.contains("\u{273b}47") && text.contains('\u{273b}') {
                lost_number_at = Some(cols);
                break;
            }
            cols -= 1;
        }
        assert!(
            lost_number_at.is_some(),
            "the score's own digits must drop before the glyph itself does"
        );
    }

    /// Every sidebar row, at every width, never exceeds the column budget --
    /// the same invariant `a_sidebar_row_degrades_gracefully_at_every_width`
    /// already holds for the pre-rot-column shape, extended to a row that
    /// actually carries a score.
    #[test]
    fn a_sidebar_row_with_a_score_degrades_gracefully_at_every_width() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Working);
        row.score = Some(47);
        for cols in 0..=64u16 {
            let text = sidebar_row_text(&row, 3, cols);
            assert!(
                style::display_width(&text) <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
    }

    // Issue #209/v3 §D: the footer signal row.

    fn alive_footer_facts() -> FooterAliveFacts {
        FooterAliveFacts {
            harness: "claude".to_string(),
            score: Some(12),
            usage_five_hour: Some(61.0),
            usage_seven_day: Some(18.0),
            unread_mail: 0,
            workflow: FooterWorkflow::Active {
                kind: "feature".to_string(),
                step: "design".to_string(),
                gated: false,
            },
            supervised: true,
            stalled: false,
        }
    }

    /// The mock's own "healthy" example (§04): fresh green verdict with a
    /// dim score number, usage windows, no mail, the workflow's kind and
    /// step, and supervised.
    #[test]
    fn footer_renders_the_healthy_example() {
        let facts = FooterFacts::Alive(alive_footer_facts());
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("claude"), "got {text:?}");
        assert!(text.contains("fresh"), "got {text:?}");
        assert!(text.contains("12"), "got {text:?}");
        assert!(text.contains("61%"), "got {text:?}");
        assert!(text.contains("18%"), "got {text:?}");
        assert!(text.contains("feature"), "got {text:?}");
        assert!(text.contains("design"), "got {text:?}");
        assert!(text.contains("supervised"), "got {text:?}");
        assert!(
            !text.contains("stalled"),
            "an unlatched session must not render the stalled badge: got {text:?}"
        );
    }

    /// Issue #310: an armed stall latch overrides the ordinary
    /// supervised/unsupervised segment with a `stalled` badge.
    #[test]
    fn footer_renders_a_stalled_badge_when_the_latch_is_armed() {
        let mut alive = alive_footer_facts();
        alive.stalled = true;
        let facts = FooterFacts::Alive(alive);
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("stalled"), "got {text:?}");
        assert!(
            !text.contains("supervised"),
            "the stalled badge replaces the supervised segment: got {text:?}"
        );
    }

    /// The mock's own "attention" example: warming, unread mail, and a
    /// gated workflow step -- `▸ {step} awaits approval`.
    #[test]
    fn footer_renders_the_attention_example() {
        let facts = FooterFacts::Alive(FooterAliveFacts {
            harness: "claude".to_string(),
            score: Some(47),
            usage_five_hour: Some(62.0),
            usage_seven_day: Some(31.0),
            unread_mail: 2,
            workflow: FooterWorkflow::Active {
                kind: "feature".to_string(),
                step: "spec".to_string(),
                gated: true,
            },
            supervised: true,
            stalled: false,
        });
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("warming"), "got {text:?}");
        assert!(text.contains('2'), "unread mail count: got {text:?}");
        assert!(
            text.contains("spec awaits approval"),
            "gated step reads as awaiting approval: got {text:?}"
        );
    }

    /// A rotting verdict, and a session with no unread mail (the dim
    /// placeholder, never a literal `0`).
    #[test]
    fn footer_renders_a_rotting_verdict_and_dim_placeholder_mail() {
        let mut alive = alive_footer_facts();
        alive.score = Some(90);
        alive.unread_mail = 0;
        let facts = FooterFacts::Alive(alive);
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("rotting"), "got {text:?}");
        assert!(text.contains(style::PLACEHOLDER), "got {text:?}");
    }

    /// A session with no active workflow at all renders the dim `▸ –`
    /// placeholder.
    #[test]
    fn footer_renders_no_active_workflow_as_a_dim_placeholder() {
        let mut alive = alive_footer_facts();
        alive.workflow = FooterWorkflow::None;
        let facts = FooterFacts::Alive(alive);
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains(style::PLACEHOLDER), "got {text:?}");
    }

    /// The mock's own "dead pane focused" example: a different message
    /// entirely -- exited-age and the restore hint, not the alive shape.
    #[test]
    fn footer_renders_the_dead_pane_example() {
        let facts = FooterFacts::Dead(FooterDeadFacts {
            harness: "codex".to_string(),
            exited_age_secs: Some(12 * 60),
            workflow: FooterWorkflow::Active {
                kind: "feature".to_string(),
                step: "implement".to_string(),
                gated: false,
            },
        });
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("codex"), "got {text:?}");
        assert!(text.contains("exited"), "got {text:?}");
        assert!(text.contains("12m"), "got {text:?}");
        assert!(text.contains("feature"), "got {text:?}");
        assert!(text.contains("implement"), "got {text:?}");
        assert!(text.contains("restore"), "got {text:?}");
    }

    /// `FooterFacts::None` (nothing focused at all) draws nothing.
    #[test]
    fn footer_draws_nothing_when_nothing_is_focused() {
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &FooterFacts::None, 40, 60)
        });
        assert!(text.trim().is_empty(), "got {text:?}");
    }

    /// §D's own workflow-segment states, at the pure `footer_workflow_spans`
    /// level: no active workflow, an ungated step (dim), and a gated step
    /// (yellow-bold with "awaits approval"/"!" wording).
    #[test]
    fn footer_workflow_segment_states() {
        let (full, compressed) = footer_workflow_spans(&FooterWorkflow::None);
        let joined = |seg: &FooterSeg| seg.iter().map(|(t, _)| t.as_str()).collect::<String>();
        assert!(joined(&full).contains(style::PLACEHOLDER));
        assert_eq!(full, compressed);

        let (full, compressed) = footer_workflow_spans(&FooterWorkflow::Active {
            kind: "feature".to_string(),
            step: "design".to_string(),
            gated: false,
        });
        assert!(joined(&full).contains("feature"));
        assert!(joined(&full).contains("design"));
        assert!(joined(&compressed).contains("design"));
        assert!(!joined(&compressed).contains("feature"));
        for (_, style) in full.iter().chain(compressed.iter()) {
            assert!(
                !style.add_modifier.contains(Modifier::BOLD),
                "an ungated workflow segment must not be bold"
            );
        }

        let (full, compressed) = footer_workflow_spans(&FooterWorkflow::Active {
            kind: "feature".to_string(),
            step: "spec".to_string(),
            gated: true,
        });
        assert!(joined(&full).contains("spec awaits approval"));
        assert!(joined(&compressed).contains("spec!"));
        for (_, style) in full.iter().chain(compressed.iter()) {
            assert_eq!(style.fg, Some(Color::Yellow));
            assert!(style.add_modifier.contains(Modifier::BOLD));
        }
    }

    /// §D's drop order: usage drops first, then the verdict's score number,
    /// then the workflow segment (full form, then compressed, then dropped
    /// entirely), then the harness label -- verdict, mail and supervision
    /// never drop.
    #[test]
    fn footer_drop_order_matches_the_spec() {
        let facts = FooterFacts::Alive(alive_footer_facts());
        let render = |cols: u16| {
            render_and_capture_text(Rect::new(0, 0, cols, 1), |f, area| {
                render_footer(f, area, &facts, 40, 60)
            })
        };

        let wide = render(80);
        assert!(wide.contains("61%"), "usage shows at 80 cols: {wide:?}");
        assert!(
            wide.contains("claude"),
            "harness shows at 80 cols: {wide:?}"
        );
        assert!(wide.contains("feature"), "workflow long form: {wide:?}");

        // Narrowing must drop usage before it drops the harness, and the
        // workflow's long form before it drops the harness too -- never the
        // verdict word, mail, or supervision.
        let mut saw_usage_drop = false;
        let mut saw_workflow_compress = false;
        let mut saw_harness_drop = false;
        for cols in (0..=80u16).rev() {
            let text = render(cols);
            if !text.contains('\u{25d4}') && !saw_usage_drop && text.contains("claude") {
                saw_usage_drop = true;
            }
            if !text.contains("feature") && text.contains("design") && !saw_workflow_compress {
                saw_workflow_compress = true;
            }
            if !text.contains("claude") {
                saw_harness_drop = true;
                // Once the harness is gone, the irreducible core must
                // survive: verdict word, mail and supervision are never
                // dropped outright (down to whatever `cols` can still hold).
                if cols >= 20 {
                    assert!(
                        text.contains("fresh") || text.contains(style::PLACEHOLDER),
                        "verdict must survive at {cols} cols: {text:?}"
                    );
                }
                break;
            }
        }
        assert!(saw_usage_drop, "usage must drop before the harness does");
        assert!(
            saw_workflow_compress,
            "the workflow segment must compress before the harness drops"
        );
        assert!(saw_harness_drop, "the harness must eventually drop too");
    }

    /// The mock's own 44-column drop-order example (§04): warming verdict
    /// (score number already gone), unread mail, the workflow compressed to
    /// `spec!` (gated -- the `!` suffix), supervised -- and, critically, NO
    /// harness label at all. Issue #209/v3 codex review finding 4: the
    /// harness must drop while the workflow segment is still compressed,
    /// not the other way around.
    #[test]
    fn footer_44_col_mock_example_renders_exactly() {
        let facts = FooterFacts::Alive(FooterAliveFacts {
            harness: "claude".to_string(),
            score: Some(47),
            usage_five_hour: Some(62.0),
            usage_seven_day: Some(31.0),
            unread_mail: 2,
            workflow: FooterWorkflow::Active {
                kind: "feature".to_string(),
                step: "spec".to_string(),
                gated: true,
            },
            supervised: true,
            stalled: false,
        });
        let text = render_and_capture_text(Rect::new(0, 0, 44, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("warming"), "got {text:?}");
        assert!(
            !text.contains("47"),
            "the score number must have dropped by 44 cols: got {text:?}"
        );
        assert!(text.contains('2'), "unread mail count: got {text:?}");
        assert!(
            text.contains("spec!"),
            "compressed gated workflow: got {text:?}"
        );
        assert!(text.contains("supervised"), "got {text:?}");
        assert!(
            !text.contains("claude"),
            "the harness must have dropped by 44 cols: got {text:?}"
        );
        assert!(
            style::display_width(&text) <= 44,
            "must never exceed its own column budget: got {text:?}"
        );
    }

    /// The dead-pane footer's own drop order: the exited notice and the
    /// restore hint are never dropped, even at an absurdly narrow width.
    #[test]
    fn footer_dead_pane_never_drops_the_exited_notice_or_restore_hint() {
        let facts = FooterFacts::Dead(FooterDeadFacts {
            harness: "codex".to_string(),
            exited_age_secs: Some(720),
            workflow: FooterWorkflow::None,
        });
        // Tight but not absurd: the harness and workflow segment have
        // dropped, but the exited notice and restore hint -- the one thing
        // this state exists to tell the operator -- still fit and survive.
        let tight = render_and_capture_text(Rect::new(0, 0, 35, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(tight.contains("exited"), "got {tight:?}");
        assert!(tight.contains("restore"), "got {tight:?}");
        assert!(
            !tight.contains("codex"),
            "harness must have dropped by 30 cols: {tight:?}"
        );

        // And at every width, including absurdly narrow ones, the row never
        // exceeds its own column budget -- the release profile is `panic =
        // "abort"`, so an overflow here would take the operator's terminal
        // with it.
        for cols in [80u16, 40, 20, 10, 1, 0] {
            let text = render_and_capture_text(Rect::new(0, 0, cols, 1), |f, area| {
                render_footer(f, area, &facts, 40, 60)
            });
            assert!(
                style::display_width(&text) <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
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
    fn dialog_row_count_saturates_instead_of_wrapping_or_overflowing() {
        // Before this existed, `rows as u16 + extra` would silently wrap on
        // a five-digit row count and, right at the u16 boundary, overflow
        // the subsequent `+` -- which panics in every debug/test build
        // (this profile has no `overflow-checks` override).
        assert_eq!(dialog_row_count(0, 4), 4);
        assert_eq!(dialog_row_count(10, 4), 14);
        assert_eq!(dialog_row_count(u16::MAX as usize, 4), u16::MAX);
        assert_eq!(dialog_row_count(100_000, 4), u16::MAX);
        assert_eq!(dialog_row_count(usize::MAX, 4), u16::MAX);
    }

    /// A pathologically large row count (tens of thousands of mail rows, say)
    /// must render without panicking and the resulting height must never
    /// exceed the area it was clamped against -- the same guarantee the
    /// small-row-count dialogs already have, just exercised at a size where
    /// the old `content_rows as u16 + 2 + 2` cast/add could wrap or overflow.
    #[test]
    fn render_list_dialog_never_panics_on_an_enormous_row_count_or_a_very_wide_row() {
        let mut rows: Vec<ListDialogRow> = (0..70_000)
            .map(|i| ListDialogRow::plain(format!("row {i}")))
            .collect();
        // A single absurdly wide row, too: `display_width` on it is far
        // larger than any real terminal, exercising the same cast risk on
        // the cursor row's own reversed-padding width math.
        rows.push(ListDialogRow::plain("x".repeat(200_000)));
        let cursor = Some(rows.len() - 1);

        let spec = ListDialogSpec {
            title: "mail",
            count: Some(rows.len()),
            rows,
            cursor,
            footer: MAIL_FOOTER,
            warn: false,
            empty_message: "(no mail)",
        };

        for area in [
            Rect::new(0, 0, 10, 3),
            Rect::new(0, 0, 40, 10),
            Rect::new(0, 0, 200, 60),
        ] {
            let backend = TestBackend::new(area.width, area.height);
            let mut term = Terminal::new(backend).expect("terminal");
            term.draw(|f| render_list_dialog(f, area, &spec))
                .expect("draw must not panic on an enormous row count");
            let buf = term.backend().buffer();
            assert_eq!(buf.area.width, area.width);
            assert_eq!(buf.area.height, area.height);
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
        // Issue #209/v3 §A5: `MAIL_FOOTER` grew a `j/k move` hint, widening
        // the "dialogs:" section's own `mail` row from 43 to 52 chars --
        // still comfortably inside the ~74-column interior a `dialog_
        // width(80)` overlay actually has (`render_list_dialog`'s own
        // horizontal padding), so the cap moves with it rather than the
        // hint being dropped to keep an arbitrary old number.
        for line in help_lines() {
            assert!(
                line.chars().count() <= 55,
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
