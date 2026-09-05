//! Pointer ownership over the geometry of the last completed draw.
//!
//! Issue #354 gave the dashboard clickable chrome. Every pointer event is
//! resolved here, against a [`FrameSnapshot`] captured when that frame was
//! drawn -- never against the live layout, which may already have moved on by
//! the time the event is read. The module is deliberately pure: no frame, no
//! terminal, no clock, so the whole of "what did the operator just click on"
//! is testable as a function of two integers.
//!
//! The dispatch that acts on a [`Hit`] lives in `dash::route_mouse`; this only
//! answers *what is there*.
use crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};

/// A sidebar session row's stable identity: the session short id. Stable
/// across re-renders and across the roster re-ordering itself, which is what
/// lets a click name a row rather than a screen line.
pub type RowId = String;

/// A work group's own id (`Pane::work_group_id`).
pub type GroupId = String;

/// One `^A x label` chord in the header's (or, later, the footer's) hint
/// cluster. The cluster is context-sensitive, so the id -- not the position
/// -- is what a hit reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintId {
    Actions,
    Nudge,
    Mail,
    Errors,
    /// `^A i` -- the per-session inspector (phase 3).
    Inspect,
    /// `^A r` -- relaunch an ended/reaped row from the spawn request the
    /// dashboard kept for it (phase 3). Only ever drawn for a row that can
    /// actually be restored; see `ui::HintContext::restorable`.
    Restore,
    Help,
    /// A hint on an OPEN DIALOG's own pinned hint row (`⏎ open`, `esc back`,
    /// `space toggle`, ...), carrying the key it stands for rather than a
    /// dashboard-level action: clicking it feeds exactly that keystroke to
    /// whichever overlay reducer is up, so a pointer can never reach a code
    /// path the keyboard could not. Hints that stand for more than one key
    /// (`j/k`, `↑↓`) get no region at all -- see `ui::footer_key_code`.
    DialogKey(KeyCode),
}

/// What sits under the pointer.
///
/// `Grid` is the only variant that belongs to the focused child; everything
/// else is the dashboard's own chrome, and `dash::route_mouse` guarantees
/// none of it reaches `Pane::forward_mouse_button`, `Pane::scroll_wheel` or
/// `Pane::write_input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A header hint chord.
    HeaderHint(HintId),
    /// The roster's first line (`  <n> live` plus the glyph counts).
    SidebarSummary,
    /// A work group's header, including its disclosure triangle.
    GroupToggle(GroupId),
    /// A session row -- including any disclosure lines drawn under it, which
    /// belong to the same row for hit-testing purposes.
    SidebarRow(RowId),
    /// The one-column rule between the sidebar and the grid.
    Divider,
    /// The focused pane's terminal.
    Grid,
    /// A footer hint chord (none exist yet; see `ui::frame_snapshot`).
    FooterHint(HintId),
    /// One visible row of an open list dialog, by its index into that
    /// dialog's own full row list (NOT its screen line): click selects,
    /// double-click or Enter activates.
    OverlayRow(usize),
    /// One hint on an open dialog's pinned hint row.
    OverlayHint(HintId),
    /// Inside an open overlay, but on none of its own targets -- its border,
    /// its blank line, a row past the end of a short list. Consumed, and a
    /// no-op.
    Overlay,
    /// Outside an open overlay, while one is open. Consumed, and a no-op:
    /// a stray click next to a modal never closes it and never reaches the
    /// child underneath.
    ModalBackdrop,
    /// Outside the frame, or a gap that owns nothing.
    None,
}

