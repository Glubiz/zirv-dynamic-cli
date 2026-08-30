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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::super::CtxResult;
use super::super::prompt::PromptRole;
use super::super::sessions::{self, Record, SessionGuard, Verb};
use super::super::signal::SignalServer;
use super::super::state::StateDir;
use super::super::supervise;
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
    /// supervisor already threads through). Issue #169: also the role this
    /// pane's own `Pane`/`sessions::Record` is stamped with at spawn time --
    /// the caller (`dash::mod::fulfill_spawn_request`) has already run this
    /// value through the depth cap before ever constructing a `PaneSpec`, so
    /// `Pane::spawn` records it as fact rather than re-deriving or
    /// re-validating it.
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

/// Compiled-in fallback for [`Pane::idle_quiet`] when a caller has no
/// `CtxConfig` to read `dash.idle_quiet_ms` from (every test call site in
/// this module). Matches `DashConfig::default`'s own `idle_quiet_ms` so a
/// test that does not care about this knob still exercises the real default.
pub(crate) const DEFAULT_IDLE_QUIET: Duration = Duration::from_millis(10_000);

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

/// Pure: whether `output_at` is old enough, as of `now`, to count as quiet.
/// The signal-less half of [`pane_is_idle`] -- mirrors `signal_still_stands`'s
/// own "no signal ever seen -- not idle" rule: no output ever recorded is not
/// quiet either, whatever else has happened. A pane still starting up (its
/// harness has not drawn its first frame yet) is not the same thing as one
/// sitting quietly at its prompt, and the two must not be confused just
/// because both currently read `None`/old.
pub(crate) fn output_quiescent(output_at: Option<Instant>, now: Instant, quiet: Duration) -> bool {
    let Some(output) = output_at else {
        return false;
    };
    quiescent_since(output, now, quiet)
}

/// Finding 5 (review): the elapsed-time arithmetic shared by every
/// idleness-by-clock decision in this codebase -- `now` counts as quiescent
/// relative to `latest` once at least `quiet` has passed. [`output_quiescent`]
/// (above, `Option<Instant>`: "no output ever recorded" reads as not-quiet)
/// and `wrap::signal_less_mail_ready` (always a concrete `Instant`, already
/// folded via `.max()`) each wrap this with their own "what counts as
/// `latest`" logic; the formula itself is kept in exactly one place so the
/// two could not silently drift out of sync with each other again.
pub(crate) fn quiescent_since(latest: Instant, now: Instant, quiet: Duration) -> bool {
    now.duration_since(latest) >= quiet
}

/// Pure: the more recent of two optional instants, the one present when only
/// one is, or `None` when neither is. Used to fold "last child output" and
/// "last thing *zirv itself* typed into this pane" into one "last activity"
/// instant for the signal-less quiescence check below -- an injection or an
/// operator keystroke is exactly as much "not quiet yet" as a byte the child
/// printed, and whichever happened later is what the quiet window has to run
/// from.
fn latest_of(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Pure: the signal-less half of [`pane_is_idle`], and what `Pane::drain`
/// uses to decide whether to retire a pending injection/operator-typing flag
/// for a signal-less pane. Quiet is measured from the *later* of the child's
/// last output and zirv's own last local input into the pane
/// ([`latest_of`]), not from output alone.
///
/// H1 (review): `output_at` alone used to be the whole story, but
/// `inject_visible`/`write_operator_input` only ever run once a pane is
/// already idle/injectable -- so on the very next `drain()` tick (tens of
/// milliseconds later), `output_at` had not moved yet (the child has not had
/// time to respond), the pane still read as "quiet", and the guard that is
/// supposed to hold `injected_awaiting_turn`/`user_typed_since_turn` for a
/// full `idle_quiet` window instead cleared it almost immediately. Two
/// concrete failures that produced: a mail-sweep injection followed within
/// one tick by an unthrottled second injection from the nudge drain landing
/// on top of it; and a swept mail line typed straight into an operator's own
/// unsubmitted, mid-composition prompt, in direct violation of G1's own
/// contract (`injectable_from`'s whole reason to exist). Folding local input
/// into the same clock the child's output already uses fixes both: an
/// injection now starts a fresh quiet window (the child genuinely gets
/// `idle_quiet` to begin responding before anything else may land), and an
/// operator's own keystroke holds the pane non-idle for `idle_quiet` after
/// their *last* one, exactly as intended.
fn signal_less_quiescent(
    output_at: Option<Instant>,
    local_input_at: Option<Instant>,
    now: Instant,
    quiet: Duration,
) -> bool {
    output_quiescent(latest_of(output_at, local_input_at), now, quiet)
}

/// Pure: whether a pane counts as idle right now, branching on whether its
/// adapter actually has a turn-signal mechanism
/// (`AgentAdapter::capabilities().turn_signal`).
///
/// * `turn_signal_capable`: unchanged from before this branch existed --
///   [`signal_still_stands`], gated on having seen at least one turn
///   boundary. A claude-shaped adapter reports one on every turn, so this is
///   the precise, low-latency signal and stays the only thing consulted for
///   it. `local_input_at` is not consulted on this branch at all: a
///   signal-carrying pane's idleness is decided by the signal, exactly as
///   before this parameter existed.
/// * signal-less (codex today): `register_turn_signal` is a no-op for it, so
///   `last_signal_at` never advances past `None` and gating on a signal would
///   leave such a pane `Working` forever -- the mail sweep and nudge drain,
///   both gated on `Idle`
///   (`Pane::injectable`/`dash::mod::is_delivery_eligible`), would then never
///   reach it at all. Its own pty *output*, and zirv's own last local input
///   into it, stand in for the signal instead: [`signal_less_quiescent`]
///   against `dash.idle_quiet_ms` (`Pane::idle_quiet`), independent of
///   `signal_at` entirely -- a signal-less pane is never consulted on that
///   axis, so nothing sent to its (unreachable) turn-signal socket could ever
///   matter to it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_is_idle(
    turn_signal_capable: bool,
    signal_at: Option<Instant>,
    output_at: Option<Instant>,
    local_input_at: Option<Instant>,
    now: Instant,
    debounce: Duration,
    idle_quiet: Duration,
) -> bool {
    if turn_signal_capable {
        signal_still_stands(signal_at, output_at, now, debounce)
    } else {
        signal_less_quiescent(output_at, local_input_at, now, idle_quiet)
    }
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
/// -- and `inject_visible` follows the same "text, then one `\r`" shape,
/// though (issue #114) it no longer writes them in the same `write_all`; see
/// [`INJECTION_SUBMIT_DELAY`] for why.
fn visible_injection_line(label: &str, body: &str) -> String {
    format!("[zirv \u{25b8} {label}] {body}")
}

/// The minimum gap `inject_visible` leaves between writing an injected line
/// and the lone `\r` that submits it -- issue #114. Moved to
/// `crate::commands::ctx::INJECTION_SUBMIT_DELAY` (issue #118) so `wrap`
/// can share the exact same value rather than pin its own copy; see that
/// constant's own doc comment for the full paste-fold story.
///
/// A hardcoded constant, deliberately not a new `.zirv` config key: an older
/// installed zirv binary hard-fails on an unknown settings key (see
/// CLAUDE.md's "This Windows dev machine" notes), so a config knob here
/// would force a coupled binary-then-config rollout for a value no operator
/// is expected to ever need to tune.
///
/// Review F1/F2 (PR #116): this used to be enforced by blocking the calling
/// thread for exactly this long inside `write_two_phase_injection`. On the
/// dashboard that thread is the single UI thread every sweep runs on
/// (`mail_sweep`, `report_back_reminder_sweep`, `deliver_queued_nudges`, all
/// iterating every pane in one tick), so a handful of injections in the same
/// tick could serially freeze redraw and input for the sum of their delays --
/// up to ~1.35s with nine panes across three sweeps. The gap is now a
/// *minimum*, not a sleep: `inject_visible` writes phase 1, stamps this
/// pane's state immediately, and records a deadline (`Self::pending_submit`)
/// for phase 2. The dashboard's own tick loop (`dash::mod::run_dashboard`,
/// alongside the sweeps that already run there) drains any pane whose
/// deadline has passed, so the effective gap may run one tick longer than
/// this constant under load -- which is fine; nothing about the paste-fold
/// fix requires the gap to be exact, only that it not collapse to zero.
///
/// `wrap`'s own pump loop reuses this constant and `write_submit_cr` below
/// for its T13 mail-advisory injection, but only into a
/// `Capabilities::defer_injection_submit` adapter (codex) -- its
/// `Action::Compact` stays single-burst, because that call site is only
/// ever reachable for claude (see `wrap::inject_compact`'s own doc comment).
pub(crate) use super::super::INJECTION_SUBMIT_DELAY;

/// Phase 1 of a deferred visible injection: the labelled line
/// ([`visible_injection_line`], scrubbed) with **no** control bytes of its
/// own. Flushed so the bytes have actually left this process before the
/// caller stamps any state on the strength of this write having happened.
///
/// Split from the submitting `\r` ([`write_submit_cr`]) so the two can cross
/// the pty as genuinely separate writes, spaced by at least
/// [`INJECTION_SUBMIT_DELAY`] -- see that constant's own doc comment for why
/// (issue #114) and for why the spacing is now enforced by a deadline the
/// caller polls rather than a blocking sleep here (review F1/F2).
fn write_injection_phase1(writer: &mut dyn Write, label: &str, body: &str) -> std::io::Result<()> {
    let line = visible_injection_line(&scrub_controls(label), &scrub_controls(body));
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Phase 2 of a deferred injection: the lone `\r` that submits whatever
/// phase 1 already typed -- the *only* control byte either write carries.
/// Also `wrap`'s own convention (F4, review PR #116): `wrap`'s pump loop
/// calls this exact function for its `Action::Compact` and T13 mail-advisory
/// injections, so the two modules cannot drift on what "the submitting
/// keypress" writes.
///
/// Safe to retry: a caller whose write fails (a closed pty, a poisoned lock)
/// simply calls this again later. Worst case a retry lands after an earlier
/// attempt actually succeeded despite reporting failure, which types one
/// extra `\r` into an already-submitted, now-empty composer -- a harmless
/// no-op keypress, not a second copy of the injected line (phase 1 is never
/// re-sent by a retry; only this function is).
pub(crate) fn write_submit_cr(writer: &mut dyn Write) -> std::io::Result<()> {
    writer.write_all(b"\r")?;
    writer.flush()?;
    Ok(())
}

/// Pure: whether a pending submit with deadline `pending` (`None` means none
/// outstanding) is due to be written as of `now`. Split out of
/// [`Pane::pending_submit_due`] so the "has the deadline passed" arithmetic
/// is testable without a real pane or a real clock race -- only
/// `Instant::now()` plus/minus a `Duration` at the call site.
pub(crate) fn submit_is_due(pending: Option<Instant>, now: Instant) -> bool {
    pending.is_some_and(|deadline| now >= deadline)
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

/// What one scroll request actually did to a pane, so the dashboard can say so
/// instead of leaving the operator with a viewport that did not move and no
/// explanation (the reported failure mode: "the chat window is still not
/// scrollable").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOutcome {
    /// Branch (A): the vt100 scrollback offset moved, and is now this many
    /// rows back from the live view.
    Scrolled(usize),
    /// Branch (A): asked to go further back, but this pane has no more
    /// recorded history.
    AtOldest,
    /// Branch (A): asked to come forward, but the pane is already live.
    AtLive,
    /// Branch (B): the child turned mouse reporting on, so the wheel event was
    /// encoded in the protocol it asked for and handed to it. The child scrolls
    /// itself; the dashboard's own scrollback is not involved.
    ForwardedMouse,
    /// Branch (C): the child is on the alternate screen and did *not* ask for
    /// mouse events. There is no history to show and nobody to hand the event
    /// to, so the only honest thing to do is say so.
    FullScreen,
}

/// Wheel-up's button number in the xterm mouse protocol; wheel-down is the
/// next one. The wheel is reported as buttons 64/65 (the 0b0100_0000 bit is
/// what marks a button number as a wheel event) in every encoding.
const MOUSE_WHEEL_UP: u8 = 64;
const MOUSE_WHEEL_DOWN: u8 = 65;

/// The largest coordinate the default (X10) encoding can express: it packs
/// `32 + coordinate` into one byte, so 223 is the end of the line. `?1006`
/// (SGR) exists precisely because terminals are routinely wider than that,
/// and it is what the dashboard asks its own terminal for
/// (`term::dash_mouse_on_bytes`) -- but a *child* picks its own encoding, so
/// the classic form still has to be encodable.
const MOUSE_X10_MAX: u16 = 223;

/// Pure: one mouse event encoded the way `encoding` says the child wants it.
///
/// `col`/`row` are **pane-local and 1-based** -- the child believes it owns a
/// terminal that starts at its own top-left, so a frame coordinate handed
/// straight through would make it act on the wrong row, which is worse than
/// not scrolling at all. `dash::pane_local_mouse` does the translation.
///
/// `press` picks SGR's final byte (`M` for a press, `m` for a release); wheel
/// events are always presses, in every encoding. The classic encodings cannot
/// say *which* button was released, so a release there is the protocol's
/// "some button came up" code (3) rather than the button's own number.
pub(crate) fn mouse_report_bytes(
    encoding: vt100::MouseProtocolEncoding,
    button: u8,
    col: u16,
    row: u16,
    press: bool,
) -> Vec<u8> {
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let final_byte = if press { 'M' } else { 'm' };
            format!("\x1b[<{button};{col};{row}{final_byte}").into_bytes()
        }
        // The classic `ESC [ M` form, and its UTF-8 variant (`?1005`), which
        // differ only in how a coordinate past 95 is written: one raw byte
        // versus that code point encoded as UTF-8. Both offset by 32, and
        // both clamp rather than wrapping a coordinate they cannot express.
        encoding => {
            let mut out = b"\x1b[M".to_vec();
            let utf8 = matches!(encoding, vt100::MouseProtocolEncoding::Utf8);
            let button = if press { button } else { 3 };
            for value in [u16::from(button), col, row] {
                let value = value.min(MOUSE_X10_MAX) + 32;
                if utf8 {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(
                        char::from_u32(u32::from(value))
                            .unwrap_or(' ')
                            .encode_utf8(&mut buf)
                            .as_bytes(),
                    );
                } else {
                    out.push(value as u8);
                }
            }
            out
        }
    }
}

