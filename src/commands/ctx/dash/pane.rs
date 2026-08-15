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
use std::time::{Duration, Instant};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Working,
    Idle,
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

/// How long after a turn signal the child may keep producing output without
/// that output being read as "a new turn started". A harness redraws its own
/// prompt, its status line and often the whole viewport right after finishing
/// a turn, and every one of those bytes used to count against the signal.
///
/// Mirrors `wrap`'s own injection debounce (`wrap::may_inject`, which requires
/// `now - last_output >= debounce` before it will type anything): the same
/// idea, applied to the pane's *state* rather than to one injection decision.
pub(crate) const IDLE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Pure: whether the turn signal at `signal_at` still stands as of `now`,
/// given the child's most recent output at `output_at` -- that is, whether a
/// turn boundary has been reported and the child has since been quiet for
/// `debounce`.
///
/// O1: `drain` used to clear the signal on **any** byte the child produced, so
/// a single post-turn repaint latched the pane into `Working` until the *next*
/// turn signal -- which, for a harness sitting idle at its prompt waiting for
/// input, never comes.
///
/// F1: the first fix for that measured the quiet window from the **signal**,
/// which was wrong on both sides of the window. Any output landing more than
/// `debounce` after a signal (a zoom or resize repaint, the operator's own
/// keystrokes echoing back, an async status line) re-latched the pane into
/// `Working` until a next signal that never came, killing delivery to it for
/// the rest of the session; and inside the window it flipped to `Idle` at
/// `signal + debounce` even while bytes were still streaming, because it never
/// looked at the output again.
///
/// Quiet is therefore measured from the **last output**, exactly as
/// `wrap::may_inject` already measures it for its own injections
/// (`now - last_output >= debounce`, `wrap.rs:256-262`): a burst keeps pushing
/// the deadline out for as long as it lasts, and one debounce after the last
/// byte the pane is idle again however long the burst ran. The two remaining
/// cases:
///
/// * no signal ever seen -- not idle (unchanged: a pane is `Working` until it
///   first reports a turn boundary), whatever it has been printing;
/// * a signal with no output recorded since -- the quiet window runs from the
///   signal itself, which is the same instant `wrap`'s own `last_output`
///   starts from when a session begins.
pub(crate) fn signal_still_stands(
    signal_at: Option<Instant>,
    output_at: Option<Instant>,
    now: Instant,
    debounce: Duration,
) -> bool {
    let Some(signal) = signal_at else {
        return false;
    };
    // Output from *before* the signal is what the signal already accounted
    // for, and measuring from it only makes the pane idle sooner, never later.
    now.duration_since(output_at.unwrap_or(signal)) >= debounce
}

/// Pure: a pane's `PaneState` from whether its last turn-boundary signal still
/// stands ([`signal_still_stands`]), whether the child has exited, and whether
/// an injection is still waiting for the turn it started to end. Exit always
/// wins -- a pane that exited mid-turn is still `Ended`, not `Working`.
///
/// R3: `injected_awaiting_turn` is what stops two independent injections
/// landing in the same tick. Injecting a line does not retract the standing
/// turn signal (the child has not produced anything yet, and the next turn
/// signal is still seconds away), so without this flag the mail sweep and the
/// nudge drain -- which run back to back in one tick and both gate on `Idle`
/// -- each saw the same idle pane and each typed into it.
///
/// G1: `user_typed_since_turn` used to be a third parameter here, folding the
/// operator's own mid-thought typing into this **displayed** state -- so an
/// operator who pressed a key with no turn signal following it left the pane
/// rendered `Working` forever (`○`/`●` in the sidebar, and the quit-confirm
/// dialog always named it as busy), even though nothing was actually running.
/// The flag is real and still matters, but only for whether an *injection* may
/// land, never for what glyph a pane renders -- see [`Pane::injectable`],
/// which is where it moved to.
fn state_from(
    signal_stands: bool,
    child_exit: Option<i32>,
    injected_awaiting_turn: bool,
) -> PaneState {
    if let Some(code) = child_exit {
        return PaneState::Ended(code);
    }
    if injected_awaiting_turn {
        return PaneState::Working;
    }
    if signal_stands {
        PaneState::Idle
    } else {
        PaneState::Working
    }
}

/// Pure: whether a pane in `state`, with `injected_awaiting_turn` and
/// `user_typed_since_turn` as they currently stand, may have a line injected
/// into it right now. `state == Idle` already implies `!injected_awaiting_turn`
/// ([`state_from`] reports `Working` while an injection is pending), so the
/// explicit check here is belt-and-suspenders against that invariant changing
/// out from under this function rather than load-bearing on its own.
///
/// G1: the operator-typing half of what `state_from` used to decide on its
/// own. F1's precondition (`wrap::may_inject`'s own
/// `!state.user_typed_since_turn`, `wrap.rs:259`) still holds -- an operator
/// mid-thought at a half-composed prompt must not have an injected line
/// submit it out from under them -- it is just no longer read off the
/// **displayed** `PaneState`, so a pane an operator typed into and then left
/// alone still renders `Idle`, is still named honestly in the quit-confirm
/// dialog, and simply is not a valid injection target until its next turn
/// signal clears the flag.
fn injectable_from(
    state: PaneState,
    injected_awaiting_turn: bool,
    user_typed_since_turn: bool,
) -> bool {
    matches!(state, PaneState::Idle) && !injected_awaiting_turn && !user_typed_since_turn
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

/// Pure: the exact text `inject_visible` writes for one labelled line,
/// matching the `zirv ▸` announcement channel's own marker
/// (`announce.rs`'s `Event::line`) so a visible injection reads as coming
/// from the same voice as everything else zirv narrates to an operator.
///
/// R4: deliberately carries **no** control characters of its own. The
/// framing used to be `"\r\n{line}\r\n"` plus a lone `"\r"`, and the leading
/// `\r\n` submitted whatever the operator had half-typed at the prompt before
/// the injected text was ever entered. `wrap::inject_compact` (`wrap.rs:477`)
/// already establishes this codebase's convention for the same job -- write
/// the text, then exactly one `\r`, because a TUI submits on carriage return
/// -- and `inject_visible` now follows it.
fn visible_injection_line(label: &str, body: &str) -> String {
    format!("[zirv \u{25b8} {label}] {body}")
}

/// The suffix `body_for_injection` appends when it had to cut a body short,
/// so the agent can tell a message that ended from one that was clipped.
const TRUNCATION_MARKER: &str = " \u{2026}[truncated]";

/// Pure: `text` with every C0 control character (`\r`, `\n`, `ESC`, and every
/// other byte below `0x20`) and `DEL` replaced by a single space, runs
/// collapsed to one space.
///
/// R3: this is the only thing standing between an untrusted mail body and the
/// child's own terminal. An interior `\r` submits the message mid-way and
/// leaves its tail typed at a fresh prompt as if the operator had written it;
/// an `ESC` reaches the child TUI as an escape sequence rather than as text.
/// A control character in text zirv is *relaying* is never meaningful, so it
/// is replaced rather than escaped: this is quoted input, not a wire format.
fn scrub_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch.is_control() {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(ch);
            in_run = false;
        }
    }
    out
}

