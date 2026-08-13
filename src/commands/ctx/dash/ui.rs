//! Pure renderers for the dashboard: every function in this module takes a
//! `&mut Frame` plus already-computed data and draws into it. No I/O, no
//! filesystem, no environment, no clock -- Task 5's event loop assembles the
//! facts (from panes, the session registry, mail/memory) and calls straight
//! through here, and every renderer is exercised with `ratatui::backend::
//! TestBackend` precisely because there is nothing else to stub.
//!
//! `SpawnDraft`/`NudgeDraft`/`MailView`/`MemoryView`/`RestoreView` are seeded
//! here with only the fields rendering needs today (`input`, `items`,
//! `cursor`); Tasks 8/9/12 own filling in whatever richer shape their own
//! overlay reducers need next.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use super::pane::PaneState;

/// The header row's live facts. `mail_broadcast`/`mail_direct` render as
/// "mail B+D" only when at least one is non-zero, mirroring `wrap.rs`'s own
/// convention of omitting a segment entirely when mail is disabled rather
/// than showing a hollow "mail 0+0".
pub struct HeaderFacts {
    pub harness: String,
    pub score: Option<u32>,
    pub usage_pct: Option<u8>,
    pub mail_broadcast: usize,
    pub mail_direct: usize,
    pub memory_count: usize,
    pub sessions: usize,
}

/// One sidebar row: a dashboard pane (`attached: true`) or a view-only
/// registry session this dashboard did not spawn (`attached: false`).
pub struct SidebarRow {
    pub glyph: char,
    pub title: String,
    pub short: String,
    pub preview: String,
    pub attached: bool,
    pub selected: bool,
}

/// A minimal draft/view struct shared by the overlay seams below. Only what
/// `render_overlay` needs to draw something today; Tasks 8/9/12 fill in
/// richer fields (mail items as `(from, body)` pairs, a compose sub-draft,
/// and so on) as they wire up each overlay's own reducer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnDraft {
    pub input: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NudgeDraft {
    pub input: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailView {
    pub input: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryView {
    pub input: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreView {
    pub input: String,
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
    /// Constructed only by Task 12's own startup restore dialog -- nothing
    /// in this plan's Task 5 scope has a roster to offer yet.
    #[allow(dead_code)]
    Restore(RestoreView),
}

/// Splits `area` into (header, sidebar, main): one header row, a
/// `sidebar_cols`-wide sidebar, a one-column separator, and everything else
/// as the active pane's grid.
pub fn layout(area: Rect, sidebar_cols: u16) -> (Rect, Rect, Rect) {
    let header_h = 1.min(area.height);
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

pub fn render_header(f: &mut Frame, area: Rect, facts: &HeaderFacts) {
    let mut parts = vec![facts.harness.clone()];
    if let Some(score) = facts.score {
        parts.push(format!("score {score}"));
    }
    if let Some(pct) = facts.usage_pct {
        parts.push(format!("usage {pct}%"));
    }
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

    let line = parts.join("  ");
    f.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

pub fn render_sidebar(f: &mut Frame, area: Rect, rows: &[SidebarRow]) {
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let attach_marker = if row.attached { ' ' } else { '~' };
            let text = format!(
                "{} {}{:<8} {} {}",
                row.glyph, attach_marker, row.short, row.title, row.preview
            );
            let style = if row.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title("panes");
    f.render_widget(List::new(items).block(block), area);
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

fn render_dialog(f: &mut Frame, area: Rect, title: &str, lines: &[String]) {
    let h = (lines.len() as u16 + 2).min(area.height);
    let w = area.width.saturating_sub(4).clamp(1, area.width);
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
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

pub fn render_overlay(f: &mut Frame, area: Rect, overlay: &Overlay) {
    match overlay {
        Overlay::None => {}
        Overlay::QuitConfirm(working) => {
            let mut lines = vec!["quit dashboard? still working:".to_string()];
            lines.extend(working.iter().cloned());
            lines.push("Enter to confirm, Esc to cancel".to_string());
            render_dialog(f, area, "quit", &lines);
        }
        Overlay::Spawn(d) => render_draft_dialog(f, area, "spawn", &d.input, &d.items, d.cursor),
        Overlay::Nudge(d) => render_draft_dialog(f, area, "nudge", &d.input, &d.items, d.cursor),
        Overlay::Mail(d) => render_draft_dialog(f, area, "mail", &d.input, &d.items, d.cursor),
        Overlay::Memory(d) => render_draft_dialog(f, area, "memory", &d.input, &d.items, d.cursor),
        Overlay::Restore(d) => {
            render_draft_dialog(f, area, "restore", &d.input, &d.items, d.cursor)
        }
    }
}

/// The sidebar/grid glyph for a pane's state: `●` Working, `○` Idle, `⏸`
/// WaitingInput, `✕` Ended (the exit code is not part of the glyph -- the
/// sidebar preview text carries it when it matters).
pub fn glyph_for(state: &PaneState) -> char {
    match state {
        PaneState::Working => '●',
        PaneState::Idle => '○',
        PaneState::WaitingInput => '⏸',
        PaneState::Ended(_) => '✕',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    #[test]
    fn layout_reserves_one_header_row_and_the_sidebar() {
        let (h, s, m) = layout(Rect::new(0, 0, 100, 30), 24);
        assert_eq!(h.height, 1);
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

    fn base_facts() -> HeaderFacts {
        HeaderFacts {
            harness: "claude".to_string(),
            score: None,
            usage_pct: None,
            mail_broadcast: 0,
            mail_direct: 0,
            memory_count: 0,
            sessions: 1,
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

    #[test]
    fn glyphs_match_the_spec() {
        assert_eq!(glyph_for(&PaneState::Working), '●');
        assert_eq!(glyph_for(&PaneState::Idle), '○');
        assert_eq!(glyph_for(&PaneState::WaitingInput), '⏸');
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

    #[test]
    fn render_overlay_quit_confirm_lists_working_panes() {
        let overlay = Overlay::QuitConfirm(vec!["wrk claude".to_string(), "wrk codex".to_string()]);
        let area = Rect::new(0, 0, 40, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay));
        assert!(text.contains("wrkclaude") || text.contains("wrk claude"));
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