/// Branch (A), against a parser rather than a whole pane: moves `parser`'s
/// scrollback offset by `delta` rows and reports what happened. Split out so
/// the clamped ends -- and the alternate screen's total absence of scrollback
/// -- are testable without a pty child.
fn scroll_parser(parser: &mut vt100::Parser, delta: isize) -> ScrollOutcome {
    let before = parser.screen().scrollback();
    let want = scroll_offset(before, delta, usize::MAX);
    parser.screen_mut().set_scrollback(want);
    let after = parser.screen().scrollback();
    if after != before {
        ScrollOutcome::Scrolled(after)
    } else if delta > 0 {
        ScrollOutcome::AtOldest
    } else {
        ScrollOutcome::AtLive
    }
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
    /// Issue #169: the role this pane was ACTUALLY spawned with
    /// (`PaneSpec::role`), server-side and forgery-proof -- a pane's own
    /// child cannot widen it after the fact, since nothing here ever reads
    /// it back from anything the child says. `dash::mod::parent_role_for`
    /// reads this instead of assuming every live pane is a `Worker`.
    role: PromptRole,
    session_id: String,
    parser: vt100::Parser,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// P2/P3: this child's membership in the console-close pid registry and
    /// its kill-on-close job object. Held for the child's whole life and
    /// released by `shutdown`/`finish_shutdown` once the child is confirmed
    /// gone -- so closing the dashboard's window, or killing the dashboard
    /// outright, takes the pane's agent with it instead of orphaning it.
    lifecycle: supervise::ChildGuard,
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
    /// Whether this pane's adapter has a real turn-signal mechanism
    /// (`AgentAdapter::capabilities().turn_signal`), captured once at spawn
    /// time from the adapter the caller resolved for `spec.agent_name`. Drives
    /// which branch of [`pane_is_idle`] `state()` uses, and whether `drain()`
    /// clears a pending injection/operator-typing flag on quiescence rather
    /// than waiting for a turn signal that will never come for this pane.
    turn_signal_capable: bool,
    /// `dash.idle_quiet_ms`, resolved to a `Duration` once at spawn time --
    /// the quiet window [`output_quiescent`] measures a signal-less pane's
    /// idleness against. Unread by a signal-carrying pane.
    idle_quiet: Duration,
    /// When zirv itself last typed into this pane -- a successful
    /// `inject_visible`, or `write_operator_input` (any keystroke the
    /// dashboard forwarded). Folded together with `last_output_at` by
    /// [`signal_less_quiescent`] (via [`latest_of`]) for a signal-less pane's
    /// idleness and for `drain()`'s flag-clearing: without this, an injection
    /// or a keystroke -- both of which only ever happen once the pane already
    /// reads quiet -- left `last_output_at` untouched, so the very next
    /// `drain()` tick still saw the pane as quiet and immediately cleared the
    /// flag it had just set (review-caught H1). Unread by a signal-carrying
    /// pane, same as `idle_quiet`.
    last_local_input_at: Option<Instant>,
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
    /// Issue #115: the address this worker pane was told, at spawn time, to
    /// report its outcome back to (`spawnreq::SpawnRequest::requested_by`,
    /// via `compose_worker_prompt`/`worker_task_prompt`'s report-back
    /// layer) -- `Some` only when the caller judged the requester
    /// addressable AND mail delivery enabled, i.e. only when a real
    /// report-back instruction was actually attached to this pane's launch.
    /// `None` for the dashboard's own orchestrator pane and for a worker
    /// pane whose requester could not be named. Set once, by
    /// `set_report_to`, never by `Pane::spawn` itself -- the caller
    /// (`fulfill_spawn_request`) already computed `req.requested_by`'s
    /// addressability for the report-back layer and is the only place that
    /// answer exists.
    report_to: Option<String>,
    /// Security review Finding 1 (2026-08-28): this pane's OWN spawn-request
    /// intake directory (`spawnreq::pane_request_dir_for`), the one path this
    /// pane's child tree was told about through `DASH_REQUESTS_ENV`. The
    /// dashboard drains it separately from every other pane's, so a request
    /// found here is, server-side, a request from THIS pane -- the identity
    /// `dash::mod::fulfill_spawn_request` classifies lineage by, instead of
    /// believing whatever `SpawnRequest::parent_session` claims. `None` for a
    /// pane with no channel of its own (every test pane, and a spawn whose
    /// directory could not be created), which can then only ever be the
    /// requester of nothing.
    intake_dir: Option<PathBuf>,
    /// Security review Finding 2 (2026-08-28): the `group::WorkGroup` this
    /// pane was spawned into (`spawnreq::SpawnRequest::work_group_id`), if
    /// any. A dashboard-spawned coordinator claims it at spawn and the
    /// dashboard closes it when this pane's own child exits -- the pane-side
    /// mirror of `agent::run_with`'s claim/close pair, without which a
    /// dash-spawned coordinator's group stayed open and unclaimed forever.
    /// Also what `dash::mod::on_quit` persists into the restore roster, so a
    /// restored pane comes back inside the same group.
    work_group_id: Option<String>,
    /// Whether `report_back_reminder_sweep`'s one-shot completion reminder
    /// has already been injected into this pane. Set the moment that
    /// injection succeeds and never cleared again -- unlike
    /// `injected_awaiting_turn`, a turn boundary does not reset it, because
    /// the whole point is "remind at most once in this pane's life," not
    /// "once per turn."
    report_reminder_sent: bool,
    /// Review F1/F2 (PR #116): the deadline for phase 2 of a deferred
    /// `inject_visible` call -- `Some` from the moment phase 1's write
    /// succeeds until phase 2's lone `\r` is actually written, `None`
    /// otherwise. `Pane::pending_submit_due`/`Pane::submit_pending` are what
    /// the dashboard's tick loop polls and drains; see
    /// [`INJECTION_SUBMIT_DELAY`]'s own doc comment for why this replaced an
    /// inline sleep.
    pending_submit: Option<Instant>,
    /// Issue #160 finding 1, review round (2026-08-28): the `LaunchMode`
    /// this pane was ACTUALLY spawned with, derived from `turn_env` itself
    /// (whether it carried the durable interactive-launch pin,
    /// `adapters::LAUNCH_MODE_ENV`/`LAUNCH_MODE_INTERACTIVE_VALUE`) rather
    /// than trusted as a separate parameter that could drift out of sync
    /// with what the child actually inherited. `dash::mod::on_quit` reads
    /// this back (`launch_mode()`) to roster `RosterPane::interactive`, so a
    /// restore can relaunch the pane on the same terms it originally had --
    /// see `restored_pane_turn_env`'s own doc comment for why an
    /// unconditional restore-as-Interactive was wrong.
    launch_mode: super::super::adapters::LaunchMode,
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
    ///
    /// `turn_signal_capable`/`idle_quiet` seed [`Pane::turn_signal_capable`]/
    /// [`Pane::idle_quiet`]: the caller resolves the adapter for
    /// `spec.agent_name` anyway (to build `argv`/`turn_env`), so it passes
    /// `adapter.capabilities().turn_signal` and `dash.idle_quiet_ms` straight
    /// through rather than this module re-resolving the adapter itself.
    ///
    /// `cwd` and `repo` are deliberately separate parameters (issue #119,
    /// code review round): `cwd` is where the child process actually runs
    /// (`command.cwd(cwd)`) -- for a dashboard-accepted linked `git worktree
    /// add` sibling, that is the worktree's own path -- while `repo` is the
    /// identity this pane's `sessions::Record` is stamped with
    /// (`Record::new(.., repo, ..)`), which drives `repo_slug` and therefore
    /// every mailbox lookup (`mail_sweep`, `apply_mail_effect`,
    /// `build_mail_view`, `zirv ctx nudge --to-session`). Those two must stay
    /// keyed off the *dashboard's own* repo regardless of which worktree the
    /// pane's argv runs in: the session/state store is shared across every
    /// pane this dashboard hosts, and a worktree-hosted pane whose `Record`
    /// pointed at the worktree instead would register under a mailbox slug
    /// nothing sweeps. Every ordinary (non-worktree) spawn passes the same
    /// path for both.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        spec: PaneSpec,
        state: &StateDir,
        cwd: &Path,
        repo: &Path,
        size: (u16, u16),
        turn_env: &[(String, String)],
        turn_signal_capable: bool,
        idle_quiet: Duration,
    ) -> CtxResult<Pane> {
        let PaneSpec {
            agent_name,
            argv,
            role,
            verb,
            session_id,
            title,
        } = spec;

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
        command.cwd(cwd);

        sessions::scrub_supervision_env(&mut command);
        // Issue #160 finding 1, review round (2026-08-28): derived from
        // `turn_env`'s own content rather than a separate parameter -- the
        // single source of truth for what this pane's child actually
        // inherits is the same `turn_env` slice `command.env` is fed from
        // below, so there is no second copy that could ever say something
        // different. See `Pane::launch_mode`'s own doc comment.
        let launch_mode = if turn_env.iter().any(|(k, v)| {
            k == super::super::adapters::LAUNCH_MODE_ENV
                && v == super::super::adapters::LAUNCH_MODE_INTERACTIVE_VALUE
        }) {
            super::super::adapters::LaunchMode::Interactive
        } else {
            super::super::adapters::LaunchMode::Headless
        };
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
        // P2/P3: adopted on the very next statement after the spawn, ahead of
        // every `?` below. Two reasons for that placement: it narrows the
        // window in which a shim's grandchild can appear before the job
        // assignment lands (see `JobGuard`'s own residual note), and it means
        // a `Pane::spawn` that fails half way through -- a reader clone, a
        // writer -- drops this guard and takes the child with it, rather than
        // returning `Err` and leaving an agent running that nothing holds a
        // handle to. `process_id` returns `None` on a backend that cannot
        // report one; there the guard is inert and behaviour is exactly
        // today's.
        let lifecycle = supervise::ChildGuard::adopt(child.process_id());
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

        let mut record = Record::new(&session_id, &agent_name, repo, verb).with_role(role.label());
        // `Record::new` stamps `std::process::id()` -- the dashboard's own pid,
        // identical for every pane, so liveness could not tell one pane's child
        // from another's. Stamp the child's real pid instead. `process_id`
        // returns `None` on a platform that cannot report it; there we leave
        // the dashboard's pid rather than a bogus one.
        //
        // Review round 2 finding 1 (issue #152): `start_time` must move with
        // `pid` in the same breath, via the same `sessions::process_start_secs`
        // reader `Record::new` itself used -- see `SessionGuard::
        // adopt_child_pid`'s doc comment for why leaving the dashboard's own
        // start time in place here is a guaranteed false "dead" the moment
        // this pane's very first liveness probe hits `EPERM` (the everyday
        // sandboxed case issue #146 exists for).
        if let Some(child_pid) = child.process_id() {
            record.pid = child_pid;
            record.start_time = sessions::process_start_secs(child_pid);
        }
        // `owner_pid` is left unset here: `SessionGuard::register` below
        // stamps it with this process's own pid -- the dashboard's -- for
        // every pane, orchestrator and worker alike, the same seam every
        // other registration path shares (`sessions::Record::owner_pid`,
        // `dash::assemble_sidebar`).
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
            role,
            session_id,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_ROWS),
            master,
            child,
            lifecycle,
            writer,
            rx,
            server,
            guard,
            state_dir: state.clone(),
            last_signal_at: None,
            last_output_at: None,
            turn_signal_capable,
            idle_quiet,
            last_local_input_at: None,
            injected_awaiting_turn: false,
            user_typed_since_turn: false,
            exit_code: None,
            done: false,
            report_to: None,
            intake_dir: None,
            work_group_id: None,
            report_reminder_sent: false,
            pending_submit: None,
            launch_mode,
        })
    }

    /// Pumps queued reader-channel bytes into the `vt100` parser, up to
    /// [`DRAIN_BUDGET_BYTES`] per call, and returns `(any, more)`: whether
    /// any bytes were actually processed this call, and whether the budget
    /// cut the drain short with bytes still queued -- so the event loop
    /// knows to come back to this pane next tick rather than blocking on it
    /// now. Also polls the child's exit status (see `poll_exit`): a pane's
    /// own output is the natural place to notice it has stopped producing
    /// any.
    ///
    /// M10: the drain used to loop until the channel was empty. A `cat` of a
    /// large file fills the unbounded channel faster than `vt100` parses it, so
    /// the drain never returned and the whole event loop -- input included, so
    /// `Ctrl+A q` too -- was unreachable for the duration. The budget bounds
    /// one call's work; the remainder waits for the next tick.
    ///
    /// HIGH (review): `any` exists on the return so the caller can cancel a
    /// stale mouse selection the moment new output rewrites this pane's grid
    /// under it -- see `dash::output_cancels_selection` -- without a second,
    /// separate probe of whether this call did anything.
    pub fn drain(&mut self) -> (bool, bool) {
        self.poll_exit();
        let (any, more) = drain_into(&self.rx, &mut self.parser, DRAIN_BUDGET_BYTES);
        if any {
            // O1: recorded, not acted on. Whether these bytes mean "a new turn
            // started" or "the harness repainted the one that just ended" is
            // `signal_still_stands`' decision, and it needs the timestamp to
            // make it.
            self.last_output_at = Some(Instant::now());
        }
        // A signal-less pane's `on_turn_signal` never fires (its socket is
        // never written to -- `register_turn_signal` is a no-op for it), so
        // it is the only place besides a turn signal that clears
        // `injected_awaiting_turn`/`user_typed_since_turn`. Without this, the
        // very first `inject_visible` (or the very first forwarded keystroke)
        // into such a pane would latch it `Working` forever -- exactly the O1
        // bug this module already fixed once for the signal-carrying case,
        // recurring on the one axis that case never had to consider.
        // Quiescence is this pane's only stand-in for a turn boundary, so it
        // is what retires both flags here, the same job a fresh signal does
        // in `on_turn_signal`. Runs every tick (not just when `any`), since a
        // pane that produced nothing at all this tick can still be the tick
        // its quiet window finally closes.
        //
        // H1 (review): checked against `signal_less_quiescent`, not
        // `output_quiescent(self.last_output_at, ...)` alone -- an injection
        // or a forwarded keystroke only ever happens while the pane already
        // reads quiet, so measuring from output alone let the very next
        // `drain()` tick (tens of milliseconds later, well under `idle_quiet`)
        // clear the flag it had just set, before the child had any real
        // chance to respond. Folding `last_local_input_at` in makes the
        // injection/keystroke itself restart the quiet window.
        if !self.turn_signal_capable
            && signal_less_quiescent(
                self.last_output_at,
                self.last_local_input_at,
                Instant::now(),
                self.idle_quiet,
            )
        {
            self.injected_awaiting_turn = false;
            self.user_typed_since_turn = false;
        }
        (any, more)
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

    /// Whether this pane's child currently holds the alternate screen
    /// (`\x1b[?1049h`), and whether it has asked to be sent mouse events --
    /// the two facts every scroll below branches on, stamped on each keylog
    /// scroll line so one capture explains a scroll that appeared to do
    /// nothing.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Whether the child turned on any xterm mouse-reporting mode
    /// (`vt100::Screen::mouse_protocol_mode`). A real harness does: a probe of
    /// this machine's `claude.exe` startup, and the recorded
    /// `tests/fixtures/claude-session.raw`, both show `?1000h ?1002h ?1003h
    /// ?1006h` -- it is a full-screen TUI that scrolls itself and wants the
    /// events to do it with.
    pub fn wants_mouse(&self) -> bool {
        !matches!(
            self.parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        )
    }

    /// The wheel, on this pane. `delta` is positive for a scroll back into
    /// history; `col`/`row` are pane-local 1-based coordinates
    /// (`dash::pane_local_mouse`).
    ///
    /// Three branches, decided per pane at scroll time and in this order:
    ///
    /// * **(B) the child asked for mouse events.** It gets the wheel event,
    ///   encoded in the protocol it selected, and scrolls itself. The
    ///   dashboard's own scrollback is not touched -- it is structurally
    ///   always empty for such a child anyway (see below), so consuming the
    ///   wheel for it was the whole bug: the dashboard swallowed the event on
    ///   behalf of a buffer that could never fill, instead of passing it to
    ///   the child that had asked for it.
    /// * **(C) no mouse reporting, but the child is on the alternate screen.**
    ///   vt100 hard-codes the alternate grid's scrollback to zero
    ///   (`vt100-0.16.2/src/screen.rs:76`, `Grid::new(size, 0)`) and
    ///   `set_scrollback` clamps to `self.scrollback.len()` on whichever grid
    ///   is drawing, so nothing can move there no matter how large
    ///   [`SCROLLBACK_ROWS`] is. Nothing to show and nobody to hand the event
    ///   to: say so rather than failing silently.
    /// * **(A) the normal screen.** The vt100 scrollback offset moves, which
    ///   is correct and genuinely useful for a child that is not a full-screen
    ///   TUI. Clamped at both ends: [`scroll_offset`] holds the bottom at `0`,
    ///   and `set_scrollback` clamps the top to however much history this pane
    ///   actually has (which is why `max` is `usize::MAX` here -- vt100 owns
    ///   that bound and is the only thing that can see it).
    ///
    /// Branch (B) writes through [`Pane::write_input`], deliberately **not**
    /// `write_operator_input`: a forwarded wheel event is navigation, not
    /// prompt composition, and `write_operator_input` would additionally mark
    /// the pane as "the operator has typed since the last turn boundary" (F1),
    /// keeping it out of reach of the idle-gated injectors for as long as
    /// somebody keeps scrolling.
    pub fn scroll_wheel(&mut self, delta: isize, col: u16, row: u16) -> CtxResult<ScrollOutcome> {
        if self.wants_mouse() {
            let button = if delta > 0 {
                MOUSE_WHEEL_UP
            } else {
                MOUSE_WHEEL_DOWN
            };
            let bytes = mouse_report_bytes(
                self.parser.screen().mouse_protocol_encoding(),
                button,
                col,
                row,
                true,
            );
            self.write_input(&bytes)?;
            return Ok(ScrollOutcome::ForwardedMouse);
        }
        Ok(self.scroll_by(delta))
    }

    /// A mouse *button* press or release, on the same terms as the wheel: the
    /// child gets it only if it asked for mouse reporting, in its own
    /// encoding and its own coordinates. Returns whether it was forwarded, so
    /// a click over a child that never asked is dropped rather than typed at
    /// it.
    ///
    /// The dashboard enables `?1000h` + `?1002h` + `?1006h` at its own
    /// terminal and deliberately not `?1003h` (`term::dash_mouse_on_bytes`),
    /// so what can arrive here -- and therefore what a child can be sent --
    /// is the wheel and button presses/releases, never free-running hover.
    /// `?1002h` does let a `Drag` event reach the dashboard's own event loop
    /// now, but `dash::mod` never routes one here: a child that wants mouse
    /// events gets its click forwarded through this function exactly as
    /// before, and the drag itself is simply not acted on for it (the same
    /// "unhandled mouse kind" fate every `Drag` had before `?1002h` was even
    /// turned on). Only a pane that does *not* want mouse reporting gets
    /// zirv's own click-drag text selection out of that same event.
    pub fn forward_mouse_button(
        &mut self,
        button: u8,
        press: bool,
        col: u16,
        row: u16,
    ) -> CtxResult<bool> {
        if !self.wants_mouse() {
            return Ok(false);
        }
        let bytes = mouse_report_bytes(
            self.parser.screen().mouse_protocol_encoding(),
            button,
            col,
            row,
            press,
        );
        self.write_input(&bytes)?;
        Ok(true)
    }

    /// The keyboard scroll bindings' half of the same decision: the vt100
    /// scrollback offset when there is one, and [`ScrollOutcome::FullScreen`]
    /// when the child owns the screen. No mouse event is synthesised here --
    /// `Ctrl+A PageUp` is not a wheel notch, and an *unprefixed* `PageUp`
    /// already reaches the child as itself, which is how a full-screen TUI is
    /// meant to be paged.
    pub fn scroll_by(&mut self, delta: isize) -> ScrollOutcome {
        if self.alternate_screen() {
            return ScrollOutcome::FullScreen;
        }
        scroll_parser(&mut self.parser, delta)
    }

    /// A half-screen of scrolling, the step `Ctrl+A PageUp`/`PageDown` moves.
    /// Half rather than a full screen so the operator keeps a few lines of
    /// overlap to read against, which is what `less`, tmux and every pager
    /// converged on.
    pub fn scroll_page(&mut self, up: bool) -> ScrollOutcome {
        let (rows, _) = self.parser.screen().size();
        let half = (rows as isize / 2).max(1);
        self.scroll_by(if up { half } else { -half })
    }

    /// Jumps to the oldest row this pane still has (`Ctrl+A Home`).
    /// `set_scrollback` clamps to the real length, so `usize::MAX` means "as
    /// far back as there is". A full-screen child has no history to jump into,
    /// so it reports [`ScrollOutcome::FullScreen`] rather than pretending to
    /// have moved.
    pub fn scroll_to_top(&mut self) -> ScrollOutcome {
        if self.alternate_screen() {
            return ScrollOutcome::FullScreen;
        }
        let before = self.scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let after = self.scrollback();
        if after != before {
            ScrollOutcome::Scrolled(after)
        } else {
            ScrollOutcome::AtOldest
        }
    }

    /// Back to the live view (`Ctrl+A End`, and every keystroke the operator
    /// sends the child -- see [`Pane::write_operator_input`]).
    pub fn scroll_to_live(&mut self) -> ScrollOutcome {
        if self.alternate_screen() {
            return ScrollOutcome::FullScreen;
        }
        let before = self.scrollback();
        self.parser.screen_mut().set_scrollback(0);
        if before == 0 {
            ScrollOutcome::AtLive
        } else {
            ScrollOutcome::Scrolled(0)
        }
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
    ///
    /// H1 (review): also stamps `last_local_input_at`, on the same
    /// before-the-write terms as `user_typed_since_turn` right above -- a
    /// signal-less pane's quiescence check folds this in
    /// ([`signal_less_quiescent`]), so a keystroke now holds such a pane
    /// non-idle for a full `idle_quiet` window measured from the operator's
    /// own *last* key, not merely until the next `drain()` tick happens to
    /// run.
    ///
    /// F1/F2 (review, PR #116): flushes a pending deferred submit
    /// (`Self::pending_submit`) first, best-effort, before the keystroke --
    /// an operator who starts typing before an injection's own settle
    /// deadline has elapsed must not have their own text land in the
    /// composer ahead of the still-unsubmitted injected line. A failed flush
    /// is not fatal here: `pending_submit` simply stays set and the tick
    /// loop retries it, and the operator's own keystroke still reaches the
    /// child either way.
    pub fn write_operator_input(&mut self, bytes: &[u8]) -> CtxResult<()> {
        if self.has_pending_submit() {
            let _ = self.submit_pending();
        }
        self.user_typed_since_turn = true;
        self.last_local_input_at = Some(Instant::now());
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
            pane_is_idle(
                self.turn_signal_capable,
                self.last_signal_at,
                self.last_output_at,
                self.last_local_input_at,
                Instant::now(),
                IDLE_DEBOUNCE,
                self.idle_quiet,
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

    /// Whether this pane's own turn-signal socket bound successfully at
    /// spawn time -- `Pane::spawn`'s own doc comment: "a bind failure
    /// degrades this pane to unsupervised (`reachable: false` on its
    /// registry record) rather than failing the spawn." Fixed for the
    /// pane's whole life (the bind is attempted exactly once, in `spawn`),
    /// so this is always current -- no registry re-read needed the way a
    /// liveness probe would.
    ///
    /// Issue #209/v3 codex review finding 5: the footer's supervision
    /// segment reads this for the focused pane instead of assuming every
    /// alive pane is supervised.
    pub fn reachable(&self) -> bool {
        self.server.is_some()
    }

    /// This pane's own zirv session id (the uuid `PaneSpec::session_id`
    /// carried in) -- the roster's own `RosterPane::session_id`, and what a
    /// verified adapter's `resume_args` is asked to resume.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Issue #115: records the address this pane was told to report its
    /// outcome back to (`None` if it was not told at all). Meant to be
    /// called at most once, right after `Pane::spawn`, by the same caller
    /// that decided whether a report-back instruction was actually attached
    /// to this pane's launch prompt -- see [`Pane::report_to`]'s own doc
    /// comment. Also resets `report_reminder_sent`, so a pane freshly given
    /// a target is always eligible for its one reminder.
    pub fn set_report_to(&mut self, report_to: Option<String>) {
        self.report_to = report_to;
        self.report_reminder_sent = false;
    }

    /// The address `report_back_reminder_sweep` reminds this pane to report
    /// its outcome back to, if any.
    pub fn report_to(&self) -> Option<&str> {
        self.report_to.as_deref()
    }

    /// Security review Finding 1: records the spawn-request directory this
    /// pane's own child tree was handed (`DASH_REQUESTS_ENV`). Called right
    /// after `Pane::spawn` by the caller that minted the directory and put it
    /// in this pane's `turn_env` -- the two must always name the same path,
    /// which is what makes "a request arrived in this directory" mean "this
    /// pane asked for it". See [`Pane::intake_dir`]'s own field comment.
    pub fn set_intake_dir(&mut self, dir: PathBuf) {
        self.intake_dir = Some(dir);
    }

    /// This pane's own spawn-request intake directory, if it was given one.
    pub fn intake_dir(&self) -> Option<&Path> {
        self.intake_dir.as_deref()
    }

    /// Security review Finding 2: records the work group this pane was
    /// spawned into. Called right after `Pane::spawn` by the caller that
    /// already admitted the spawn into that group -- see
    /// [`Pane::work_group_id`]'s own field comment.
    pub fn set_work_group_id(&mut self, id: Option<String>) {
        self.work_group_id = id;
    }

    /// The work group this pane belongs to, if any.
    pub fn work_group_id(&self) -> Option<&str> {
        self.work_group_id.as_deref()
    }

    /// Whether `report_back_reminder_sweep`'s one-shot reminder has already
    /// been injected into this pane.
    pub fn report_reminder_sent(&self) -> bool {
        self.report_reminder_sent
    }

    /// Marks this pane as having received its one-shot report-back reminder,
    /// so `report_back_reminder_sweep` never injects a second one.
    pub fn mark_report_reminder_sent(&mut self) {
        self.report_reminder_sent = true;
    }

    /// Whether this pane's child has produced any output at all since it was
    /// spawned -- `report_back_reminder_sweep`'s cheapest available signal
    /// for "this worker actually ran and went quiet" as opposed to "this
    /// pane has never done anything yet" (both read as merely `injectable()`
    /// otherwise). Reuses `last_output_at`, the same timestamp `drain()`
    /// already stamps on every batch of bytes read from the child, rather
    /// than adding a new field that duplicates it.
    pub fn has_produced_output(&self) -> bool {
        self.last_output_at.is_some()
    }

    /// This pane's registry verb (`Verb::Chat` for the orchestrator,
    /// `Verb::Dash` for a worker pane) -- see the field's own doc comment.
    pub fn verb(&self) -> Verb {
        self.verb
    }

    /// The role this pane was ACTUALLY spawned with (issue #169) -- what
    /// `dash::mod::parent_role_for` reads instead of assuming every live
    /// pane is a `Worker`. Set once, at `Pane::spawn`, from `PaneSpec::role`;
    /// nothing here ever revises it from anything the pane's own child says.
    pub fn role(&self) -> PromptRole {
        self.role
    }

    /// The `LaunchMode` this pane was ACTUALLY spawned with (issue #160
    /// finding 1) -- derived once, at `Pane::spawn`, from whether `turn_env`
    /// carried the durable interactive-launch pin. `dash::mod::on_quit`
    /// reads this back to roster `RosterPane::interactive`, so a restore
    /// can relaunch this pane on the same terms it originally had, rather
    /// than unconditionally as `Interactive`.
    pub fn launch_mode(&self) -> super::super::adapters::LaunchMode {
        self.launch_mode
    }

    /// The sidebar's one-line preview: the bottom-most non-blank row of this
    /// pane's current screen.
    pub fn last_line(&self) -> String {
        last_line_of(self.screen())
    }

    /// Writes a visible, clearly-labelled line into the child's own pty --
    /// `"[zirv ▸ {label}] {body}"` -- and schedules the lone `\r` that
    /// submits it for at least [`INJECTION_SUBMIT_DELAY`] later (issue #114
    /// / review F1/F2, PR #116: [`write_injection_phase1`] /
    /// [`Self::pending_submit`]). Used by Task 9's idle-gated intervention
    /// (an operator nudge, a swept mail message, or a report-back reminder)
    /// to put text in front of the agent the same way a human typing at the
    /// prompt would, rather than any side channel the agent has to know to
    /// look for.
    ///
    /// The caller must have already checked `state() == PaneState::Idle`:
    /// this method does not gate itself -- writing into a `Working` pane
    /// would interleave with whatever the agent is already sending, which is
    /// exactly the failure mode idle-gating exists to prevent.
    ///
    /// Returns as soon as phase 1's write lands -- **no sleeping here** (F2:
    /// this used to block the caller for `INJECTION_SUBMIT_DELAY`, which on
    /// the dashboard is the single UI thread every sweep shares). On success
    /// the pane reports `Working` until its next turn signal
    /// (`injected_awaiting_turn`), so a second idle-gated caller later in the
    /// same tick sees a busy pane rather than the stale `Idle` this one just
    /// acted on, and `last_local_input_at`/`pending_submit` are both stamped
    /// immediately at phase 1 -- not deferred to phase 2 -- so a signal-less
    /// pane's quiet window and the retry sweeps both see this injection as
    /// "in flight" the instant it starts, not only once it is fully
    /// submitted. A failed phase-1 write leaves every flag alone: no bytes
    /// are known to have reached the child, so no turn is pending and no
    /// submission is owed; the caller's `Err` surfaces it rather than
    /// silently limping on.
    ///
    /// Phase 2 -- the actual `\r` -- is drained later, by
    /// [`Self::submit_pending`], called once [`Self::pending_submit_due`]
    /// says the deadline has passed (`dash::mod::run_dashboard`'s own tick
    /// loop does this for every pane, every tick) or eagerly by
    /// [`Self::write_operator_input`] if the operator starts typing into this
    /// pane before the deadline arrives on its own.
    pub fn inject_visible(&mut self, label: &str, body: &str) -> CtxResult<()> {
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer;
            write_injection_phase1(sink, label, body)?;
        }
        let now = Instant::now();
        self.last_local_input_at = Some(now);
        self.injected_awaiting_turn = true;
        self.pending_submit = Some(now + INJECTION_SUBMIT_DELAY);
        Ok(())
    }

    /// Whether this pane has a deferred injection submission
    /// (`Self::pending_submit`) whose deadline has passed as of `now`. The
    /// dashboard's tick loop calls this for every pane, every tick, and
    /// [`Self::submit_pending`] on the ones that answer `true`.
    pub(crate) fn pending_submit_due(&self, now: Instant) -> bool {
        submit_is_due(self.pending_submit, now)
    }

    /// Whether this pane has ANY deferred injection submission outstanding,
    /// regardless of whether its deadline has passed yet -- what
    /// `write_operator_input` checks before an operator's own keystroke
    /// reaches the composer, so a half-typed injection is never left sitting
    /// unsubmitted behind whatever the operator types next.
    pub(crate) fn has_pending_submit(&self) -> bool {
        self.pending_submit.is_some()
    }

    /// Writes the lone `\r` that submits a deferred `inject_visible` call
    /// ([`write_submit_cr`]) and clears [`Self::pending_submit`] -- but only
    /// once the write itself succeeds. A failed write leaves
    /// `pending_submit` set exactly as it was, so the next call (the tick
    /// loop's next pass, or the operator's next keystroke) simply retries
    /// it; see `write_submit_cr`'s own doc comment for why a retried `\r` is
    /// always safe. A no-op, successfully, when nothing is pending.
    pub fn submit_pending(&mut self) -> CtxResult<()> {
        if self.pending_submit.is_none() {
            return Ok(());
        }
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer;
            write_submit_cr(sink)?;
        }
        self.pending_submit = None;
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
        // P2/P3: the child is gone (or as gone as `quit_child` could make
        // it), so it must leave the console-close registry and its job handle
        // must close -- an explicit call, not `Drop`, because the release
        // profile is `panic = "abort"`.
        self.lifecycle.release();
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
            // P1: tree-kill first, narrow kill second. `Child::kill()` is a
            // `TerminateProcess` against the *direct* child, which for an
            // npm-installed agent is `cmd.exe /c claude.cmd` -- killing it
            // left the real `node` agent running with nothing watching it.
            // The tree-kill is best-effort and its return value is not
            // evidence of anything, so the narrow kill still runs behind it
            // and `wait` remains the only proof of death. Never a control
            // byte into the pty master: conhost broadcasts those to every
            // client of the pseudoconsole (see `wrap::quit_child`).
            #[cfg(not(unix))]
            if let Some(pid) = self.child.process_id() {
                supervise::kill_tree(pid);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.lifecycle.release();
        wrap::unpublish_socket_path(&self.state_dir, &self.session_id);
        self.guard.release();
        Ok(())
    }

    /// Issue #84: swaps this pane's harness/model in place, keeping its
    /// registry short id (the same socket, the same mail/nudge address) --
    /// only the pty, the child, its job/console-close guard, the writer, the
    /// reader channel, the vt100 screen, and the turn-signal capability/
    /// idle-quiet knobs the new adapter carries are replaced. Mirrors
    /// `Pane::spawn`'s own pty assembly and `wrap::quit_child`'s ask-then-
    /// escalate shutdown of the old child; resolving the new adapter, its
    /// argv, and its turn-signal env goes through the exact same
    /// `handover::resolve_swap_launch`/`build_turn_env` seams
    /// `wrap::perform_handover_swap` uses, so the two live-swap call sites
    /// can never drift on what a swap's fresh launch actually carries. The
    /// handoff packet rides the same positional/task-prompt channel every
    /// restart already uses (`wrap::restart_prompt` -- never a system-prompt
    /// injection), so a target adapter with no system-prompt mechanism at
    /// all (codex) receives it exactly the same way.
    ///
    /// The caller has already decided this is a safe moment to act (`Pane::
    /// state() == PaneState::Idle`, or the operator's own explicit override)
    /// before calling this -- this method itself does not gate on idleness,
    /// the same division of responsibility `inject_visible` already follows.
    pub fn handover(
        &mut self,
        cfg: &super::super::config::CtxConfig,
        req: &super::super::handover::HandoverRequest,
        handoff_note: &super::super::handoff::Handoff,
        role: PromptRole,
        repo: &Path,
        size: (u16, u16),
    ) -> CtxResult<()> {
        let (new_adapter, extra) = super::super::handover::resolve_swap_launch(cfg, req)?;
        let new_argv: Vec<String> = {
            let prompt_text = wrap::restart_prompt(handoff_note);
            let command = new_adapter.interactive_cmd(Some(&prompt_text), &extra);
            std::iter::once(command.get_program().to_string_lossy().to_string())
                .chain(command.get_args().map(|a| a.to_string_lossy().to_string()))
                .collect()
        };
        // NON-GOAL residual (2026-08-28, filed rather than silently
        // omitted): `handover::build_turn_env` does not push the durable
        // interactive-launch pin, so `self.launch_mode` still reads this
        // pane's ORIGINAL spawn mode after a handover even though the
        // successor child below never actually receives the pin either
        // way. Out of scope for issue #160's fix round, which named exactly
        // three call sites (`fulfill_spawn_request`, `run_dashboard`'s
        // first pane, `restored_pane_turn_env`), all in `dash::mod`, not
        // this one -- a pane that both underwent a handover AND survives a
        // later dashboard restore is the only case this residual reaches.
        let turn_env = super::super::handover::build_turn_env(
            new_adapter.as_ref(),
            self.server.as_ref(),
            &self.session_id,
            repo,
            role,
            req.target_model.as_deref(),
        );
        let quit_sequence = new_adapter.quit_sequence().to_string();
        let new_agent_name = new_adapter.name().to_string();
        let turn_signal_capable = new_adapter.capabilities().turn_signal;
        let idle_quiet = Duration::from_millis(cfg.dash.idle_quiet_ms);

        // Finding #2: every fallible step for the *successor* runs first,
        // before the old child is touched at all. Previously the old child
        // was quit and its lifecycle released up front, so a later failure
        // here (a missing adapter binary hitting `guard_cmd_shim_reparse`,
        // a pty/spawn failure) left a dead pane still pinned to the
        // dashboard's own pid with no child to show for it. Now, on any
        // `?` below, `self` is untouched and the old child keeps running.
        let (cols, rows) = size;
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let (program, rest) = new_argv
            .split_first()
            .ok_or("dashboard pane: empty argv, nothing to spawn")?;
        super::super::adapters::guard_cmd_shim_reparse(program, rest)?;
        let mut command = CommandBuilder::new(program);
        for arg in rest {
            command.arg(arg);
        }
        command.cwd(repo);
        sessions::scrub_supervision_env(&mut command);
        for (key, value) in &turn_env {
            command.env(key, value);
        }

        let mut first_writer = pair.master.take_writer()?;
        wrap::answer_inherit_cursor_probe(&mut *first_writer);
        let writer = Arc::new(Mutex::new(first_writer));

        let child = pair.slave.spawn_command(command)?;
        let lifecycle = supervise::ChildGuard::adopt(child.process_id());
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

        // The successor is fully assembled and alive now -- only committing
        // remains, so it is safe to retire the old child.
        //
        // P5 (mirrors `wrap`'s own restart/handover arm): park the record on
        // the dashboard's own (unquestionably alive) pid for the duration of
        // the swap, so a concurrent `sessions::list` sweep -- the
        // dashboard's own ~1s refresh included -- can never delete this very
        // much live pane's record while the old child is being killed and no
        // new one exists yet.
        self.guard.adopt_child_pid(std::process::id());
        {
            let mut writer_guard = self
                .writer
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer_guard;
            wrap::quit_child(sink, &mut self.child, &quit_sequence, QUIT_GRACE)?;
        }
        // The old child is gone (or as gone as `quit_child` could make it),
        // so it leaves the console-close registry and its job handle closes
        // -- the same explicit release `shutdown` performs, except this pane
        // is not itself ending: the new child's own guard, adopted above, is
        // committed into `self.lifecycle` right below.
        self.lifecycle.release();

        if let Some(child_pid) = child.process_id() {
            self.guard.adopt_child_pid(child_pid);
        }

        self.agent_name = new_agent_name;
        self.master = master;
        self.child = child;
        self.lifecycle = lifecycle;
        self.writer = writer;
        self.rx = rx;
        self.parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        self.turn_signal_capable = turn_signal_capable;
        self.idle_quiet = idle_quiet;
        self.last_signal_at = None;
        self.last_output_at = None;
        self.last_local_input_at = None;
        self.injected_awaiting_turn = false;
        self.user_typed_since_turn = false;
        self.exit_code = None;
        // F1/F2: the old child's pty is gone, so any deferred `\r` it was
        // still owed would now write into the successor's composer instead
        // -- drop it rather than carry it across the swap.
        self.pending_submit = None;
        // F5 (review, PR #116): the one-shot report-back reminder is scoped
        // to a child SESSION, not to this pane's own lifetime across a swap.
        // A handover keeps `report_to` (the requester is still owed a
        // report from whichever session is now running in this pane) but a
        // successor that has not yet reported anything must be eligible for
        // its own reminder -- unlike a restore (F3), which resurrects the
        // SAME logical session and must therefore keep its sent flag.
        self.report_reminder_sent = false;

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

    // Task A: the signal-less idleness path (`output_quiescent`/
    // `pane_is_idle`). A codex-shaped adapter never reports a turn boundary at
    // all, so gating idleness on one leaves such a pane `Working` forever;
    // these cover the output-quiescence stand-in on its own terms, pure and
    // without a real child.

    /// No output ever recorded is not quiet, whatever else has happened --
    /// the signal-less mirror of `signal_still_stands`' own "no signal ever
    /// seen -- not idle" rule. A pane still starting up (its harness has not
    /// drawn a first frame yet) must not read the same as one sitting quietly
    /// at its prompt just because both currently have no timestamp.
    #[test]
    fn output_quiescent_is_false_with_no_output_ever_recorded() {
        let quiet = Duration::from_millis(200);
        let now = Instant::now();
        assert!(!output_quiescent(None, now, quiet));
        assert!(
            !output_quiescent(None, now + Duration::from_secs(30), quiet),
            "no amount of elapsed time substitutes for output that never happened"
        );
    }

    /// Once there is a last-output timestamp, quiet is a plain elapsed-time
    /// check against it -- false inside the window, true once it closes, and
    /// it does not decay back to working with nothing further happening.
    #[test]
    fn output_quiescent_flips_once_the_quiet_window_elapses_since_the_last_output() {
        let quiet = Duration::from_millis(200);
        let output = Instant::now();
        assert!(
            !output_quiescent(Some(output), output + Duration::from_millis(50), quiet),
            "still inside the quiet window"
        );
        assert!(
            !output_quiescent(Some(output), output + Duration::from_millis(199), quiet),
            "one tick short of the window is still not quiet"
        );
        assert!(
            output_quiescent(Some(output), output + Duration::from_millis(200), quiet),
            "exactly at the window is quiet"
        );
        assert!(
            output_quiescent(Some(output), output + Duration::from_secs(30), quiet),
            "and it does not decay back once quiet"
        );
    }

    /// `pane_is_idle`'s branch selection: a signal-capable pane is exactly
    /// `signal_still_stands` -- quiet output alone, with no signal ever seen,
    /// is not enough -- while a signal-less pane is exactly
    /// `signal_less_quiescent`, and `signal_at` is never consulted for it at
    /// all (matching reality: nothing ever writes to a signal-less pane's own
    /// turn-signal socket, so a `Some` there could not honestly arise, but
    /// the branch must not depend on that being true to behave correctly).
    /// `local_input_at` is likewise never consulted on the signal-capable
    /// branch.
    #[test]
    fn pane_is_idle_branches_on_turn_signal_capability() {
        let debounce = Duration::from_millis(500);
        let idle_quiet = Duration::from_millis(200);
        let output = Instant::now();

        assert!(
            !pane_is_idle(
                true,
                None,
                Some(output),
                None,
                output + Duration::from_secs(30),
                debounce,
                idle_quiet
            ),
            "signal-capable: no signal ever seen stays working, however long the output has been quiet"
        );
        assert!(
            !pane_is_idle(
                true,
                None,
                Some(output),
                Some(output + Duration::from_secs(29)),
                output + Duration::from_secs(30),
                debounce,
                idle_quiet
            ),
            "signal-capable: local_input_at is never consulted on this branch either"
        );

        assert!(
            !pane_is_idle(
                false,
                Some(output),
                Some(output),
                None,
                output + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "signal-less: still inside its own quiet window"
        );
        assert!(
            pane_is_idle(
                false,
                Some(output),
                Some(output),
                None,
                output + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "signal-less: quiet window elapsed, idle with a signal present"
        );
        assert!(
            pane_is_idle(
                false,
                None,
                Some(output),
                None,
                output + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "signal-less: same outcome with no signal_at at all -- it is never consulted"
        );
    }

    /// H1 (review): local input holds a signal-less pane non-idle for a full
    /// `idle_quiet` window measured from *itself*, even with no child output
    /// at all recorded since -- the fix for the bug where an injection or a
    /// keystroke, which only ever happen while the pane already reads quiet,
    /// left `output_at` untouched and so read as still-quiet on the very next
    /// tick.
    #[test]
    fn pane_is_idle_measures_signal_less_quiet_from_the_latest_of_output_and_local_input() {
        let debounce = Duration::from_millis(500);
        let idle_quiet = Duration::from_millis(200);
        let output = Instant::now();

        // No output at all, only local input: not idle until idle_quiet has
        // elapsed since that input, and idle after.
        assert!(
            !pane_is_idle(
                false,
                None,
                None,
                Some(output),
                output + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "a fresh local input alone must hold the pane non-idle"
        );
        assert!(
            pane_is_idle(
                false,
                None,
                None,
                Some(output),
                output + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "and release it once idle_quiet has elapsed since that input"
        );

        // Output happened first and is already quiet, but local input landed
        // later: the later timestamp governs, not the older output.
        let later_input = output + Duration::from_millis(150);
        assert!(
            !pane_is_idle(
                false,
                None,
                Some(output),
                Some(later_input),
                later_input + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "output alone looks quiet (150ms+50ms=200ms old) but the later local \
             input must be what quiescence is measured from"
        );
        assert!(
            pane_is_idle(
                false,
                None,
                Some(output),
                Some(later_input),
                later_input + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "idle once idle_quiet has elapsed since the later of the two"
        );

        // And symmetrically, output arriving after an old local input is what
        // governs.
        let later_output = output + Duration::from_millis(150);
        assert!(
            !pane_is_idle(
                false,
                None,
                Some(later_output),
                Some(output),
                later_output + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "fresh output after old local input must also restart the window"
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

    /// The root cause of the second scrolling report, pinned against the real
    /// vt100: the alternate screen has **no scrollback at all**. vt100 builds
    /// the alternate grid with `Grid::new(size, 0)` and `set_scrollback` acts
    /// on whichever grid is drawing, so while a full-screen TUI child holds
    /// `\x1b[?1049h` every scroll request clamps straight back to `0` -- no
    /// value of `SCROLLBACK_ROWS` can change that, which is why branch (B)
    /// forwards arrows instead of moving an offset that cannot move.
    #[test]
    fn the_alternate_screen_has_no_scrollback_for_any_offset_to_move_in() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        assert!(!parser.screen().alternate_screen());
        assert!(
            matches!(scroll_parser(&mut parser, 3), ScrollOutcome::Scrolled(3)),
            "sanity: the normal screen scrolls"
        );
        parser.screen_mut().set_scrollback(0);

        // The harness enters full-screen mode.
        parser.process(b"\x1b[?1049h");
        assert!(parser.screen().alternate_screen());
        for line in 0..10 {
            parser.process(format!("alt{line}\r\n").as_bytes());
        }
        assert_eq!(
            scroll_parser(&mut parser, 3),
            ScrollOutcome::AtOldest,
            "nothing was recorded and nothing can move: this is the whole bug"
        );
        assert_eq!(parser.screen().scrollback(), 0);
    }

    /// The same fact against a **real recorded claude session** rather than a
    /// hand-written escape sequence, since two rounds of this bug have already
    /// been fixed against the wrong mechanism.
    /// `tests/fixtures/claude-session.raw` is a gitignored capture of a real
    /// interactive session (present on the machine that recorded it, absent in
    /// CI -- skipped there, like `ui`'s own fixture test): claude sends
    /// `\x1b[?1049h` about six kilobytes in and never leaves for the remaining
    /// ~550 KB, so for essentially the whole session the pane is on the
    /// alternate screen, where vt100 records no history at all. That is why
    /// the previous fix -- raising the parser's scrollback length -- changed
    /// nothing the operator could see.
    #[test]
    fn a_real_claude_session_spends_itself_on_the_alternate_screen() {
        let path = std::path::Path::new("tests/fixtures/claude-session.raw");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!(
                "skipping a_real_claude_session_spends_itself_on_the_alternate_screen: {} not present",
                path.display()
            );
            return;
        };

        let mut parser = vt100::Parser::new(40, 120, SCROLLBACK_ROWS);
        parser.process(&bytes);
        assert!(
            parser.screen().alternate_screen(),
            "a real claude session ends on the alternate screen"
        );
        assert_eq!(
            scroll_parser(&mut parser, 3),
            ScrollOutcome::AtOldest,
            "and there is no scrollback there for any offset to move in -- \
             which is the whole of the reported bug"
        );
        // And it is not merely full-screen: it asked to be sent mouse events
        // (`?1000h ?1002h ?1003h ?1006h` are all in the capture), in the SGR
        // encoding. So the wheel is *its* event -- the dashboard consuming it
        // for a buffer that can never fill is the bug, and branch (B) hands it
        // over instead.
        assert_ne!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "a real claude session turns mouse reporting on"
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr,
            "and selects SGR coordinates (?1006h)"
        );
    }

    /// Branch (A)'s reported outcomes, both ends included -- "already at the
    /// oldest line" and "already at the live view" are what the header says
    /// instead of the silence that got this reported twice.
    #[test]
    fn scroll_parser_reports_movement_and_both_clamped_ends() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        assert_eq!(
            scroll_parser(&mut parser, 3),
            ScrollOutcome::AtOldest,
            "a pane with no history yet cannot scroll back"
        );
        assert_eq!(scroll_parser(&mut parser, -3), ScrollOutcome::AtLive);

        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        assert_eq!(scroll_parser(&mut parser, 3), ScrollOutcome::Scrolled(3));
        assert_eq!(scroll_parser(&mut parser, -1), ScrollOutcome::Scrolled(2));
        assert_eq!(scroll_parser(&mut parser, -5), ScrollOutcome::Scrolled(0));
        assert_eq!(scroll_parser(&mut parser, -5), ScrollOutcome::AtLive);
        // Past the oldest recorded row: vt100 clamps, and the second attempt
        // has genuinely nowhere left to go.
        assert!(matches!(
            scroll_parser(&mut parser, 10_000),
            ScrollOutcome::Scrolled(_)
        ));
        assert_eq!(scroll_parser(&mut parser, 10_000), ScrollOutcome::AtOldest);
    }

    /// Branch (B)'s bytes, in both encodings a child can select. Getting these
    /// wrong makes the child act on the wrong row (or read them as typed
    /// input), which is worse than not scrolling, so the exact sequences are
    /// pinned.
    #[test]
    fn a_forwarded_wheel_encodes_the_way_the_child_asked_for() {
        // SGR (`?1006h`), which is what a real claude session selects: wheel
        // up is button 64, wheel down 65, and a wheel event is a press (`M`).
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Sgr,
                MOUSE_WHEEL_UP,
                7,
                3,
                true
            ),
            b"\x1b[<64;7;3M".to_vec()
        );
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Sgr,
                MOUSE_WHEEL_DOWN,
                7,
                3,
                true
            ),
            b"\x1b[<65;7;3M".to_vec()
        );
        // A release only differs in the final byte -- SGR is the encoding that
        // can still say *which* button came up.
        assert_eq!(
            mouse_report_bytes(vt100::MouseProtocolEncoding::Sgr, 0, 1, 1, false),
            b"\x1b[<0;1;1m".to_vec()
        );
        assert_eq!(
            mouse_report_bytes(vt100::MouseProtocolEncoding::Sgr, 2, 4, 9, true),
            b"\x1b[<2;4;9M".to_vec()
        );
        // The classic form cannot, so a release there is the protocol's
        // "some button came up" code (3), whichever button it was.
        assert_eq!(
            mouse_report_bytes(vt100::MouseProtocolEncoding::Default, 2, 1, 1, false),
            vec![0x1b, b'[', b'M', 3 + 32, 33, 33]
        );
        // SGR is not limited to a byte, which is the whole reason it exists.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Sgr,
                MOUSE_WHEEL_UP,
                400,
                90,
                true
            ),
            b"\x1b[<64;400;90M".to_vec()
        );

        // The classic X10 form: `ESC [ M` then three bytes, each offset by 32.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Default,
                MOUSE_WHEEL_UP,
                7,
                3,
                true
            ),
            vec![0x1b, b'[', b'M', 64 + 32, 7 + 32, 3 + 32]
        );
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Default,
                MOUSE_WHEEL_DOWN,
                1,
                1,
                true
            ),
            vec![0x1b, b'[', b'M', 65 + 32, 33, 33]
        );
        // A coordinate the single byte cannot express clamps instead of
        // wrapping round to the top-left corner.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Default,
                MOUSE_WHEEL_UP,
                400,
                3,
                true
            ),
            vec![0x1b, b'[', b'M', 96, (MOUSE_X10_MAX + 32) as u8, 35]
        );
        // The UTF-8 variant writes the same numbers as code points.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Utf8,
                MOUSE_WHEEL_UP,
                200,
                3,
                true
            ),
            {
                let mut want = b"\x1b[M".to_vec();
                want.push(96);
                want.extend_from_slice("\u{e8}".as_bytes());
                want.push(35);
                want
            }
        );
    }

    /// The encoding is the child's own, read off the parser rather than
    /// assumed: `?1006h` selects SGR, `?1006l` puts it back, and a harness
    /// that never asks for mouse reporting at all must not be sent events.
    #[test]
    fn the_mouse_protocol_is_read_from_the_child_not_assumed() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "a child that has asked for nothing gets nothing"
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );

        // `?1000h` is VT200 press/release tracking in vt100's own mapping
        // (`?9h` is the X10 press-only mode).
        parser.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::PressRelease
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );

        parser.process(b"\x1b[?1006l\x1b[?1000l");
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
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

    /// F7 (review, PR #116): the byte-level invariant that used to be proved
    /// against `injection_bytes` -- a production-dead function that
    /// duplicated `write_injection_phase1`/`write_submit_cr`'s own logic --
    /// is now proved directly against the two real functions the shipped
    /// path calls, via the same `RecordingWriter` seam every other test in
    /// this section uses. Whatever control characters an untrusted body
    /// carries, the bytes that land in the pty across both writes contain
    /// exactly one -- the trailing `\r` that submits the line. Anything else
    /// would be a second submission (an interior `\r`) or an escape sequence
    /// typed at the child.
    #[test]
    fn an_injection_writes_exactly_one_control_byte() {
        let mut writer = RecordingWriter { chunks: Vec::new() };
        write_injection_phase1(
            &mut writer,
            "mail from claude\r/aaaa1111",
            "line one\r\nline two\u{1b}[2Jline three\u{7f}",
        )
        .expect("phase 1 write must succeed against an in-memory sink");
        write_submit_cr(&mut writer).expect("phase 2 write must succeed");

        let bytes: Vec<u8> = writer.chunks.iter().flatten().copied().collect();
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

    /// Records each `write_all` call as its own chunk (this impl always
    /// accepts the whole buffer in one `write` call, so one `write_all`
    /// produces exactly one chunk here), so a test can assert the two-write
    /// shape issue #114 requires without a real pty.
    struct RecordingWriter {
        chunks: Vec<Vec<u8>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.chunks.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Issue #114 / review F1/F2 (PR #116): phase 1 writes only the labelled
    /// line, with no control bytes of its own -- the settle gap before the
    /// submitting `\r` is no longer enforced by a sleep inside this write at
    /// all (see [`INJECTION_SUBMIT_DELAY`]'s own doc comment); it is a
    /// deadline the caller (`Pane::inject_visible`/`Pane::pending_submit`)
    /// schedules and the dashboard's tick loop later drains.
    #[test]
    fn write_injection_phase1_writes_only_the_labelled_line() {
        let mut writer = RecordingWriter { chunks: Vec::new() };

        write_injection_phase1(&mut writer, "nudge from operator", "hello")
            .expect("write must succeed against an in-memory sink");

        assert_eq!(
            writer.chunks.len(),
            1,
            "phase 1 is exactly one write: {:?}",
            writer.chunks
        );
        assert_eq!(
            String::from_utf8_lossy(&writer.chunks[0]),
            "[zirv \u{25b8} nudge from operator] hello",
            "the write is the visible line, with nothing appended"
        );
        assert!(
            !writer.chunks[0].iter().any(|b| *b < 0x20 || *b == 0x7f),
            "phase 1 carries no control bytes of its own: {:?}",
            String::from_utf8_lossy(&writer.chunks[0])
        );
    }

    /// F4 (review, PR #116; issue #118): `write_submit_cr` is the one
    /// function both `dash::pane`'s deferred injections and `wrap`'s own
    /// T13 mail-advisory injection into a `defer_injection_submit` adapter
    /// call for phase 2 -- a due pane's submission is always exactly this
    /// one byte. (`wrap::inject_compact`/`Action::Compact` stays
    /// single-burst; that call site is only ever reachable for claude, see
    /// its own doc comment.)
    #[test]
    fn write_submit_cr_writes_exactly_one_byte() {
        let mut writer = RecordingWriter { chunks: Vec::new() };
        write_submit_cr(&mut writer).expect("write must succeed against an in-memory sink");
        assert_eq!(writer.chunks, vec![b"\r".to_vec()]);
    }

    /// Pure: `submit_is_due` is what `Pane::pending_submit_due` delegates to,
    /// so its three cases are testable without a real clock race -- only
    /// `Instant::now()` plus/minus a `Duration`.
    #[test]
    fn submit_is_due_true_only_once_the_deadline_has_passed() {
        let now = Instant::now();
        assert!(
            !submit_is_due(Some(now + Duration::from_millis(10)), now),
            "not yet due"
        );
        assert!(submit_is_due(Some(now), now), "due exactly at the deadline");
        assert!(
            submit_is_due(Some(now - Duration::from_millis(1)), now),
            "still due once the deadline has passed"
        );
        assert!(!submit_is_due(None, now), "nothing pending is never due");
    }

    /// A writer whose Nth `write` call fails, so a phase-2 retry can be
    /// exercised without a real pty. Every other call (before and after the
    /// failure) records its chunk exactly like `RecordingWriter`.
    struct FlakyWriter {
        calls: usize,
        fail_at: usize,
        chunks: Vec<Vec<u8>>,
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == self.fail_at {
                return Err(std::io::Error::other("simulated write failure"));
            }
            self.chunks.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// F1/F2 (review, PR #116): a phase-2 write failure must retry as a lone
    /// `\r` on the next attempt -- never by re-sending phase 1, which would
    /// type a second copy of the injected line onto the still-unsubmitted
    /// first (the exact bug the redesign exists to close). This is provable
    /// structurally at the function level: `write_submit_cr` never touches
    /// phase 1's text at all, so a retry that only ever calls
    /// `write_submit_cr` again cannot duplicate the line, whatever `Pane`
    /// state wraps it (`Pane::submit_pending` only clears `pending_submit`
    /// after this call returns `Ok`, so a failure here leaves it set for
    /// exactly this retry).
    #[test]
    fn a_failed_submit_cr_write_is_safely_retryable_without_resending_the_line() {
        let mut writer = FlakyWriter {
            calls: 0,
            fail_at: 1,
            chunks: Vec::new(),
        };

        write_submit_cr(&mut writer).expect_err("the first write call fails");
        assert!(
            writer.chunks.is_empty(),
            "a failed write leaves nothing recorded: {:?}",
            writer.chunks
        );

        // Retry: no further failures scheduled.
        writer.fail_at = 0;
        write_submit_cr(&mut writer).expect("the retry succeeds");
        assert_eq!(
            writer.chunks,
            vec![b"\r".to_vec()],
            "the retry writes exactly one lone CR -- never the line again"
        );
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

    /// Task A: a child that prints exactly one line at startup -- standing in
    /// for a real harness drawing its first frame -- and then produces
    /// nothing further for the rest of its (long) life. Lets a test observe a
    /// signal-less pane's quiet window closing against a real, deterministic
    /// last-output timestamp rather than racing a harness that might repaint
    /// on its own.
    #[cfg(windows)]
    pub(crate) fn silent_after_first_line_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "echo hello & ping -n 60 127.0.0.1 >nul".to_string(),
        ]
    }

    #[cfg(unix)]
    pub(crate) fn silent_after_first_line_argv() -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo hello; sleep 60".to_string(),
        ]
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
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

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

    /// Code review (issue #119, round 2), BLOCKER: a worktree-hosted pane's
    /// child runs at `cwd` (the worktree), but its registry `Record` -- and
    /// therefore `repo_slug`, and therefore which mailbox `mail_sweep`/
    /// `zirv ctx nudge --to-session` reads for it -- must stay keyed off the
    /// dashboard's own `repo`, never the worktree it happens to run in. Two
    /// distinct paths prove the split actually reached `Record::new`
    /// (`sessions::resolve_prefix`, the real lookup path `zirv ctx nudge`
    /// itself uses) rather than only the two `Pane::spawn` parameters being
    /// accepted syntactically.
    #[test]
    fn a_pane_spawned_at_a_different_cwd_keeps_the_dashboard_repo_in_its_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let dashboard_repo = tmp.path().join("dashboard-repo");
        let worktree_cwd = tmp.path().join("linked-worktree");
        std::fs::create_dir_all(&dashboard_repo).expect("mkdir dashboard-repo");
        std::fs::create_dir_all(&worktree_cwd).expect("mkdir linked-worktree");
        assert_ne!(
            dashboard_repo, worktree_cwd,
            "the two paths must actually differ for this test to mean anything"
        );

        let mut spec = test_spec("33333333-2222-4333-8444-555555555555");
        // Long-lived, not `trivial_argv()`: `sessions::resolve_prefix` below
        // only returns a `Liveness::Live` record, and a process that has
        // already exited by the time this test gets to it would make the
        // lookup racy rather than proving anything about `Record::repo`.
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &worktree_cwd,
            &dashboard_repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let record = sessions::resolve_prefix(&state, pane.short())
            .expect("the freshly spawned pane must be live and resolvable");
        assert_eq!(
            record.repo, dashboard_repo,
            "Record::repo must be the dashboard's own repo, not the pane's cwd"
        );
        assert_eq!(
            record.repo_slug,
            super::super::super::state::repo_slug(&dashboard_repo),
            "repo_slug (the mailbox key mail_sweep/nudge --to-session actually use) must \
             follow Record::repo"
        );

        pane.shutdown("").expect("shutdown");
    }

    /// Finding #2: a failure in the *successor's* setup (here, a missing
    /// adapter binary -- `resolve_program` does not check existence, so
    /// `ready()`/`resolve_swap_launch` succeed and the OS spawn itself is
    /// what fails) must leave the old pane running untouched, not dead and
    /// pinned to the dashboard's own pid. Before the fix, the old child was
    /// quit and its lifecycle released before this failure could even be
    /// observed.
    #[test]
    fn handover_failure_in_successor_setup_leaves_the_old_pane_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "22222222-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let old_agent_name = pane.agent().to_string();
        assert!(
            !matches!(pane.state(), PaneState::Ended(_)),
            "the long-lived child must still be alive before the failing handover"
        );

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(
                tmp.path()
                    .join("no-such-adapter-binary")
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        let err = pane
            .handover(
                &cfg,
                &req,
                &handoff_note,
                PromptRole::Worker,
                &repo,
                (80, 24),
            )
            .expect_err("a missing adapter binary must fail the swap, not silently succeed");
        assert!(
            !err.to_string().is_empty(),
            "must report why the swap failed"
        );

        assert_eq!(
            pane.agent(),
            old_agent_name,
            "the old pane's identity must be unchanged after a failed swap"
        );
        assert!(
            !matches!(pane.state(), PaneState::Ended(_)),
            "the old child must still be running -- it must never have been quit"
        );

        // Test-plumbing only: this child (`long_lived_argv`) never reads its
        // pty input, so `shutdown`'s polite ask-then-wait always burns the
        // full `QUIT_GRACE` (production, unchanged) before falling through to
        // the same kill. `finish_shutdown` is the escalation half on its own
        // -- already public, already used by the batched-shutdown path -- so
        // teardown here is immediate instead of a real multi-second wait.
        pane.finish_shutdown().expect("shutdown");
    }

    /// F5 (review, PR #116): a handover swaps in a successor SESSION
    /// continuing the same task -- `report_to` stays (the requester is still
    /// owed a report from whatever is now running in this pane), but the
    /// one-shot completion reminder is scoped per child session, so the
    /// successor must be eligible for its own reminder even if the
    /// predecessor had already received one. `report_back_reminder_sweep`
    /// (`dash::mod`) gates on exactly this flag, so a pane reading `false`
    /// here is what makes it eligible to be reminded again.
    ///
    /// The successor's own argv is deliberately not a real agent (`ping`
    /// with extra positional args it will reject and exit on almost
    /// immediately) -- only the pty spawn itself has to succeed for this
    /// test's purposes, the same ABSOLUTE rule every other test in this
    /// module already follows.
    #[test]
    fn handover_resets_the_report_reminder_flag_for_the_successor_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "77777777-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        pane.set_report_to(Some("aaaa1111".to_string()));
        pane.mark_report_reminder_sent();
        assert!(
            pane.report_reminder_sent(),
            "sanity: the predecessor session was already reminded"
        );

        let cfg = crate::commands::ctx::config::CtxConfig {
            #[cfg(windows)]
            agent_bin: Some("ping -n 3 127.0.0.1".to_string()),
            #[cfg(unix)]
            agent_bin: Some("sleep 3".to_string()),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        pane.handover(
            &cfg,
            &req,
            &handoff_note,
            PromptRole::Worker,
            &repo,
            (80, 24),
        )
        .expect("the swap must succeed against a trivially spawnable program");

        assert_eq!(
            pane.report_to(),
            Some("aaaa1111"),
            "F5: the requester is still owed a report from the successor session"
        );
        assert!(
            !pane.report_reminder_sent(),
            "F5: a fresh child session must be eligible for its own one-shot reminder"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
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
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

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

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// F1/F2, end to end on a real supervised child: `inject_visible` must
    /// not block the caller (no inline sleep), phase 1's stamping is
    /// immediate, `pending_submit_due` only flips true once
    /// `INJECTION_SUBMIT_DELAY` has actually elapsed, and `submit_pending`
    /// drains it -- writing the lone `\r` against the real pty writer -- and
    /// clears `has_pending_submit` once that write lands.
    #[test]
    fn inject_visible_does_not_block_and_its_pending_submit_drains_after_the_deadline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "55555555-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );

        let started = Instant::now();
        pane.inject_visible("nudge from operator", "hello")
            .expect("inject");
        assert!(
            started.elapsed() < INJECTION_SUBMIT_DELAY,
            "F2: inject_visible must return immediately, never sleep for the settle gap"
        );

        // Stamped at phase 1, not deferred to the CR.
        assert!(
            matches!(pane.state(), PaneState::Working),
            "injected_awaiting_turn is set immediately: {:?}",
            pane.state()
        );
        assert!(
            pane.has_pending_submit(),
            "a submission is now owed for this injection"
        );
        assert!(
            !pane.pending_submit_due(Instant::now()),
            "the deadline has not elapsed yet"
        );

        std::thread::sleep(INJECTION_SUBMIT_DELAY + Duration::from_millis(20));
        assert!(
            pane.pending_submit_due(Instant::now()),
            "due once the settle gap has actually passed"
        );

        pane.submit_pending().expect("the deferred CR write");
        assert!(
            !pane.has_pending_submit(),
            "draining a due submit clears it"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// F1/F2, end to end: an operator who starts typing before an
    /// injection's own settle deadline has elapsed must have the pending
    /// `\r` flushed first, so their own keystroke never lands ahead of the
    /// still-unsubmitted injected line.
    #[test]
    fn write_operator_input_flushes_a_pending_submit_before_the_keystroke() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "66666666-2222-4333-8444-777777777777";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );

        pane.inject_visible("nudge from operator", "hello")
            .expect("inject");
        assert!(
            pane.has_pending_submit(),
            "sanity: a submission is owed before the operator types"
        );

        pane.write_operator_input(b"half a thought")
            .expect("forwarding a keystroke must succeed while the child is alive");
        assert!(
            !pane.has_pending_submit(),
            "the pending CR must be flushed before the keystroke reaches the composer"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
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
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

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

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// Task A end to end: a signal-less pane's real supervised child prints
    /// once at startup and then goes quiet for the rest of its life. The pane
    /// must read `Working` while still inside the quiet window and
    /// `Idle`/`injectable` once the window closes -- with no turn signal ever
    /// sent to it (a codex-shaped adapter never sends one; nothing in this
    /// test's own child does either).
    #[test]
    fn a_signal_less_pane_becomes_idle_after_the_quiet_period_and_not_before() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "77777777-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(1000);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], false, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            pane.last_line().contains("hello"),
            "the startup line must have landed before the rest of this test can mean anything: {:?}",
            pane.last_line()
        );
        assert!(
            matches!(pane.state(), PaneState::Working),
            "still inside the quiet window right after the startup line: {:?}",
            pane.state()
        );
        assert!(!pane.injectable(), "not yet reachable while still working");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut became_idle = false;
        while std::time::Instant::now() < deadline {
            pane.drain();
            if matches!(pane.state(), PaneState::Idle) {
                became_idle = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            became_idle,
            "must become idle once the quiet window closes, with no turn signal ever sent"
        );
        assert!(
            pane.injectable(),
            "and therefore reachable by the mail sweep/nudge drain"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// Task A regression guard: a signal-carrying pane must ignore output
    /// quiescence entirely, exactly as before this feature existed -- it
    /// stays `Working` well past the quiet window with no turn signal ever
    /// sent, so the two branches of `pane_is_idle` provably do not bleed into
    /// each other.
    #[test]
    fn a_signal_carrying_pane_ignores_output_quiescence_entirely() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "88888888-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(200);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], true, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.last_line().contains("hello"), "sanity: the child ran");

        std::thread::sleep(idle_quiet * 10);
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Working),
            "a signal-carrying pane stays working through a long quiet period with no \
             signal sent: {:?}",
            pane.state()
        );
        assert!(!pane.injectable());

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// H1 (review): the bug this test pins -- a signal-less pane's own
    /// `inject_visible` must hold it uninjectable for a full `idle_quiet`
    /// window measured from the injection itself, not from the child's
    /// (already-old) last output. Before the fix, an injection never moved
    /// `last_output_at`, so the very next `drain()` tick -- tens of
    /// milliseconds later, nowhere near a full `idle_quiet` -- still read the
    /// pane as quiet and immediately cleared `injected_awaiting_turn`,
    /// letting a second injector (the nudge drain running right after the
    /// mail sweep in the same tick) land straight on top of the first.
    #[test]
    fn a_signal_less_pane_stays_uninjectable_for_a_full_window_after_its_own_injection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "aaaaaaaa-3333-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(500);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], false, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.last_line().contains("hello"), "sanity: the child ran");

        // Wait out the quiet window from the startup line so the pane is
        // genuinely idle before this test's own injection.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            pane.drain();
            if pane.injectable() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "must become injectable before this test can mean anything"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        pane.inject_visible("test", "one").expect("first injection");
        assert!(
            matches!(pane.state(), PaneState::Working),
            "immediately busy after a successful injection"
        );

        // The very next tick, well under idle_quiet later: must NOT have been
        // cleared by the stale (already-old) output timestamp.
        std::thread::sleep(Duration::from_millis(50));
        pane.drain();
        assert!(
            !pane.injectable(),
            "H1: a signal-less pane must not go back to injectable within one \
             tick of its own injection"
        );
        assert!(matches!(pane.state(), PaneState::Working));

        // Still not injectable only partway through the window.
        std::thread::sleep(idle_quiet / 2);
        pane.drain();
        assert!(
            !pane.injectable(),
            "still short of a full idle_quiet window since the injection"
        );

        // And injectable again once a full idle_quiet has elapsed since the
        // injection itself.
        std::thread::sleep(idle_quiet);
        pane.drain();
        assert!(
            pane.injectable(),
            "reachable again once idle_quiet has elapsed since the injection"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// H1 (review): a signal-less pane has no turn signal to tell "the
    /// operator is still typing" from "the child finished", so -- unlike a
    /// signal-carrying pane, where G1 deliberately keeps typing off the
    /// *displayed* state and only gates `injectable()` -- a signal-less
    /// pane's own idleness clock blends local input in on the same axis
    /// (`pane_is_idle`'s signal-less branch, via `signal_less_quiescent`):
    /// a keystroke holds it `Working`, not merely uninjectable, for a full
    /// `idle_quiet` window measured from that keystroke, even with no child
    /// output at all following it. Also pins that the flag is not cleared by
    /// stale quiescence on the very next tick.
    #[test]
    fn operator_typing_holds_a_signal_less_pane_working_for_a_full_quiet_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "bbbbbbbb-3333-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(500);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], false, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.last_line().contains("hello"), "sanity: the child ran");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            pane.drain();
            if pane.injectable() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "must become idle before this test can mean anything"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        pane.write_operator_input(b"half a thought")
            .expect("forwarding a keystroke must succeed while the child is alive");

        // The very next tick, well under idle_quiet later: must still read
        // as busy -- not cleared by the stale (already-old) output
        // timestamp.
        std::thread::sleep(Duration::from_millis(50));
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Working),
            "H1: a keystroke into a signal-less pane must hold it non-idle for \
             a full quiet window, not just until the next drain tick: {:?}",
            pane.state()
        );
        assert!(!pane.injectable());

        // Still working only partway through the window.
        std::thread::sleep(idle_quiet / 2);
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Working),
            "still short of a full idle_quiet window since the keystroke: {:?}",
            pane.state()
        );

        // And idle again once a full idle_quiet has elapsed since the last
        // keystroke.
        std::thread::sleep(idle_quiet);
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Idle),
            "idle again once idle_quiet has elapsed since the last keystroke: {:?}",
            pane.state()
        );
        assert!(pane.injectable());

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    #[test]
    fn shutdown_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = test_spec("22222222-2222-4333-8444-555555555555");
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

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
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
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