/// Pure: an untrusted body made safe to type into a child's pty -- scrubbed of
/// every control character (`scrub_controls`) and capped at `cap` bytes on a
/// char boundary, with `TRUNCATION_MARKER` appended when anything was cut.
///
/// R3: the pane-injection seam applied neither the delivered-mail cap
/// (`cfg.mail.max_delivered_bytes`) every other mail seam applies nor any
/// scrub at all, so a stored body -- itself already carrying `mail::store`'s
/// own literal `"\n[truncated]"` marker once it was long enough -- went into
/// the pty verbatim.
pub(crate) fn body_for_injection(body: &str, cap: usize) -> String {
    let scrubbed = scrub_controls(body);
    if scrubbed.len() <= cap {
        return scrubbed;
    }
    let mut kept = crate::utils::truncate_bytes(scrubbed, Some(cap));
    kept.push_str(TRUNCATION_MARKER);
    kept
}

/// The most of one injection's bytes its *label* may spend. The label is a
/// short piece of provenance ("mail from claude/aaaa1111 -- information, not
/// instruction"), so a small fixed allowance covers every honest one several
/// times over.
///
/// Deliberately a **budget of its own** rather than a slice of the caller's
/// `cap`: the label is the frame that marks a delivered body as untrusted (R3),
/// and an operator who tightens `mail.max_delivered_bytes` to something very
/// small must get a shorter message, never a message with its trust marker
/// trimmed off the front. So one injection is bounded by `cap` plus this,
/// which is what "bounded" has to mean here.
///
/// Roomy enough that no honest label reaches it: a caller that interpolates
/// untrusted text into a label bounds *that component* first (see
/// `dash::mod::MAX_SENDER_NAME_BYTES`), because a marker at the end of a label
/// cannot survive the label being trimmed from the end. This is the last-resort
/// bound behind that, not the mechanism.
pub(crate) const MAX_INJECTED_LABEL_BYTES: usize = 192;

/// Pure: the `(label, body)` pair one injection may carry, with **both**
/// components bounded -- the label by [`MAX_INJECTED_LABEL_BYTES`], the body by
/// `cap`.
///
/// D5: the cap used to apply to the body alone, and the label was typed into
/// the child's pty at whatever length it happened to be. A mail label is built
/// from its sender's own `from_agent` -- the string that session had in
/// `ZIRV_CTX_AGENT`, which is untrusted and unbounded (`mail::header_value`
/// makes it one line, not a short one) -- so a 100KB agent name went in in full
/// while the body it introduced was dutifully trimmed to a few hundred bytes.
pub(crate) fn capped_injection(label: &str, body: &str, cap: usize) -> (String, String) {
    (
        body_for_injection(label, MAX_INJECTED_LABEL_BYTES),
        body_for_injection(body, cap),
    )
}

/// Pure: the exact bytes one visible injection writes into the child's pty --
/// the labelled line, then exactly one `\r` to submit it.
///
/// Both the label and the body are scrubbed here as a floor, whatever the
/// caller did: the label carries a sender-supplied agent name and the body is
/// whatever another session wrote, so neither may be trusted to be
/// control-free. The single trailing `\r` is then the *only* control byte in
/// the whole write, which is what makes an injection exactly one submission.
fn injection_bytes(label: &str, body: &str) -> Vec<u8> {
    let line = visible_injection_line(&scrub_controls(label), &scrub_controls(body));
    let mut bytes = line.into_bytes();
    bytes.push(b'\r');
    bytes
}

/// How many rows of history one pane's `vt100::Parser` keeps once they scroll
/// off the top of its screen -- what `Pane::scroll_by`/`scroll_page`/
/// `scroll_to_top` move around in.
///
/// Was `0`, which is vt100's own "keep nothing" (`grid.rs` only pushes a
/// retired row into the scrollback `if self.scrollback_len > 0`), so a pane's
/// history was not merely unreachable, it was never recorded -- the reason
/// `set_scrollback` alone would not have fixed anything.
///
/// 1000 is tmux's own order of magnitude (its `history-limit` default is
/// 2000) and is bounded, deliberately: vt100 stores a row as a `Vec<Cell>` of
/// 32-byte cells, so a full buffer costs `rows * cols * 32` -- about 6 MB per
/// pane at 200 columns, and only after 1000 rows have actually scrolled off
/// that pane. Nine of those (`dash.max_panes`) is the worst case, and the
/// worst case is a dashboard that has been running long enough to have earned
/// it.
const SCROLLBACK_ROWS: usize = 1000;

