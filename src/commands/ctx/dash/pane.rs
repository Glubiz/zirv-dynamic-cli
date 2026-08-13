//! One dashboard pane: a supervised ConPTY/pty child behind its own
//! `vt100::Parser`, using the same supervision primitives `wrap` uses
//! (registry record, turn-signal server, env scrub) so a pane is a first-
//! class session, not a shortcut.
//!
//! The PTY spawn follows `wrap.rs`'s own pattern faithfully (see
//! `wrap.rs:1044-1106`): the cursor-probe answer is written before
//! `spawn_command` (the Windows console-host deadlock `answer_inherit_
//! cursor_probe`'s own doc comment explains), `take_writer` is called
//! exactly once, and the reader thread uses the same 8192-byte buffer. The
//! one deliberate difference from `wrap`: a pane's bytes go to its reader
//! channel only, never to this process's own stdout -- the `vt100` parser is
//! the sole consumer, there is no shared stdout to lock, and a pane never
//! relaunches in place (a dashboard quits and spawns a fresh pane instead),
//! so there is no generation counter to guard a stale reader thread either.
//!
//! Nothing in the binary drives a `Pane` yet outside this module's own
//! tests: Task 5's event loop is what constructs `PaneSpec`s and calls
//! `Pane::spawn`/`drain`/`resize`/`on_turn_signal`/`screen` from a running
//! dashboard, and Task 4's `ui.rs` renders through `screen()`/`last_line()`.
//! `#![allow(dead_code)]` covers this module until that wiring lands, the
//! same reasoning `wrap::read_socket_path` already documents for a single
//! function: a real API with no in-tree caller yet is not the same thing as
//! code that should be deleted.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::super::CtxResult;
use super::super::prompt::PromptRole;
use super::super::sessions::{self, Record, SessionGuard, Verb};
use super::super::signal::SignalServer;
use super::super::state::StateDir;
use super::super::wrap;

/// Matches `wrap::quit_child`'s own grace period for the same ask-then-
/// escalate shape.
const QUIT_GRACE: Duration = Duration::from_secs(5);

/// A pane's display state, driven by turn signals and the child's own exit.
/// `WaitingInput` is reserved for a future, more specific signal (a prompt
/// pattern detected in the screen, say) than anything Task 3 derives; no
/// producer sets it yet, and `state_from` never returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Working,
    Idle,
    WaitingInput,
    Ended(i32),
}

/// What `Pane::spawn` needs to launch and register a pane. `argv` is the
/// full program-plus-arguments invocation -- built by the caller from an
/// adapter's `interactive_cmd`/`build_launch`, prompt composition and
/// `AgentAdapter::model_args` already folded in, exactly as `wrap.rs`'s own
/// `launch_command` is.
pub struct PaneSpec {
    pub agent_name: String,
    pub argv: Vec<String>,
    /// Shapes the composed prompt and argv the caller builds *before*
    /// constructing this spec (the same `prompt::compose` role every other
    /// supervisor already threads through) -- accepted here for interface
    /// completeness with the rest of the launch pipeline, but a pane itself
    /// has no per-role behavior once its argv is fixed, so `Pane::spawn`
    /// reads and discards it rather than storing a field nothing reads.
    pub role: PromptRole,
    pub verb: Verb,
    /// uuid, minted by the caller: the pane's registry short id and
    /// turn-signal socket are both derived from this.
    pub session_id: String,
    /// Sidebar label ("orch", "wrk codex", ...).
    pub title: String,
}

/// Pure: a pane's `PaneState` from whether a turn-boundary signal has been
/// seen since the child's last output, and whether the child has exited.
/// Exit always wins -- a pane that exited mid-turn is still `Ended`, not
/// `Working`.
fn state_from(signal_seen_recently: bool, child_exit: Option<i32>) -> PaneState {
    if let Some(code) = child_exit {
        return PaneState::Ended(code);
    }
    if signal_seen_recently {
        PaneState::Idle
    } else {
        PaneState::Working
    }
}

