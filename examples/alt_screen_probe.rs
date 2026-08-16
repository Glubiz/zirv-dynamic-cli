//! Throwaway diagnostic: does the harness child enter the *alternate screen*?
//!
//! This settles whether a dashboard pane can have scrollback at all. vt100
//! hardcodes the alternate grid's scrollback length to zero
//! (`vt100-0.16.2/src/screen.rs:76`), so a child that switches to the
//! alternate screen (`ESC [ ? 1049 h`) can never accumulate history, no
//! matter how large the parser's scrollback is -- exactly as a real terminal
//! cannot scroll back through `vim`.
//!
//! Spawns the harness in its own pty (never this terminal), reads whatever it
//! paints for a few seconds, kills it, and reports what the byte stream
//! actually contained. No prompt is sent, so no model request is made.
//!
//!     cargo run --example alt_screen_probe                 # cmd /c claude
//!     cargo run --example alt_screen_probe -- claude       # explicit
//!     cargo run --example alt_screen_probe -- vim          # known-positive
//!
//! Delete once pane scrolling is settled.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const ROWS: u16 = 30;
const COLS: u16 = 100;
const WATCH: Duration = Duration::from_secs(6);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // On Windows an npm-installed `claude` is `claude.cmd`, which only the
    // command interpreter can launch -- the same reason `resolve_program`
    // exists in the real code path.
    let (program, rest): (String, Vec<String>) = match args.split_first() {
        Some((first, tail)) => (first.clone(), tail.to_vec()),
        None if cfg!(windows) => (
            "cmd.exe".to_string(),
            vec!["/c".to_string(), "claude".to_string()],
        ),
        None => ("claude".to_string(), Vec::new()),
    };

    println!("probing: {program} {rest:?}  ({ROWS}x{COLS}, {WATCH:?})\n");

    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows: ROWS,
        cols: COLS,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("openpty failed: {e}");
            return;
        }
    };

    let mut cmd = CommandBuilder::new(&program);
    for a in &rest {
        cmd.arg(a);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("spawn failed: {e}");
            return;
        }
    };
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("reader failed: {e}");
            let _ = child.kill();
            return;
        }
    };

    // ConPTY opens by asking for the cursor position (`ESC [ 6 n`) and will
    // not pump the child's output until something answers. Without this the
    // probe reads exactly those four bytes and nothing else -- which is
    // precisely what it did before this reply was added.
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("writer failed: {e}");
            let _ = child.kill();
            return;
        }
    };

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut parser = vt100::Parser::new(ROWS, COLS, 1000);
    let mut raw: Vec<u8> = Vec::new();
    let deadline = Instant::now() + WATCH;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => {
                // Answer every cursor-position request, or the child stalls.
                if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                    use std::io::Write as _;
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                }
                raw.extend_from_slice(&bytes);
                parser.process(&bytes);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    // How much history the parser managed to retire, asked the same way the
    // dashboard asks it.
    parser.screen_mut().set_scrollback(usize::MAX);
    let scrollback = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);

    let saw = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
    let enter_1049 = saw(b"\x1b[?1049h");
    let leave_1049 = saw(b"\x1b[?1049l");
    let enter_47 = saw(b"\x1b[?47h") || saw(b"\x1b[?1047h");

    println!("bytes read from the child : {}", raw.len());
    println!(
        "raw (escaped, first 400)  : {:?}",
        String::from_utf8_lossy(&raw[..raw.len().min(400)])
    );
    println!("ESC[?1049h (alt screen on): {enter_1049}");
    println!("ESC[?1049l (alt screen off): {leave_1049}");
    println!("ESC[?47h / ?1047h          : {enter_47}");
    println!(
        "vt100 alternate_screen()   : {}",
        parser.screen().alternate_screen()
    );
    println!(
        "vt100 application_cursor() : {}",
        parser.screen().application_cursor()
    );
    println!("scrollback rows retired    : {scrollback}");
    println!();
    if parser.screen().alternate_screen() {
        println!(
            "VERDICT: the child is on the ALTERNATE screen. vt100 gives that grid\n\
             zero scrollback, so a pane can never scroll its own history -- the\n\
             wheel has to be forwarded to the child instead."
        );
    } else if scrollback > 0 {
        println!(
            "VERDICT: the child is on the NORMAL screen and retired {scrollback} rows\n\
             into scrollback, so pane scrollback is genuinely available and any\n\
             failure to scroll is a bug in the dashboard's own scroll path."
        );
    } else {
        println!(
            "VERDICT: normal screen, but nothing retired into scrollback. The child\n\
             repaints in place rather than scrolling, so there is no history to show\n\
             until it emits more than one screenful."
        );
    }
}