/// The geometry of one drawn frame. Built by `ui::frame_snapshot` from the
/// same layout and [`ui::RosterFrame`](super::ui::RosterFrame) that were
/// rendered, so a hit can never name a row the frame did not actually draw.
///
/// Every rect is in screen coordinates and half-open: a rect covers `x` up to
/// but not including `right()`, and `y` up to but not including `bottom()`.
/// Zero-size rects hit nothing, which is how "no divider fits" and "no
/// sidebar (zoomed)" are expressed without a special case at every lookup.
#[derive(Debug, Clone, Default)]
pub struct FrameSnapshot {
    /// The whole terminal.
    pub frame: Rect,
    /// The sidebar column, empty while zoomed. The wheel treats this as one
    /// region regardless of which entry it lands on.
    pub sidebar: Rect,
    /// The divider column between sidebar and grid, empty when none fits.
    pub divider: Rect,
    /// The focused pane's grid; the whole frame while zoomed.
    pub grid: Rect,
    /// Drawn roster entries, in draw order, each rect covering the entry and
    /// its disclosure lines.
    pub rows: Vec<(Rect, Hit)>,
    /// Every entry the roster has this frame, drawn or not: the keyboard's
    /// navigation order and the viewport's index space. Carried here so the
    /// event loop has one place to read the frame's own tree from.
    pub roster: Vec<Hit>,
    /// The header's hint chords and where they were drawn.
    pub header_hints: Vec<(Rect, HintId)>,
    /// The footer's hint chords, if it ever grows any.
    pub footer_hints: Vec<(Rect, HintId)>,
    /// Whether this frame was drawn zoomed (no chrome at all).
    pub zoomed: bool,
    /// The open overlay's own rect, if one was open.
    pub overlay: Option<Rect>,
    /// The open list dialog's visible rows, in draw order, each paired with
    /// its index into that dialog's own full row list. Empty for a dialog
    /// with no rows (a draft/compose buffer), and for no overlay at all.
    pub overlay_rows: Vec<(Rect, usize)>,
    /// The open dialog's own pinned hint row, span by span.
    pub overlay_hints: Vec<(Rect, HintId)>,
    /// How many list rows the open dialog had room for -- the page size its
    /// PageUp/PageDown and its wheel move by. Zero with no dialog open, and
    /// for one with no room for a single row.
    pub overlay_capacity: usize,
}