/// The bottom-most non-blank row of a vt100 screen, right-trimmed. Empty
/// when the whole screen is blank. Used for the sidebar's one-line preview.
fn last_line_of(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    for row in (0..rows).rev() {
        let mut line = String::new();
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

/// A supervised ConPTY/pty child rendered through its own `vt100` screen.
pub struct Pane {
    title: String,
    agent_name: String,
    session_id: String,
    parser: vt100::Parser,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    rx: mpsc::Receiver<Vec<u8>>,
    server: Option<SignalServer>,
    guard: SessionGuard,
    state_dir: StateDir,
    /// Set by `on_turn_signal`, cleared the next time `drain` sees new
    /// bytes: "the last thing this pane told us was a turn boundary, and it
    /// has not produced anything since."
    signal_seen_recently: bool,
    exit_code: Option<i32>,
    /// Idempotency guard for `shutdown` -- the release profile is
    /// `panic = "abort"`, so `Drop` is not guaranteed and every exit arm
    /// that leaves a pane's owner must call `shutdown` explicitly (mirrors
    /// `RawGuard`/`SessionGuard`'s own `done`/`released` fields).
    done: bool,
}

impl Pane {
    /// Spawns `spec.argv` behind a ConPTY/pty sized `size` (`(cols, rows)`,
    /// matching `wrap::window_size`'s own convention), binds this pane's own
    /// turn-signal socket at `state.socket_for(&spec.session_id)`, and
    /// registers it in the session registry. `turn_env` is applied after the
    /// supervision-env scrub, exactly as `wrap`'s own `apply_session_env`
    /// does -- the caller builds it from `adapter.register_turn_signal`
    /// against that same deterministic socket path, so the env a pane's
    /// child inherits and the socket this pane binds always agree.
    ///
    /// A bind failure degrades this pane to unsupervised (`reachable:
    /// false` on its registry record) rather than failing the spawn: a
    /// dashboard pane that cannot act on a wake-up is still a legitimate,
    /// visible session, the same call `wrap` makes for `--no-supervise`/a
    /// failed bind.
    pub fn spawn(
        spec: PaneSpec,
        state: &StateDir,
        repo: &Path,
        size: (u16, u16),
        turn_env: &[(String, String)],
    ) -> CtxResult<Pane> {
        let PaneSpec {
            agent_name,
            argv,
            role,
            verb,
            session_id,
            title,
        } = spec;
        // See the field's own doc comment: nothing inside a pane reads the
        // role once its argv is fixed.
        let _ = role;

        let (cols, rows) = size;
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let (program, rest) = argv
            .split_first()
            .ok_or("dashboard pane: empty argv, nothing to spawn")?;
        let mut command = CommandBuilder::new(program);
        for arg in rest {
            command.arg(arg);
        }
        command.cwd(repo);

        sessions::scrub_supervision_env(&mut command);
        for (key, value) in turn_env {
            command.env(key, value);
        }

        // Taken and answered before the spawn: on Windows the console host
        // has to be answered before it will service the child at all (see
        // `wrap::answer_inherit_cursor_probe`'s own doc comment).
        let mut first_writer = pair.master.take_writer()?;
        wrap::answer_inherit_cursor_probe(&mut *first_writer);
        let writer = Arc::new(Mutex::new(first_writer));

        let child = pair.slave.spawn_command(command)?;
        // The slave side is not needed past the spawn; dropping it here
        // (rather than keeping the whole `PtyPair` alive) mirrors the
        // explicit `drop(pair.slave)` this codebase's own pty tests already
        // use after a spawn.
        drop(pair.slave);
        let master = pair.master;

        let mut reader = master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        let server = SignalServer::bind(&state.socket_for(&session_id)).ok();
        if let Some(server) = &server {
            wrap::publish_socket_path(state, &session_id, server.path());
        }

        let record = Record::new(&session_id, &agent_name, repo, verb);
        let record = if server.is_some() {
            record
        } else {
            record.unreachable()
        };
        let guard = SessionGuard::register(state, record);

        Ok(Pane {
            title,
            agent_name,
            session_id,
            parser: vt100::Parser::new(rows, cols, 0),
            master,
            child,
            writer,
            rx,
            server,
            guard,
            state_dir: state.clone(),
            signal_seen_recently: false,
            exit_code: None,
            done: false,
        })
    }

    /// Pumps every byte currently queued on the reader channel into the
    /// `vt100` parser. Returns whether any new bytes arrived, so a caller
    /// can decide whether a redraw is worth doing. Also polls the child's
    /// exit status (see `poll_exit`): a pane's own output is the natural
    /// place to notice it has stopped producing any.
    pub fn drain(&mut self) -> bool {
        self.poll_exit();
        let mut any = false;
        while let Ok(bytes) = self.rx.try_recv() {
            self.parser.process(&bytes);
            any = true;
        }
        if any {
            self.signal_seen_recently = false;
        }
        any
    }

    /// The current screen, for `dash::ui`'s renderers.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Writes raw bytes into the child's pty, e.g. forwarded key input or
    /// (Task 9) a visible injected line.
    pub fn write_input(&mut self, bytes: &[u8]) -> CtxResult<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| "dashboard pane: writer lock poisoned")?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resizes both the pty and the `vt100` parser, so the two never
    /// disagree about how big this pane's screen is.
    pub fn resize(&mut self, rows: u16, cols: u16) -> CtxResult<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// This pane's current `PaneState`, from state already cached by
    /// `drain`/`on_turn_signal`/`poll_exit` -- no I/O of its own, so it is
    /// cheap enough to call every frame.
    pub fn state(&self) -> PaneState {
        state_from(self.signal_seen_recently, self.exit_code)
    }

    /// Drains every turn signal currently queued on this pane's socket. Also
    /// polls the child's exit status, the same as `drain`: a turn boundary
    /// and a child exit are both "this pane stopped producing on its own",
    /// and either is a fine place to notice the other.
    pub fn on_turn_signal(&mut self) {
        self.poll_exit();
        if let Some(server) = &self.server {
            while server.try_recv().is_some() {
                self.signal_seen_recently = true;
            }
        }
    }

    /// This pane's registry short id -- its nudge/mail address.
    pub fn short(&self) -> &str {
        self.guard.short()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn agent(&self) -> &str {
        &self.agent_name
    }

    /// The sidebar's one-line preview: the bottom-most non-blank row of this
    /// pane's current screen.
    pub fn last_line(&self) -> String {
        last_line_of(self.screen())
    }

    /// Idempotent: sends `quit_sequence` (grace period, then `kill`, exactly
    /// as `wrap::quit_child` does for its own child), releases this pane's
    /// registry record and unpublishes its socket path. A second call is a
    /// no-op -- see `done`'s own doc comment.
    pub fn shutdown(&mut self, quit_sequence: &str) -> CtxResult<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer;
            wrap::quit_child(sink, &mut self.child, quit_sequence, QUIT_GRACE)?;
        }
        wrap::unpublish_socket_path(&self.state_dir, &self.session_id);
        self.guard.release();
        Ok(())
    }

    /// Caches the child's exit code the first time it is observed, so
    /// `state()` can stay a cheap, side-effect-free read: `try_wait` needs
    /// `&mut Child`, `state()` does not take `&mut self`, so every mutating
    /// caller (`drain`, `on_turn_signal`) polls on the pane's behalf.
    fn poll_exit(&mut self) {
        if self.exit_code.is_some() {
            return;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exit_code = Some(status.exit_code() as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_state_maps_turn_signals_to_glyph_states() {
        assert!(matches!(state_from(false, None), PaneState::Working));
        assert!(matches!(state_from(true, None), PaneState::Idle));
        assert!(matches!(state_from(true, Some(0)), PaneState::Ended(0)));
        assert!(matches!(state_from(false, Some(3)), PaneState::Ended(3)));
    }

    #[test]
    fn last_line_returns_bottom_most_non_blank_row() {
        let mut parser = vt100::Parser::new(4, 10, 0);
        parser.process(b"hello\r\nworld\r\n");
        assert_eq!(last_line_of(parser.screen()), "world");
    }

    #[test]
    fn last_line_is_empty_on_a_blank_screen() {
        let parser = vt100::Parser::new(4, 10, 0);
        assert_eq!(last_line_of(parser.screen()), "");
    }

    /// A trivial, immediately-exiting command: never a real agent (the
    /// ABSOLUTE rule this plan spells out), just enough of a child for
    /// `Pane::spawn` to have something real to supervise. Mirrors the
    /// platform split `wrap.rs`'s own pty tests already use (`cmd /c` on
    /// Windows, `sh -c` on unix) rather than depending on either being on
    /// the other platform's `PATH`.
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

    fn test_spec(session_id: &str) -> PaneSpec {
        PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: PromptRole::Worker,
            verb: Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk test".to_string(),
        }
    }

    #[test]
    fn spawn_drain_and_shutdown_round_trip_on_a_real_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = test_spec("11111111-2222-4333-8444-555555555555");
        let mut pane = Pane::spawn(spec, &state, &repo, (80, 24), &[]).expect("spawn");

        assert_eq!(pane.agent(), "test-agent");
        assert_eq!(pane.title(), "wrk test");
        assert!(!pane.short().is_empty());

        // The child exits immediately; give it a bounded window to be
        // reaped rather than asserting on the very first poll.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut ended = false;
        while std::time::Instant::now() < deadline {
            pane.drain();
            if matches!(pane.state(), PaneState::Ended(_)) {
                ended = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ended, "the child must be reaped within the deadline");

        pane.shutdown("").expect("first shutdown");
        pane.shutdown("").expect("shutdown must be idempotent");
    }

    #[test]
    fn shutdown_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = test_spec("22222222-2222-4333-8444-555555555555");
        let mut pane = Pane::spawn(spec, &state, &repo, (80, 24), &[]).expect("spawn");

        pane.shutdown("")
            .expect("first shutdown releases the guard");
        let short = pane.short().to_string();
        let record_path = state.sessions().join(format!("{short}.json"));
        assert!(
            !record_path.exists(),
            "the registry record must be gone after shutdown"
        );

        // Second call must not error and must not touch anything that is
        // already gone.
        pane.shutdown("")
            .expect("second shutdown is a no-op, not an error");
        assert!(!record_path.exists());
    }
}