/// Pure: the scrollback offset `current` moves to under a scroll of `delta`
/// rows -- positive back into history, negative toward the live view -- held
/// inside `[0, max]`.
///
/// Both ends are real: `0` is the live bottom, past which "scroll down" is a
/// no-op rather than an underflow (`current` is a `usize`), and `max` is
/// however much history that pane has actually accumulated, past which
/// "scroll up" stops instead of running off into blank rows. `isize`
/// arithmetic throughout, so a wheel burst of many notches cannot wrap.
pub(crate) fn scroll_offset(current: usize, delta: isize, max: usize) -> usize {
    let want = (current as isize).saturating_add(delta);
    if want <= 0 {
        return 0;
    }
    (want as usize).min(max)
}

/// The most bytes one [`Pane::drain`] feeds the vt100 parser before it yields
/// back to the event loop (M10). 256 KiB is many screens' worth of output --
/// far more than a redraw ever shows -- so a normal burst still drains in one
/// call, while a firehose (`cat big.log`) is bounded to this per tick.
const DRAIN_BUDGET_BYTES: usize = 256 * 1024;

/// Pure-ish: pumps queued messages from `rx` into `parser` until either the
/// channel is empty or `budget` bytes have been processed. Returns
/// `(any, more)` -- whether anything was processed, and whether the budget cut
/// the drain short (bytes may still be queued). Separated from [`Pane::drain`]
/// so the budget behaviour is testable against a plain `mpsc` channel without
/// a real pty child.
fn drain_into(
    rx: &mpsc::Receiver<Vec<u8>>,
    parser: &mut vt100::Parser,
    budget: usize,
) -> (bool, bool) {
    let mut processed = 0usize;
    let mut any = false;
    loop {
        if processed >= budget {
            // Stopped on the budget, not on an empty channel: treat as
            // "more may remain" so the loop returns here next tick.
            return (any, true);
        }
        match rx.try_recv() {
            Ok(bytes) => {
                processed += bytes.len();
                parser.process(&bytes);
                any = true;
            }
            // Empty or Disconnected: nothing more to take right now.
            Err(_) => return (any, false),
        }
    }
}

/// A supervised ConPTY/pty child rendered through its own `vt100` screen.
pub struct Pane {
    title: String,
    agent_name: String,
    /// The registry verb this pane was spawned with (`Verb::Chat` for the
    /// dashboard's own orchestrator pane, `Verb::Dash` for a worker pane) --
    /// Task 9's mail sweep uses this to tell the two apart: an orchestrator
    /// pane is never body-injected, only a worker pane is (the trust split
    /// the spec calls for; `dash::mod::is_delivery_eligible`).
    verb: Verb,
    session_id: String,
    parser: vt100::Parser,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    rx: mpsc::Receiver<Vec<u8>>,
    server: Option<SignalServer>,
    guard: SessionGuard,
    state_dir: StateDir,
    /// When this pane last reported a turn boundary (`on_turn_signal`), and
    /// when `drain` last saw bytes from the child. `signal_still_stands`
    /// weighs the two against `IDLE_DEBOUNCE`; see its own doc comment for why
    /// a single timestamp pair replaced the old "any output clears the signal"
    /// boolean (O1).
    last_signal_at: Option<Instant>,
    last_output_at: Option<Instant>,
    /// Set by a successful `inject_visible`, cleared by the next turn signal
    /// (`on_turn_signal`): "this pane was handed something to do and has not
    /// reported finishing it yet." See `state_from`'s own doc comment -- this
    /// is what keeps two idle-gated injections out of the same tick.
    injected_awaiting_turn: bool,
    /// Set by `write_operator_input` (every keystroke the dashboard forwards
    /// to this pane), cleared by the next turn signal: "the operator is
    /// mid-thought in this pane." See [`Pane::injectable`]'s own doc comment
    /// (G1) -- the same precondition `wrap::may_inject` holds before it types
    /// anything, but gates injection only, not the pane's displayed state.
    user_typed_since_turn: bool,
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
        // FIX 2a (command-injection defense): a pty pane assembles its own
        // CommandBuilder, so it never passes through supervise::spawn_tapped's
        // guard. Apply the same cmd.exe argv-reparse policy here. A no-op off
        // Windows and for any program that is not the `cmd.exe /c <shim>` form.
        super::super::adapters::guard_cmd_shim_reparse(program, rest)?;
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

        let mut record = Record::new(&session_id, &agent_name, repo, verb);
        // `Record::new` stamps `std::process::id()` -- the dashboard's own pid,
        // identical for every pane, so liveness could not tell one pane's child
        // from another's. Stamp the child's real pid instead. `process_id`
        // returns `None` on a platform that cannot report it; there we leave
        // the dashboard's pid rather than a bogus one.
        if let Some(child_pid) = child.process_id() {
            record.pid = child_pid;
        }
        let record = if server.is_some() {
            record
        } else {
            record.unreachable()
        };
        let guard = SessionGuard::register(state, record);

