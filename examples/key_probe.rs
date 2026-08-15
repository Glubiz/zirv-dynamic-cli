//! Throwaway diagnostic: prints exactly what crossterm delivers for each
//! keystroke in *your* terminal.
//!
//! Run it, press the keys the dashboard cares about (Ctrl+A, then Ctrl+A s,
//! Ctrl+A q, arrows, Shift+Enter, the mouse wheel), and paste the output.
//! It answers the one question code review cannot: whether `Ctrl+A` arrives
//! as `Char('a') + CONTROL`, as the raw control byte `Char('\u{1}')`, or as
//! something `is_prefix_key` does not recognise at all.
//!
//! Safe to run anywhere: it takes over the terminal only while running, and
//! restores raw mode on every exit path. Press `Esc` to quit.
//!
//!     cargo run --example key_probe
//!
//! Not part of the shipped binary; delete once the dashboard input path is
//! settled.

use std::io::Write;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// Mirrors `dash::is_prefix_key` exactly, so the probe reports the same
/// verdict the dashboard would reach for the very same event.
fn is_prefix_key(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('\u{01}') => true,
        _ => false,
    }
}

fn main() {
    if let Err(e) = enable_raw_mode() {
        eprintln!("could not enter raw mode: {e}");
        return;
    }
    // Mouse capture so wheel events show up too -- the dashboard does not
    // enable this yet, which is why panes cannot scroll.
    let mouse = crossterm::execute!(std::io::stdout(), EnableMouseCapture).is_ok();

    let mut out = std::io::stdout();
    let size = crossterm::terminal::size().unwrap_or((0, 0));
    let _ = write!(
        out,
        "zirv key probe -- size {}x{}, mouse capture {}\r\n\
         press keys (Ctrl+A, Ctrl+A s, arrows, Shift+Enter, wheel); Esc quits\r\n\r\n",
        size.0,
        size.1,
        if mouse { "on" } else { "UNAVAILABLE" }
    );
    let _ = out.flush();

    loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => {
                let _ = write!(out, "read error: {e}\r\n");
                break;
            }
        };

        match &ev {
            Event::Key(key) => {
                let prefix = if is_prefix_key(key) {
                    "  <-- MATCHES the dashboard prefix"
                } else {
                    ""
                };
                let _ = write!(
                    out,
                    "KEY  code={:?}  mods={:?}  kind={:?}{}\r\n",
                    key.code, key.modifiers, key.kind, prefix
                );
                // Only Press is acted on by the dashboard; flag anything else
                // so a terminal that reports Release/Repeat is obvious here.
                if key.kind != KeyEventKind::Press {
                    let _ = write!(
                        out,
                        "     (kind is not Press -- the dashboard ignores this event)\r\n"
                    );
                }
                if key.code == KeyCode::Esc {
                    break;
                }
            }
            other => {
                let _ = write!(out, "{other:?}\r\n");
            }
        }
        let _ = out.flush();
    }

    if mouse {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    println!("\r\nprobe finished; terminal restored");
}
