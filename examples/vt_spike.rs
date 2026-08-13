// examples/vt_spike.rs — THROWAWAY spike for the zirv dashboard's GO/NO-GO
// gate (docs/superpowers/plans/2026-08-13-zirv-dashboard.md, Task 1). Proves
// vt100 can render an interactive claude-shaped TUI through a ConPTY. This
// file is deleted or promoted into a real dashboard module depending on the
// gate's outcome — it is not shipped as-is.
//
// Modes:
//   cargo run --example vt_spike -- --check [path]
//       Automated: feeds raw PTY bytes from `path` (default
//       tests/fixtures/claude-session.raw) through vt100 and asserts basic
//       sanity. Never spawns anything, never touches the terminal.
//
//   cargo run --example vt_spike -- [--record <path>] <program> [args...]
//       Manual: spawns `<program> [args...]` in a ConPTY, renders its output
//       through vt100 into a ratatui frame with a dummy sidebar and a
//       one-line header, and forwards keystrokes to the child. Ctrl+Q quits
//       and restores the terminal. `--record <path>` additionally appends
//       every raw output byte read from the child to `path`, so a session
//       can be captured for later use as a `--check` fixture.
//
// Windows ConPTY notes (copied from src/commands/ctx/wrap.rs:27-60 and
// wrap.rs:1040-1110 — see those for the full story): portable-pty 0.9
// hard-codes PSEUDOCONSOLE_INHERIT_CURSOR, so conhost emits an `ESC[6n`
// cursor-position probe on the pty and BLOCKS servicing the child until
// something answers it on the pty's own input side. The synthetic reply
// below must be written to the writer BEFORE `spawn_command`, or every
// child hangs forever on Windows.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, text::Text};

const DEFAULT_FIXTURE: &str = "tests/fixtures/claude-session.raw";

/// See wrap.rs:47 — the reply conhost's PSEUDOCONSOLE_INHERIT_CURSOR probe
/// is blocking on before it will service the child at all.
#[cfg(windows)]
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// Dummy sidebar width and header height the manual probe renders the child
/// PTY offset by, matching Task 1's "(terminal cols - 26, terminal rows -
/// 3)" spawn sizing.
const SIDEBAR_COLS: u16 = 26;
const RESERVED_ROWS: u16 = 3;

fn answer_inherit_cursor_probe(writer: &mut (dyn Write + Send)) {
    #[cfg(windows)]
    {
        let _ = writer.write_all(CURSOR_POSITION_REPORT);
        let _ = writer.flush();
    }
    #[cfg(not(windows))]
    let _ = writer;
}

// ---------------------------------------------------------------------
// --check mode
// ---------------------------------------------------------------------

fn check_fixture(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| {
        format!(
            "fixture missing: {} ({e}); record one first with the manual mode's --record flag, \
             e.g. `cargo run --example vt_spike -- --record {} claude` (run it, drive a short \
             session, then quit with Ctrl+Q), then re-run --check",
            path.display(),
            path.display(),
        )
    })?;

    let mut parser = vt100::Parser::new(40, 120, 0);
    parser.process(&bytes); // no panic proven by reaching the next line

    let screen = parser.screen();
    let (rows, cols) = screen.size();
    if (rows, cols) != (40, 120) {
        return Err(format!("screen size drifted to {rows}x{cols}"));
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_row >= rows || cursor_col >= cols {
        return Err(format!(
            "cursor out of bounds: ({cursor_row}, {cursor_col}) in a {rows}x{cols} screen"
        ));
    }

    let non_blank = (0..rows).any(|r| {
        (0..cols).any(|c| {
            screen
                .cell(r, c)
                .map(|cell| !cell.contents().trim().is_empty())
                .unwrap_or(false)
        })
    });
    if !non_blank {
        return Err("fixture rendered an entirely blank screen".into());
    }

    // Round-trip through a resize: must not panic and must actually apply.
    parser.screen_mut().set_size(20, 80);
    parser.process(b"after-resize");
    let (rows, cols) = parser.screen().size();
    if (rows, cols) != (20, 80) {
        return Err(format!(
            "resize did not apply: got {rows}x{cols}, expected 20x80"
        ));
    }

    Ok(())
}