        Ok(Pane {
            title,
            agent_name,
            verb,
            session_id,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_ROWS),
            master,
            child,
            writer,
            rx,
            server,
            guard,
            state_dir: state.clone(),
            last_signal_at: None,
            last_output_at: None,
            injected_awaiting_turn: false,
            user_typed_since_turn: false,
            exit_code: None,
            done: false,
        })
    }

    /// Pumps queued reader-channel bytes into the `vt100` parser, up to
    /// [`DRAIN_BUDGET_BYTES`] per call, and returns whether the budget cut the
    /// drain short with bytes still queued -- so the event loop knows to come
    /// back to this pane next tick rather than blocking on it now. Also polls
    /// the child's exit status (see `poll_exit`): a pane's own output is the
    /// natural place to notice it has stopped producing any.
    ///
    /// M10: the drain used to loop until the channel was empty. A `cat` of a
    /// large file fills the unbounded channel faster than `vt100` parses it, so
    /// the drain never returned and the whole event loop -- input included, so
    /// `Ctrl+A q` too -- was unreachable for the duration. The budget bounds
    /// one call's work; the remainder waits for the next tick.
    pub fn drain(&mut self) -> bool {
        self.poll_exit();
        let (any, more) = drain_into(&self.rx, &mut self.parser, DRAIN_BUDGET_BYTES);
        if any {
            // O1: recorded, not acted on. Whether these bytes mean "a new turn
            // started" or "the harness repainted the one that just ended" is
            // `signal_still_stands`' decision, and it needs the timestamp to
            // make it.
            self.last_output_at = Some(Instant::now());
        }
        more
    }

    /// The current screen, for `dash::ui`'s renderers. Already reflects this
    /// pane's scrollback offset: `vt100::Screen::cell` reads through
    /// `Grid::visible_rows`, which splices in the scrolled-back rows, so
    /// `ui::render_grid` draws the scrolled view with no change of its own.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// How many rows back from the live view this pane is currently showing;
    /// `0` is live. `ui::scroll_marker` turns it into the operator-facing
    /// marker.
    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// Scrolls this pane by `delta` rows -- positive back into history,
    /// negative toward the live view. Clamped at both ends: [`scroll_offset`]
    /// holds the bottom at `0`, and `vt100::Screen::set_scrollback` clamps the
    /// top to however much history this pane actually has (which is why `max`
    /// is `usize::MAX` here -- vt100 owns that bound and is the only thing that
    /// can see it).
    pub fn scroll_by(&mut self, delta: isize) {
        let want = scroll_offset(self.scrollback(), delta, usize::MAX);
        self.parser.screen_mut().set_scrollback(want);
    }

    /// A half-screen of scrolling, the step `Ctrl+A PageUp`/`PageDown` moves.
    /// Half rather than a full screen so the operator keeps a few lines of
    /// overlap to read against, which is what `less`, tmux and every pager
    /// converged on.
    pub fn scroll_page(&mut self, up: bool) {
        let (rows, _) = self.parser.screen().size();
        let half = (rows as isize / 2).max(1);
        self.scroll_by(if up { half } else { -half });
    }

    /// Jumps to the oldest row this pane still has (`Ctrl+A Home`).
    /// `set_scrollback` clamps to the real length, so `usize::MAX` means "as
    /// far back as there is".
    pub fn scroll_to_top(&mut self) {
        self.parser.screen_mut().set_scrollback(usize::MAX);
    }

    /// Back to the live view (`Ctrl+A End`, and every keystroke the operator
    /// sends the child -- see [`Pane::write_operator_input`]).
    pub fn scroll_to_live(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Forwards operator keystrokes into the child's pty and records that the
    /// operator has typed since this pane's last turn boundary
    /// (`user_typed_since_turn`), so no idle-gated injection lands in the
    /// middle of a half-composed prompt. Every keystroke `run_dashboard`
    /// routes to the focused pane goes through here; `inject_visible`
    /// deliberately does not, since it is the thing being gated.
    ///
    /// The flag is set before the write, not after: a write that failed part
    /// way through has still put bytes in front of the operator's cursor.
    ///
    /// Also snaps this pane back to the live view, the way tmux leaves copy
    /// mode the moment you type: an operator typing into a pane whose viewport
    /// is pinned 200 rows up would otherwise see nothing at all happen. This
    /// is deliberately on the *operator input* seam rather than on
    /// `write_input`, so an idle-gated `inject_visible` does not yank the view
    /// out from under someone reading history -- and, for the same reason, new
    /// output from the child does not either (vt100 keeps a non-zero offset
    /// pinned to its row as rows retire past it).
    pub fn write_operator_input(&mut self, bytes: &[u8]) -> CtxResult<()> {
        self.user_typed_since_turn = true;
        self.scroll_to_live();
        self.write_input(bytes)
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
        state_from(
            signal_still_stands(
                self.last_signal_at,
                self.last_output_at,
                Instant::now(),
                IDLE_DEBOUNCE,
            ),
            self.exit_code,
            self.injected_awaiting_turn,
        )
    }

    /// Whether this pane may have a line injected into it right now -- the
    /// mail sweep's and the nudge drain's own eligibility gate. See
    /// [`injectable_from`] for the full reasoning (G1): `state()` alone is no
    /// longer enough, because the operator's own mid-thought typing is
    /// deliberately excluded from it.
    pub fn injectable(&self) -> bool {
        injectable_from(
            self.state(),
            self.injected_awaiting_turn,
            self.user_typed_since_turn,
        )
    }

    /// Drains every turn signal currently queued on this pane's socket. Also
    /// polls the child's exit status, the same as `drain`: a turn boundary
    /// and a child exit are both "this pane stopped producing on its own",
    /// and either is a fine place to notice the other.
    /// A fresh signal also clears `injected_awaiting_turn` and
    /// `user_typed_since_turn`: the turn an injection (or the operator's own
    /// typing) started has now ended, so the pane is genuinely idle again and
    /// eligible for the next one. Both are cleared on a turn boundary for the
    /// same reason `wrap::InjectionState::on_turn` clears its own.
    pub fn on_turn_signal(&mut self) {
        self.poll_exit();
        if let Some(server) = &self.server {
            while server.try_recv().is_some() {
                self.last_signal_at = Some(Instant::now());
                self.injected_awaiting_turn = false;
                self.user_typed_since_turn = false;
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

    /// This pane's own zirv session id (the uuid `PaneSpec::session_id`
    /// carried in) -- the roster's own `RosterPane::session_id`, and what a
    /// verified adapter's `resume_args` is asked to resume.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// This pane's registry verb (`Verb::Chat` for the orchestrator,
    /// `Verb::Dash` for a worker pane) -- see the field's own doc comment.
    pub fn verb(&self) -> Verb {
        self.verb
    }

    /// The sidebar's one-line preview: the bottom-most non-blank row of this
    /// pane's current screen.
    pub fn last_line(&self) -> String {
        last_line_of(self.screen())
    }

    /// Writes a visible, clearly-labelled line into the child's own pty --
    /// `"[zirv ▸ {label}] {body}"` followed by exactly one `\r` to submit it,
    /// the same framing `wrap::inject_compact` uses. Used by Task 9's
    /// idle-gated intervention (an operator nudge, or a swept mail message) to
    /// put text in front of the agent the same way a human typing at the
    /// prompt would, rather than any side channel the agent has to know to
    /// look for.
    ///
    /// The caller must have already checked `state() == PaneState::Idle`:
    /// this method does not gate itself -- writing into a `Working` pane
    /// would interleave with whatever the agent is already sending, which is
    /// exactly the failure mode idle-gating exists to prevent.
    ///
    /// On success the pane reports `Working` until its next turn signal
    /// (`injected_awaiting_turn`), so a second idle-gated caller later in the
    /// same tick sees a busy pane rather than the stale `Idle` this one just
    /// acted on. A failed write leaves the flag alone: nothing was typed, so
    /// nothing is pending.
    pub fn inject_visible(&mut self, label: &str, body: &str) -> CtxResult<()> {
        // One write, not two (line then `\r`): a single `write_all` cannot
        // leave a half-typed line behind if the second write fails. The
        // control-character scrub is applied inside `injection_bytes` for
        // every caller, mail sweep and operator nudge alike (R3).
        self.write_input(&injection_bytes(label, body))?;
        self.injected_awaiting_turn = true;
        Ok(())
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

    /// M9: the first half of a *batched* shutdown -- sends this pane's harness
    /// quit sequence and returns immediately, without waiting out any grace.
    /// A caller shutting down many panes calls this on every pane first, then
    /// waits on all of them together against one shared budget
    /// ([`Pane::try_exited`]/[`Pane::finish_shutdown`]), rather than paying a
    /// full grace period per pane serially. Best-effort and idempotent: a
    /// no-op once the pane is already `done` or its child has exited.
    pub fn request_quit(&mut self, quit_sequence: &str) {
        if self.done {
            return;
        }
        self.poll_exit();
        if self.exit_code.is_some() {
            return;
        }
        let _ = self.write_input(quit_sequence.as_bytes());
    }

    /// Whether this pane's child has exited (polls once). The batched-shutdown
    /// wait loop polls every pane through here within its shared grace window.
    pub fn try_exited(&mut self) -> bool {
        self.poll_exit();
        self.exit_code.is_some()
    }

    /// The escalation half of a batched shutdown, run once the shared grace
    /// window has elapsed: kills the child if it has not exited on its own,
    /// then releases this pane's registry record and unpublishes its socket.
    /// Idempotent via `done`, exactly like [`Pane::shutdown`] -- calling both
    /// is safe, the second is a no-op.
    pub fn finish_shutdown(&mut self) -> CtxResult<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        self.poll_exit();
        if self.exit_code.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
pub(crate) mod tests {
    use super::*;

    #[test]
    fn pane_state_maps_turn_signals_to_glyph_states() {
        assert!(matches!(state_from(false, None, false), PaneState::Working));
        assert!(matches!(state_from(true, None, false), PaneState::Idle));
        assert!(matches!(
            state_from(true, Some(0), false),
            PaneState::Ended(0)
        ));
        assert!(matches!(
            state_from(false, Some(3), false),
            PaneState::Ended(3)
        ));
    }

    // O1: the post-turn repaint debounce. Every case is decided from two
    // timestamps and a window, so none of it needs a real child.

    /// A harness repainting its prompt straight after a turn must not latch
    /// the pane into `Working`: once the debounce window has elapsed with
    /// nothing further from the child, the signal still stands.
    #[test]
    fn a_repaint_right_after_a_turn_signal_leaves_the_pane_idle() {
        let debounce = Duration::from_millis(500);
        let signal = Instant::now();
        let repaint = signal + Duration::from_millis(50);

        assert!(
            !signal_still_stands(Some(signal), Some(repaint), repaint, debounce),
            "inside the window the burst is still undecided, so the pane is not yet idle"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(repaint),
                signal + Duration::from_millis(600),
                debounce
            ),
            "and once the window closes with nothing further, the signal stands"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(repaint),
                signal + Duration::from_secs(30),
                debounce
            ),
            "it does not decay: a pane idle at its prompt stays reachable"
        );
    }

    /// F1: output that keeps coming keeps the pane `Working` for as long as it
    /// lasts -- the quiet window restarts on every byte, so a burst that runs
    /// for a minute never looks idle part way through it.
    #[test]
    fn continuous_output_after_a_turn_signal_keeps_the_pane_working() {
        let debounce = Duration::from_millis(500);
        let signal = Instant::now();

        // A byte every 100ms for three seconds: at no point is the pane idle,
        // because the last byte is never more than 100ms old.
        for step in 1..=30u64 {
            let at = signal + Duration::from_millis(100 * step);
            assert!(
                !signal_still_stands(Some(signal), Some(at), at, debounce),
                "streaming output at +{}ms must not read as idle",
                100 * step
            );
        }
    }

    /// F1, the bug the old rule had on the far side of the window: output
    /// arriving *after* `signal + debounce` used to latch the pane into
    /// `Working` until a next turn signal that, for a harness sitting at its
    /// prompt, never comes -- so a zoom repaint or an echoed keystroke killed
    /// delivery to that pane for the rest of the session. A burst is now just
    /// a burst: once it stops, one debounce later the pane is idle again.
    #[test]
    fn a_late_repaint_burst_goes_idle_again_once_it_stops() {
        let debounce = Duration::from_millis(500);
        let signal = Instant::now();
        let burst_end = signal + Duration::from_millis(900);

        assert!(
            !signal_still_stands(Some(signal), Some(burst_end), burst_end, debounce),
            "while the burst is running the pane is working"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(burst_end),
                burst_end + Duration::from_millis(600),
                debounce
            ),
            "and a debounce after the last byte it is reachable again"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(burst_end),
                burst_end + Duration::from_secs(300),
                debounce
            ),
            "it does not decay back to working with nothing further happening"
        );
    }

    #[test]
    fn a_pane_is_working_until_it_first_reports_a_turn_boundary() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        assert!(!signal_still_stands(None, None, now, debounce));
        assert!(
            !signal_still_stands(None, Some(now), now, debounce),
            "output alone never makes a pane idle"
        );
        assert!(
            !signal_still_stands(None, None, now + Duration::from_secs(30), debounce),
            "and no amount of quiet substitutes for a turn boundary"
        );
        assert!(
            !signal_still_stands(Some(now), None, now, debounce),
            "a fresh signal with no output recorded measures its quiet from the signal"
        );
        assert!(
            signal_still_stands(Some(now), None, now + Duration::from_millis(600), debounce),
            "and is idle once that window elapses"
        );
        assert!(
            signal_still_stands(Some(now), Some(now - Duration::from_secs(5)), now, debounce),
            "output from before the signal is what the signal already accounted for"
        );
    }

    /// R3: a pane that was just injected into is `Working` even though its
    /// last observed signal still says "idle" -- and an exit still wins over
    /// both.
    #[test]
    fn a_pending_injection_reports_working_until_the_next_turn_signal() {
        assert!(matches!(state_from(true, None, true), PaneState::Working));
        assert!(matches!(state_from(false, None, true), PaneState::Working));
        assert!(
            matches!(state_from(true, Some(0), true), PaneState::Ended(0)),
            "an exited pane is Ended regardless of a pending injection"
        );
    }

    /// G1: operator typing is the same "do not inject" signal
    /// `wrap::may_inject` already honours -- a half-composed prompt must not be
    /// submitted by an injected line landing on top of it -- but, unlike
    /// before, it no longer changes what `PaneState` the pane reports: a pane
    /// the operator typed into and then left alone still renders `Idle`, it is
    /// just not `injectable` until its next turn signal.
    #[test]
    fn operator_typing_keeps_a_pane_uninjectable_but_still_renders_idle() {
        assert!(
            !injectable_from(PaneState::Idle, false, true),
            "typing makes the pane ineligible for injection"
        );
        assert!(
            injectable_from(PaneState::Idle, false, false),
            "and the very next turn boundary, which clears the flag, makes it eligible again"
        );
        assert!(
            !injectable_from(PaneState::Working, false, true),
            "a working pane is never injectable regardless of typing"
        );
        assert!(
            !injectable_from(PaneState::Ended(0), false, true),
            "an ended pane is never injectable regardless of typing"
        );
    }

    /// G1: `injectable_from`'s explicit `injected_awaiting_turn` check is
    /// belt-and-suspenders (state `Idle` already implies it is false), but it
    /// must still hold on its own terms.
    #[test]
    fn a_pending_injection_is_never_injectable_even_if_state_somehow_says_idle() {
        assert!(!injectable_from(PaneState::Idle, true, false));
    }

    /// The clamp/step arithmetic behind the wheel and `Ctrl+A PageUp`: neither
    /// end may run away, and a `usize` offset must never underflow past the
    /// live view.
    #[test]
    fn scroll_offset_clamps_at_the_live_view_and_at_the_end_of_history() {
        assert_eq!(scroll_offset(0, 3, 100), 3, "a wheel notch scrolls back");
        assert_eq!(scroll_offset(3, -3, 100), 0, "and back down again");
        assert_eq!(
            scroll_offset(0, -3, 100),
            0,
            "scrolling down at the live view is a no-op, not an underflow"
        );
        assert_eq!(
            scroll_offset(98, 3, 100),
            100,
            "scrolling up stops at the end of the recorded history"
        );
        assert_eq!(
            scroll_offset(100, 1, 100),
            100,
            "and stays there rather than running into blank rows"
        );
        assert_eq!(
            scroll_offset(5, 0, 100),
            5,
            "a zero-row scroll changes nothing"
        );
        assert_eq!(
            scroll_offset(0, 10, 0),
            0,
            "a pane with no history at all cannot be scrolled"
        );
    }

    /// A burst of wheel notches (or a `usize::MAX` "jump to the top") must
    /// saturate rather than wrap: the arithmetic runs in `isize`, and both
    /// extremes are reachable from a real terminal.
    #[test]
    fn scroll_offset_saturates_instead_of_wrapping() {
        assert_eq!(scroll_offset(0, isize::MAX, 100), 100);
        assert_eq!(scroll_offset(100, isize::MIN, 100), 0);
        assert_eq!(
            scroll_offset(1000, isize::MAX, usize::MAX),
            isize::MAX as usize,
            "a jump to the top saturates; vt100's own clamp then cuts it to the real history"
        );
    }

    /// End to end through the real parser, no child needed: rows that scroll
    /// off the top are recorded, `scroll_by`/`scroll_to_top`/`scroll_to_live`
    /// move the viewport over them, and the *rendered* screen follows -- which
    /// is what lets `ui::render_grid` stay unchanged.
    #[test]
    fn a_parser_with_scrollback_shows_retired_rows_when_scrolled_back() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        assert_eq!(parser.screen().scrollback(), 0, "starts at the live view");

        parser
            .screen_mut()
            .set_scrollback(scroll_offset(0, 3, 1000));
        assert_eq!(parser.screen().scrollback(), 3);
        assert_eq!(
            last_line_of(parser.screen()),
            "line7",
            "three rows back, the bottom row is three lines earlier"
        );

        // Past the end of the recorded history: vt100 clamps rather than
        // showing blanks, and reports the clamped value back.
        parser.screen_mut().set_scrollback(usize::MAX);
        let top = parser.screen().scrollback();
        assert!(
            top > 0 && top < usize::MAX,
            "clamped to real history: {top}"
        );

        parser.screen_mut().set_scrollback(0);
        assert_eq!(parser.screen().scrollback(), 0);
        assert_eq!(last_line_of(parser.screen()), "line9", "back to live");
    }

    /// The regression that made scrollback unreachable in the first place: the
    /// parser was built with a scrollback length of `0`, so vt100 discarded
    /// every retired row instead of keeping it. With no recorded history there
    /// is nothing for any amount of `set_scrollback` to show.
    #[test]
    fn a_parser_without_scrollback_records_no_history_at_all() {
        let mut parser = vt100::Parser::new(3, 20, 0);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            parser.screen().scrollback(),
            0,
            "nothing was ever recorded, so the offset clamps straight back to live"
        );
    }

    /// Return-to-live is the operator's own typing, and only that: output the
    /// child produces while the operator is reading history must leave the
    /// viewport where they put it (vt100 pins a non-zero offset to its row as
    /// rows retire past it), and so must an idle-gated injection.
    #[test]
    fn new_output_does_not_yank_a_scrolled_back_view() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(3);
        let pinned = last_line_of(parser.screen());

        parser.process(b"fresh output\r\n");
        assert_eq!(
            last_line_of(parser.screen()),
            pinned,
            "the scrolled-back view stays on the same text as the child keeps printing"
        );
        assert!(
            parser.screen().scrollback() > 3,
            "the offset grew with the history so the view could stay put"
        );
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

    /// Pure: the exact text a visible injection writes, matching
    /// `announce.rs`'s own `zirv ▸` marker.
    #[test]
    fn visible_injection_line_matches_the_zirv_announce_format() {
        assert_eq!(
            visible_injection_line("nudge from operator", "hello"),
            "[zirv \u{25b8} nudge from operator] hello"
        );
    }

    /// R4: the line carries no control characters at all. A leading `\r\n`
    /// used to submit whatever the operator had half-typed at the prompt
    /// before the injected text was ever entered; the lone trailing `\r`
    /// `inject_visible` adds is the only submission in the whole framing,
    /// exactly as in `wrap::inject_compact`.
    #[test]
    fn visible_injection_line_submits_nothing_of_its_own() {
        let line = visible_injection_line("mail from claude/aaaa1111", "check the build");
        assert!(
            !line.contains('\r') && !line.contains('\n'),
            "no control characters may frame the line: {line:?}"
        );
    }

    /// R3: every control character in an untrusted body becomes one space,
    /// and a run of them becomes one space, not several.
    #[test]
    fn body_for_injection_scrubs_every_control_character() {
        assert_eq!(
            body_for_injection("first\r\nsecond", 4096),
            "first second",
            "an interior CRLF must not survive to submit the message halfway"
        );
        assert_eq!(body_for_injection("a\rb", 4096), "a b");
        assert_eq!(
            body_for_injection("a\u{1b}[31mred\u{7f}", 4096),
            "a [31mred ",
            "ESC and DEL are text to be quoted, never bytes for the child TUI"
        );
        assert_eq!(
            body_for_injection("a\r\n\r\n\tb", 4096),
            "a b",
            "a run of control characters collapses to a single space"
        );
        assert_eq!(
            body_for_injection("plain text", 4096),
            "plain text",
            "an ordinary body is passed through untouched"
        );
    }

    /// R3: the delivered-mail cap (`cfg.mail.max_delivered_bytes`) applies at
    /// this seam too, and cutting never splits a char.
    #[test]
    fn body_for_injection_truncates_at_the_cap_on_a_char_boundary() {
        let long = "x".repeat(100);
        let got = body_for_injection(&long, 10);
        assert_eq!(got, format!("{}{TRUNCATION_MARKER}", "x".repeat(10)));

        // 'é' is two bytes: a cap landing inside it drops the whole char.
        let got = body_for_injection("aé", 2);
        assert_eq!(got, format!("a{TRUNCATION_MARKER}"));

        assert_eq!(
            body_for_injection("short", 5),
            "short",
            "a body exactly at the cap is not marked truncated"
        );
    }

    /// R3, at the byte level: whatever control characters an untrusted body
    /// carries, the write that lands in the pty contains exactly one -- the
    /// trailing `\r` that submits it. Anything else would be a second
    /// submission (an interior `\r`) or an escape sequence typed at the child.
    #[test]
    fn an_injection_writes_exactly_one_control_byte() {
        let bytes = injection_bytes(
            "mail from claude\r/aaaa1111",
            "line one\r\nline two\u{1b}[2Jline three\u{7f}",
        );
        let controls: Vec<usize> = bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b < 0x20 || **b == 0x7f)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            controls.len(),
            1,
            "exactly one control byte may reach the pty: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(controls[0], bytes.len() - 1, "and it is the last byte");
        assert_eq!(bytes[bytes.len() - 1], b'\r', "and it is the submission");
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

    /// A trivial child that stays alive well past any of these tests' own
    /// deadlines, and reaps itself if the test somehow never shuts it down.
    /// Same never-a-real-agent rule and same platform split as `trivial_argv`;
    /// `ping -n N 127.0.0.1` is already this codebase's own long-lived
    /// Windows test child (`wrap.rs`'s turn-signal transport test).
    #[cfg(windows)]
    pub(crate) fn long_lived_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "ping -n 60 127.0.0.1".to_string(),
        ]
    }

    #[cfg(unix)]
    pub(crate) fn long_lived_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "sleep 60".to_string()]
    }

    /// Drives one turn signal into `pane`'s own socket and waits, bounded, for
    /// the pane to report `Idle`. Returns whether it got there. Gives up
    /// immediately if the child exited (`Ended` outranks every other state, so
    /// no number of signals would ever move it back to `Idle`).
    ///
    /// Two phases, because of F1: idleness is now "quiet for a debounce",
    /// measured from the last output or, with none recorded, from the signal
    /// itself. So the retry loop stops sending the moment a signal has been
    /// observed -- each further signal would restart the quiet window and this
    /// helper would spin until its own deadline.
    pub(crate) fn signal_until_idle(pane: &mut Pane, state: &StateDir, session_id: &str) -> bool {
        let socket = state.socket_for(session_id);
        let signal = crate::commands::ctx::signal::TurnSignal {
            session_id: session_id.to_string(),
            turn: 1,
            score: 0,
            verdict: crate::commands::ctx::rot::Verdict::Healthy,
            transcript_path: None,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let before = pane.last_signal_at;

        // Phase 1: land exactly one signal, retrying until the pane observes
        // a newer one than it already had.
        while std::time::Instant::now() < deadline {
            pane.on_turn_signal();
            if matches!(pane.state(), PaneState::Ended(_)) {
                return false;
            }
            if pane.last_signal_at != before {
                break;
            }
            let _ = crate::commands::ctx::signal::send(&socket, &signal);
            std::thread::sleep(Duration::from_millis(50));
        }

        // Phase 2: wait out the debounce with nothing further sent.
        while std::time::Instant::now() < deadline {
            pane.on_turn_signal();
            match pane.state() {
                PaneState::Idle => return true,
                PaneState::Ended(_) => return false,
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
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
        assert_eq!(pane.verb(), Verb::Dash);

        // Smoke test: a live pane's writer accepts a visible injection
        // (content correctness is covered separately by
        // `visible_injection_line_matches_the_zirv_announce_format`, which
        // does not need a real child at all).
        pane.inject_visible("nudge from operator", "hello")
            .expect("inject_visible must succeed while the child is alive");

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

    /// R3, end to end on a real supervised child: an idle pane that is
    /// injected into reports `Working` immediately -- so a second idle-gated
    /// caller in the same tick skips it -- and goes back to `Idle` only once
    /// the turn the injection started reports finishing.
    #[test]
    fn an_injection_makes_a_pane_busy_until_its_next_turn_signal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "33333333-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(spec, &state, &repo, (80, 24), &[]).expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );

        pane.inject_visible("nudge from operator", "hello")
            .expect("inject");
        assert!(
            matches!(pane.state(), PaneState::Working),
            "a freshly injected pane is busy, not idle: {:?}",
            pane.state()
        );

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the next turn signal must clear the pending injection"
        );

        pane.shutdown("").expect("shutdown");
    }

    /// F1/G1, end to end on a real supervised child: a keystroke the dashboard
    /// forwards to a pane takes it out of reach of both idle-gated injectors
    /// until the pane reports its next turn boundary -- but, per G1, this must
    /// no longer show up in the pane's own **displayed** state: the sidebar
    /// glyph and the quit-confirm dialog (both driven by `state()`) must keep
    /// reading the pane as `Idle`, only `injectable()` may say otherwise.
    #[test]
    fn operator_typing_makes_a_pane_ineligible_but_leaves_its_glyph_idle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "44444444-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(spec, &state, &repo, (80, 24), &[]).expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );
        assert!(pane.injectable(), "idle with nothing typed is injectable");

        pane.write_operator_input(b"half a thought")
            .expect("forwarding a keystroke must succeed while the child is alive");
        assert!(
            matches!(pane.state(), PaneState::Idle),
            "G1: typing with no turn signal following it must not change the \
             displayed state -- the pane is not mid-turn, it is mid-thought: {:?}",
            pane.state()
        );
        assert!(
            !pane.injectable(),
            "an operator mid-thought is not an injection target"
        );

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the next turn boundary clears the operator-typing flag"
        );
        assert!(
            pane.injectable(),
            "and the pane is reachable again once it does"
        );

        pane.shutdown("").expect("shutdown");
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

    /// M10: a drain stops once it has processed its byte budget and reports
    /// that more remains, so the event loop is never starved by a firehose.
    #[test]
    fn drain_into_stops_at_the_budget_and_reports_more_remaining() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // Five 4-byte messages = 20 bytes; a 10-byte budget stops partway.
        for _ in 0..5 {
            tx.send(b"abcd".to_vec()).expect("send");
        }
        let mut parser = vt100::Parser::new(4, 40, 0);
        let (any, more) = drain_into(&rx, &mut parser, 10);
        assert!(any, "some bytes were processed");
        assert!(
            more,
            "the budget cut the drain short with bytes still queued"
        );
        assert!(rx.try_recv().is_ok(), "messages remain on the channel");
    }

    /// A channel that empties under budget reports nothing remaining; a drained
    /// (and disconnected) channel reports neither work done nor more remaining.
    #[test]
    fn drain_into_reports_no_more_when_the_channel_empties() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"hi".to_vec()).expect("send");
        let mut parser = vt100::Parser::new(4, 40, 0);
        let (any, more) = drain_into(&rx, &mut parser, 1024);
        assert!(any);
        assert!(
            !more,
            "an emptied channel under budget has nothing remaining"
        );

        drop(tx);
        let (any2, more2) = drain_into(&rx, &mut parser, 1024);
        assert!(!any2 && !more2, "a drained, closed channel is quiet");
    }

    /// M9: the batched-shutdown primitives -- ask to quit without waiting, then
    /// escalate and release the record -- take a live pane down and free its
    /// registry entry, idempotently.
    #[test]
    fn request_quit_then_finish_shutdown_releases_the_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("66666666-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(spec, &state, &repo, (80, 24), &[]).expect("spawn");
        let short = pane.short().to_string();
        let record_path = state.sessions().join(format!("{short}.json"));
        assert!(
            record_path.exists(),
            "the record exists while the pane runs"
        );

        // No real quit sequence for a sleep/ping child, so the escalation half
        // (kill) is what ends it; either way the record must be released.
        pane.request_quit("");
        pane.finish_shutdown().expect("finish_shutdown");
        assert!(!record_path.exists(), "the record is released");

        // Idempotent, and interchangeable with `shutdown`.
        pane.finish_shutdown()
            .expect("finish_shutdown is idempotent");
        pane.shutdown("")
            .expect("shutdown after finish_shutdown is a no-op");
    }
}