/// Pure: what `(x, y)` landed on in the frame `snap` describes.
///
/// Order is the behaviour contract's own layering, outside in: outside the
/// frame owns nothing; an open overlay owns the entire frame (its own hint
/// row, its own visible rows, the rest of itself, and the backdrop around
/// it); then the chrome, nearest-drawn first -- header hints, footer hints,
/// roster entries, divider -- and finally the grid.
///
/// Phase 3 refines the phase-1 "an overlay consumes everything" rule: the
/// dialog's own targets are addressable (`OverlayHint` first, since a hint
/// span sits inside the dialog rect, then `OverlayRow`), everything else
/// inside it is `Overlay` and everything outside it is `ModalBackdrop` --
/// both still consumed, neither ever reaching the child.
pub fn hit_test(snap: &FrameSnapshot, x: u16, y: u16) -> Hit {
    let point = Position::new(x, y);
    let contains = |rect: Rect| !rect.is_empty() && rect.contains(point);
    if !contains(snap.frame) {
        return Hit::None;
    }
    if let Some(overlay) = snap.overlay {
        if !contains(overlay) {
            return Hit::ModalBackdrop;
        }
        for (rect, id) in &snap.overlay_hints {
            if contains(*rect) {
                return Hit::OverlayHint(*id);
            }
        }
        for (rect, index) in &snap.overlay_rows {
            if contains(*rect) {
                return Hit::OverlayRow(*index);
            }
        }
        return Hit::Overlay;
    }
    if !snap.zoomed {
        for (rect, id) in &snap.header_hints {
            if contains(*rect) {
                return Hit::HeaderHint(*id);
            }
        }
        for (rect, id) in &snap.footer_hints {
            if contains(*rect) {
                return Hit::FooterHint(*id);
            }
        }
        for (rect, hit) in &snap.rows {
            if contains(*rect) {
                return hit.clone();
            }
        }
        if contains(snap.divider) {
            return Hit::Divider;
        }
    }
    if contains(snap.grid) {
        Hit::Grid
    } else {
        Hit::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The approved 200×50 frame, with a work group whose lead is selected:
    /// summary line, group header, the lead plus its eight disclosure lines,
    /// then a plain child.
    fn group_frame() -> FrameSnapshot {
        FrameSnapshot {
            frame: Rect::new(0, 0, 200, 50),
            sidebar: Rect::new(0, 2, 44, 46),
            divider: Rect::new(44, 2, 1, 46),
            grid: Rect::new(45, 2, 155, 46),
            rows: vec![
                (Rect::new(0, 2, 44, 1), Hit::SidebarSummary),
                (Rect::new(0, 3, 44, 1), Hit::GroupToggle("g".into())),
                (Rect::new(0, 4, 44, 9), Hit::SidebarRow("lead".into())),
                (Rect::new(0, 13, 44, 1), Hit::SidebarRow("child".into())),
            ],
            header_hints: vec![(Rect::new(180, 0, 20, 1), HintId::Help)],
            footer_hints: vec![(Rect::new(180, 49, 20, 1), HintId::Errors)],
            ..Default::default()
        }
    }

    /// Every region owns its top-left and its bottom-right cell, and neither
    /// the column past its right edge nor the row past its bottom edge --
    /// half-open, so two rects that share an edge cannot both claim it and no
    /// single column is ever unreachable between them.
    #[test]
    fn every_region_owns_exactly_its_own_half_open_rect() {
        let snap = group_frame();
        let regions = snap.rows.clone().into_iter().chain([
            (snap.divider, Hit::Divider),
            (snap.grid, Hit::Grid),
            (snap.header_hints[0].0, Hit::HeaderHint(HintId::Help)),
            (snap.footer_hints[0].0, Hit::FooterHint(HintId::Errors)),
        ]);
        for (rect, expected) in regions {
            assert_eq!(hit_test(&snap, rect.x, rect.y), expected, "{rect:?}");
            assert_eq!(
                hit_test(&snap, rect.right() - 1, rect.bottom() - 1),
                expected,
                "{rect:?} bottom-right"
            );
            assert_ne!(hit_test(&snap, rect.right(), rect.y), expected, "{rect:?}");
            assert_ne!(hit_test(&snap, rect.x, rect.bottom()), expected, "{rect:?}");
        }
    }

    /// A group header, its children and the summary line are each their own
    /// target, and every disclosure line under the selected row still answers
    /// with that row's id -- clicking a fact belongs to the row it hangs off.
    #[test]
    fn the_summary_group_header_and_a_rows_disclosure_all_name_their_own_row() {
        let snap = group_frame();
        assert_eq!(hit_test(&snap, 10, 2), Hit::SidebarSummary);
        assert_eq!(hit_test(&snap, 0, 3), Hit::GroupToggle("g".into()));
        for y in 4..13 {
            assert_eq!(
                hit_test(&snap, 6, y),
                Hit::SidebarRow("lead".into()),
                "disclosure line {y} must belong to its own row"
            );
        }
        assert_eq!(hit_test(&snap, 6, 13), Hit::SidebarRow("child".into()));
        // Below the last drawn entry the sidebar owns nothing: the wheel
        // still scrolls there (`route_mouse` checks the column), but no row
        // may be selected by clicking empty space.
        assert_eq!(hit_test(&snap, 6, 30), Hit::None);
    }

    /// Zoomed: no sidebar, no divider, no hints -- the grid is the frame, and
    /// a click anywhere in it belongs to the child.
    #[test]
    fn a_zoomed_frame_has_no_chrome_at_all() {
        let mut snap = group_frame();
        snap.zoomed = true;
        snap.sidebar = Rect::default();
        snap.divider = Rect::default();
        snap.grid = snap.frame;
        for (x, y) in [(0, 0), (0, 3), (43, 6), (44, 2), (199, 49)] {
            assert_eq!(hit_test(&snap, x, y), Hit::Grid, "({x}, {y})");
        }
    }

    /// An open overlay owns every pointer event in the frame: inside it, or
    /// the backdrop around it. Nothing underneath is reachable, which is what
    /// makes `route_mouse` able to consume the wheel while a dialog is up.
    #[test]
    fn an_open_overlay_owns_the_whole_frame() {
        let mut snap = group_frame();
        snap.overlay = Some(Rect::new(50, 10, 100, 20));
        for y in 0..50 {
            for x in 0..200 {
                let expected = if (50..150).contains(&x) && (10..30).contains(&y) {
                    Hit::Overlay
                } else {
                    Hit::ModalBackdrop
                };
                assert_eq!(hit_test(&snap, x, y), expected, "({x}, {y})");
            }
        }
    }

    /// Issue #354 phase 3: an open list dialog's own targets. Its hint row
    /// wins over its rows (a hint span sits inside the dialog rect), its
    /// visible rows answer with their index into the FULL list -- so a
    /// scrolled dialog names the right entry, not the right screen line --
    /// and every other cell inside it is `Overlay`.
    #[test]
    fn an_open_dialogs_rows_and_hints_are_each_their_own_target() {
        let mut snap = group_frame();
        snap.overlay = Some(Rect::new(50, 10, 100, 20));
        // A dialog scrolled to entry 30: three visible rows, then the pinned
        // hint row two lines below them.
        snap.overlay_rows = vec![
            (Rect::new(52, 11, 96, 1), 30),
            (Rect::new(52, 12, 96, 1), 31),
            (Rect::new(52, 13, 96, 1), 32),
        ];
        snap.overlay_hints = vec![
            (Rect::new(52, 28, 6, 1), HintId::DialogKey(KeyCode::Enter)),
            (Rect::new(61, 28, 8, 1), HintId::DialogKey(KeyCode::Esc)),
        ];
        assert_eq!(hit_test(&snap, 52, 11), Hit::OverlayRow(30));
        assert_eq!(hit_test(&snap, 147, 13), Hit::OverlayRow(32));
        assert_eq!(
            hit_test(&snap, 53, 28),
            Hit::OverlayHint(HintId::DialogKey(KeyCode::Enter))
        );
        assert_eq!(
            hit_test(&snap, 68, 28),
            Hit::OverlayHint(HintId::DialogKey(KeyCode::Esc))
        );
        // The gap between two hints, the blank line under the last row, and
        // the dialog's own border are all inside it and all inert.
        for (x, y) in [(59, 28), (52, 20), (50, 10), (149, 29)] {
            assert_eq!(hit_test(&snap, x, y), Hit::Overlay, "({x}, {y})");
        }
        // And nothing under the dialog is reachable, however many roster
        // rows and header hints the frame also drew.
        assert_eq!(hit_test(&snap, 10, 2), Hit::ModalBackdrop);
        assert_eq!(hit_test(&snap, 6, 4), Hit::ModalBackdrop);
        assert_eq!(hit_test(&snap, 185, 0), Hit::ModalBackdrop);
        assert_eq!(hit_test(&snap, 185, 49), Hit::ModalBackdrop);
    }

    /// A row rect that survived a resize with zero size hits nothing, and the
    /// dialog still owns the cell -- never the grid underneath it.
    #[test]
    fn a_zero_size_overlay_row_or_hint_never_escapes_the_modal() {
        let mut snap = group_frame();
        snap.overlay = Some(Rect::new(50, 10, 100, 20));
        snap.overlay_rows = vec![(Rect::new(52, 11, 0, 1), 4)];
        snap.overlay_hints = vec![(Rect::new(52, 28, 6, 0), HintId::DialogKey(KeyCode::Esc))];
        assert_eq!(hit_test(&snap, 52, 11), Hit::Overlay);
        assert_eq!(hit_test(&snap, 52, 28), Hit::Overlay);
    }

    /// Zero-size rects hit nothing, everywhere: an empty frame, an empty
    /// divider (no column between sidebar and grid), an empty row rect, and
    /// an overlay that was opened with no room to draw it -- which still
    /// takes the pointer, as a backdrop.
    #[test]
    fn zero_size_rects_hit_nothing() {
        assert_eq!(hit_test(&FrameSnapshot::default(), 0, 0), Hit::None);
        let mut snap = group_frame();
        snap.divider = Rect::new(44, 2, 0, 46);
        assert_eq!(hit_test(&snap, 44, 6), Hit::None);
        snap.rows
            .push((Rect::new(0, 20, 0, 0), Hit::SidebarRow("ghost".into())));
        assert_eq!(hit_test(&snap, 0, 20), Hit::None);
        snap.overlay = Some(Rect::default());
        assert_eq!(hit_test(&snap, 0, 0), Hit::ModalBackdrop);
        assert_eq!(hit_test(&snap, 199, 49), Hit::ModalBackdrop);
    }

    /// Outside the frame entirely -- a resize race, or a terminal reporting a
    /// coordinate past its own size -- owns nothing rather than falling
    /// through to the grid.
    #[test]
    fn a_point_outside_the_frame_owns_nothing() {
        let snap = group_frame();
        for (x, y) in [(200, 0), (0, 50), (200, 50), (u16::MAX, u16::MAX)] {
            assert_eq!(hit_test(&snap, x, y), Hit::None, "({x}, {y})");
        }
    }
}