fn run_check(path_arg: Option<&str>) -> i32 {
    let path = PathBuf::from(path_arg.unwrap_or(DEFAULT_FIXTURE));
    match check_fixture(&path) {
        Ok(()) => {
            println!("SPIKE CHECK: PASS");
            0
        }
        Err(e) => {
            eprintln!("SPIKE CHECK: FAIL — {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------
// manual mode
// ---------------------------------------------------------------------

enum PtyEvent {
    Output(Vec<u8>),
    Closed,
}

fn pty_dims(term_cols: u16, term_rows: u16) -> (u16, u16) {
    // (cols, rows) — matches the task's "(terminal cols - 26, terminal
    // rows - 3)" sizing, floored so a tiny terminal doesn't underflow.
    let cols = term_cols.saturating_sub(SIDEBAR_COLS).max(10);
    let rows = term_rows.saturating_sub(RESERVED_ROWS).max(5);
    (cols, rows)
}

fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// crossterm `KeyEvent` -> bytes to write to the child pty. Covers the
/// terminal basics the plan calls out: Enter, arrows, Tab, Ctrl-<x>,
/// Alt-<x>, and plain/UTF-8 characters.
fn encode_key(key: KeyEvent) -> Vec<u8> {
    if key.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = key.code
    {
        let mut buf = [0u8; 4];
        let mut v = vec![0x1b];
        v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return v;
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

fn is_quit(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn render_grid(f: &mut Frame, area: Rect, screen: &vt100::Screen) {
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
            let style = Style::default()
                .fg(map_color(cell.fgcolor()))
                .bg(map_color(cell.bgcolor()))
                .add_modifier(modifiers);
            target.set_style(style);

            if cell.is_wide() {
                skip_next = true;
            }
        }
    }
}

fn draw_ui(f: &mut Frame, screen: &vt100::Screen, recording: bool, program: &str) {
    let area = f.area();
    let header_h = 1.min(area.height);
    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: header_h,
    };
    let body_y = area.y + header_h;
    let body_h = area.height.saturating_sub(header_h);
    let sidebar_w = SIDEBAR_COLS.min(area.width);
    let sidebar = Rect {
        x: area.x,
        y: body_y,
        width: sidebar_w,
        height: body_h,
    };
    let main = Rect {
        x: area.x + sidebar_w,
        y: body_y,
        width: area.width.saturating_sub(sidebar_w),
        height: body_h,
    };

    let title = format!(
        " vt100 spike — {program}{} — Ctrl+Q to quit ",
        if recording { " [recording]" } else { "" }
    );
    f.render_widget(
        Paragraph::new(Text::from(title)).style(Style::default().add_modifier(Modifier::BOLD)),
        header,
    );

    let sidebar_block = Block::default().borders(Borders::ALL).title("panes");
    f.render_widget(
        Paragraph::new("1. spike\n   (dummy sidebar —\n    not a real pane list)")
            .block(sidebar_block),
        sidebar,
    );

    render_grid(f, main, screen);
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

fn run_manual(program: String, args: Vec<String>, record_path: Option<PathBuf>) -> i32 {
    let (term_cols, term_rows) = match crossterm::terminal::size() {
        Ok(size) => size,
        Err(e) => {
            eprintln!("could not read terminal size: {e}");
            return 1;
        }
    };
    let (pty_cols, pty_rows) = pty_dims(term_cols, term_rows);

    let pair = match native_pty_system().openpty(PtySize {
        rows: pty_rows,
        cols: pty_cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("openpty failed: {e}");
            return 1;
        }
    };

    let mut command = CommandBuilder::new(&program);
    for arg in &args {
        command.arg(arg);
    }

    // take_writer + answer the cursor-probe BEFORE spawn_command — see the
    // module doc comment and wrap.rs:1090-1101.
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("take_writer failed: {e}");
            return 1;
        }
    };
    answer_inherit_cursor_probe(&mut *writer);

    let mut child = match pair.slave.spawn_command(command) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to spawn {program}: {e}");
            return 1;
        }
    };

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("try_clone_reader failed: {e}");
            let _ = child.kill();
            return 1;
        }
    };

    let (tx, rx) = mpsc::channel::<PtyEvent>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(PtyEvent::Closed);
                    return;
                }
                Ok(n) => {
                    if tx.send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);

    // Panic hook restores the terminal before the default hook prints —
    // the spike must never leave the caller's terminal in alt-screen/raw
    // mode even if something above panics.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    if let Err(e) = enable_raw_mode() {
        eprintln!("enable_raw_mode failed: {e}");
        let _ = child.kill();
        return 1;
    }
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        eprintln!("EnterAlternateScreen failed: {e}");
        let _ = child.kill();
        return 1;
    }

    let exit_code = 0;
    let mut exit_reason = String::new();
    let mut cur_pty_cols = pty_cols;
    let mut cur_pty_rows = pty_rows;

    let run_result: Result<(), String> = (|| {
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;

        loop {
            let mut closed = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    PtyEvent::Output(bytes) => {
                        parser.process(&bytes);
                        if let Some(path) = &record_path
                            && let Ok(mut f) =
                                OpenOptions::new().create(true).append(true).open(path)
                        {
                            let _ = f.write_all(&bytes);
                        }
                    }
                    PtyEvent::Closed => closed = true,
                }
            }
            if closed {
                exit_reason = "child pty closed".into();
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                exit_reason = format!("child exited: {status:?}");
                break;
            }

            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if is_quit(&key) {
                            exit_reason = "quit (Ctrl+Q)".into();
                            break;
                        }
                        let bytes = encode_key(key);
                        if !bytes.is_empty() {
                            let _ = writer.write_all(&bytes);
                            let _ = writer.flush();
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => {
                        let (new_pty_cols, new_pty_rows) = pty_dims(cols, rows);
                        if new_pty_cols != cur_pty_cols || new_pty_rows != cur_pty_rows {
                            cur_pty_cols = new_pty_cols;
                            cur_pty_rows = new_pty_rows;
                            let _ = pair.master.resize(PtySize {
                                rows: cur_pty_rows,
                                cols: cur_pty_cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            parser.screen_mut().set_size(cur_pty_rows, cur_pty_cols);
                        }
                    }
                    _ => {}
                }
            }

            let recording = record_path.is_some();
            let program_ref = program.as_str();
            let draw = terminal.draw(|f| draw_ui(f, parser.screen(), recording, program_ref));
            if let Err(e) = draw {
                return Err(format!("draw failed: {e}"));
            }
        }
        Ok(())
    })();

    // Every exit path from here restores the terminal before printing
    // anything to the (now-restored) real stdout.
    restore_terminal();
    let _ = std::panic::take_hook(); // drop our hook; process is exiting anyway

    if let Err(e) = run_result {
        eprintln!("vt_spike: {e}");
        let _ = child.kill();
        return 1;
    }

    let _ = child.kill();
    if exit_reason.is_empty() {
        exit_reason = "loop ended".into();
    }
    println!("vt_spike: {exit_reason}");
    exit_code
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  vt_spike --check [path]\n  vt_spike [--record <path>] <program> [args...]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("--check") {
        let code = run_check(args.get(1).map(String::as_str));
        std::process::exit(code);
    }

    let mut record_path: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--record" {
            let Some(path) = args.get(i + 1) else {
                usage();
            };
            record_path = Some(PathBuf::from(path));
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }

    let Some((program, rest)) = rest.split_first() else {
        usage();
    };

    let code = run_manual(program.clone(), rest.to_vec(), record_path);
    std::process::exit(code);
}
