//! `zirv ctx wrap`: the interactive supervisor. It spawns the operator's own
//! agent command behind a pty, passes bytes through byte for byte, and never
//! makes the session worse than an unwrapped one -- any supervision failure
//! degrades, once and for all, to pure passthrough.
//!
//! **What wrap may type into the child**, in full:
//!
//! * a `/compact` (with `COMPACT_FOCUS`) or a restart, from the rot
//!   escalation ladder, only at a verified-idle turn boundary (`may_inject`);
//! * (T13) **one labelled advisory line** when new mail arrives -- the same
//!   `[zirv ▸ mail] ...` shape `dash::pane` uses for its own visible
//!   injections, under exactly the same idle gate.
//!
//! The mail contract is therefore: *an advisory line at verified idle,
//! message bodies never, consumption never.* An earlier version of this
//! module promised that mail and nudges were stderr-only and that wrap
//! "never writes to the pty" for them; that is no longer true of mail, and
//! deliberately so -- an orchestrator inside the session could otherwise only
//! learn about mail by blind polling. What has not changed:
//!
//! * **bodies never travel**. `mail_facts` drops them at the seam, so nothing
//!   downstream of it holds a body to leak. Reading mail stays a deliberate
//!   `zirv ctx inbox`.
//! * **wrap never consumes mail.** Consumption moves a message into `read/`,
//!   where no other session finds it; that is the session's own call.
//! * **a nudge is still advisory only** -- its guidance body reaches an
//!   interactive session through no channel at all.
//! * **mail can never degrade a session.** An unreadable mailbox, a poisoned
//!   writer or a missing identity all no-op or fall back to the `zirv ▸`
//!   announcement channel; none of them calls `note_failure`.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::adapters::AgentAdapter;
use super::announce::{Announcer, Event};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::NormalizedEvent;
use super::handoff::{self, Handoff};
use super::pace;
use super::prompt::PromptRole;
use super::rot::Verdict;
use super::signal::TurnSignal;
use super::state::StateDir;
use super::supervise::Watcher;
use super::term::{RawGuard, STDIN_FD, window_size};
use super::{CtxResult, adapters};

const PUMP_POLL: Duration = Duration::from_millis(100);
const DEFAULT_SIZE: (u16, u16) = (80, 24);
// Matches the grace period `supervise::terminate` already uses for the same
// ask-then-escalate shape.
const QUIT_GRACE: Duration = Duration::from_secs(5);

/// A Cursor Position Report for row 1, column 1 -- the reply a terminal sends
/// when something asks it where the cursor is with `ESC[6n`.
///
/// DO NOT DELETE THIS. portable-pty 0.9.0 creates every Windows pseudoconsole
/// with `PSUEDOCONSOLE_INHERIT_CURSOR` hard-coded (portable-pty
/// `src/win/psuedocon.rs`, not reachable through `openpty`). That flag makes
/// conhost emit `ESC[6n` on the pty and then *block* until a Cursor Position
/// Report comes back on the pty's input pipe -- before it services the child at
/// all. Nothing in `wrap` ever answered, so every wrapped command hung forever
/// on Windows; even `wrap --no-supervise -- cmd /c exit 0` never exited.
/// Writing this one synthetic reply into the master unblocks the console host.
///
/// It is written once per pty generation, which includes the relaunch path: a
/// restart opens a brand-new pseudoconsole that deadlocks exactly the same way.
///
/// It stays even now that raw mode works and a real terminal could answer for
/// itself, because `wrap` also has to run with stdin redirected (CI, a pipe),
/// where there is no terminal to answer at all. `CprFilter` below keeps the two
/// replies from colliding.
#[cfg(windows)]
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// Answers the `PSUEDOCONSOLE_INHERIT_CURSOR` probe described on
/// `CURSOR_POSITION_REPORT`. A no-op everywhere else: no other platform's pty
/// asks anything before it will run a child.
///
/// `pub(in crate::commands::ctx)` rather than private: `dash::pane`'s own PTY
/// spawn (the same deadlock, same fix) reuses this exact function instead of
/// duplicating it.
pub(in crate::commands::ctx) fn answer_inherit_cursor_probe(writer: &mut (dyn Write + Send)) {
    #[cfg(windows)]
    {
        let _ = writer.write_all(CURSOR_POSITION_REPORT);
        let _ = writer.flush();
    }
    #[cfg(not(windows))]
    let _ = writer;
}

/// How long after a pty is opened a Cursor Position Report arriving on stdin is
/// assumed to be the answer to that pty's own `ESC[6n`. Generous: it covers a
/// terminal that is slow to answer, and still expires long before a TUI could
/// plausibly ask for the cursor itself and want the reply.
#[cfg(windows)]
const CPR_FILTER_WINDOW: Duration = Duration::from_secs(3);

/// Swallows the *real* terminal's answer to the console-host probe.
///
/// `wrap` forwards the inner pty's output verbatim, so the `ESC[6n` from
/// `CURSOR_POSITION_REPORT`'s story also reaches the user's own terminal, which
/// -- now that raw mode works -- dutifully answers it on our stdin. The inner
/// console was already satisfied by the synthetic reply, so forwarding that
/// answer would type `ESC[24;1R` into the agent's TUI as if the user had. One
/// report per pty generation is dropped, and only inside a short window after
/// that pty opened, so a report the agent itself asked for later still arrives.
#[derive(Debug, Default)]
pub struct CprFilter {
    armed_until: Option<Instant>,
}

impl CprFilter {
    /// Armed on Windows only: no other platform's pty sends the probe, so on
    /// unix this filter is permanently inert and stdin is passed through byte
    /// for byte.
    pub fn arm(&mut self, now: Instant) {
        #[cfg(windows)]
        {
            self.armed_until = Some(now + CPR_FILTER_WINDOW);
        }
        #[cfg(not(windows))]
        let _ = now;
    }

    /// Returns the bytes to forward. Borrows unless something was actually
    /// removed, so the common path copies nothing.
    pub fn filter<'a>(&mut self, bytes: &'a [u8], now: Instant) -> std::borrow::Cow<'a, [u8]> {
        let Some(until) = self.armed_until else {
            return std::borrow::Cow::Borrowed(bytes);
        };
        if now >= until {
            self.armed_until = None;
            return std::borrow::Cow::Borrowed(bytes);
        }
        let Some(range) = find_cursor_position_report(bytes) else {
            return std::borrow::Cow::Borrowed(bytes);
        };
        // One report is all the probe can produce; anything later in this
        // session is the agent's own business.
        self.armed_until = None;
        let mut kept = Vec::with_capacity(bytes.len() - range.len());
        kept.extend_from_slice(&bytes[..range.start]);
        kept.extend_from_slice(&bytes[range.end..]);
        std::borrow::Cow::Owned(kept)
    }
}

/// Locates one `ESC [ <rows> ; <cols> R` in `bytes`. Deliberately strict about
/// the shape: anything else beginning with `ESC[` is a key the user pressed.
fn find_cursor_position_report(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    while at + 1 < bytes.len() {
        if bytes[at] != 0x1b || bytes[at + 1] != b'[' {
            at += 1;
            continue;
        }
        let mut cursor = at + 2;
        let mut digits = 0;
        let mut semicolons = 0;
        while let Some(byte) = bytes.get(cursor) {
            match byte {
                b'0'..=b'9' => digits += 1,
                b';' => semicolons += 1,
                b'R' if digits > 0 && semicolons == 1 => return Some(at..cursor + 1),
                _ => break,
            }
            cursor += 1;
        }
        at += 1;
    }
    None
}

#[derive(Debug, clap::Args)]
pub struct WrapArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Pure passthrough: no scoring, no injection.
    #[arg(long, default_value_t = false)]
    pub no_supervise: bool,
    /// The interactive agent command, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// Start even though this process looks like it is already inside an
    /// agent session. Off by default: a nested interactive supervisor can
    /// take the outer session down.
    #[arg(long, default_value_t = false)]
    pub allow_nested: bool,
    /// T10: skip the interactive usage-pacing prompt (the soft-band pause
    /// and the at-the-ceiling refusal shown before launch) and go straight
    /// to launching. Named in the prompt's own text as the flag alternative
    /// to a keypress, for a scripted or CI launch with nobody to answer it.
    /// Never silent about *why* it launched anyway -- see `run_with`'s own
    /// interactive-gate call site.
    #[arg(long, default_value_t = false)]
    pub force_pace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    Output(usize),
    Input(usize),
    PtyClosed,
}

#[derive(Debug, Clone)]
pub struct InjectionState {
    /// The turn number the agent last reported, for display only. It counts
    /// turns within one transcript, so it restarts at 1 after a relaunch and
    /// shrinks when a compaction rewrites the file -- which is why nothing
    /// that has to move forwards is keyed on it.
    pub last_turn: u64,
    /// Turn signals this supervisor has received, ever. Monotonic across
    /// relaunches and compactions by construction, because it counts what
    /// arrived here rather than what the transcript says about itself.
    pub signals_seen: u64,
    pub verdict: Verdict,
    pub score: u32,
    pub user_typed_since_turn: bool,
    pub last_output: Instant,
    /// The last time the operator's own keystroke reached this pty --
    /// `last_output`'s counterpart for input, tracked only so a signal-less
    /// adapter's own idleness ([`signal_less_mail_ready`]) can be measured
    /// from the *later* of the two, the same `dash::pane::latest_of` fold a
    /// signal-less dashboard pane already applies. A turn-signal-capable
    /// session never reads this field: its idleness is decided by the signal
    /// alone (`may_inject`), exactly as before this field existed.
    pub last_input: Instant,
    /// `signals_seen` at the moment an action fired. The next action waits for
    /// a strictly newer signal than that one.
    pub cooldown_at_signal: Option<u64>,
    pub degraded: bool,
}

impl Default for InjectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl InjectionState {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_turn: 0,
            signals_seen: 0,
            verdict: Verdict::Healthy,
            score: 0,
            user_typed_since_turn: false,
            last_output: now,
            last_input: now,
            cooldown_at_signal: None,
            degraded: false,
        }
    }

    pub fn on_event(&mut self, event: PumpEvent, now: Instant) {
        match event {
            PumpEvent::Output(_) => self.last_output = now,
            PumpEvent::Input(_) => {
                self.user_typed_since_turn = true;
                self.last_input = now;
            }
            PumpEvent::PtyClosed => {}
        }
    }

    pub fn on_turn(&mut self, signal: &TurnSignal) {
        self.last_turn = signal.turn;
        self.signals_seen += 1;
        self.verdict = signal.verdict;
        self.score = signal.score;
        self.user_typed_since_turn = false;
    }
}

/// Whether the last-armed cooldown has been cleared by a later turn. Shared
/// by `may_inject` and `action_for`'s `Advise` arm: an advisory needs none of
/// `may_inject`'s other preconditions (it only prints, never types into the
/// agent), but it still must not re-fire on every ~100ms poll tick within the
/// same turn once the pump has armed the cooldown for it.
///
/// Keyed on the supervisor's own signal count rather than the turn number the
/// transcript reports. A relaunch starts a fresh session whose turns count from
/// one again, and a compaction rewrites the transcript so its turn count
/// shrinks; a cooldown armed at "turn 30" then never cleared, and supervision
/// went silent for the rest of the run.
fn cooldown_cleared(state: &InjectionState) -> bool {
    state
        .cooldown_at_signal
        .is_none_or(|at| state.signals_seen > at)
}

/// Both spec preconditions, and nothing else: a turn boundary has been reported
/// and the user is idle. Everything about which verdict deserves which action
/// lives in the escalation ladder, not here.
pub fn may_inject(state: &InjectionState, now: Instant, debounce: Duration) -> bool {
    !state.degraded
        && state.signals_seen > 0
        && !state.user_typed_since_turn
        && now.duration_since(state.last_output) >= debounce
        && cooldown_cleared(state)
}

/// Issue #84: whether a pending `zirv ctx handover` request may be acted on
/// right now, versus refused with "mid-turn; retry once idle, or pass
/// --force". `force` always wins outright (the operator asked to interrupt
/// whatever the session is doing); otherwise this is exactly `may_inject`'s
/// own "verified-idle turn boundary" precondition -- the same quiesce check
/// every other injection in this module already gates on, reused rather than
/// reinvented for this seam.
pub fn handover_may_act(
    state: &InjectionState,
    now: Instant,
    debounce: Duration,
    force: bool,
) -> bool {
    force || may_inject(state, now, debounce)
}

/// Whether a session with no turn-signal mechanism at all (codex today) has
/// been quiet long enough for T13's live mail advisory to be typed into it.
///
/// `may_inject`'s `state.signals_seen > 0` precondition can never pass for
/// such a session: `register_turn_signal` is a no-op for it, so `on_turn` is
/// never called and `signals_seen` stays `0` for the session's entire life.
/// Before this, T13's poll arm therefore always fell back to `MailAction::
/// Announce` for it -- a documented residual (see Known Issues, "wrap's own
/// live mail advisory has no equivalent for a signal-less adapter"), and the
/// mirror image of the bug `dash::pane::pane_is_idle` already fixed for a
/// signal-less dashboard pane.
///
/// Mirrors `dash::pane::signal_less_quiescent` exactly: quiet is measured
/// from the *later* of the child's last output and the operator's own last
/// keystroke into it, not from output alone -- the same fold
/// `dash::pane::latest_of` applies, for the same reason (see that function's
/// own doc comment: an injection or a keystroke has to restart the quiet
/// window, or the very next poll tick reads the pane as idle again before the
/// child has had any real chance to respond). Finding 5 (review): the
/// elapsed-time check itself is `dash::pane::quiescent_since`, shared rather
/// than reimplemented here, so the two formulas cannot drift apart again.
/// Deliberately **not** folded
/// into `may_inject` itself: that function also gates `Compact`/`Restart`,
/// and a debounce-only idle guess is not something this codebase wants
/// deciding whether to type `/compact` into a session. `may_inject`'s own
/// `state.signals_seen > 0` precondition is what actually protects that path
/// for a signal-less adapter (codex today): `register_turn_signal` is a
/// no-op for it regardless of `capabilities().events` (issue #86 gave codex
/// real event *parsing*, which is a separate mechanism from turn-signal
/// *posting*), so `signals_seen` never advances and `may_inject` stays
/// permanently false -- this is scoped to the one caller that actually
/// needs the quiet-time-only question.
pub fn signal_less_mail_ready(state: &InjectionState, now: Instant, quiet: Duration) -> bool {
    !state.degraded
        && super::dash::pane::quiescent_since(state.last_output.max(state.last_input), now, quiet)
}

/// The T13 mail poll's own eligibility question, branching on
/// `turn_signal_capable` (`AgentAdapter::capabilities().turn_signal`)
/// exactly the way `dash::pane::pane_is_idle` already branches for a
/// dashboard pane: `may_inject` for an adapter that reports turn boundaries,
/// [`signal_less_mail_ready`] for one that cannot. Split out of the pump
/// loop's own call site so the branch is testable without a real pty.
pub fn mail_inject_ready(
    turn_signal_capable: bool,
    state: &InjectionState,
    now: Instant,
    debounce: Duration,
    idle_quiet: Duration,
) -> bool {
    if turn_signal_capable {
        may_inject(state, now, debounce)
    } else {
        signal_less_mail_ready(state, now, idle_quiet)
    }
}

/// Sent as arguments to the adapter's compaction command. PreCompact hooks
/// cannot add instructions to a compaction, so this is the only channel for them.
pub const COMPACT_FOCUS: &str = "Preserve the current task and its acceptance criteria, the file paths touched so far, any unresolved errors or failing tests, and the exact next step. Drop resolved tangents and full file dumps.";

pub const TRANSCRIPT_ENV: &str = "ZIRV_CTX_TRANSCRIPT";

/// The pre-F5 name: one global file under the state dir root. Still *read*
/// (see `read_socket_path`) so a supervisor started by an older build stays
/// discoverable, but never written any more: two concurrent supervisors
/// clobbered each other's entry, and whoever read it afterwards got a socket
/// belonging to somebody else's session.
pub const SOCKET_PATH_FILE: &str = "socket-path";

/// `<state>/socket-path-<short8>`, one per supervisor, named after the same
/// short id the socket itself and the session registry record already use.
pub const SOCKET_PATH_PREFIX: &str = "socket-path-";

pub fn socket_path_file_for(session: &str) -> String {
    format!("{SOCKET_PATH_PREFIX}{}", super::sessions::short_id(session))
}

/// Publishes where this supervisor bound its turn-signal socket, so `zirv ctx
/// status`, external tooling and the pty tests can find it. Best-effort, like
/// every other piece of state-dir housekeeping: failing to publish must never
/// fail a launch.
pub fn publish_socket_path(state: &StateDir, session: &str, socket: &Path) {
    let _ = super::state::create_private_dir_all(state.root());
    let _ = super::state::write_private(
        &state.root().join(socket_path_file_for(session)),
        &socket.display().to_string(),
    );
}

/// Removes this supervisor's published socket path. Paired with
/// `publish_socket_path` at the one place `wrap` leaves the pump, so a dead
/// session's file does not linger to be picked as "the newest" by a later
/// reader with no session of its own.
pub fn unpublish_socket_path(state: &StateDir, session: &str) {
    let _ = std::fs::remove_file(state.root().join(socket_path_file_for(session)));
}

/// Resolves the socket path a reader should use.
///
/// `session` is the reader's own `ZIRV_CTX_SESSION` (or whichever session it
/// is asking about). When it is given, only *that* session's file is
/// considered: silently handing back a different live session's socket is
/// precisely the cross-session confusion F5 exists to end, so a named session
/// with no file of its own falls through to the legacy file and then to
/// `None`, never to a neighbour's socket.
///
/// With no session -- an operator at a shell, or a test that never learned
/// the id -- the newest published file wins, which is the closest honest
/// approximation of "the session I am looking at" available without one.
///
/// The legacy global file is the last fallback either way, so a supervisor
/// left over from a pre-F5 build is still reachable.
///
/// No production caller inside this binary reads it back today -- `wrap` is
/// the only writer, and everything downstream of it already holds the socket
/// path directly. It exists because publishing the file is a *contract with
/// external tooling* (that is why it is written at all), and shipping a
/// writer whose naming scheme has no canonical reader is how the pre-F5
/// global file's semantics got lost in the first place. The pty tests are its
/// in-tree consumer.
#[allow(dead_code)]
pub fn read_socket_path(state: &StateDir, session: Option<&str>) -> Option<String> {
    let read = |path: PathBuf| -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if let Some(session) = session {
        return read(state.root().join(socket_path_file_for(session)))
            .or_else(|| read(state.root().join(SOCKET_PATH_FILE)));
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(state.root()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_published = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(SOCKET_PATH_PREFIX));
            if !is_published {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(seen, _)| modified >= *seen) {
                newest = Some((modified, path));
            }
        }
    }
    if let Some((_, path)) = newest
        && let Some(found) = read(path)
    {
        return Some(found);
    }

    read(state.root().join(SOCKET_PATH_FILE))
}

/// Which file wrap watches for compactions and reads handoff context from.
///
/// wrap cannot derive it. It spawns the user's own command, and the agent
/// mints its own session id inside that process, so the only party that knows
/// the path is the hook running in the agent: it travels on the turn signal.
/// A relaunch invalidates it, because the fresh session writes somewhere new.
/// `ZIRV_CTX_TRANSCRIPT` pins it for an agent whose hook cannot report one.
#[derive(Debug, Default)]
pub struct TranscriptSource {
    pinned: Option<PathBuf>,
    reported: Option<PathBuf>,
}

impl TranscriptSource {
    pub fn new(pinned: Option<PathBuf>) -> Self {
        Self {
            pinned,
            reported: None,
        }
    }

    /// `None` while no session has reported one, which is the honest answer:
    /// guessing a path means watching a file nobody writes.
    pub fn path(&self) -> Option<&Path> {
        self.pinned.as_deref().or(self.reported.as_deref())
    }

    pub fn adopt(&mut self, reported: Option<&str>) {
        if let Some(path) = reported.filter(|p| !p.is_empty()) {
            self.reported = Some(PathBuf::from(path));
        }
    }

    /// The relaunched agent is a new session writing a new file, so the old
    /// path must not be watched for one more poll.
    pub fn forget(&mut self) {
        self.reported = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Advise,
    Compact,
    Restart,
}

/// Advisories only print, so they need no injection window. Compaction and
/// restart type into the agent, so both preconditions apply.
pub fn action_for(state: &InjectionState, now: Instant, debounce: Duration) -> Action {
    if state.degraded {
        return Action::None;
    }
    match state.verdict {
        Verdict::Healthy => Action::None,
        Verdict::Advise if cooldown_cleared(state) => Action::Advise,
        Verdict::Compact if may_inject(state, now, debounce) => Action::Compact,
        Verdict::Restart if may_inject(state, now, debounce) => Action::Restart,
        _ => Action::None,
    }
}

/// How often the pump looks in the mailbox for a message that arrived while
/// the session was already running (T13).
///
/// Deliberately far coarser than `PUMP_POLL`: this is the only filesystem
/// read on the pump's own tick path (the bar's reads are throttled separately,
/// by `BAR_THROTTLE`), and a wake-up two seconds late costs an orchestrator
/// nothing, while a `read_dir` every 100ms would be permanent load on a
/// session `wrap` has promised never to make worse.
const MAIL_POLL: Duration = Duration::from_secs(2);

/// The most bytes of *sender-supplied* text one advisory line may carry per
/// identity field. `from_agent` is whatever the sending session had in
/// `ZIRV_CTX_AGENT` -- untrusted and unbounded, and `mail::header_value` only
/// guarantees it is *one* line, not a short one -- and the short id is derived
/// from an equally untrusted session id. Roomy enough that no honest value
/// reaches it; the same reasoning as `dash::pane`'s own label budget.
const MAX_MAIL_IDENTITY_BYTES: usize = 48;

/// Whether this session polls the mailbox at all. Every gate is checked
/// before anything touches the filesystem:
///
/// * `mail.enabled = false` -- an operator who turned mail off must never be
///   told mail is waiting, the same rule `mail::unread_counts` already applies
///   to the bar and to delivery. Nothing is read, so behaviour is exactly what
///   it was before there was a poll arm at all.
/// * no registered identity -- `sessions::short_id` came back empty, so this
///   session cannot be the addressee of anything and has no honest
///   per-session view of the mailbox to read.
/// * degraded -- `--no-supervise` (set at construction) and every later
///   `note_failure` both promise pure passthrough, so the mailbox is not even
///   read on such a session's behalf. The status bar's own `mail 2+1` segment
///   is not gated on this, so an operator watching a degraded session is
///   still told what is waiting.
fn mail_polling_enabled(mail_enabled: bool, session_short: &str, degraded: bool) -> bool {
    mail_enabled && !session_short.is_empty() && !degraded
}

/// The unread messages this session may actually see, oldest first, or `None`
/// under exactly the conditions `mail::unread_counts` returns `None` for
/// (mail disabled, or a read error). An unreadable mailbox is silently
/// ignored rather than degrading or interrupting the session.
///
/// Strictly read-only: `wrap` never consumes mail. Consumption moves a
/// message into `read/`, where no other session will ever find it, and that
/// belongs to the session's own `zirv ctx inbox`.
fn unread_mail_for_session(
    state: &super::state::StateDir,
    repo: &Path,
    agent: &str,
    session_short: &str,
    mail_enabled: bool,
) -> Option<Vec<(PathBuf, super::mail::Message)>> {
    if !mail_enabled {
        return None;
    }
    super::mail::list(
        state,
        &super::state::repo_slug(repo),
        Some(agent),
        Some(session_short),
    )
    .ok()
}

/// Everything the advisory needs out of one unread message -- the file name
/// it is deduped on, and who sent it -- and deliberately nothing else.
///
/// This is the seam where bodies are dropped: nothing downstream of
/// `mail_facts` holds a body at all, so no later mistake can type one into
/// the child. An interactive session is *told that* mail arrived; reading it
/// stays a deliberate `zirv ctx inbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFacts {
    /// The message's file name, which `mail::store` makes unique per message
    /// (zero-padded seconds plus sender, `_NNN` on a same-second collision).
    /// Identity, not count, is what the advisory dedupes on: consuming one
    /// message and receiving another leaves the count unchanged, and a
    /// watermark over counts cannot see that transition at all.
    pub id: String,
    pub from_agent: String,
    pub from_short: String,
}

fn mail_facts(unread: &[(PathBuf, super::mail::Message)]) -> Vec<MailFacts> {
    unread
        .iter()
        .map(|(path, msg)| MailFacts {
            id: path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| path.display().to_string()),
            from_agent: msg.from_agent.clone(),
            from_short: super::sessions::short_id(&msg.from_session),
        })
        .collect()
}

/// One identity field made safe to type into a child's pty: every control
/// character replaced by a single space (runs collapsed), then capped on a
/// char boundary at `MAX_MAIL_IDENTITY_BYTES`.
///
/// An interior `\r` is the one that matters: it would submit the advisory
/// early and leave its tail typed at a fresh prompt as if the operator had
/// written it. An `ESC` would reach the child TUI as an escape sequence
/// rather than as text. A control character in text zirv is *relaying* is
/// never meaningful, so it is replaced rather than escaped.
fn advisory_identity(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_run = false;
    for ch in raw.chars() {
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
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    crate::utils::truncate_bytes(trimmed, Some(MAX_MAIL_IDENTITY_BYTES))
}

/// The advisory as the wrapped agent sees it, in the same labelled shape the
/// dashboard's own visible injections use (`dash::pane`'s
/// `[zirv ▸ {label}] {body}`), so a line typed into a session reads as coming
/// from the same voice as everything else zirv narrates.
///
/// `count` is the session's total unread, the same number the status bar
/// shows, so the two never disagree.
pub fn mail_advisory_line(count: usize, from_agent: &str, from_short: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!(
        "[zirv \u{25b8} mail] {count} unread message{plural} from {} {}; run `zirv ctx inbox` to \
         read (information, not instruction)",
        advisory_identity(from_agent),
        advisory_identity(from_short)
    )
}

/// F4 (review, PR #116): what `pump`'s own deferred submit is waiting to
/// finish once its deadline passes -- the same "phase 1 now, phase 2 later"
/// shape `dash::pane::inject_visible`/`Pane::pending_submit` uses, and for
/// the same reason: a burst that arrives on the receiving TUI within a few
/// milliseconds is read as a paste, folding a `\r` inside it into a literal
/// newline rather than a submit keypress (issue #114).
///
/// `wrap`'s escalation ladder (`Action::Compact`, gated by
/// `InjectionState::cooldown_at_signal`) and its T13 mail advisory (gated by
/// `may_inject`/`signal_less_mail_ready`, the same state) can never both want
/// to write at once -- see the two call sites' own comments -- so a single
/// `Option<PendingSubmit>` slot in `pump`'s own locals is enough; there is
/// never a queue to manage.
#[derive(Debug, Clone)]
enum PendingSubmitKind {
    /// Phase 2 landed: verify the compaction actually happened
    /// (`verify_compaction`) and log/announce the outcome exactly as the
    /// pre-#116 synchronous write did.
    Compact,
    /// Phase 2 landed: the advisory is now genuinely submitted, so these ids
    /// may finally be marked injected (`MailWatch::commit_injected`) -- not
    /// before, which is F4's own fix: committing on the write succeeding
    /// used to run the instant phase 1 landed, before the child had even
    /// seen the submitting keypress.
    MailAdvisory { ids: Vec<String> },
}

#[derive(Debug, Clone)]
struct PendingSubmit {
    deadline: Instant,
    kind: PendingSubmitKind,
}

/// What one mail poll concluded. Both non-empty arms carry the ids they
/// covered, so the pump commits them only once the corresponding write
/// actually landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailAction {
    /// Nothing new, or everything new has already been said.
    None,
    /// Type one advisory line into the child: something new arrived and the
    /// child is idle at a turn boundary.
    Inject {
        count: usize,
        from_agent: String,
        from_short: String,
        ids: Vec<String>,
    },
    /// Not safe to type right now, so say it on the `zirv ▸` channel instead
    /// and keep the injection owed for a later poll.
    Announce { count: usize, ids: Vec<String> },
}

/// The pump's mail bookkeeping: when it last looked, and which messages have
/// already been advised about (and how).
///
/// `injected` is the terminal state -- the session itself has been told, so
/// that message is done. `announced` only records that the operator was told
/// on stderr while the child was busy; the injection stays owed, and is
/// retried at every later poll until it lands, which is why an announcement
/// never moves an id into `injected`.
#[derive(Debug, Default)]
pub struct MailWatch {
    last_poll: Option<Instant>,
    injected: super::mail::AdvisedIds,
    announced: super::mail::AdvisedIds,
}

impl MailWatch {
    fn due(&self, now: Instant) -> bool {
        self.last_poll
            .is_none_or(|last| now.duration_since(last) >= MAIL_POLL)
    }

    fn polled(&mut self, now: Instant) {
        self.last_poll = Some(now);
    }

    /// Drops bookkeeping for messages that are no longer unread -- consumed
    /// by this session's own `zirv ctx inbox`, or pruned by `mail::store_to`.
    /// Keeps both sets bounded by what is actually in the mailbox, and means
    /// a file name that somehow came back is treated as the new message it
    /// looks like rather than as one already advised. Finding 3 (review):
    /// this is the id-set/prune shape `mail::AdvisedIds` now shares with the
    /// dashboard's own orchestrator advisory.
    fn forget_missing(&mut self, current: &[MailFacts]) {
        let ids: Vec<&str> = current.iter().map(|facts| facts.id.as_str()).collect();
        self.injected.forget_missing(ids.iter().copied());
        self.announced.forget_missing(ids.iter().copied());
    }

    /// Pure: what to do about `current`, given what has already been advised
    /// and whether the child may be typed into right now (`may_inject`).
    /// Mutates nothing, so a write that fails commits nothing either.
    fn decide(&self, current: &[MailFacts], may_inject: bool) -> MailAction {
        let unadvised: Vec<&MailFacts> = current
            .iter()
            .filter(|facts| !self.injected.contains(&facts.id))
            .collect();
        // Oldest first out of `mail::list`, so the last one is the newest
        // arrival: the sender worth naming on a line that has room for one.
        let Some(newest) = unadvised.last() else {
            return MailAction::None;
        };
        let ids: Vec<String> = unadvised.iter().map(|facts| facts.id.clone()).collect();
        let count = current.len();
        if may_inject {
            return MailAction::Inject {
                count,
                from_agent: newest.from_agent.clone(),
                from_short: newest.from_short.clone(),
                ids,
            };
        }
        if ids.iter().all(|id| self.announced.contains(id)) {
            return MailAction::None;
        }
        MailAction::Announce { count, ids }
    }

    fn commit_injected(&mut self, ids: &[String]) {
        for id in ids {
            self.injected.insert(id);
            self.announced.remove(id);
        }
    }

    /// Records an announcement **only when it actually reached the
    /// operator** (`Announcer::try_emit`'s own answer).
    ///
    /// R5: this used to be an unconditional `commit_announced` right after an
    /// `Announcer::emit` that returns `()` and quietly swallows both a
    /// disabled channel (`--quiet`, `[chrome] events = false`) and a failed
    /// stderr write. `announced` then said "the operator has been told" about
    /// a line nobody ever saw, and since `announced` suppresses the next
    /// poll's announcement, the advisory was never repeated either. For an
    /// adapter with no turn-signal mechanism `may_inject` never becomes true
    /// at all, so this channel is that advisory's *only* surface: one
    /// swallowed emit meant it was never injected and never announced --
    /// lost, permanently, on a session that had no other way to hear about
    /// it. (The message itself was never at risk: it stays unread in the
    /// mailbox and keeps counting in the status bar. This is about the
    /// advisory surface, not durable state.)
    ///
    /// Leaving the ids unannounced on a failure is the whole fix: the next
    /// poll simply tries again, and `injected` is untouched either way, so
    /// the injection stays owed exactly as before.
    fn note_announcement(&mut self, ids: &[String], landed: bool) {
        if landed {
            self.commit_announced(ids);
        }
    }

    fn commit_announced(&mut self, ids: &[String]) {
        for id in ids {
            self.announced.insert(id);
        }
    }
}

/// Finding #7: `handover::take_request`'s own cadence gate, the same "is it
/// due yet" shape as [`MailWatch::due`] above but tracked in its own
/// `Option<Instant>` (the pump loop's `last_handover_poll`) rather than
/// folded into `MailWatch` itself -- a session with mail polling
/// disabled/degraded must not also starve its handover-request check, and
/// vice versa. `None` (never polled yet) is always due, exactly like `due`.
fn handover_poll_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= MAIL_POLL)
}

/// Thin delegate to `mail::unread_counts` (moved there in Task 7 so the
/// dashboard's header facts can share it too): the T12b bar's own
/// `mail 2+1` rendering reads the `(broadcast, direct-to-this-session)`
/// split this returns.
fn unread_mail_counts(
    state: &super::state::StateDir,
    repo: &Path,
    agent: &str,
    session_short: &str,
    mail_enabled: bool,
) -> Option<(usize, usize)> {
    super::mail::unread_counts(state, repo, agent, session_short, mail_enabled)
}

/// The full one-write shape a compact injection used to be, kept as a pure,
/// still-tested invariant helper: phase 1's text (`{command} {COMPACT_FOCUS}`,
/// no control bytes of its own) then phase 2's lone `\r`
/// (`dash::pane::write_submit_cr`, `wrap`'s own convention that
/// `dash::pane`'s doc comments already cite).
///
/// F4 (review, PR #116): `pump`'s `Action::Compact` arm no longer calls this
/// -- like `dash::pane::inject_visible`, it writes phase 1 alone and defers
/// phase 2 to a scheduled deadline (`PendingSubmit`), so a paste-fold on the
/// receiving TUI cannot fold the submitting keypress into the injected text.
/// This function still composes exactly the two primitives production uses,
/// so it stays an honest invariant to test against rather than a second,
/// possibly-drifting copy of the same logic.
///
/// `#[allow(dead_code)]`: no production caller remains after F4 (mirrors
/// `read_socket_path`'s own reasoning, above -- a real, still-correct API
/// with no in-tree caller is not the same thing as code that should be
/// deleted; this one's callers are its own unit tests).
#[allow(dead_code)]
pub fn inject_compact(sink: &mut dyn Write, compact_command: &str) -> CtxResult<()> {
    write_compact_phase1(sink, compact_command)?;
    super::dash::pane::write_submit_cr(sink)?;
    Ok(())
}

/// Phase 1 of a deferred `/compact` injection: the command plus
/// [`COMPACT_FOCUS`], with **no** trailing `\r` -- a TUI submits on carriage
/// return, which phase 2 ([`super::dash::pane::write_submit_cr`]) supplies
/// separately, at least [`super::dash::pane::INJECTION_SUBMIT_DELAY`] later.
fn write_compact_phase1(sink: &mut dyn Write, compact_command: &str) -> CtxResult<()> {
    write!(sink, "{compact_command} {COMPACT_FOCUS}")?;
    sink.flush()?;
    Ok(())
}

/// Watches the transcript for the compaction the injection was supposed to
/// cause. No blind keystroke retries: either it is recorded or wrap retreats.
pub fn verify_compaction(
    watcher: &mut Watcher,
    adapter: &dyn AgentAdapter,
    deadline: Instant,
) -> CtxResult<bool> {
    while Instant::now() < deadline {
        if let Some(appended) = watcher.read_appended()?
            && adapter
                .parse_events(&appended.lines)
                .iter()
                .any(|event| matches!(event, NormalizedEvent::Compaction))
        {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(false)
}

pub fn restart_prompt(handoff: &Handoff) -> String {
    format!(
        "The previous session in this terminal ran out of usable context and was restarted by \
zirv ctx. Continue from the handoff below. Re-read the listed files before changing them, and \
do not redo work marked as done.\n\n{}",
        handoff.to_markdown()
    )
}

/// One-way switch to pure passthrough. Once supervision has proven unreliable
/// in a session it stays off: a wrapped session must never be worse than an
/// unwrapped one.
pub fn note_failure(
    state: &mut InjectionState,
    log_target: Option<(&StateDir, &str)>,
    what: &str,
    announcer: &Announcer,
) {
    state.degraded = true;
    announcer.emit(&Event::Degraded {
        cause: what.to_string(),
    });
    if let Some((state_dir, session)) = log_target {
        let _ = super::log::append(
            state_dir,
            &super::log::Decision {
                ts: super::state::now_secs(),
                session,
                verb: "wrap",
                verdict: "n/a",
                score: 0,
                action: "degrade",
                detail: what,
            },
        );
    }
}

/// Polls until `child` exits or `deadline` passes, returning whether it exited.
fn wait_for_exit(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    deadline: Instant,
) -> CtxResult<bool> {
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(false)
}

/// Ask the TUI to quit, then escalate. A TUI that will not leave politely is
/// killed rather than left running under a supervisor that has moved on.
///
/// Two rungs, deliberately: the adapter's own quit sequence, then
/// `child.kill()`. There is **no Ctrl-C rung**, and one must never be added
/// back (F1).
///
/// Writing `\x03` into the pty master is not a signal to *this* child. On
/// Windows the master is a ConPTY, and conhost turns that byte into a console
/// control event that it broadcasts to **every** process attached to the
/// pseudoconsole -- and portable-pty 0.9.0 spawns without
/// `CREATE_NEW_PROCESS_GROUP`, so there is no group to narrow the broadcast
/// to. A `wrap` that had been launched inside another zirv session therefore
/// took the *outer* session's agent down with the child it meant to quit.
/// (On unix the byte is only marginally better behaved: the line discipline
/// delivers SIGINT to the whole foreground process group of that pty.)
///
/// `child.kill()` is the narrow primitive that has none of that reach: a
/// `TerminateProcess`/`kill` against the one handle this supervisor owns.
/// Note that portable-pty's Windows `do_kill` inverts its own success check
/// and `kill` swallows the result, so a failed kill is invisible here -- see
/// Known Issues; that is a reason to be conservative about what else we try,
/// not a reason to reach for a console-wide broadcast.
///
/// P1: on Windows the escalation rung is now a **tree**-kill by pid
/// (`supervise::kill_tree`, the same `taskkill /T /F /PID <n>` `exec`/`loop`
/// have always used) run *before* the narrow `child.kill()`. `TerminateProcess`
/// against the direct child is not enough for an npm-installed agent, where
/// that direct child is `cmd.exe /c claude.cmd` and the agent itself is a
/// `node` grandchild: quitting a session -- or restarting one on a rot verdict
/// -- left that grandchild alive, and a freshly spawned replacement then ran
/// alongside it on the same repo. The tree-kill is by **pid only**, with fixed
/// flags and a decimal pid, so it is neither a shell invocation nor a console
/// broadcast; it is not a substitute for the narrow kill (taskkill may not be
/// on `PATH`) and its result is not evidence of anything. `wait_for_exit`/
/// `wait` stay the only proof of death. Unix is untouched: portable-pty does
/// `setsid` + `TIOCSCTTY` there, so the child is a session leader and dies
/// with its pty.
pub fn quit_child(
    sink: &mut dyn Write,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    quit_sequence: &str,
    grace: Duration,
) -> CtxResult<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let _ = write!(sink, "{quit_sequence}");
    let _ = sink.flush();
    if wait_for_exit(child, Instant::now() + grace)? {
        return Ok(());
    }

    #[cfg(not(unix))]
    if let Some(pid) = child.process_id() {
        super::supervise::kill_tree(pid);
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// Builds a child's supervision environment: scrub first, then set whatever
/// this supervisor actually owns. The single place both the initial launch
/// and every relaunch go through, so the scrub cannot be forgotten on one of
/// them (F3).
///
/// The scrub is unconditional, and that is the whole point. `turn_env` is
/// empty whenever the socket bind failed -- and without the scrub the child
/// then inherited the *outer* session's `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET`
/// straight out of this process's environment (`CommandBuilder::new` seeds
/// itself from `std::env::vars_os`), so its hooks reported turn boundaries
/// into a supervisor that belonged to somebody else's session. "No socket of
/// my own" has to degrade to unsupervised, never to supervised-by-another.
fn apply_session_env(builder: &mut CommandBuilder, turn_env: &[(String, String)]) {
    super::sessions::scrub_supervision_env(builder);
    for (key, value) in turn_env {
        builder.env(key, value);
    }
}

/// Pumps one pty master's output to stdout for as long as `generation` still
/// matches `my_generation`. A restart opens a fresh inner pty rather than
/// respawning onto the old one (verified: once its session-leader child has
/// exited, this platform refuses a second `spawn_command` on that slave with
/// EBADF), so the old reader thread outlives its pty by a little and must
/// never mistake that pty's own closure for the current one's.
fn spawn_output_thread(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<PumpEvent>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    my_generation: u64,
    stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
) {
    std::thread::spawn(move || {
        let still_current =
            || generation.load(std::sync::atomic::Ordering::SeqCst) == my_generation;
        let mut buf = [0u8; 8192];
        let mut stdout = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    if still_current() {
                        let _ = tx.send(PumpEvent::PtyClosed);
                    }
                    return;
                }
                Ok(n) => {
                    // A restart may have superseded this thread while the
                    // read was in flight. Bytes read from an abandoned pty
                    // must never reach stdout (they would interleave with
                    // the current generation's own output on the same
                    // stdout handle) or the event channel (a stale Output
                    // could refresh `last_output` for the wrong pty). But
                    // the thread must keep draining rather than exit here:
                    // the old pty's session may still be alive mid-quit,
                    // and leaving its output buffer to back up is exactly
                    // what stalls that child's own exit (see quit_child's
                    // tests, which needed the identical drain to unblock).
                    if !still_current() {
                        continue;
                    }
                    // Held only around the write itself (T12b): the same
                    // lock the bar's own redraw takes, so one assembled bar
                    // buffer can never land in the middle of a child-byte
                    // write and vice versa. A poisoned lock (a panic
                    // elsewhere while holding it) still yields its guard --
                    // the child's own output must never be dropped because
                    // some unrelated code panicked while holding this lock.
                    let write_result = {
                        let _guard = stdout_lock.lock().unwrap_or_else(|e| e.into_inner());
                        stdout.write_all(&buf[..n]).and_then(|()| stdout.flush())
                    };
                    if write_result.is_err() {
                        if still_current() {
                            let _ = tx.send(PumpEvent::PtyClosed);
                        }
                        return;
                    }
                    if tx.send(PumpEvent::Output(n)).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// Opens a brand-new inner pty sized to the current window and spawns the
/// adapter's interactive command into it with the handoff as the initial
/// prompt. The old pty cannot be reused for this (see `spawn_output_thread`),
/// so a restart always moves the wrapped agent to a fresh one; the outer
/// side (the user's own terminal, the raw-mode guard) is untouched.
type RelaunchedSession = (
    portable_pty::PtyPair,
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
);

/// The handoff plus whatever the user themselves wrapped: `wrap -- claude
/// --model opus` has to come back as an opus session, not a default one.
fn relaunch_command(
    adapter: &dyn AgentAdapter,
    handoff: &Handoff,
    extra: &[String],
) -> std::process::Command {
    adapter.interactive_cmd(Some(&restart_prompt(handoff)), extra)
}

/// The user's own flags, minus everything a restart regenerates for itself.
///
/// Delegates to `exec::extra_launch_flags` rather than reimplementing it: both
/// verbs have to agree on what escaping a rotted session means, and `exec`
/// already covers `--session-id`, `--resume`, `-c`, `--continue`,
/// `--fork-session` and their `=`-bound spellings.
///
/// `wrap`'s argv always names the program it spawns -- an empty one is rejected
/// before this is reached -- so the prefix is the adapter's own launch prefix,
/// with the same fallback `exec` uses for an argv that opens with a flag. No
/// `known_prompt`: wrap's initial prompt is positional, and the leading
/// positionals are dropped by `extra_launch_flags` on shape alone.
fn restart_launch_flags(adapter: &dyn AgentAdapter, launch_command: &[String]) -> Vec<String> {
    let prefix = if launch_command
        .first()
        .is_none_or(|first| first.starts_with('-'))
    {
        0
    } else {
        adapter.launch_prefix_len()
    };
    super::exec::extra_launch_flags(launch_command, prefix, None)
}

/// Item 5 (regression fix): the pty size a restart's fresh session opens at
/// -- reserved when the bar is still alive, exactly like the initial launch
/// and every ordinary resize while it stays that way, so a mid-session
/// restart cannot hand the child the reserved row the bar is about to keep
/// drawing over. The raw terminal size otherwise (the bar was never
/// eligible, or has already degraded, in which case the pty tracks full
/// size like a bar-less session -- see B1's `resize_decision`).
fn relaunch_size(bar: &BarRuntime, terminal_size: (u16, u16)) -> (u16, u16) {
    super::chrome::reserved_pty_size(terminal_size, bar.active())
}

fn relaunch(
    adapter: &dyn AgentAdapter,
    repo: &Path,
    handoff: &Handoff,
    extra: &[String],
    turn_env: &[(String, String)],
    size: (u16, u16),
) -> CtxResult<RelaunchedSession> {
    let pair = native_pty_system().openpty(PtySize {
        rows: size.1,
        cols: size.0,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let command = relaunch_command(adapter, handoff, extra);
    // FIX 2a (command-injection defense): the relaunch rebuilds its own
    // CommandBuilder from the adapter's Command, so -- like the first launch
    // below and the dashboard pane -- it must clear the cmd.exe argv-reparse
    // guard itself rather than rely on supervise::spawn_tapped (the pty path
    // never reaches it). A no-op off Windows and for any non-shim program.
    {
        let program = command.get_program().to_string_lossy().to_string();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        adapters::guard_cmd_shim_reparse(&program, &args)?;
    }
    let mut builder = CommandBuilder::new(command.get_program());
    for arg in command.get_args() {
        builder.arg(arg);
    }
    builder.cwd(repo);
    // Without this the fresh session has no socket to report turn boundaries
    // on, and supervision would silently end at the first restart. Scrubbed
    // first either way -- see `apply_session_env`.
    apply_session_env(&mut builder, turn_env);

    // Before the spawn, and before anything else touches this pty: on Windows
    // the console host will not service the child at all until it is answered.
    // A restart opens a fresh pseudoconsole, so it re-deadlocks without this.
    let mut writer = pair.master.take_writer()?;
    answer_inherit_cursor_probe(&mut *writer);

    let child = pair.slave.spawn_command(builder)?;

    let reader = pair.master.try_clone_reader()?;

    Ok((pair, child, reader, writer))
}

/// T10: how often the skippable pause and the confirmation prompt poll for a
/// keypress -- short enough to feel responsive, long enough not to busy-loop
/// the CPU while a human decides (or doesn't).
const INTERACTIVE_GATE_POLL_MS: u64 = 200;

/// T10: the skippable half of the interactive gate (`InteractiveGate::
/// Pause`) -- waits up to `seconds`, returning the instant any key arrives.
/// Best-effort: a `crossterm` raw-mode/poll/read failure degrades to "no
/// keypress arrived", so the pause simply runs its full course rather than
/// erroring -- the same posture every other input-detection path in this
/// crate already takes (a spend gate must never crash a launch over a
/// terminal quirk).
fn interactive_skippable_pause(seconds: u64) {
    let _ = crossterm::terminal::enable_raw_mode();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let poll_for = remaining.min(Duration::from_millis(INTERACTIVE_GATE_POLL_MS));
        match crossterm::event::poll(poll_for) {
            Ok(true) => {
                if matches!(
                    crossterm::event::read(),
                    Ok(crossterm::event::Event::Key(_))
                ) {
                    break;
                }
            }
            Ok(false) => {}
            Err(_) => break,
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
}

/// T10: the deliberate-confirmation half (`InteractiveGate::Refuse`) --
/// blocks until a key arrives, since a hard ceiling has no safe timeout to
/// fall through to (the whole point is "refuse unless a human overrides
/// it"). Only `y`/`Y` confirms; any other key, or an input-detection
/// failure, refuses -- the same "unknown must not be read as permission"
/// rule `pace::decide` itself already follows for usage data.
fn interactive_confirm() -> bool {
    let _ = crossterm::terminal::enable_raw_mode();
    let mut confirmed = false;
    loop {
        match crossterm::event::poll(Duration::from_millis(INTERACTIVE_GATE_POLL_MS)) {
            Ok(true) => match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => {
                    confirmed = matches!(
                        key.code,
                        crossterm::event::KeyCode::Char('y') | crossterm::event::KeyCode::Char('Y')
                    );
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            },
            Ok(false) => continue,
            Err(_) => break,
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
    confirmed
}

/// T10: applies the resolved `InteractiveGate` before the pty is ever
/// opened. `force_pace` short-circuits both wait shapes with one line naming
/// why, never silently -- an operator who passed the flag still deserves to
/// see what they skipped, not just a launch with no explanation. Returns
/// `Err` to refuse (the caller returns it straight out of `run_with`, the
/// same pattern every other pre-terminal refusal in this function already
/// uses); every other case returns `Ok(())` to launch.
pub(in crate::commands::ctx) fn apply_interactive_gate(
    gate: pace::InteractiveGate,
    force_pace: bool,
) -> CtxResult<()> {
    match gate {
        pace::InteractiveGate::Launch => Ok(()),
        pace::InteractiveGate::Pause { message, seconds } => {
            if force_pace {
                eprintln!("zirv ctx wrap: {message} (--force-pace: launching now)");
                return Ok(());
            }
            eprintln!("zirv ctx wrap: {message}");
            interactive_skippable_pause(seconds);
            Ok(())
        }
        pace::InteractiveGate::Refuse { message } => {
            if force_pace {
                eprintln!("zirv ctx wrap: {message} (--force-pace: launching anyway)");
                return Ok(());
            }
            eprintln!("zirv ctx wrap: {message}");
            if interactive_confirm() {
                Ok(())
            } else {
                Err(
                    "zirv ctx wrap: refusing to launch (usage at the ceiling); pass \
                     --force-pace or confirm with 'y' to launch anyway"
                        .into(),
                )
            }
        }
    }
}

/// `role` is a caller-supplied parameter rather than a `WrapArgs` field: it is
/// not something a user ever types on the `wrap` command line, only something
/// another verb (`zirv ctx chat`) decides on the caller's behalf. Both callers
/// pass `PromptRole::Orchestrator` today -- `chat`, and the bare `wrap` verb
/// itself, whose command an operator is sitting in front of and driving
/// interactively. A relaunch inside `pump` never re-decides: it reuses this
/// launch's own composed prompt, so it keeps this launch's role by
/// construction.
/// `session` lets a caller that already generated a session id for its own
/// purposes (`chat.rs`'s launch banner, printed before this function is ever
/// called) hand it in rather than have two different ids exist for the same
/// launch. `None` (every caller but `chat`) keeps today's behavior: a fresh
/// id minted here.
///
/// No writer parameter: this function never had anything of its own to print
/// on a healthy path, and its one former write (a rare internal pump
/// failure) went to `output::error` on stderr instead (item 6 audit) --
/// printing it to a caller-supplied stdout writer, the same stream the
/// wrapped session's own pty bytes already occupy, is exactly the kind of
/// silently-lost diagnostic that motivated the fix.
pub fn run_with(
    args: &WrapArgs,
    repo: &Path,
    env: EnvLookup<'_>,
    role: PromptRole,
    session: Option<super::event::SessionId>,
    verb: super::sessions::Verb,
) -> CtxResult<i32> {
    if args.command.is_empty() {
        return Err("no command to wrap; pass it after --".into());
    }

    // F2, before anything reads config, resolves an adapter, or touches the
    // terminal: an interactive supervisor started *inside* another agent's
    // session can take that outer session down (see
    // `sessions::nested_session_evidence`). Returned as an `Err` rather than
    // printed here, because `run_with` deliberately has no writer of its own
    // (see this function's doc comment); `ctx`'s dispatch prints it on
    // stderr through `output::error`.
    if let Some(refusal) = super::sessions::nesting_refusal("wrap", env, args.allow_nested) {
        return Err(refusal.into());
    }

    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Announcements are gated by `cfg.chrome.events` (which already folds in
    // `--quiet`, `ZIRV_CTX_QUIET` and `[chrome] events`), never by whether
    // the terminal is big enough or colour-capable for the banner and bar: a
    // piped stderr in CI still wants these lines. `--no-supervise` is the one
    // exception: it promises pure passthrough ("no scoring, no injection"),
    // so nothing about supervision has anything to narrate either.
    let announcer = if args.no_supervise {
        Announcer::silent()
    } else {
        Announcer::new(cfg.chrome.events, console::colors_enabled_stderr())
    };
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    // Selection happens here so an unknown or unverified agent fails before the
    // terminal is touched.
    // T84: `adapter` is `mut` so a live `zirv ctx handover` swap (see the
    // handover request check inside `pump`) can replace the boxed trait
    // object in place -- every existing `adapter.<method>()` call site below
    // and inside `pump` keeps working unchanged, since method resolution
    // auto-derefs through `&mut Box<dyn AgentAdapter>` exactly as it does
    // through `&dyn AgentAdapter`.
    let mut adapter = adapters::select(agent_name, &args.command, &cfg)?;

    // `select` defaults to claude when detection finds nothing to back it,
    // which is fine for a caller (like `exec`) that already gates every
    // claude-specific behavior on `command_matches_adapter`. `wrap` must not
    // spawn a command it can only guess is claude and then start typing
    // claude-only escape sequences (`/exit\r`, `/compact ...`) into it: an
    // undetected command with no explicit `--agent` fails loudly here,
    // before the terminal is ever touched, instead of running silently
    // unsupervised.
    //
    // `--no-supervise` and `--simple` are exempt: both promise pure
    // passthrough, neither injects anything or types into the child, so there
    // is nothing left for a wrong guess to get wrong.
    let passthrough_only = args.no_supervise || args.simple;
    if !passthrough_only
        && agent_name.is_none()
        && !adapters::command_matches_adapter(adapter.as_ref(), false, &args.command)
    {
        let program = args.command.first().map(String::as_str).unwrap_or("");
        // Named generically rather than hardcoding one adapter: the actual
        // options come from the registry (gate-enabled and `ready()` right
        // now), so a second working adapter shows up here without an edit.
        let available = adapters::available_adapter_names(&cfg);
        let agent_hint = if available.is_empty() {
            "pass --agent <name>".to_string()
        } else {
            format!("pass --agent {}", available.join("/"))
        };
        return Err(format!(
            "zirv ctx wrap: could not tell which agent '{program}' is; {agent_hint} \
             (or your agent's name), run it with --no-supervise for pure passthrough, \
             or run this command unwrapped"
        )
        .into());
    }

    let state_dir = super::state::StateDir::resolve(env)?;
    let session = session.unwrap_or_else(super::event::SessionId::new_v4);

    // T10: the launch-time pacing gate -- before this fix, `wrap` (and, by
    // extension, `zirv ctx chat`'s orchestrator and every dashboard pane,
    // which launch through this same function) never consulted `pace` at
    // all, so an operator's dashboard-heavy workload had no proactive
    // protection whatsoever, only the reactive `scan_for_limit` catching a
    // vendor-imposed limit after the fact. Deliberately placed before any
    // pty/terminal work below (never on the redraw path, which stays
    // network-free per CLAUDE.md) and skipped outright for `--no-supervise`,
    // whose whole promise is "nothing supervisory happens" -- `--simple`
    // does NOT skip it (its own doc comment already promises "supervision,
    // pacing and hooks still apply").
    //
    // Also gated on both stdin *and* stdout being real terminals: the whole
    // design (a skippable pause, a 'y'-to-confirm refusal) assumes a human
    // is there to answer it, exactly the same double-check `chrome::
    // dash_eligible` already makes for the same reason. Without a real
    // terminal there is nobody to prompt -- under `cargo test`'s piped
    // stdio in particular, `crossterm`'s raw-mode/poll calls have no console
    // to act on, so blocking here would either error out immediately (best
    // case) or hang a test suite waiting for a keypress that can never
    // arrive (worst case). A non-interactive `wrap` invocation is out of
    // scope for this fix -- `exec`/`loop` are the supervisors for headless
    // work, and already gate correctly.
    if !args.no_supervise && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let gate = pace::interactive_gate(&state_dir, &cfg, adapter.provider(), true);
        apply_interactive_gate(gate, args.force_pace)?;
    }

    // `--no-supervise` promises pure passthrough (its own help text says so),
    // and so does a wrapped command that matches no adapter: injecting this
    // adapter's flags into a program that may not be it would leak them into
    // its output.
    let skip_injection = passthrough_only
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    // Still needed standalone below: `pump`'s own best-effort memory-harvest
    // call on a restart (unrelated to prompt composition, which `compile`
    // now owns) reads this same slug.
    let memory_slug = super::state::repo_slug(repo);
    // Issue #44: gathers memory, the derived harness roster and the
    // canonical `.zirv/context/` layer, and attaches the policy report --
    // see `compile::compile`'s own doc comment.
    let compiled = super::compile::compile(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        skip_injection,
        &cfg,
        adapter.as_ref(),
        role,
        &state_dir,
        super::state::now_secs(),
    );
    // The wrapped command's own argv may already carry the adapter's
    // system-prompt flag; merge it in rather than letting `prompt_args` below
    // silently override it with a second occurrence.
    let (launch_command, composed) = super::prompt::merge_command_line_prompt(
        adapter.as_ref(),
        &args.command,
        compiled.composed,
        None,
        role,
    );
    let prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        &launch_command,
        composed.as_ref(),
        &state_dir,
        session.as_str(),
    )?;
    super::prompt::log_injection(
        &state_dir,
        "wrap",
        session.as_str(),
        composed.as_ref(),
        adapter.system_prompt_supported(&launch_command),
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        adapter.system_prompt_supported(&launch_command),
    ));
    // Stripping the user's own --append-system-prompt (see
    // merge_command_line_prompt) can empty the argv even though args.command
    // itself was not empty at the top of this function, e.g. `wrap -- --
    // append-system-prompt foo` with nothing else. That must be an error, not
    // a panic on a hot path where release is panic = "abort".
    let (program, rest) = launch_command
        .split_first()
        .ok_or("no command to wrap; pass it after --")?;
    // Bug B (harness/model parity, 2026-08-22): the same seam every real
    // launch now calls (`adapters::policy_launch_args`) -- the shipped-
    // default "sandboxed, no prompts" posture plus any explicit `[policy]`
    // restriction. Computed once, from `rest` (the wrapped command's own
    // trailing argv, whether zirv-built via `chat.rs::build_launch` or a
    // hand-typed `zirv ctx wrap -- <command>`), and reused for both the
    // first launch and every restart below (`relaunch_extra`), exactly like
    // `prompt_args` already is. `flags_pin_policy` reads `rest`, so an
    // operator's own explicit `--sandbox`/`--ask-for-approval`/
    // `--permission-mode`/`--disallowedTools` still wins.
    //
    // Deliberately **not** gated on `skip_injection` (which also folds in
    // `args.simple`): `--simple` promises no *injected instruction text*,
    // and the sandbox posture is a safety flag layer, not instruction text
    // (see `chat.rs`'s own `--simple` test for the identical call). It is
    // gated on the two reasons `skip_injection` exists for that *do* apply
    // here: `--no-supervise`'s own contract is pure passthrough (nothing
    // zirv-added at all), and a wrapped command that does not actually
    // match this adapter must never receive this adapter's flags -- the
    // same leakage risk `skip_injection` exists to prevent for `prompt_
    // args`.
    let policy_skip = args.no_supervise
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let policy_extra = if policy_skip {
        Vec::new()
    } else {
        adapters::policy_launch_args(&cfg, adapter.as_ref(), rest)
    };
    // Visible, not silent: the shipped-default posture (or the operator's
    // own opt-out/override) is announced once, here, at session start -- not
    // re-announced on a restart, since `policy_extra` is computed once above
    // and simply reused by `relaunch_extra`. A no-op under `--no-supervise`
    // (`announcer` is `Announcer::silent()` there already).
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if policy_extra.is_empty() {
            "not applied (operator flags, an unmatched wrapped command, --no-supervise, or \
             [sandbox] enabled = false)"
                .to_string()
        } else {
            policy_extra.join(" ")
        },
    });

    let mut supervision = InjectionState::new();
    supervision.degraded = args.no_supervise;

    let server = if args.no_supervise {
        None
    } else {
        match super::signal::SignalServer::bind(&state_dir.socket_for(session.as_str())) {
            Ok(server) => {
                // Published per session (F5): the pre-F5 global file meant a
                // second supervisor overwrote the first one's entry, and a
                // reader then found somebody else's socket under it.
                publish_socket_path(&state_dir, session.as_str(), server.path());
                Some(server)
            }
            Err(_) => {
                note_failure(
                    &mut supervision,
                    Some((&state_dir, session.as_str())),
                    "socket unavailable",
                    &announcer,
                );
                None
            }
        }
    };

    // Registered here, after the bind, rather than earlier: the record has to
    // say whether this session can act on a wake-up, and only the bind
    // result knows.
    //
    // N6/NEW-3: `wrap` claims nudge markers exclusively from its turn-signal
    // arm (`if let Some(server) = server && ...`), so with no socket --
    // `--no-supervise`, or a bind that failed -- a marker written for this
    // session is never claimed, and the nudge silently does nothing forever.
    // The first fix for that dropped such sessions from the registry
    // entirely, which cured the silent nudge by making the session
    // *invisible*: it disappeared from `zirv ctx status` too, so an operator
    // whose `wrap` had failed to bind could not see it running at all.
    // Recorded as `reachable: false` instead -- `status` shows it as
    // `unreachable`, and `nudge` refuses it with a reason.
    //
    // Keyed on the socket rather than on `--no-supervise`/`--simple` as
    // such: `--simple` only skips prompt injection, still binds a socket and
    // still claims markers, so it stays a legitimate (advisory) target; and
    // a bind failure under a plain `wrap` is exactly as unreachable as
    // `--no-supervise` is.
    //
    // Best-effort, released right after `pump` returns below -- `wrap`'s own
    // control flow always funnels through that one point, unlike `exec`'s
    // scattered early returns, so a single release suffices here.
    let record = super::sessions::Record::new(session.as_str(), adapter.name(), repo, verb);
    let record = if server.is_some() {
        record
    } else {
        record.unreachable()
    };
    let mut session_guard = super::sessions::SessionGuard::register(&state_dir, record);

    // Deliberately not derived from `session`: that id belongs to wrap, not to
    // the agent it spawns, so a derived path names a file nobody ever writes.
    let mut transcript = TranscriptSource::new(env(TRANSCRIPT_ENV).map(PathBuf::from));

    let (cols, rows) = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    // Probed (and, on success, held) ahead of `RawGuard::enter` below: the
    // chrome eligibility decision (in particular whether the bar may draw at
    // all) needs to know this before the pty is even sized, and the bar's
    // own escape sequences need VT on regardless of whether the wrapped
    // command is itself claude or codex. Restored explicitly alongside
    // `raw`, at the one place this function ever leaves the pump.
    let mut vt_guard = super::term::enable_vt_output().ok();
    let vt_ok = vt_guard.is_some();
    // `IsTerminal` on stdout specifically, not `window_size(STDIN_FD)`'s own
    // success: on unix that probes stdin's own fd, so `zirv chat > log` (or
    // `wrap`) left stdin attached to a real terminal still banered straight
    // into the redirected file. The size itself still comes from
    // `window_size`, which is the only source `wrap` has for it.
    let stdout_is_tty = std::io::stdout().is_terminal();
    let chrome = super::chrome::ChromeCaps::probe(
        stdout_is_tty,
        vt_ok,
        (cols, rows),
        &cfg.chrome,
        args.simple,
        args.no_supervise,
    );
    let (pty_cols, pty_rows) = super::chrome::reserved_pty_size((cols, rows), chrome.bar);
    let mut pair = native_pty_system().openpty(PtySize {
        rows: pty_rows,
        cols: pty_cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // FIX 2a (command-injection defense): the first-launch pty assembly does
    // not pass through supervise::spawn_tapped's guard either, so apply the
    // same cmd.exe argv-reparse policy over the full downstream argv -- the
    // wrapped command's own args plus zirv's injected prompt args, which carry
    // repo-sourced text. A no-op off Windows and for any non-shim program.
    {
        let mut guarded: Vec<String> = rest.to_vec();
        guarded.extend(policy_extra.iter().cloned());
        guarded.extend(prompt_args.iter().cloned());
        adapters::guard_cmd_shim_reparse(program, &guarded)?;
    }
    let mut command = CommandBuilder::new(program);
    for arg in rest {
        command.arg(arg);
    }
    for arg in &policy_extra {
        command.arg(arg);
    }
    for arg in &prompt_args {
        command.arg(arg);
    }
    command.cwd(repo);

    // Kept for the relaunch too: a fresh session with no socket to report on
    // would leave the rest of the run unsupervised.
    // `AGENT_ENV` is exported unconditionally, unlike the turn-signal env
    // (which needs a bound socket): it names the same fact `ctx.toml`'s own
    // `agent` config key would, so a nested `zirv ctx ...` call inside this
    // session's own children defaults to this session's own harness. Kept in
    // `turn_env` (despite the name) because a relaunch reuses this exact
    // vector, and the freshly relaunched session needs it too.
    let mut turn_env: Vec<(String, String)> = server
        .as_ref()
        .map(|server| {
            adapter
                .register_turn_signal(
                    &super::event::SessionRef {
                        id: session.clone(),
                        cwd: repo.to_path_buf(),
                    },
                    server.path(),
                )
                .env
        })
        .unwrap_or_default();
    turn_env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
    // The seat this session sits in, for the `zirv ctx hook pretool` guard
    // running inside it. Orchestrator-only, and preferring an operator's own
    // `--model`/`--model=` passthrough in `rest` (the same flag vector this
    // launch actually spawns with) over `cfg.chat.model`; see
    // `adapters::seat_model_env`. Kept in `turn_env` for the same reason
    // `AGENT_ENV` is: a relaunch reuses this exact vector, and the fresh
    // session sits in the same seat.
    //
    // Only a `chat` launch may fall back to `cfg.chat.model`: that is `chat`'s
    // own knob, spliced into the argv it hands us (`chat::extra_with_model`),
    // so for that caller the fallback and `rest` agree anyway. The bare `wrap`
    // verb never applies it, and became an Orchestrator (so it reaches this at
    // all) only once the role also picked its prompt layers -- claiming a
    // configured model this launch did not spawn with would have the guard
    // refuse dispatches at a tier the session is not actually on.
    let seat_cfg_model = match verb {
        super::sessions::Verb::Chat => cfg.chat.model.as_deref(),
        _ => None,
    };
    turn_env.extend(adapters::seat_model_env(role, rest, seat_cfg_model));
    // Scrubbed before any of it is applied -- see `apply_session_env`. When
    // the bind above failed, `turn_env` carries only `AGENT_ENV`, and the
    // scrub is the only thing standing between this child and the outer
    // session's identity.
    apply_session_env(&mut command, &turn_env);

    // One writer, shared: the stdin pump and (from Task C4) the injector both
    // need it, and `take_writer` can only be called once. Its contents (not
    // the Arc itself) get swapped out on a restart, so every holder of this
    // Arc transparently starts writing to the fresh pty.
    //
    // Taken before the spawn rather than after it, because on Windows the
    // console host has to be answered before it will service the child at all
    // -- see `CURSOR_POSITION_REPORT`.
    let mut first_writer = pair.master.take_writer()?;
    answer_inherit_cursor_probe(&mut *first_writer);
    let writer = std::sync::Arc::new(std::sync::Mutex::new(first_writer));

    let mut child = pair.slave.spawn_command(command)?;
    // P2/P3: adopted the instant the child exists -- registered for the
    // console-close sweep and put in a kill-on-close job, so neither closing
    // the window nor killing zirv outright can orphan the agent.
    let mut child_guard = super::supervise::ChildGuard::adopt(child.process_id());
    // P5: the registry record was filed above with `std::process::id()` --
    // zirv's own pid, which stays alive exactly as long as the wrapper rather
    // than as long as the agent. Point it at the child, the same override
    // `dash::pane::Pane::spawn` makes. `pump` re-points it after every
    // relaunch.
    if let Some(child_pid) = child.process_id() {
        session_guard.adopt_child_pid(child_pid);
    }

    let reader = pair.master.try_clone_reader()?;
    let (tx, rx) = mpsc::channel::<PumpEvent>();
    // Bumped on every restart so a stale reader thread from an abandoned pty
    // never reports a false PtyClosed for the pty that replaced it.
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Guards every write to the real stdout: the output thread's own
    // child-byte writes, and (T12b) the bar's assembled redraw buffer. One
    // `Mutex<()>` rather than wrapping `Stdout` itself, since both writers
    // already hold their own handle and only need to serialize *when* they
    // write, not share the handle.
    let stdout_lock = std::sync::Arc::new(std::sync::Mutex::new(()));

    // PTY to stdout.
    spawn_output_thread(
        reader,
        tx.clone(),
        generation.clone(),
        0,
        stdout_lock.clone(),
    );

    // Armed for the pty just opened, and re-armed by `pump` for every pty a
    // restart opens after it.
    let cpr_filter = std::sync::Arc::new(std::sync::Mutex::new(CprFilter::default()));
    cpr_filter
        .lock()
        .map_err(|_| "cpr filter poisoned")?
        .arm(Instant::now());

    // stdin to PTY.
    let input_tx = tx.clone();
    let input_writer = std::sync::Arc::clone(&writer);
    let input_filter = std::sync::Arc::clone(&cpr_filter);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    // The console host's own probe was already answered
                    // synthetically; the terminal's duplicate answer must not
                    // reach the agent as keystrokes.
                    let bytes = {
                        let Ok(mut filter) = input_filter.lock() else {
                            return;
                        };
                        filter.filter(&buf[..n], Instant::now())
                    };
                    if bytes.is_empty() {
                        continue;
                    }
                    let Ok(mut sink) = input_writer.lock() else {
                        return;
                    };
                    if sink.write_all(&bytes).is_err() || sink.flush().is_err() {
                        return;
                    }
                    drop(sink);
                    if input_tx.send(PumpEvent::Input(bytes.len())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Raw mode is best-effort: without a terminal (a pipe, or CI) the wrapper
    // still passes bytes through.
    let mut raw = RawGuard::enter(STDIN_FD).ok();

    let mut bar = BarRuntime::new(
        chrome,
        adapter.name().to_string(),
        adapter.provider().to_string(),
        super::sessions::short_id(session.as_str()),
        cfg.mail.enabled,
        stdout_lock.clone(),
        (cols, rows),
        cfg.pace.collector_max_age_secs,
    );
    // C5: `raw.is_some()` as well as `bar.chrome.bar`. `RawGuard::enter` is
    // what stashes the console modes and installs the emergency restore
    // handler (F4), so when it failed there is nothing armed to undo a
    // scroll region -- writing one anyway would fence off the terminal's
    // last row with no handler able to put it back, which is strictly worse
    // than having no bar. A failed `enter` also means this is not a real
    // terminal in the first place, so the bar has nothing to draw on.
    if bar.chrome.bar && raw.is_some() {
        let region = super::chrome::scroll_region_sequence(bar.rows);
        let region_ok = match stdout_lock.lock() {
            Ok(_guard) => {
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(region.as_bytes())
                    .and_then(|()| stdout.flush())
                    .is_ok()
            }
            Err(_) => false,
        };
        // Same degrade-on-failure as every other bar write: a scroll region
        // that never actually got set must not leave the bar thinking it is
        // still safely confining the child.
        bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, region_ok);
        // F4: from here the real console has a scroll region fencing off its
        // last row, so an external kill owes it a `CSI r` on the way out.
        // Only when the write actually landed -- a region that was never set
        // needs no reset.
        super::term::set_bar_active(region_ok);
    }

    let debounce = Duration::from_millis(cfg.wrap.debounce_ms);
    let inject_timeout = Duration::from_millis(cfg.wrap.inject_timeout_ms);

    // Carried into `relaunch_command` too, so a restart does not silently drop
    // the injected prompt the first command already carries.
    //
    // Not the raw argv: a restart is a deliberate escape from the conversation
    // that rotted, so anything pinning the launch to it (`--continue`,
    // `--resume <id>`, `--session-id <id>`, `--fork-session`, and their
    // `=`-bound spellings) has to go, or `wrap -- claude --continue` relaunches
    // straight back into the session it was leaving and burns the restart
    // budget doing it. `exec` already worked this out; this is that same
    // function, and the positional prompt it also strips is one `relaunch`
    // regenerates from the handoff anyway.
    let relaunch_extra: Vec<String> = policy_extra
        .iter()
        .cloned()
        .chain(restart_launch_flags(adapter.as_ref(), &launch_command))
        .chain(prompt_args.iter().cloned())
        .collect();

    // T84: owned rather than borrowed from `cfg`/`adapter` at the call site,
    // so a live handover swap inside `pump` can update it in place for
    // whatever the *new* adapter's own distiller default is -- otherwise a
    // rot-triggered restart after a handover would keep quoting the
    // predecessor's model name to a distiller that may not even recognise it.
    let mut distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    let exit = pump(
        &mut child,
        &mut child_guard,
        &mut session_guard,
        &rx,
        &mut pair,
        &mut supervision,
        server.as_ref(),
        &mut adapter,
        &writer,
        &mut transcript,
        &state_dir,
        &session,
        debounce,
        inject_timeout,
        repo,
        cfg.handoff.tail_items,
        &mut distiller_model,
        Duration::from_secs(cfg.handoff.timeout_secs),
        &cfg,
        &memory_slug,
        QUIT_GRACE,
        tx,
        generation,
        &relaunch_extra,
        &mut turn_env,
        &cpr_filter,
        &announcer,
        &mut bar,
        role,
    );
    // P2/P3: `pump` only ever returns once the child has exited (every arm
    // waits on it), so the pid leaves the console-close registry and the job
    // handle closes here, explicitly -- `panic = "abort"` means `Drop` is no
    // safety net. Before `session_guard.release()` purely for symmetry with
    // the order they were taken in.
    child_guard.release();
    session_guard.release();
    // Paired with `publish_socket_path` above, at the same single point every
    // other per-session artifact is released: a dead supervisor's file must
    // not linger to be picked as "the newest" by a later `read_socket_path`
    // that has no session id of its own.
    unpublish_socket_path(&state_dir, session.as_str());

    reset_bar(&bar);
    if let Some(guard) = raw.as_mut() {
        let _ = guard.restore();
    }
    if let Some(guard) = vt_guard.as_mut() {
        let _ = guard.restore();
    }

    match exit {
        Ok(code) => Ok(code),
        Err(e) => {
            // Item 6 audit: `w` is stdout in production, the same stream the
            // wrapped session's own pty bytes are already occupying -- a
            // rare internal pump failure (a `try_wait`/`wait` I/O error) used
            // to print its only diagnostic there, where it could be scrolled
            // off, overwritten by the child's own next redraw, or -- for
            // `zirv chat > log` -- land only in a redirected file instead of
            // the operator's own terminal. `output::error` matches
            // `output::error`'s own stream and styling, the same fix as
            // chat.rs's no-adapter diagnostic (item 1).
            crate::output::error(format!("zirv ctx wrap: {e}"));
            Ok(1)
        }
    }
}

/// Mutable T12b bookkeeping for the reserved status bar, bundled into one
/// struct rather than further widening `pump`'s already-long parameter list.
/// `disabled` is a one-way switch exactly like `InjectionState::degraded`:
/// once a probe, lock or write failure sets it, nothing here ever clears it
/// again, and the child is never touched by that decision (see
/// `chrome::after_redraw_attempt`). `recovered` (B1) is a second, later
/// one-way switch: it tracks whether the *disable* has already been reacted
/// to (row cleared, pty widened back to full size), so that reaction runs
/// exactly once no matter which caller noticed the disable first.
struct BarRuntime {
    chrome: super::chrome::Chrome,
    disabled: bool,
    recovered: bool,
    last_text: Option<String>,
    last_draw: Instant,
    harness: String,
    /// The resolved adapter's own `AgentAdapter::provider()` (`"anthropic"`
    /// for claude, `"openai"` for codex), so `redraw_bar_if_due` reads the
    /// usage window for *this* session's account rather than the legacy
    /// unscoped file every session used to share. See `window::has_no_usage_
    /// source`/`load_for` and [[Usage and Pacing]].
    provider: String,
    /// This session's own short id (`sessions::short_id`'s vocabulary), used
    /// to scope the mail count `unread_mail_count` reads to messages this
    /// session may actually see, not every session's mail in the repo.
    session_short: String,
    mail_enabled: bool,
    stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
    rows: u16,
    cols: u16,
    /// Unix-seconds timestamp of the last passive codex rollout scan this
    /// bar ran (`0` means never), throttled to `CODEX_BAR_SCAN_SECS` --
    /// wrap's own version of the freshness a wrapped codex session would
    /// otherwise only get from an inner session's own pacing gate or a
    /// statusline tee it does not have.
    last_codex_scan: u64,
    /// Copied from `cfg.pace.collector_max_age_secs` at construction: how
    /// stale a stored codex reading has to be before `refresh_codex_usage`
    /// bothers scanning at all (its own internal staleness gate, separate
    /// from `CODEX_BAR_SCAN_SECS`'s call-rate floor).
    collector_max_age_secs: u64,
}

impl BarRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        chrome: super::chrome::Chrome,
        harness: String,
        provider: String,
        session_short: String,
        mail_enabled: bool,
        stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
        size: (u16, u16),
        collector_max_age_secs: u64,
    ) -> Self {
        Self {
            disabled: !chrome.bar,
            chrome,
            recovered: false,
            // Never drawn yet, so the first due check always draws. Process
            // uptime under a second (a fast test run, a fresh container)
            // would make a bare subtraction panic (an abort, on the release
            // profile this ships with); `checked_sub` degrades to "draw
            // immediately" instead, which is the same outcome a real elapsed
            // second would have produced anyway.
            last_text: None,
            last_draw: Instant::now()
                .checked_sub(BAR_THROTTLE)
                .unwrap_or_else(Instant::now),
            harness,
            provider,
            session_short,
            mail_enabled,
            stdout_lock,
            cols: size.0,
            rows: size.1,
            // Never scanned yet, so the very first redraw due is eligible
            // immediately (`now.saturating_sub(0) >= CODEX_BAR_SCAN_SECS` is
            // true for any real `now`).
            last_codex_scan: 0,
            collector_max_age_secs,
        }
    }

    fn active(&self) -> bool {
        self.chrome.bar && !self.disabled
    }
}

/// The 1s redraw throttle: usage and mail are read from disk only when this
/// has elapsed, never on the byte-pump path.
const BAR_THROTTLE: Duration = Duration::from_secs(1);

/// Floor between a wrapped codex session's own passive rollout scans
/// (`BarRuntime::last_codex_scan`), independent of `BAR_THROTTLE`'s 1s
/// redraw cadence: a rollout scan is a real filesystem walk, not a single
/// stat call, so it must not run on every redraw tick just because the bar
/// text happened to change. Never HTTP -- see `redraw_bar_if_due`.
///
/// Item 5: shared with `pace::refresh_sources`'s own codex scan floor via
/// `window::CODEX_SCAN_FLOOR_SECS` rather than a second, independently
/// numbered constant.
use super::window::CODEX_SCAN_FLOOR_SECS as CODEX_BAR_SCAN_SECS;

/// Item 2 (regression fix): applies a `ResizeDecision::disables_bar` outcome
/// to `bar`'s own bookkeeping. The dims move to the *current* size *before*
/// `disabled` flips, not after or never: `reset_bar`'s later `bar_reset_
/// sequence(bar.rows)` (both the recovery call right after a disabling
/// resize, and the final session cleanup) addresses `bar.rows` to clear the
/// reserved row, and a stale, larger row number left over from before the
/// shrink points past the now-smaller terminal. A real terminal clamps an
/// out-of-range cursor move to its own last row and blanks a line of the
/// child's own live output there instead of the row that actually used to
/// hold the bar.
fn disable_bar_at_current_size(bar: &mut BarRuntime, size: (u16, u16)) {
    bar.cols = size.0;
    bar.rows = size.1;
    bar.disabled = true;
}

/// Writes the reset sequence: region cleared, reserved row blanked. Called
/// from two places -- the final cleanup alongside `RawGuard::restore`, and
/// (B1) `recover_bar_to_full_size` the moment the bar degrades mid-session --
/// so callers decide when it is due; this just performs it. A no-op when the
/// bar was never eligible in the first place.
fn reset_bar(bar: &BarRuntime) {
    if !bar.chrome.bar {
        return;
    }
    let sequence = super::chrome::bar_reset_sequence(bar.rows);
    let mut reset_ok = false;
    if let Ok(_guard) = bar.stdout_lock.lock() {
        let mut stdout = std::io::stdout();
        reset_ok = stdout
            .write_all(sequence.as_bytes())
            .and_then(|()| stdout.flush())
            .is_ok();
    }
    // F4: `BAR_ACTIVE` means "the real console currently has a scroll region
    // we set", so it is cleared only once the undo actually landed. A reset
    // that failed leaves the console still fenced, and the emergency handler
    // still owes it a `CSI r`.
    if reset_ok {
        super::term::set_bar_active(false);
    }
}

/// B1 (blocking fix): the moment the bar shows disabled -- whichever caller
/// noticed first, a too-small resize or a redraw/lock failure with no resize
/// event of its own -- this clears the reserved row exactly once and widens
/// the child pty to the full current size. Without it the pty stayed pinned
/// at its last reserved height forever, and a later widen never reached the
/// child: a degraded session must behave exactly like a bar-less one from
/// here on, including tracking every resize after this at full size.
/// Idempotent (`bar.recovered` guards it), so calling this every tick is
/// cheap and safe.
fn recover_bar_to_full_size(
    bar: &mut BarRuntime,
    pair: &mut portable_pty::PtyPair,
    size: (u16, u16),
) {
    if !super::chrome::bar_needs_recovery(bar.chrome.bar, bar.disabled, bar.recovered) {
        return;
    }
    bar.recovered = true;
    reset_bar(bar);
    let _ = pair.master.resize(PtySize {
        rows: size.1,
        cols: size.0,
        pixel_width: 0,
        pixel_height: 0,
    });
}

/// Redraws the bar when it is active, its rendered text actually changed,
/// and the 1s throttle has elapsed. Usage and mail are read from disk only
/// here, gated by that same throttle, never from the byte-pump path. Any
/// lock or write failure disables the bar permanently and leaves the child
/// untouched; `now` is threaded in rather than read internally so the
/// throttle itself stays testable without a real clock.
fn redraw_bar_if_due(
    bar: &mut BarRuntime,
    supervision: &InjectionState,
    state_dir: &super::state::StateDir,
    repo: &Path,
    now: Instant,
) {
    if !bar.active() || now.duration_since(bar.last_draw) < BAR_THROTTLE {
        return;
    }
    bar.last_draw = now;

    // Passive refresh only: a wrapped codex session has no statusline tee, so
    // the bar would otherwise stay a permanent placeholder for its entire
    // life. Scans are floored to once per `CODEX_BAR_SCAN_SECS`; HTTP stays
    // off this path entirely -- `wrap` must never make a session worse, and
    // a network call on the redraw path could stall it.
    if bar.provider == super::window::CODEX_USAGE_PROVIDER {
        let now_secs = super::state::now_secs();
        if now_secs.saturating_sub(bar.last_codex_scan) >= CODEX_BAR_SCAN_SECS {
            bar.last_codex_scan = now_secs;
            // Resolved via `crate::utils::home_dir()`, the same as `pace::
            // refresh_sources` and `usage.rs`'s own refresh -- not left to
            // `refresh_codex_usage`'s internal `dirs::home_dir()` fallback,
            // which calls `SHGetKnownFolderPath` directly on Windows and so
            // ignores `HOME`/`USERPROFILE`, the one thing a test's
            // `HomeGuard` can actually override.
            let sessions_dir = crate::utils::home_dir()
                .ok()
                .map(|h| h.join(".codex").join("sessions"));
            super::window::refresh_codex_usage(
                state_dir,
                sessions_dir.as_deref(),
                now_secs,
                bar.collector_max_age_secs,
            );
        }
    }

    // Per-provider since this fix: `window::load` is the legacy machine-wide
    // file every session used to share, so a wrapped codex session's bar
    // used to render whatever Anthropic numbers a claude session happened to
    // leave there. `load_for` falls back to that same legacy file for
    // claude's own provider (`anthropic`), so this is a no-op for the common
    // case; for a provider with no usage source at all (codex/openai today)
    // it now reads as `UsageWindows::default()`, which `status_bar` already
    // renders as the placeholder dash, never a misleading `0%`. No renderer
    // change needed at all, only which windows are read.
    //
    // `window::available` drops any window whose `resets_at` has provably
    // passed (or, absent a `resets_at`, that has outlived its own span)
    // before the reading ever reaches the bar: a reading that old is a stale
    // number pretending to be current, exactly what motivated this filter.
    // Still a pure in-memory/file read -- no scan, no network -- on this
    // throttled redraw path.
    let windows = super::window::available(
        &super::window::load_for(state_dir, &bar.provider).unwrap_or_default(),
        super::state::now_secs(),
    );
    let usage_five_hour = windows.five_hour.map(|w| w.used_percentage);
    let usage_seven_day = windows.seven_day.map(|w| w.used_percentage);
    let unread_mail = unread_mail_counts(
        state_dir,
        repo,
        &bar.harness,
        &bar.session_short,
        bar.mail_enabled,
    );

    let state = super::chrome::BarState {
        harness: bar.harness.clone(),
        score: (supervision.signals_seen > 0).then_some(supervision.score),
        verdict: (supervision.signals_seen > 0).then_some(supervision.verdict),
        usage_five_hour,
        usage_seven_day,
        unread_mail,
        degraded: supervision.degraded,
    };
    let text = super::chrome::status_bar(&state, bar.cols, bar.chrome.colour);
    if !super::chrome::bar_text_changed(bar.last_text.as_deref(), &text) {
        return;
    }

    let sequence = super::chrome::bar_redraw_sequence(bar.rows, &text);
    let wrote = match bar.stdout_lock.lock() {
        Ok(_guard) => {
            let mut stdout = std::io::stdout();
            stdout
                .write_all(sequence.as_bytes())
                .and_then(|()| stdout.flush())
        }
        Err(_) => Err(std::io::Error::other("stdout lock poisoned")),
    };
    bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, wrote.is_ok());
    if wrote.is_ok() {
        bar.last_text = Some(text);
    }
}

/// Issue #37: the clean-session-end companion to the pre-existing rot/
/// timeout-restart harvest call in the `Action::Restart` arm inside `pump`
/// below. Called from both places `pump` reports a genuinely clean
/// `SessionEnded` (a `try_wait` exit and a `PtyClosed` event) -- never from
/// the relaunch-failed exit further down, which already ran a harvest as
/// part of its own `Action::Restart` handling just above it: two harvest
/// model calls at the same boundary is exactly what issue #37's "one entry
/// point" rule forbids. Gated on `cfg.memory.harvest` here too, before the
/// transcript is even read, so an operator who left harvesting off never
/// pays for the read or either model call `memory::harvest_at_session_end`
/// can make. Best-effort: any failure is discarded, never surfaced to the
/// caller, so it can never turn a clean exit into a failed one.
#[allow(clippy::too_many_arguments)]
fn harvest_at_clean_exit(
    adapter: &dyn AgentAdapter,
    transcript: &TranscriptSource,
    tail_items: usize,
    distiller_model: &str,
    distiller_timeout: Duration,
    repo: &Path,
    state_dir: &super::state::StateDir,
    memory_slug: &str,
    cfg: &CtxConfig,
) {
    if !cfg.memory.enabled || !cfg.memory.harvest {
        return;
    }
    let jsonl = transcript
        .path()
        .map(|path| std::fs::read_to_string(path).unwrap_or_default())
        .unwrap_or_default();
    let ctx = adapter.structural_context(&jsonl, tail_items);
    let _ = super::memory::harvest_at_session_end(
        adapter,
        distiller_model,
        &ctx,
        distiller_timeout,
        repo,
        state_dir,
        memory_slug,
        cfg,
    );
}

/// What a successful [`perform_handover_swap`] changed, for the ack and the
/// `zirv ▸` announcement -- both models named, matching the decision-log
/// entry's own contract (CLAUDE.md, "Record in the decision log with both
/// models named").
struct HandoverOutcome {
    from_agent: String,
    from_model: String,
    to_agent: String,
    to_model: String,
    stored: CtxResult<PathBuf>,
    source: &'static str,
}

/// Issue #84: swaps the orchestrator seat's model or harness in place,
/// mirroring `pump`'s own `Action::Restart` arm (distill via the existing
/// handoff machinery, quit the old child, open a fresh pty, relaunch) but
/// generalized to a possibly *different* adapter and model, resolved from
/// `req`. The caller (`pump`) has already decided this is a safe moment to
/// act (idle, or `--force`) and has already parked the registry record on
/// zirv's own pid.
///
/// On success, `*adapter`/`*distiller_model`/`*turn_env` are all updated in
/// place so every later tick of the same `pump` loop -- a subsequent
/// compact/quit sequence, a capabilities-gated mail delivery, a rot-
/// triggered restart -- runs against the new harness, not the old one.
/// `session_guard`/`session` are never touched here beyond `adopt_child_pid`:
/// the session keeps its existing registry short id throughout, which is
/// what makes mail sent before the swap still deliverable after it, and
/// `zirv ctx nudge` still resolve to the same address.
///
/// The handoff packet rides the same channel every wrap restart already
/// uses -- the interactive launch's own positional/task prompt
/// (`relaunch_command`/`restart_prompt`), never a system-prompt injection --
/// so a successor with no system-prompt injection mechanism at all (codex)
/// receives it exactly the same way a same-harness restart already would.
#[allow(clippy::too_many_arguments)]
fn perform_handover_swap(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    child_guard: &mut super::supervise::ChildGuard,
    session_guard: &mut super::sessions::SessionGuard,
    pair: &mut portable_pty::PtyPair,
    writer: &std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    generation: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: &mpsc::Sender<PumpEvent>,
    cpr_filter: &std::sync::Arc<std::sync::Mutex<CprFilter>>,
    bar: &mut BarRuntime,
    adapter: &mut Box<dyn AgentAdapter>,
    distiller_model: &mut String,
    turn_env: &mut Vec<(String, String)>,
    transcript: &mut TranscriptSource,
    server: Option<&super::signal::SignalServer>,
    session: &super::event::SessionId,
    repo: &Path,
    cfg: &CtxConfig,
    state_dir: &super::state::StateDir,
    role: PromptRole,
    memory_slug: &str,
    grace: Duration,
    tail_items: usize,
    distiller_timeout: Duration,
    last_size: (u16, u16),
    announcer: &Announcer,
    req: &super::handover::HandoverRequest,
) -> CtxResult<HandoverOutcome> {
    let from_agent = adapter.name().to_string();
    let from_model = turn_env
        .iter()
        .find(|(k, _)| k == adapters::SEAT_MODEL_ENV)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "default".to_string());

    let jsonl = transcript
        .path()
        .map(|path| std::fs::read_to_string(path).unwrap_or_default())
        .unwrap_or_default();
    let ctx = adapter.structural_context(&jsonl, tail_items);
    let (note, source) = handoff::distill_or_structural(
        adapter.as_ref(),
        distiller_model.as_str(),
        &ctx,
        distiller_timeout,
        announcer.enabled,
    );
    let stored = handoff::store(state_dir, repo, session.as_str(), &note);
    // N6: same rule the ordinary restart arm follows -- opt-in, and only
    // from a genuinely distilled handoff.
    if source == "distilled" {
        let _ = super::memory::harvest_durable(
            adapter.as_ref(),
            distiller_model.as_str(),
            &note,
            repo,
            state_dir,
            memory_slug,
            cfg,
        );
    }

    // Everything from here on names the *new* harness. Resolved before the
    // old child is touched, so an unknown target agent (a race against the
    // operator's own config change, or a stale request) fails before
    // anything is torn down.
    let (new_adapter, new_extra_flags) = super::handover::resolve_swap_launch(cfg, req)?;
    let new_turn_env = super::handover::build_turn_env(
        new_adapter.as_ref(),
        server,
        session.as_str(),
        repo,
        role,
        req.target_model.as_deref(),
    );

    let (new_generation, quit) = match writer.lock() {
        Ok(mut sink) => {
            let bumped = generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let quit = quit_child(&mut *sink, child, adapter.quit_sequence(), grace);
            (Some(bumped), quit)
        }
        Err(_) => (None, Err("pty writer poisoned".into())),
    };
    // That session is over, same as an ordinary restart: whatever it was
    // writing is now a dead file, and the successor reports its own on its
    // first turn.
    transcript.forget();
    quit?;
    let new_generation = new_generation.ok_or("generation not bumped; pty writer poisoned")?;

    let (fresh_pair, fresh_child, fresh_reader, fresh_writer) = relaunch(
        new_adapter.as_ref(),
        repo,
        &note,
        &new_extra_flags,
        &new_turn_env,
        relaunch_size(bar, last_size),
    )?;
    spawn_output_thread(
        fresh_reader,
        tx.clone(),
        generation.clone(),
        new_generation,
        bar.stdout_lock.clone(),
    );
    if let Ok(mut sink) = writer.lock() {
        *sink = fresh_writer;
    }
    if let Ok(mut filter) = cpr_filter.lock() {
        filter.arm(Instant::now());
    }
    *pair = fresh_pair;
    *child = fresh_child;
    // P1/P2/P3: released before the new adoption, same ordering the ordinary
    // restart arm uses, so a pid the OS has already recycled can never be
    // deregistered out from under the fresh child.
    child_guard.release();
    *child_guard = super::supervise::ChildGuard::adopt(child.process_id());
    if let Some(child_pid) = child.process_id() {
        session_guard.adopt_child_pid(child_pid);
    }

    let to_agent = new_adapter.name().to_string();
    let to_model = req
        .target_model
        .clone()
        .unwrap_or_else(|| "default".to_string());

    // Commit the swap: the boxed adapter and everything derived from it are
    // replaced together, so the rest of this `pump` loop's life runs against
    // the new harness consistently.
    *adapter = new_adapter;
    *distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    *turn_env = new_turn_env;
    bar.harness = adapter.name().to_string();
    bar.provider = adapter.provider().to_string();

    Ok(HandoverOutcome {
        from_agent,
        from_model,
        to_agent,
        to_model,
        stored,
        source,
    })
}

#[allow(clippy::too_many_arguments)]
fn pump(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    // P2/P3/P5: both are swapped over to the fresh child on every relaunch,
    // so the replaced child's tree can never outlive the supervisor that
    // replaced it and the registry record never points at a dead pid.
    child_guard: &mut super::supervise::ChildGuard,
    session_guard: &mut super::sessions::SessionGuard,
    rx: &mpsc::Receiver<PumpEvent>,
    pair: &mut portable_pty::PtyPair,
    supervision: &mut InjectionState,
    server: Option<&super::signal::SignalServer>,
    // T84: `&mut Box<dyn AgentAdapter>`, not `&dyn AgentAdapter` -- a live
    // `zirv ctx handover` swap replaces the boxed trait object in place
    // (`*adapter = new_adapter`) once the swap has actually happened, so
    // every later tick's `adapter.<method>()` call (unchanged syntax, thanks
    // to auto-deref through `&mut Box<dyn _>`) resolves against the new
    // harness. See the handover request check near the top of this loop.
    adapter: &mut Box<dyn AgentAdapter>,
    writer: &std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    transcript: &mut TranscriptSource,
    state_dir: &super::state::StateDir,
    session: &super::event::SessionId,
    debounce: Duration,
    inject_timeout: Duration,
    repo: &Path,
    tail_items: usize,
    // T84: `&mut String`, not `&str` -- a handover swap recomputes this for
    // the new adapter's own distiller default, so a rot-triggered restart
    // *after* a handover does not keep quoting the predecessor's model name.
    distiller_model: &mut String,
    distiller_timeout: Duration,
    // N6: read alongside `distiller_model`/`distiller_timeout` at the one
    // restart site below (`Action::Restart`), never anywhere else in the
    // pump -- the harvest call is gated on `cfg.memory.harvest` internally.
    cfg: &CtxConfig,
    memory_slug: &str,
    grace: Duration,
    tx: mpsc::Sender<PumpEvent>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    extra: &[String],
    // T84: `&mut Vec<_>`, not `&[_]` -- a handover swap rebuilds this for the
    // new adapter/model (fresh `AGENT_ENV`/`SEAT_MODEL_ENV`/turn-signal env),
    // so a *later* rot-triggered restart relaunches with the right identity.
    turn_env: &mut Vec<(String, String)>,
    cpr_filter: &std::sync::Arc<std::sync::Mutex<CprFilter>>,
    announcer: &Announcer,
    bar: &mut BarRuntime,
    // T84: needed to recompute `SEAT_MODEL_ENV` on a handover swap
    // (`adapters::seat_model_env`), the same role this session's own first
    // launch was composed for.
    role: super::prompt::PromptRole,
) -> CtxResult<i32> {
    let mut last_size = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    // T13: the live mail wake-up. See `mail_polling_enabled` for the gates: a
    // session that cannot receive mail at all never reads the mailbox once.
    let mut mail_watch = MailWatch::default();
    // Finding #7: `handover::take_request`'s own polling cadence, tracked
    // independently of `mail_watch` -- see the check site's own doc comment.
    let mut last_handover_poll: Option<Instant> = None;
    // F4 (review, PR #116): at most one deferred injection submission
    // outstanding at a time -- see `PendingSubmit`'s own doc comment.
    let mut pending_submit: Option<PendingSubmit> = None;

    loop {
        if let Some(status) = child.try_wait()? {
            // Let the reader thread flush whatever is still buffered.
            while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
            let code = status.exit_code() as i32;
            // Item 6 audit: the wrapped session just ended, whether the
            // agent quit cleanly or crashed. Previously silent -- nothing
            // printed at all, so the session appeared to just stop.
            announcer.emit(&Event::SessionEnded {
                agent: adapter.name().to_string(),
                code,
            });
            harvest_at_clean_exit(
                adapter.as_ref(),
                transcript,
                tail_items,
                distiller_model.as_str(),
                distiller_timeout,
                repo,
                state_dir,
                memory_slug,
                cfg,
            );
            return Ok(code);
        }

        while let Ok(event) = rx.try_recv() {
            if event == PumpEvent::PtyClosed {
                let status = child.wait()?;
                let code = status.exit_code() as i32;
                announcer.emit(&Event::SessionEnded {
                    agent: adapter.name().to_string(),
                    code,
                });
                harvest_at_clean_exit(
                    adapter.as_ref(),
                    transcript,
                    tail_items,
                    distiller_model.as_str(),
                    distiller_timeout,
                    repo,
                    state_dir,
                    memory_slug,
                    cfg,
                );
                return Ok(code);
            }
            supervision.on_event(event, Instant::now());
        }

        if let Some(server) = server
            && let Some(signal) = server.try_recv()
        {
            transcript.adopt(signal.transcript_path.as_deref());
            let previous_verdict = supervision.verdict;
            supervision.on_turn(&signal);
            if let Some(event) =
                super::announce::verdict_change(previous_verdict, supervision.verdict, signal.score)
            {
                announcer.emit(&event);
            }

            // T13: mail is no longer read here. The poll arm below owns it
            // end to end -- it runs on its own `MAIL_POLL` cadence rather
            // than only when the agent happens to report a turn, so a
            // message that arrives mid-turn no longer waits for one.

            // N4: an interactive session is only ever *advised* of a nudge:
            // never restarted, and never handed the nudge's own message
            // body. Claiming the marker here (rather than on a byte-pump or
            // per-tick path) keeps this arm as cheap as it always was.
            // C4: `from` is the *sender*, read out of the marker file, and
            // the disposition is `Advisory` -- an interactive session never
            // receives message bodies, so the line has to point the operator
            // at `zirv ctx inbox` rather than promise the guidance will
            // "be picked up as mail".
            if let Some(from) = super::sessions::claim_nudge_marker(state_dir, &bar.session_short) {
                announcer.emit(&Event::Nudge {
                    from,
                    disposition: super::announce::NudgeDisposition::Advisory,
                });
            }
        }

        // T84: `zirv ctx handover`. `take_request` is a real file read +
        // remove, and used to run on *every* ~100ms pump tick for the
        // session's entire lifetime -- the overwhelming majority of which
        // find nothing there (finding #7, issue-close review). Gated on the
        // same `MAIL_POLL` (2s) cadence the mail poll arm below already
        // uses, tracked independently (`last_handover_poll`) so a session
        // with mail polling disabled/degraded still gets its own cadence,
        // and vice versa -- a handover request still answers within one
        // cadence tick either way, since the request sits in a state-dir
        // file until this loop notices it regardless of how often it looks.
        // `may_inject` is exactly the "verified-idle turn boundary" check
        // every other injection already gates on -- reusing it here is what
        // "quiesce" means for this feature, per the module's own doc
        // comment on `handover.rs`.
        let now = Instant::now();
        let handover_poll_due = handover_poll_due(last_handover_poll, now);
        if handover_poll_due {
            last_handover_poll = Some(now);
        }
        if handover_poll_due
            && let Some(req) = super::handover::take_request(state_dir, &bar.session_short)
        {
            let may_act = handover_may_act(supervision, Instant::now(), debounce, req.force);
            if !may_act {
                let reason = "mid-turn; retry once idle, or pass --force".to_string();
                super::handover::write_ack(
                    state_dir,
                    &bar.session_short,
                    &super::handover::HandoverAck {
                        ok: false,
                        reason: Some(reason.clone()),
                        ..Default::default()
                    },
                );
                announcer.emit(&Event::HandoverRefused {
                    reason: reason.clone(),
                });
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "handover",
                        score: supervision.score,
                        action: "handover-refused",
                        detail: &reason,
                    },
                );
            } else {
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
                // Same P5 reasoning as a rot-triggered restart: park the
                // record on zirv's own (unquestionably alive) pid for the
                // duration of the swap, since a concurrent `sessions::list`
                // sweep must never delete this very much live session's
                // record while its old child is being killed and no new one
                // exists yet.
                session_guard.adopt_child_pid(std::process::id());
                match perform_handover_swap(
                    child,
                    child_guard,
                    session_guard,
                    pair,
                    writer,
                    &generation,
                    &tx,
                    cpr_filter,
                    bar,
                    adapter,
                    distiller_model,
                    turn_env,
                    transcript,
                    server,
                    session,
                    repo,
                    cfg,
                    state_dir,
                    role,
                    memory_slug,
                    grace,
                    tail_items,
                    distiller_timeout,
                    last_size,
                    announcer,
                    &req,
                ) {
                    Ok(outcome) => {
                        let stored_text = match &outcome.stored {
                            Ok(path) => path.display().to_string(),
                            Err(e) => format!("not stored: {e}"),
                        };
                        super::handover::write_ack(
                            state_dir,
                            &bar.session_short,
                            &super::handover::HandoverAck {
                                ok: true,
                                reason: None,
                                from_agent: Some(outcome.from_agent.clone()),
                                from_model: Some(outcome.from_model.clone()),
                                to_agent: Some(outcome.to_agent.clone()),
                                to_model: Some(outcome.to_model.clone()),
                                stored: Some(stored_text.clone()),
                            },
                        );
                        announcer.emit(&Event::Handover {
                            from_agent: outcome.from_agent.clone(),
                            from_model: outcome.from_model.clone(),
                            to_agent: outcome.to_agent.clone(),
                            to_model: outcome.to_model.clone(),
                            stored: stored_text.clone(),
                        });
                        let _ = super::log::append(
                            state_dir,
                            &super::log::Decision {
                                ts: super::state::now_secs(),
                                session: session.as_str(),
                                verb: "wrap",
                                verdict: "handover",
                                score: supervision.score,
                                action: "handover",
                                detail: &format!(
                                    "{}/{} -> {}/{} ({} handoff at {})",
                                    outcome.from_agent,
                                    outcome.from_model,
                                    outcome.to_agent,
                                    outcome.to_model,
                                    outcome.source,
                                    stored_text
                                ),
                            },
                        );
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        super::handover::write_ack(
                            state_dir,
                            &bar.session_short,
                            &super::handover::HandoverAck {
                                ok: false,
                                reason: Some(reason.clone()),
                                ..Default::default()
                            },
                        );
                        note_failure(
                            supervision,
                            Some((state_dir, session.as_str())),
                            &reason,
                            announcer,
                        );
                        let _ = super::log::append(
                            state_dir,
                            &super::log::Decision {
                                ts: super::state::now_secs(),
                                session: session.as_str(),
                                verb: "wrap",
                                verdict: "handover",
                                score: supervision.score,
                                action: "handover-failed",
                                detail: &reason,
                            },
                        );
                        let status = child.wait()?;
                        let code = status.exit_code() as i32;
                        announcer.emit(&Event::SessionEnded {
                            agent: adapter.name().to_string(),
                            code,
                        });
                        return Ok(code);
                    }
                }
            }
        }

        // T12b: ticks on the ordinary ~100ms poll, so the bar still
        // refreshes (usage, mail, a still-degrading session) both right
        // after a turn signal and during a long turn with none at all.
        // `redraw_bar_if_due` is what actually enforces the 1s throttle and
        // the no-op-when-unchanged check; this call is cheap otherwise.
        redraw_bar_if_due(bar, supervision, state_dir, repo, Instant::now());

        match action_for(supervision, Instant::now(), debounce) {
            Action::None => {}
            Action::Advise => {
                announcer.emit(&Event::RotAdvisory {
                    score: supervision.score,
                    tokens: 0,
                });
                // Advise once per turn.
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
            }
            Action::Compact => {
                // F4 (review, PR #116): phase 1 only -- the command plus
                // `COMPACT_FOCUS`, with no trailing `\r`. The old single
                // `write_all` (text + `\r` together) is exactly the paste-fold
                // bug F1/F2 fixed for `dash::pane`'s own injections: a
                // codex-shaped composer reads a burst that arrives within a
                // few milliseconds as a paste and folds the `\r` into it as a
                // literal newline rather than a submit keypress, leaving
                // `/compact` typed but never actually submitted.
                let injected = writer
                    .lock()
                    .map_err(|_| "pty writer poisoned".to_string())
                    .and_then(|mut sink| {
                        let command = adapter.compact_command().unwrap_or("/compact");
                        write_compact_phase1(&mut *sink, command).map_err(|e| e.to_string())
                    });

                // Arm the cooldown as soon as phase 1 is attempted, exactly
                // as before -- this is what stops `action_for` from
                // re-selecting `Action::Compact` on a later tick while this
                // one's submission is still pending, so there is never more
                // than one `PendingSubmit` outstanding at a time.
                supervision.cooldown_at_signal = Some(supervision.signals_seen);

                match injected {
                    Ok(()) => {
                        // Phase 2 (the lone `\r`) and everything that used to
                        // run right after it -- `verify_compaction`, the
                        // `Event::Compact` announcement, the decision-log
                        // entry -- are deferred to the tick that finds this
                        // deadline has passed; see the drain step below.
                        pending_submit = Some(PendingSubmit {
                            deadline: Instant::now() + super::dash::pane::INJECTION_SUBMIT_DELAY,
                            kind: PendingSubmitKind::Compact,
                        });
                    }
                    Err(_) => {
                        // Phase 1 itself failed: no bytes are known to have
                        // reached the child, so there is nothing to verify
                        // and nothing pending -- finalize immediately, the
                        // same "compact injection failed" outcome the
                        // synchronous write used to report for this case.
                        announcer.emit(&Event::Compact { verified: false });
                        note_failure(
                            supervision,
                            Some((state_dir, session.as_str())),
                            "compact injection failed",
                            announcer,
                        );
                        let _ = super::log::append(
                            state_dir,
                            &super::log::Decision {
                                ts: super::state::now_secs(),
                                session: session.as_str(),
                                verb: "wrap",
                                verdict: "compact",
                                score: supervision.score,
                                action: "inject-unverified",
                                detail: &transcript
                                    .path()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_else(|| "no transcript reported".to_string()),
                            },
                        );
                    }
                }
            }
            Action::Restart => {
                supervision.cooldown_at_signal = Some(supervision.signals_seen);

                // P5: park the record on zirv's own (unquestionably alive) pid
                // for the duration of the restart. Everything from here to the
                // respawn below -- the distiller call, `quit_child`'s grace
                // ladder, the fresh pty -- happens while the *old* child is
                // being killed, and `sessions::list` sweeps any record whose
                // pid is dead. That listing runs on other processes' schedules
                // (`zirv ctx status`, `nudge`, `send --to-session`, a
                // dashboard's own ~1s registry refresh), so leaving the record
                // pointing at the child being killed meant a concurrent reader
                // could delete this very much live session's record mid-restart
                // and strand every message addressed to it. The real child pid
                // is adopted again the moment there is one.
                session_guard.adopt_child_pid(std::process::id());

                let jsonl = transcript
                    .path()
                    .map(|path| std::fs::read_to_string(path).unwrap_or_default())
                    .unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, tail_items);
                let (note, source) = handoff::distill_or_structural(
                    adapter.as_ref(),
                    distiller_model.as_str(),
                    &ctx,
                    distiller_timeout,
                    announcer.enabled,
                );
                let stored = handoff::store(state_dir, repo, session.as_str(), &note);
                // N6: opt-in (`cfg.memory.harvest`, default off) and only
                // from a genuinely distilled handoff -- never the mechanical
                // structural fallback. Best-effort: a harvest failure must
                // never turn a successful restart into a failed one.
                if source == "distilled" {
                    let _ = super::memory::harvest_durable(
                        adapter.as_ref(),
                        distiller_model.as_str(),
                        &note,
                        repo,
                        state_dir,
                        memory_slug,
                        cfg,
                    );
                }

                // The writer is taken first, and the generation is bumped only
                // once this restart is genuinely under way. The two used to be
                // the other way round, which meant a poisoned writer -- the
                // one way `quit` can fail -- left the generation bumped over a
                // child that had never even been asked to quit: `relaunched`
                // stayed false, the pump fell through to `child.wait()`, and
                // the old reader thread's `still_current` was already false, so
                // that very much alive TUI painted to nobody for the rest of
                // the run.
                let (new_generation, quit) = match writer.lock() {
                    Ok(mut sink) => {
                        // Bumped before the old child is even asked to quit:
                        // its reader thread's own EOF can land at any point
                        // from here on (quit_child alone may take up to
                        // `grace`), and once bumped that thread's
                        // `still_current` check is already false, so a pty
                        // closing on its way out can never be mistaken for the
                        // fresh one that is about to replace it.
                        let bumped =
                            generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        let quit = quit_child(&mut *sink, child, adapter.quit_sequence(), grace)
                            .map_err(|e| e.to_string());
                        (Some(bumped), quit)
                    }
                    Err(_) => (None, Err("pty writer poisoned".to_string())),
                };

                // That session is over. Whatever it was writing is now a dead
                // file, and the replacement reports its own on its first turn.
                transcript.forget();

                // Item 6 audit: captured so `note_failure`'s own announcement
                // can name *why* the restart failed -- quit/writer-lock
                // trouble, or the fresh pty/spawn itself -- rather than the
                // same generic "relaunch failed" either way. Neither error
                // used to be kept past this match at all.
                let mut relaunch_error = quit.as_ref().err().cloned();
                let relaunched = match (new_generation, quit.is_ok()) {
                    (Some(new_generation), true) => {
                        match relaunch(
                            adapter.as_ref(),
                            repo,
                            &note,
                            extra,
                            turn_env.as_slice(),
                            relaunch_size(bar, last_size),
                        ) {
                            Ok((fresh_pair, fresh_child, fresh_reader, fresh_writer)) => {
                                spawn_output_thread(
                                    fresh_reader,
                                    tx.clone(),
                                    generation.clone(),
                                    new_generation,
                                    bar.stdout_lock.clone(),
                                );
                                if let Ok(mut sink) = writer.lock() {
                                    *sink = fresh_writer;
                                }
                                // The fresh pty ran its own console-host probe,
                                // so the terminal is about to answer that one
                                // too; see `CprFilter`.
                                if let Ok(mut filter) = cpr_filter.lock() {
                                    filter.arm(Instant::now());
                                }
                                *pair = fresh_pair;
                                *child = fresh_child;
                                // P1/P2/P3: the old child was tree-killed by
                                // `quit_child` above, so releasing its guard
                                // now only takes it out of the console-close
                                // registry and closes a job with nothing left
                                // in it. Released *before* the new adoption
                                // so a pid the OS has already recycled cannot
                                // be deregistered out from under the fresh
                                // child.
                                child_guard.release();
                                *child_guard =
                                    super::supervise::ChildGuard::adopt(child.process_id());
                                // P5: and the registry record follows the
                                // child it names. Left pointing at the
                                // replaced child's dead pid, `sessions::list`
                                // would sweep the record and this very much
                                // live session would disappear from `zirv ctx
                                // status`.
                                if let Some(child_pid) = child.process_id() {
                                    session_guard.adopt_child_pid(child_pid);
                                }
                                true
                            }
                            Err(e) => {
                                relaunch_error = Some(e.to_string());
                                false
                            }
                        }
                    }
                    _ => false,
                };

                if relaunched {
                    announcer.emit(&Event::Restart {
                        style: source.to_string(),
                        stored: match &stored {
                            Ok(path) => path.display().to_string(),
                            Err(e) => format!("not stored: {e}"),
                        },
                    });
                } else {
                    let reason = relaunch_error.unwrap_or_else(|| "relaunch failed".to_string());
                    note_failure(
                        supervision,
                        Some((state_dir, session.as_str())),
                        &reason,
                        announcer,
                    );
                }
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "restart",
                        score: supervision.score,
                        action: if relaunched {
                            "restart"
                        } else {
                            "restart-failed"
                        },
                        detail: &match stored {
                            Ok(path) => format!("{source} handoff at {}", path.display()),
                            Err(e) => format!("{source} handoff not stored: {e}"),
                        },
                    },
                );
                if !relaunched {
                    let status = child.wait()?;
                    let code = status.exit_code() as i32;
                    // The `Degraded` announcement just above already said
                    // *why* (the relaunch failure reason); this says the
                    // session is over, the same as every other exit point.
                    announcer.emit(&Event::SessionEnded {
                        agent: adapter.name().to_string(),
                        code,
                    });
                    return Ok(code);
                }
            }
        }

        // T13: the live mail wake-up.
        //
        // Deliberately *after* the escalation ladder above: a compaction or
        // restart that just fired armed `cooldown_at_signal`, which makes
        // `may_inject` false here, so this arm falls back to the
        // announcement channel rather than typing a second line into a child
        // that was just handed a `/compact`. The existing cooldown is the
        // mutual exclusion; nothing extra is needed for it.
        //
        // Every failure here is a no-op by construction: an unreadable
        // mailbox yields `None` and nothing happens, a poisoned writer
        // degrades to the announcement channel, and neither ever calls
        // `note_failure` -- mail is advisory, and a wrapped session must
        // never be made worse by it.
        let now = Instant::now();
        if mail_polling_enabled(bar.mail_enabled, &bar.session_short, supervision.degraded)
            && mail_watch.due(now)
        {
            mail_watch.polled(now);
            if let Some(unread) = unread_mail_for_session(
                state_dir,
                repo,
                adapter.name(),
                &bar.session_short,
                bar.mail_enabled,
            ) {
                let facts = mail_facts(&unread);
                mail_watch.forget_missing(&facts);
                // A signal-less adapter (codex today) never satisfies
                // `may_inject`'s own `signals_seen > 0` precondition -- see
                // `signal_less_mail_ready`'s own doc comment. `cfg.dash.
                // idle_quiet_ms` is reused rather than a new wrap-only knob:
                // it already means exactly "how long a signal-less session's
                // pty must be quiet before zirv treats it as idle", the same
                // question this is, and it is already an operator-only,
                // non-`REPO_FORBIDDEN` timing knob over a session the
                // operator chose to run interactively.
                let ready = mail_inject_ready(
                    adapter.capabilities().turn_signal,
                    supervision,
                    now,
                    debounce,
                    Duration::from_millis(cfg.dash.idle_quiet_ms),
                );
                match mail_watch.decide(&facts, ready) {
                    MailAction::None => {}
                    MailAction::Announce { count, ids } => {
                        // R5: `try_emit`, not `emit` -- an advisory the
                        // channel swallowed must stay unannounced so the next
                        // poll retries it. See `MailWatch::note_announcement`.
                        let landed = announcer.try_emit(&Event::MailWaiting { count });
                        mail_watch.note_announcement(&ids, landed);
                    }
                    MailAction::Inject {
                        count,
                        from_agent,
                        from_short,
                        ids,
                    } => {
                        // F4 (review, PR #116): phase 1 only, same reasoning
                        // as `Action::Compact` above -- a single write of the
                        // line plus its submitting `\r` is the same
                        // paste-fold hazard on a codex-shaped composer.
                        // `commit_injected` moves from "the write succeeded"
                        // to "phase 2's `\r` actually landed", below: the old
                        // code committed the instant this write returned
                        // `Ok`, which -- once the write was split -- would
                        // have marked the advisory delivered before the
                        // child had even seen the keypress that submits it.
                        let line = mail_advisory_line(count, &from_agent, &from_short);
                        let wrote = match writer.lock() {
                            Ok(mut sink) => sink
                                .write_all(line.as_bytes())
                                .and_then(|()| sink.flush())
                                .is_ok(),
                            Err(_) => false,
                        };
                        if wrote {
                            pending_submit = Some(PendingSubmit {
                                deadline: Instant::now()
                                    + super::dash::pane::INJECTION_SUBMIT_DELAY,
                                kind: PendingSubmitKind::MailAdvisory { ids },
                            });
                        } else {
                            // Same R5 rule on the degrade path: a poisoned
                            // writer plus a swallowed announcement must leave
                            // the advisory owed, not quietly discharged.
                            let landed = announcer.try_emit(&Event::MailWaiting { count });
                            mail_watch.note_announcement(&ids, landed);
                        }
                    }
                }
            }
        }

        // F4 (review, PR #116): drains `pending_submit` once its deadline
        // has passed -- the phase-2 half of whichever injection is
        // outstanding (`Action::Compact` or the T13 mail advisory just
        // above; never both, see `PendingSubmit`'s own doc comment). Runs
        // every tick, unconditionally, exactly as `wrap`'s escalation ladder
        // and mail poll already do -- the `~100ms` `PUMP_POLL` cadence is
        // well under `INJECTION_SUBMIT_DELAY`, so the deadline is checked
        // far more often than it needs to be, which is what "poll the
        // deadline" means here rather than "sleep for it".
        let now = Instant::now();
        if pending_submit.as_ref().is_some_and(|p| now >= p.deadline) {
            let cr_ok = match writer.lock() {
                Ok(mut sink) => super::dash::pane::write_submit_cr(&mut *sink).is_ok(),
                Err(_) => false,
            };
            if cr_ok {
                // `if let` rather than an infallible unwrap/expect: `wrap`'s
                // own hot-path rule (CLAUDE.md) is no panicking on this loop
                // under any condition, even one this guard believes cannot
                // happen. A `None` here would simply mean nothing to finish,
                // which is a safe no-op.
                if let Some(pending) = pending_submit.take() {
                    match pending.kind {
                        PendingSubmitKind::Compact => {
                            // Exactly what the old synchronous write did
                            // right after its own single `write_all`
                            // succeeded: verify, announce, log. `transcript`
                            // is read fresh here (not captured at phase 1),
                            // which only matters if a transcript path
                            // arrives in the ~50ms settle window -- a strict
                            // improvement over the old code, which could
                            // never see one.
                            let failure = match transcript.path() {
                                None => Some("no transcript reported, compaction unverifiable"),
                                Some(path) => {
                                    let seen = verify_compaction(
                                        &mut Watcher::new(path.to_path_buf()),
                                        adapter.as_ref(),
                                        Instant::now() + inject_timeout,
                                    )
                                    .unwrap_or(false);
                                    if seen {
                                        None
                                    } else {
                                        Some("compaction not verified")
                                    }
                                }
                            };
                            let verified = failure.is_none();
                            announcer.emit(&Event::Compact { verified });
                            if let Some(reason) = failure {
                                note_failure(
                                    supervision,
                                    Some((state_dir, session.as_str())),
                                    reason,
                                    announcer,
                                );
                            }
                            let _ = super::log::append(
                                state_dir,
                                &super::log::Decision {
                                    ts: super::state::now_secs(),
                                    session: session.as_str(),
                                    verb: "wrap",
                                    verdict: "compact",
                                    score: supervision.score,
                                    action: if verified {
                                        "inject"
                                    } else {
                                        "inject-unverified"
                                    },
                                    detail: &transcript
                                        .path()
                                        .map(|path| path.display().to_string())
                                        .unwrap_or_else(|| "no transcript reported".to_string()),
                                },
                            );
                        }
                        PendingSubmitKind::MailAdvisory { ids } => {
                            mail_watch.commit_injected(&ids);
                        }
                    }
                }
            }
            // else: `cr_ok` was false -- leave `pending_submit` set exactly
            // as it was, so the next tick retries the lone `\r`. See
            // `dash::pane::write_submit_cr`'s own doc comment for why a
            // retried CR is always safe (worst case one extra, harmless
            // Enter keypress), never a re-send of phase 1's text.
        }

        if let Ok(size) = window_size(STDIN_FD)
            && size != last_size
        {
            last_size = size;
            // B1: `chrome::resize_decision` is the single source of truth for
            // what a resize does to the bar and the pty -- see its own tests
            // for the shrink-below-floor and widen-after-degrade cases this
            // used to get wrong (the child pinned at a stale reserved size
            // forever, even once the terminal widened back out).
            let decision = super::chrome::resize_decision(bar.active(), size);
            if decision.disables_bar {
                disable_bar_at_current_size(bar, size);
            } else if decision.set_scroll_region {
                bar.cols = size.0;
                bar.rows = size.1;
                let region = super::chrome::scroll_region_sequence(bar.rows);
                let region_ok = match bar.stdout_lock.lock() {
                    Ok(_guard) => {
                        let mut stdout = std::io::stdout();
                        stdout
                            .write_all(region.as_bytes())
                            .and_then(|()| stdout.flush())
                            .is_ok()
                    }
                    Err(_) => false,
                };
                bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, region_ok);
                // C5: set on success, never *cleared* on failure. A resize
                // whose region write failed leaves whatever region was
                // already in effect still fencing the console, so clearing
                // the flag here would tell the emergency handler it owes the
                // terminal nothing while the terminal was still fenced.
                // `BAR_ACTIVE` means "we have set a region that is still
                // outstanding", and only a successful `reset_bar` retires it.
                if region_ok {
                    super::term::set_bar_active(true);
                }
                if !bar.disabled {
                    // The bar's own row moved; the next throttle tick must
                    // redraw it even if the text is unchanged.
                    bar.last_text = None;
                }
            }
            let _ = pair.master.resize(PtySize {
                rows: decision.pty_size.1,
                cols: decision.pty_size.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        // B1: catches a disable that just happened above (this tick's
        // resize shrank below the floor) *and* one that happened earlier
        // with no resize event at all (a redraw or lock failure) -- either
        // way, `bar_needs_recovery` makes this a one-time, idempotent
        // reaction, so calling it unconditionally every tick is cheap and
        // correct in both cases.
        recover_bar_to_full_size(bar, pair, last_size);

        std::thread::sleep(PUMP_POLL);
    }
}

/// `Orchestrator`, not `Worker`: the operator is sitting in front of the
/// command `wrap` supervises, driving it themselves, which is the same seat
/// `chat` builds (CLAUDE.md classifies both as interactive Orchestrator
/// sessions). The role now also picks the adapter's own layer and the
/// user-layer file (`prompt::with_adapter_layer`, `prompt::
/// WORKER_PROMPT_FILE`), so passing `Worker` here would inject
/// worker-conventions text into an operator's own session and silently drop
/// their `~/.zirv/system-prompt.md` -- a session made worse by being wrapped,
/// which is the one thing `wrap` must never do.
pub fn run<W: Write>(args: &WrapArgs, _w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(
        args,
        &repo,
        &env,
        PromptRole::Orchestrator,
        None,
        super::sessions::Verb::Wrap,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // Every pty-driven test is unix-only: the supervision it exercises needs a
    // unix socket for turn signals and raw mode for passthrough.
    #[cfg(unix)]
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    #[cfg(unix)]
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn test_bar(size: (u16, u16)) -> BarRuntime {
        BarRuntime::new(
            super::super::chrome::Chrome {
                banner: true,
                bar: true,
                colour: false,
            },
            "claude".to_string(),
            "anthropic".to_string(),
            "sess0000".to_string(),
            true,
            std::sync::Arc::new(std::sync::Mutex::new(())),
            size,
            900,
        )
    }

    /// Item 2 (Usage and Pacing follow-up): the status bar used to read the
    /// single legacy `usage.json` regardless of which adapter it was
    /// wrapping, so a codex (`openai`) session's bar rendered whatever
    /// Anthropic numbers a claude session happened to leave there. It now
    /// reads `window::load_for(state_dir, &bar.provider)`, so a codex
    /// session's usage segment must show the placeholder dash (no usage
    /// source) even while a real claude reading sits in the legacy file --
    /// and a claude session must still see it, via the same legacy-file
    /// fallback `load_for` already gives `anthropic`.
    #[test]
    fn a_codex_sessions_bar_does_not_inherit_claudes_own_legacy_usage_reading() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // The codex bar below is due for its own passive rollout scan (never
        // scanned yet), which resolves `~/.codex/sessions` via `HOME`/
        // `USERPROFILE` -- must not be left pointed at this machine's real
        // home directory. An empty one yields no rollouts, so the assertions
        // below still hold: the passive scan itself is exercised (it runs,
        // finds nothing, leaves the stored state untouched), not bypassed.
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        super::super::window::store(
            &state,
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 42.0,
                    resets_at: 0,
                    // Fresh, not epoch-1: this test is about the legacy-file
                    // fallback, not about `window::available`'s own staleness
                    // filter (covered separately), so the reading must still
                    // be inside its own five_hour span.
                    observed_at: super::super::state::now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store the legacy (claude) reading");

        let mut codex_bar = test_bar((80, 1));
        codex_bar.provider = "openai".to_string();
        redraw_bar_if_due(
            &mut codex_bar,
            &InjectionState::new(),
            &state,
            tmp.path(),
            Instant::now(),
        );
        let codex_text = codex_bar.last_text.expect("the bar drew something");
        assert!(
            !codex_text.contains("42%"),
            "codex must not inherit claude's own legacy reading: {codex_text}"
        );
        assert!(
            codex_text.contains("usage \u{2013}"),
            "and must show the placeholder, not a fabricated zero: {codex_text}"
        );

        let mut claude_bar = test_bar((80, 1));
        redraw_bar_if_due(
            &mut claude_bar,
            &InjectionState::new(),
            &state,
            tmp.path(),
            Instant::now(),
        );
        let claude_text = claude_bar.last_text.expect("the bar drew something");
        assert!(
            claude_text.contains("42%"),
            "claude keeps reading the legacy file via its own provider fallback: {claude_text}"
        );
    }

    /// A reading whose window has certainly reset (`resets_at` long past)
    /// must not render as a live percentage just because it is the newest
    /// thing on disk -- that is exactly the stale-14%-months-old bug this
    /// fix addresses. The bar must fall back to its usual placeholder, same
    /// as the true no-source case.
    #[test]
    fn a_bar_with_an_expired_window_shows_the_placeholder_not_a_stale_percent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        super::super::window::store(
            &state,
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 14.0,
                    resets_at: 1, // long past any real wall clock
                    observed_at: 1,
                }),
                seven_day: None,
            },
        )
        .expect("store an expired reading");

        let mut bar = test_bar((80, 1));
        redraw_bar_if_due(
            &mut bar,
            &InjectionState::new(),
            &state,
            tmp.path(),
            Instant::now(),
        );
        let text = bar.last_text.expect("the bar drew something");
        assert!(
            !text.contains("14%"),
            "an expired window must not render as a current percent: {text}"
        );
        assert!(
            text.contains("usage \u{2013}"),
            "expired-and-nothing-else renders the placeholder: {text}"
        );
    }

    /// Item 2 (regression): a shrink that disables the bar must move
    /// `bar.rows`/`bar.cols` to the *current* size before disabling, so the
    /// reset sequence that follows (recovery, or the final session cleanup)
    /// addresses the row that is actually still on screen. Left stale (the
    /// old, larger row number), the real terminal clamps the out-of-range
    /// cursor move to its own last row and blanks a line of the child's own
    /// live output there.
    #[test]
    fn reset_after_a_shrink_degrade_targets_the_current_last_row() {
        let mut bar = test_bar((100, 30));
        assert_eq!(bar.rows, 30, "launched tall");

        disable_bar_at_current_size(&mut bar, (100, 5));

        assert_eq!(
            bar.rows, 5,
            "the dims move to the current size before disabling"
        );
        assert_eq!(bar.cols, 100);
        assert!(bar.disabled);

        let reset = super::super::chrome::bar_reset_sequence(bar.rows);
        assert!(
            reset.contains("\x1b[5;1H"),
            "the reset must address the CURRENT last row: {reset:?}"
        );
        assert!(
            !reset.contains("\x1b[30;1H"),
            "must not address the stale, now out-of-range row: {reset:?}"
        );
    }

    /// Item 5 (regression): a restart's fresh pty has to reserve the bottom
    /// row while the bar is still alive, exactly like the initial launch
    /// and an ordinary resize -- otherwise the freshly relaunched child gets
    /// the full terminal height and the bar immediately starts drawing over
    /// its own last line.
    #[test]
    fn a_relaunch_while_the_bar_is_active_keeps_the_reserved_row() {
        let bar = test_bar((100, 30));
        assert!(bar.active(), "sanity: freshly constructed and eligible");
        assert_eq!(
            relaunch_size(&bar, (100, 30)),
            (100, 29),
            "the relaunch must reserve the same row the live session already has"
        );
    }

    /// And once the bar has degraded, a restart is ordinary full-size
    /// forwarding, exactly like every other pty operation once `bar.active()`
    /// is false (B1).
    #[test]
    fn a_relaunch_after_the_bar_has_degraded_uses_the_full_size() {
        let mut bar = test_bar((100, 30));
        bar.disabled = true;
        assert_eq!(relaunch_size(&bar, (100, 30)), (100, 30));
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn zirv_bin() -> PathBuf {
        // cargo test builds the bin target, so it sits next to the test binary's
        // grandparent directory (target/debug/deps/<test> -> target/debug/zirv).
        std::env::current_exe()
            .expect("current_exe")
            .parent()
            .and_then(|p| p.parent())
            .expect("target dir")
            .join(if cfg!(windows) { "zirv.exe" } else { "zirv" })
    }

    #[cfg(unix)]
    pub(crate) fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Drives `zirv ctx wrap` from inside an outer PTY, which is the only way to
    /// exercise raw-mode passthrough end to end.
    #[cfg(unix)]
    pub(crate) struct Harness {
        pub reader: Box<dyn Read + Send>,
        pub writer: Box<dyn Write + Send>,
        pub child: Box<dyn portable_pty::Child + Send + Sync>,
        /// C9: keeps the throwaway state/HOME directory alive for as long as
        /// the wrapped subprocess is. Dropped with the harness, which is
        /// after the child has been waited on in every test that uses one.
        _sandbox: tempfile::TempDir,
    }

    /// The general form: `flags` are wrap's own arguments, inserted before the
    /// `--` separator. `spawn_wrap` below is the common case (`--agent claude`)
    /// most tests want; this exists for the tests that need to vary wrap's own
    /// flags (`--no-supervise`, an omitted `--agent`, ...).
    #[cfg(unix)]
    pub(crate) fn spawn_wrap_with_flags(
        extra_env: &[(&str, String)],
        flags: &[&str],
        wrapped: &[&str],
    ) -> Harness {
        spawn_wrap_with_flags_in(
            &std::env::current_dir().expect("cwd"),
            extra_env,
            flags,
            wrapped,
        )
    }

    /// I2 regression (2026-08-23): as of the zirv-managed context migration
    /// this checkout carries its own committed `.zirv/context/{common,claude,
    /// codex}.md` and `.zirv/memory/*.md`. `spawn_wrap_with_flags` pins the
    /// spawned child's cwd to *this* process's cwd (see the comment below) so
    /// mail-slug tests agree on "repo" -- but during `cargo test` that cwd
    /// *is* the checkout root, so any test asserting the exact composed
    /// prompt now picks up the checkout's own repo layers on top of the
    /// built-in default, not just what it explicitly wrote into a fixture.
    /// This variant takes an explicit `cwd` so a prompt-composition test can
    /// point the child at an isolated, unrelated temp directory instead --
    /// keeping the mail-slug tests (which still want cwd == this process's
    /// cwd) on `spawn_wrap_with_flags` unchanged.
    #[cfg(unix)]
    pub(crate) fn spawn_wrap_with_flags_in(
        cwd: &std::path::Path,
        extra_env: &[(&str, String)],
        flags: &[&str],
        wrapped: &[&str],
    ) -> Harness {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(zirv_bin());
        cmd.arg("ctx");
        cmd.arg("wrap");
        for flag in flags {
            cmd.arg(flag);
        }
        cmd.arg("--");
        for arg in wrapped {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm");
        // `zirv ctx wrap`'s own `run()` derives its "repo" from
        // `std::env::current_dir()` of the wrap process itself -- the same
        // call the test-side helpers (`store_mail_for_cwd`, `repo_slug`
        // lookups) make from *this* process to compute the slug they file
        // and expect mail under. Without pinning it here, portable-pty's
        // `CommandBuilder` (see the HOME comment just below) falls back to
        // `chdir`-ing into HOME instead, so the two processes silently
        // disagree on "repo": the spawned `wrap` never finds mail the test
        // stored, and a test waiting on the mail advisory hangs rather than
        // failing loudly, since nothing further ever arrives on the pty.
        // Callers that don't need slug agreement (no mail involved) can pass
        // an isolated `cwd` instead -- see `spawn_wrap_with_flags_in` above.
        cmd.cwd(cwd);
        // C9: a throwaway state dir and HOME by default. These tests spawn a
        // real `zirv ctx wrap`, which resolves its state dir from the
        // environment -- so without this they registered sessions, published
        // socket paths, wrote decision logs and dropped handoffs straight
        // into the developer's own `~/.zirv`/state directory, where a later
        // real session would then read them. Applied *before* `extra_env`,
        // so a test that pins its own `ZIRV_CTX_STATE_DIR` (most of them do,
        // because they read it back) still wins.
        let sandbox = tempfile::tempdir().expect("tempdir");
        cmd.env(
            crate::commands::ctx::state::STATE_ENV,
            sandbox.path().join("state").display().to_string(),
        );
        // The directory has to actually exist, not just be a plausible
        // path. Historically this was load-bearing for the spawn itself:
        // with no `.cwd()` set, portable-pty's CommandBuilder falls back to
        // `chdir`-ing into `HOME` before exec (see `as_command` in its
        // `cmdbuilder.rs`), and a HOME that only existed on paper made that
        // chdir fail with ENOENT -- surfaced by `spawn_command` as "No such
        // file or directory", indistinguishable at a glance from the
        // wrapped binary itself being missing. The `.cwd()` pin above now
        // takes precedence over that fallback, but the spawned `zirv` still
        // resolves real paths under HOME (`~/.zirv`, state fallbacks), so
        // the sandbox home stays a directory that exists, not a fiction.
        let home_dir = sandbox.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create sandbox home");
        for home in ["HOME", "USERPROFILE"] {
            cmd.env(home, home_dir.display().to_string());
        }
        // Hermetic against the developer's own environment (F2): this spawns
        // the real `zirv` binary, which reads the process environment, so a
        // suite run from inside an agent session would otherwise trip the
        // nesting guard instead of running the test. Removed *before*
        // `extra_env`, which several tests use to pin `ZIRV_CTX_TRANSCRIPT`
        // deliberately. T8: see `testenv::scrub_supervision_env_for_test`'s
        // own doc comment for why this is a test-side scrub, not an extended
        // production one.
        crate::commands::ctx::testenv::scrub_supervision_env_for_test(&mut cmd);
        // T10 fix regression (2026-08-23): every one of these harnesses spawns
        // a *real* `zirv ctx wrap` attached to a real pty on both stdin and
        // stdout, which is exactly the condition the launch-time interactive
        // pacing gate (`pace::interactive_gate`, applied in `run_with` right
        // before spawn) is gated on. With a fresh, isolated state dir (no
        // usage source has ever been observed) that gate resolves to a blind
        // `pace.blind_delay_secs` (60s) pause on every single test, which is
        // both why this suite took 31 minutes in CI and why the pty output
        // these tests assert on exactly (argv, prompt text, `--sandbox`/
        // `--ask-for-approval` flags) had the pause's own banner text mixed
        // into it -- `read_until`'s fixed budget elapses mid-pause and the
        // assertions see nothing, or the wrong thing. Neither the pacing gate
        // nor the shipped sandbox posture is what these tests exist to cover
        // (`force_pace_skips_the_wait_or_confirmation_for_both_pause_and_
        // refuse` and `a_supervised_wrap_carries_the_shipped_sandbox_posture_
        // for_codex` are, and opt back in explicitly via `extra_env`, applied
        // after these two and so free to override them), so both are off by
        // default here -- the same `base_env`-defaults-it-once shape
        // `exec.rs`/`run_loop.rs` already use to zero their own blind delay,
        // just disabling the gate outright rather than zeroing its delay,
        // since this harness spawns a real subprocess and cannot inject a
        // `FakeClock`.
        cmd.env("ZIRV_CTX_PACE", "false");
        cmd.env("ZIRV_CTX_SANDBOX", "false");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn wrap");
        drop(pair.slave);
        Harness {
            reader: pair.master.try_clone_reader().expect("reader"),
            writer: pair.master.take_writer().expect("writer"),
            child,
            _sandbox: sandbox,
        }
    }

    #[cfg(unix)]
    pub(crate) fn spawn_wrap(extra_env: &[(&str, String)], wrapped: &[&str]) -> Harness {
        spawn_wrap_with_flags(extra_env, &["--agent", "claude"], wrapped)
    }

    /// Reads until `needle` appears or the timeout expires.
    #[cfg(unix)]
    pub(crate) fn read_until(
        reader: &mut Box<dyn Read + Send>,
        needle: &str,
        timeout: Duration,
    ) -> String {
        let deadline = Instant::now() + timeout;
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while Instant::now() < deadline {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if seen.contains(needle) {
                        return seen;
                    }
                }
                Err(_) => break,
            }
        }
        seen
    }

    /// Finds `flag`'s value in a space-joined argv rendering (`stub-tui.sh`
    /// prints `argv: %s` from `"$*"`), on the assumption the value itself
    /// has no whitespace -- true for a prompt-file path under a state dir
    /// that is itself a plain tempdir.
    #[cfg(unix)]
    pub(crate) fn flag_value<'a>(seen: &'a str, flag: &str) -> Option<&'a str> {
        let mut tokens = seen.split_whitespace();
        while let Some(token) = tokens.next() {
            if token == flag {
                return tokens.next();
            }
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn flag_value_finds_the_token_right_after_the_flag() {
        let seen = "argv: --append-system-prompt-file /tmp/x/prompts/abc.md\r\nstub-tui ready\r\n";
        assert_eq!(
            flag_value(seen, "--append-system-prompt-file"),
            Some("/tmp/x/prompts/abc.md")
        );
        assert_eq!(flag_value(seen, "--session-id"), None);
    }

    // F1: `quit_child` must never write a console control byte into the pty.

    /// A child that never exits on its own, so `quit_child` has to walk its
    /// whole ladder, and that records whether it was killed.
    ///
    /// A stub rather than a real pty child on purpose: the sink is then a
    /// plain `Vec<u8>` whose exact bytes can be asserted on (the whole point
    /// of the test), and it runs on Windows too -- which is the platform the
    /// `\x03\x03` rung was actually dangerous on, and where every other
    /// `quit_child` test is `cfg(unix)`-gated away.
    #[derive(Debug, Clone, Default)]
    struct KillFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl KillFlag {
        fn killed(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[derive(Debug)]
    struct StubbornChild {
        killed: KillFlag,
    }

    impl portable_pty::ChildKiller for StubbornChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.killed
                .0
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(StubbornChild {
                killed: self.killed.clone(),
            })
        }
    }

    impl portable_pty::Child for StubbornChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(self
                .killed
                .killed()
                .then(|| portable_pty::ExitStatus::with_exit_code(1)))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Ok(portable_pty::ExitStatus::with_exit_code(1))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    fn stubborn_child() -> (Box<dyn portable_pty::Child + Send + Sync>, KillFlag) {
        let killed = KillFlag::default();
        (
            Box::new(StubbornChild {
                killed: killed.clone(),
            }),
            killed,
        )
    }

    /// The regression this whole round exists for. On Windows the pty master
    /// is a ConPTY, and conhost turns a `\x03` written into it into a console
    /// control event broadcast to *every* process attached to the
    /// pseudoconsole -- with no `CREATE_NEW_PROCESS_GROUP` to narrow it to,
    /// because portable-pty 0.9.0 does not spawn with one. A nested `wrap`
    /// quitting its own child therefore killed the outer session's agent
    /// too. The ladder is now the adapter's quit sequence, then the single
    /// narrow `child.kill()`, and nothing in between.
    #[test]
    fn quit_child_never_writes_a_control_c_into_the_pty() {
        let (mut child, killed) = stubborn_child();
        let mut sink: Vec<u8> = Vec::new();

        quit_child(&mut sink, &mut child, "/exit\r", Duration::from_millis(10)).expect("quit");

        assert!(
            !sink.contains(&0x03),
            "no console control byte may reach the pty: {sink:?}"
        );
        assert_eq!(
            sink,
            b"/exit\r".to_vec(),
            "only the adapter's own quit sequence is written: {:?}",
            String::from_utf8_lossy(&sink)
        );
        assert!(
            killed.killed(),
            "a child that ignores the quit sequence escalates straight to the narrow kill"
        );
    }

    #[test]
    fn quit_child_writes_nothing_at_all_to_a_child_that_has_already_exited() {
        let (mut child, killed) = stubborn_child();
        // Already dead before `quit_child` is called.
        portable_pty::ChildKiller::kill(&mut *child).expect("kill");
        let mut sink: Vec<u8> = Vec::new();

        quit_child(&mut sink, &mut child, "/exit\r", Duration::from_millis(10)).expect("quit");

        assert!(sink.is_empty(), "nothing to say to a dead child: {sink:?}");
        assert!(killed.killed());
    }

    // F2: the nesting guard.

    /// An env map for the guard tests, always pinning `ZIRV_CTX_AGENT_BIN`
    /// at a path that cannot exist.
    ///
    /// Safety belt, not a fixture detail. `adapters::select` calls `ready()`,
    /// so an unusable agent_bin makes a launch structurally impossible: if
    /// the guard under test ever regresses, these tests fail on a missing
    /// binary instead of opening a pty and spawning a real nested agent into
    /// whatever session the suite is running in.
    fn nested_env(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        let mut env: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            "/nonexistent/agent-must-never-launch".to_string(),
        );
        env
    }

    fn wrap_args_in(command: &[&str], allow_nested: bool) -> WrapArgs {
        WrapArgs {
            agent: Some("claude".to_string()),
            no_supervise: false,
            command: command.iter().map(|s| (*s).to_string()).collect(),
            simple: false,
            allow_nested,
            force_pace: false,
        }
    }

    #[test]
    fn wrap_refuses_to_start_inside_a_supervised_session_and_names_the_evidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = nested_env(&[(
            adapters::SESSION_ENV,
            "abcdef12-3456-4789-8abc-def012345678",
        )]);

        let err = run_with(
            &wrap_args_in(&["claude"], false),
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("a wrap inside a supervised session must refuse");

        let msg = err.to_string();
        assert!(
            msg.contains("refusing to start inside an existing agent session"),
            "got {msg}"
        );
        assert!(
            msg.contains("abcdef12"),
            "names the outer session it found: {msg}"
        );
        assert!(
            msg.contains("--allow-nested"),
            "says how to override: {msg}"
        );
    }

    /// The Claude Code marker pair is the second, independent source of
    /// evidence: a `zirv chat` typed into a Claude Code session's own shell
    /// exports no `ZIRV_CTX_*` at all when that session was started outside
    /// zirv, but it is still nested.
    #[test]
    fn wrap_refuses_inside_a_claude_code_session_too() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = nested_env(&[("CLAUDE_PID", "4242"), ("CLAUDECODE", "1")]);

        let err = run_with(
            &wrap_args_in(&["claude"], false),
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("refuses");
        assert!(err.to_string().contains("Claude Code"), "got {err}");
    }

    /// With the override on, the guard is out of the way and the run reaches
    /// the *next* refusal in `run_with` -- the undetected-command one. That
    /// specific later error is the evidence the guard was passed, without
    /// this test ever having to spawn a pty or an agent.
    #[test]
    fn allow_nested_overrides_the_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [(
            adapters::SESSION_ENV.to_string(),
            "abcdef12-3456-4789-8abc-def012345678".to_string(),
        )]
        .into();
        let mut args = wrap_args_in(&["echo", "hello"], true);
        args.agent = None;

        let err = run_with(
            &args,
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("gets past the nesting guard, then fails on the undetected command");
        let msg = err.to_string();
        assert!(
            !msg.contains("refusing to start inside"),
            "the guard was overridden: {msg}"
        );
        assert!(msg.contains("could not tell which agent"), "got {msg}");
    }

    /// And the environment variable is the second, equivalent override, for
    /// an operator who cannot reach the command line (a wrapper script, a
    /// CI job).
    #[test]
    fn the_allow_nested_environment_variable_overrides_the_guard_too() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            (
                super::super::sessions::ALLOW_NESTED_ENV.to_string(),
                "true".to_string(),
            ),
        ]
        .into();
        let mut args = wrap_args_in(&["echo", "hello"], false);
        args.agent = None;

        let err = run_with(
            &args,
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("past the guard, onto the undetected command");
        assert!(
            !err.to_string().contains("refusing to start inside"),
            "got {err}"
        );
    }

    /// A plain terminal is not nested, and nothing about the guard changes
    /// what a normal run does: the same undetected-command refusal, reached
    /// the same way.
    #[test]
    fn a_wrap_outside_any_session_is_not_gated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut args = wrap_args_in(&["echo", "hello"], false);
        args.agent = None;
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("echo matches no adapter");
        assert!(
            !err.to_string().contains("refusing to start inside"),
            "got {err}"
        );
    }

    /// Finding #1: `wrap` launches/supervises a harness, so a syntax error
    /// in the operator's own HOME `ctx.toml` must refuse outright (via
    /// `CtxConfig::load_for_launch`) rather than silently degrading to
    /// permissive pacing/policy/sandbox defaults right before a harness
    /// spawns. Reached before the (later, weaker) undetected-command
    /// refusal `a_wrap_outside_any_session_is_not_gated` exercises, proving
    /// the config load itself is what fails here.
    #[test]
    fn a_home_layer_syntax_error_refuses_to_launch_naming_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(home.join(".zirv/ctx.toml"), "[score\n").expect("write broken home layer");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let mut args = wrap_args_in(&["echo", "hello"], false);
        args.agent = None;

        let err = run_with(
            &args,
            &repo,
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("a broken home layer must refuse to launch");
        let msg = err.to_string();
        assert!(
            msg.contains(&home.join(".zirv").join("ctx.toml").display().to_string()),
            "names the broken file: {msg}"
        );
    }

    // F3: a child never inherits another session's identity.

    /// The bind-failure case, which is the one that actually bit: `wrap`
    /// binds no socket, so it has no turn-signal env of its own to set, and
    /// every `ZIRV_CTX_*` the child sees would otherwise be the *outer*
    /// session's -- inherited straight out of this process's environment,
    /// because `CommandBuilder::new` seeds itself from `std::env::vars_os`.
    #[test]
    fn a_child_never_inherits_the_outer_sessions_socket_or_id() {
        let _outer = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                adapters::SESSION_ENV,
                Some("outer111-2222-4333-8444-555555555555"),
            ),
            (adapters::SOCKET_ENV, Some("/tmp/outer.sock")),
            (TRANSCRIPT_ENV, Some("/tmp/outer.jsonl")),
        ]);

        // Sanity, and the reason the scrub has to be explicit: an untouched
        // builder really does carry the outer session's values.
        let inherited = CommandBuilder::new("echo");
        assert_eq!(
            inherited
                .get_env(adapters::SESSION_ENV)
                .and_then(|v| v.to_str()),
            Some("outer111-2222-4333-8444-555555555555"),
            "sanity: portable-pty seeds a builder from the process environment"
        );

        // No socket of its own: every supervision variable must be *absent*,
        // not inherited.
        let mut no_socket = CommandBuilder::new("echo");
        apply_session_env(&mut no_socket, &[]);
        for key in super::super::sessions::SUPERVISION_ENV {
            assert_eq!(
                no_socket.get_env(key),
                None,
                "{key} must not reach a child of an unsupervised wrap"
            );
        }

        // And with a socket of its own, only its own values reach the child.
        let mut supervised = CommandBuilder::new("echo");
        apply_session_env(
            &mut supervised,
            &[(
                adapters::SESSION_ENV.to_string(),
                "inner999-2222-4333-8444-555555555555".to_string(),
            )],
        );
        assert_eq!(
            supervised
                .get_env(adapters::SESSION_ENV)
                .and_then(|v| v.to_str()),
            Some("inner999-2222-4333-8444-555555555555"),
        );
        assert_eq!(
            supervised.get_env(adapters::SOCKET_ENV),
            None,
            "the outer socket must not survive alongside the inner id"
        );
        assert_eq!(supervised.get_env(TRANSCRIPT_ENV), None);
    }

    // F5: one published socket path per session.

    #[test]
    fn two_concurrent_wraps_do_not_clobber_each_others_socket_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let one = "aaaa1111-2222-4333-8444-555555555555";
        let two = "bbbb2222-2222-4333-8444-555555555555";

        publish_socket_path(&state, one, Path::new("/tmp/one.sock"));
        publish_socket_path(&state, two, Path::new("/tmp/two.sock"));

        assert_eq!(
            read_socket_path(&state, Some(one)).as_deref(),
            Some("/tmp/one.sock"),
            "the second launch must not have overwritten the first"
        );
        assert_eq!(
            read_socket_path(&state, Some(two)).as_deref(),
            Some("/tmp/two.sock")
        );
        assert!(
            !state.root().join(SOCKET_PATH_FILE).exists(),
            "the clobber-prone global file is never written any more"
        );

        // And releasing one leaves the other reachable.
        unpublish_socket_path(&state, one);
        assert_eq!(read_socket_path(&state, Some(one)), None);
        assert_eq!(
            read_socket_path(&state, Some(two)).as_deref(),
            Some("/tmp/two.sock")
        );
    }

    #[test]
    fn a_reader_resolves_its_own_sessions_socket_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mine = "cccc3333-2222-4333-8444-555555555555";
        let theirs = "dddd4444-2222-4333-8444-555555555555";

        publish_socket_path(&state, mine, Path::new("/tmp/mine.sock"));
        publish_socket_path(&state, theirs, Path::new("/tmp/theirs.sock"));

        assert_eq!(
            read_socket_path(&state, Some(mine)).as_deref(),
            Some("/tmp/mine.sock"),
            "a reader that knows its own session id never gets somebody else's socket"
        );
        // A short id resolves the same file the full session id does.
        assert_eq!(
            read_socket_path(&state, Some("cccc3333")).as_deref(),
            Some("/tmp/mine.sock")
        );
        // With no session to go on, *some* published socket is the best
        // honest answer -- but only one of the two real ones, never a mix.
        let anonymous = read_socket_path(&state, None).expect("a published socket");
        assert!(
            anonymous == "/tmp/mine.sock" || anonymous == "/tmp/theirs.sock",
            "got {anonymous}"
        );
    }

    /// Backward tolerance: a supervisor from a pre-F5 build published the
    /// global file and nothing else. A reader must still find it.
    #[test]
    fn a_reader_still_falls_back_to_the_legacy_global_socket_path_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        super::super::state::create_private_dir_all(state.root()).expect("mkdir");
        std::fs::write(state.root().join(SOCKET_PATH_FILE), "/tmp/legacy.sock").expect("write");

        assert_eq!(
            read_socket_path(&state, None).as_deref(),
            Some("/tmp/legacy.sock")
        );
        assert_eq!(
            read_socket_path(&state, Some("cccc3333")).as_deref(),
            Some("/tmp/legacy.sock"),
            "a session with no file of its own still falls back"
        );

        // A per-session file wins over the legacy one for its own session.
        publish_socket_path(
            &state,
            "cccc3333-2222-4333-8444-555555555555",
            Path::new("/tmp/mine.sock"),
        );
        assert_eq!(
            read_socket_path(&state, Some("cccc3333")).as_deref(),
            Some("/tmp/mine.sock")
        );
    }

    #[test]
    fn reading_a_state_dir_with_nothing_published_is_none_rather_than_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        assert_eq!(read_socket_path(&state, None), None);
        assert_eq!(read_socket_path(&state, Some("cccc3333")), None);
    }

    /// T10: `--force-pace` skips the interactive wait/confirmation
    /// deterministically -- the one half of `apply_interactive_gate` that is
    /// unit-testable without a real terminal, since the non-`force_pace`
    /// branches call into `crossterm`'s raw-mode keypress detection
    /// (`interactive_skippable_pause`/`interactive_confirm`), which needs an
    /// actual console and is exercised only by hand and by the `gate ==
    /// Launch` case reaching zero I/O at all in every other wrap test (the
    /// terminal-presence check ahead of this function already keeps them
    /// off this path entirely under `cargo test`'s piped stdio).
    #[test]
    fn force_pace_skips_the_wait_or_confirmation_for_both_pause_and_refuse() {
        assert!(apply_interactive_gate(pace::InteractiveGate::Launch, false).is_ok());
        assert!(apply_interactive_gate(pace::InteractiveGate::Launch, true).is_ok());

        assert!(
            apply_interactive_gate(
                pace::InteractiveGate::Pause {
                    message: "usage 85.0% of the five_hour window".to_string(),
                    seconds: 999_999,
                },
                true,
            )
            .is_ok(),
            "force_pace must not block on a pause that would otherwise wait ~11 days"
        );

        assert!(
            apply_interactive_gate(
                pace::InteractiveGate::Refuse {
                    message: "usage 99.9% of the five_hour window".to_string(),
                },
                true,
            )
            .is_ok(),
            "force_pace must launch anyway even at the hard ceiling"
        );
    }

    #[test]
    fn wrap_needs_a_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: None,
            no_supervise: false,
            command: Vec::new(),
            simple: false,
            allow_nested: false,
            force_pace: false,
        };
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("nothing to wrap");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    /// M5's undetected-command refusal used to hardcode "pass --agent
    /// claude", which only ever named one adapter no matter how many the
    /// registry actually holds. The error must instead name whatever the
    /// registry currently reports as available (gate-enabled and `ready()`),
    /// so a second working adapter shows up here without an edit to this
    /// string.
    #[test]
    fn the_undetected_command_error_names_the_registry_rather_than_claude() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: None,
            no_supervise: false,
            command: vec!["echo".to_string(), "hello".to_string()],
            simple: false,
            allow_nested: false,
            force_pace: false,
        };
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("echo matches no adapter");
        let msg = err.to_string();
        assert!(
            msg.contains("--agent"),
            "still tells the user how to fix it: {msg}"
        );
        for name in adapters::available_adapter_names(&CtxConfig::default()) {
            assert!(
                msg.contains(name),
                "must name available adapter '{name}': {msg}"
            );
        }
    }

    /// N1: `merge_command_line_prompt` strips the user's own
    /// `--append-system-prompt` and its value out of the passthrough argv.
    /// When that flag pair was the *entire* wrapped command, the argv is
    /// empty after the merge even though `args.command` itself was not empty
    /// at the top of `run_with`. This must be a returned error, not a panic:
    /// release is `panic = "abort"` and this is a supervisor hot path.
    /// `--agent claude` is explicit here so the adapter-match gate does not
    /// also suppress composition: the wrapped "command" is a bare flag pair,
    /// which detection would never recognize as any adapter's own binary.
    #[test]
    fn a_prompt_flag_that_empties_the_argv_after_merging_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: Some("claude".to_string()),
            no_supervise: false,
            command: vec!["--append-system-prompt".to_string(), "foo".to_string()],
            simple: false,
            allow_nested: false,
            force_pace: false,
        };
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("nothing left to wrap");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapped_program_output_reaches_the_terminal() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"), "got: {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// I2: a user's own --append-system-prompt inside the wrapped command must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[cfg(unix)]
    #[test]
    fn a_users_own_append_system_prompt_is_merged_not_dropped() {
        // A real state dir (with no override, this test used to run against
        // one) is not test-isolated, and on macOS its default path contains
        // a space ("Application Support"), which breaks whitespace-based
        // parsing of a prompt-file path out of the stub's echoed argv below.
        // A tempdir sidesteps both.
        let state = tempfile::tempdir().expect("tempdir");
        // I2 regression (2026-08-23): this test asserts the exact composed
        // prompt text, so it cannot run with the wrapped child's cwd pinned
        // to this checkout -- the checkout now carries its own committed
        // `.zirv/context/*.md` and `.zirv/memory/*.md` (the zirv-managed
        // context migration), which would merge into the composed prompt on
        // top of the built-in default and the user's own flag under test.
        // No mail is involved here, so there is no need for the spawned
        // child's cwd to match this process's cwd for slug agreement --
        // point it at an isolated, unrelated temp dir instead.
        let repo = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap_with_flags_in(
            repo.path(),
            &[("ZIRV_CTX_STATE_DIR", state.path().display().to_string())],
            &["--agent", "claude"],
            &[
                "sh",
                &script,
                "--append-system-prompt",
                "always answer in Danish",
            ],
        );

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert_eq!(
            seen.matches("--append-system-prompt").count(),
            1,
            "exactly one flag must reach the wrapped agent: {seen:?}"
        );

        // M7: the composed prompt travels either on argv
        // (--append-system-prompt <text>) or, when the configured agent
        // binary supports it, in a private file referenced by
        // --append-system-prompt-file <path>. The invariant under test is
        // that the user's own instruction survives the merge, not which
        // mechanism carried it, so read whichever one actually fired.
        let carried_text = match flag_value(&seen, "--append-system-prompt-file") {
            Some(path) => {
                std::fs::read_to_string(path).expect("prompt file referenced on argv is readable")
            }
            None => seen.clone(),
        };
        assert!(
            carried_text.contains("always answer in Danish"),
            "the user's own instruction must survive: {carried_text:?}"
        );
        assert!(
            carried_text.contains("zirv session conventions"),
            "zirv's own layer is still present: {carried_text:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Bug B (harness/model parity, 2026-08-22): the shipped-default
    /// "sandboxed, no prompts" posture reaches a plain (non-dashboard)
    /// `wrap` launch too, not only the seams a human never watches. Not
    /// runnable on this Windows dev machine (`#[cfg(unix)]`, mirroring
    /// every neighbouring live-argv test in this module); intended for CI.
    #[cfg(unix)]
    #[test]
    fn a_supervised_wrap_carries_the_shipped_sandbox_posture_for_codex() {
        let script = fixture("stub-tui.sh").display().to_string();
        // This is the one test in the suite the shipped sandbox posture must
        // actually reach, so it opts back in over the harness's own default
        // (see `spawn_wrap_with_flags`'s `ZIRV_CTX_SANDBOX=false`) rather than
        // relying on whatever `[sandbox]` happens to default to.
        let mut h = spawn_wrap_with_flags(
            &[("ZIRV_CTX_SANDBOX", "true".to_string())],
            &["--agent", "codex"],
            &["sh", &script],
        );

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(
            seen.contains("--sandbox workspace-write")
                || seen.contains("--sandbox\nworkspace-write"),
            "the shipped sandbox posture must reach a plain wrap launch: {seen:?}"
        );
        assert!(
            seen.contains("--ask-for-approval never") || seen.contains("--ask-for-approval\nnever"),
            "got: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Bug (2026-08-02 validation of 2.5.0): `--no-supervise`'s own help text
    /// promises "no scoring, no injection", but it only turned off scoring;
    /// the system prompt was still composed and injected. `--no-supervise`
    /// must skip injection exactly like `--simple` does, including leaving a
    /// user's own `--append-system-prompt` untouched (nothing left to merge
    /// it into once nothing is composed).
    #[cfg(unix)]
    #[test]
    fn no_supervise_injects_nothing_and_leaves_the_users_own_flag_untouched() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap_with_flags(
            &[],
            &["--agent", "claude", "--no-supervise"],
            &[
                "sh",
                &script,
                "--append-system-prompt",
                "always answer in Danish",
            ],
        );

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert_eq!(
            seen.matches("--append-system-prompt").count(),
            1,
            "the user's own flag must pass through untouched: {seen:?}"
        );
        assert!(
            seen.contains("always answer in Danish"),
            "the user's own instruction is not stripped: {seen:?}"
        );
        assert!(
            !seen.contains("zirv session conventions"),
            "no-supervise is pure passthrough, no injection: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// M5: with no `--agent` given, `adapters::select` falls back to
    /// `ClaudeAdapter` when detection finds no match at all. Silently running
    /// the wrapped command anyway (even with injection suppressed) means
    /// `wrap` guesses an agent it cannot back up and can still start typing
    /// claude-only escape sequences into a foreign program. `echo` is not
    /// claude or codex, so wrap must refuse with an actionable error instead
    /// of running it unsupervised.
    #[cfg(unix)]
    #[test]
    fn wrapping_an_undetected_command_with_no_explicit_agent_is_a_clear_error() {
        let mut h = spawn_wrap_with_flags(&[], &[], &["echo", "hello"]);

        // Read before waiting: once the child (the pty's only other side) has
        // exited and every slave-side reference is closed, this platform can
        // drop whatever was still buffered in the master's queue rather than
        // let it be read afterwards (the same teardown quirk documented on
        // `wait_or_kill` above).
        let seen = read_until(&mut h.reader, "--agent", Duration::from_secs(5));
        assert!(
            seen.contains("--agent"),
            "the error must say how to fix it: {seen:?}"
        );
        assert!(
            !seen.contains("hello"),
            "the wrapped command must never have run: {seen:?}"
        );

        let status = h.child.wait().expect("wait");
        assert_ne!(
            status.exit_code(),
            0,
            "an unresolvable agent must fail, not run unsupervised"
        );
    }

    /// `--no-supervise` promises pure passthrough in its own help text: no
    /// scoring, no injection, nothing ever typed into the child. The M5 gate
    /// ran ahead of that decision, so it refused invocations where there was
    /// nothing left for a wrong guess to get wrong -- including the wrapper
    /// scripts around claude that the README's alias recipe encourages.
    #[cfg(unix)]
    #[test]
    fn no_supervise_passes_an_undetected_command_through_instead_of_refusing() {
        let mut h = spawn_wrap_with_flags(&[], &["--no-supervise"], &["echo", "hello"]);

        let seen = read_until(&mut h.reader, "hello", Duration::from_secs(5));
        assert!(
            seen.contains("hello"),
            "pure passthrough must actually run the command: {seen:?}"
        );
        assert!(
            !seen.contains("--agent"),
            "and must not refuse it: {seen:?}"
        );

        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn keystrokes_pass_through_byte_for_byte() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        h.writer
            .write_all("hello wrap\r".as_bytes())
            .expect("write");
        h.writer.flush().expect("flush");
        let seen = read_until(&mut h.reader, "echo: hello wrap", Duration::from_secs(10));
        assert!(seen.contains("echo: hello wrap"), "got: {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapped_exit_code_is_propagated() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        h.writer.write_all(b"/fail\r").expect("write");
        h.writer.flush().expect("flush");
        let status = h.child.wait().expect("wait");
        assert_eq!(
            status.exit_code(),
            5,
            "wrap must not swallow the agent's code"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrap_exits_when_the_wrapped_program_exits_on_its_own() {
        let mut h = spawn_wrap(&[], &["sh", "-c", "printf done\\n; exit 0"]);
        let seen = read_until(&mut h.reader, "done", Duration::from_secs(10));
        assert!(seen.contains("done"), "got {seen:?}");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_wrapped_binary_fails_without_wrecking_the_terminal() {
        let mut h = spawn_wrap(&[], &["/nonexistent/agent-binary"]);
        let status = h.child.wait().expect("wait");
        assert_ne!(status.exit_code(), 0);
        let seen = read_until(&mut h.reader, "", Duration::from_millis(300));
        assert!(
            !seen.contains("panicked"),
            "no panic on the hot path: {seen:?}"
        );
    }

    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    fn turn_signal(turn: u64, verdict: Verdict) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn,
            score: 64,
            verdict,
            transcript_path: None,
        }
    }

    /// The shape a real Stop hook sends: the verdict plus the file the agent
    /// is actually writing.
    #[cfg(unix)]
    fn turn_signal_for(turn: u64, verdict: Verdict, transcript: &std::path::Path) -> TurnSignal {
        TurnSignal {
            transcript_path: Some(transcript.display().to_string()),
            ..turn_signal(turn, verdict)
        }
    }

    fn ready_state(now: Instant) -> InjectionState {
        let mut state = InjectionState::new();
        state.on_turn(&turn_signal(3, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        state
    }

    const DEBOUNCE: Duration = Duration::from_secs(3);

    #[test]
    fn a_fresh_state_never_injects() {
        let now = Instant::now();
        let state = InjectionState::new();
        assert!(
            !may_inject(&state, now, DEBOUNCE),
            "no turn boundary seen yet"
        );
    }

    #[test]
    fn an_idle_user_at_a_turn_boundary_may_be_injected_into() {
        let now = Instant::now();
        assert!(may_inject(&ready_state(now), now, DEBOUNCE));
    }

    /// Issue #84, acceptance: "a swap mid-turn is refused without --force".
    /// A fresh state (no turn boundary reported yet -- the same shape a
    /// session mid-turn actually has, since it never satisfies `may_inject`'s
    /// own `signals_seen > 0` precondition) refuses a plain handover request,
    /// but `--force` overrides the refusal outright.
    #[test]
    fn a_handover_is_refused_mid_turn_without_force() {
        let now = Instant::now();
        let mid_turn = InjectionState::new();
        assert!(
            !handover_may_act(&mid_turn, now, DEBOUNCE, false),
            "mid-turn, no --force: must refuse"
        );
        assert!(
            handover_may_act(&mid_turn, now, DEBOUNCE, true),
            "--force overrides the mid-turn refusal"
        );
    }

    /// The mirror case: an idle session at a verified turn boundary needs no
    /// `--force` at all.
    #[test]
    fn a_handover_at_an_idle_turn_boundary_needs_no_force() {
        let now = Instant::now();
        assert!(handover_may_act(&ready_state(now), now, DEBOUNCE, false));
    }

    #[test]
    fn typing_after_the_turn_blocks_injection() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Input(1), now);
        assert!(
            !may_inject(&state, now, DEBOUNCE),
            "the user is mid-thought"
        );
    }

    #[test]
    fn recent_output_blocks_injection_until_the_debounce_passes() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Output(120), now);
        assert!(!may_inject(&state, now, DEBOUNCE));
        assert!(
            may_inject(&state, now + Duration::from_secs(4), DEBOUNCE),
            "quiet for longer than the debounce"
        );
    }

    // Root cause 3 (this task): `may_inject` needs `signals_seen > 0`, which
    // a signal-less adapter (codex today) can never satisfy -- its
    // `register_turn_signal` is a no-op, so `on_turn` never runs and the mail
    // advisory (T13) fell back to `MailAction::Announce` forever, exactly as
    // the pre-fix `mail_with_no_injection_window_yet_is_announced_on_stderr_
    // instead` pins for a claude session that has not yet reported its first
    // turn. `signal_less_mail_ready`/`mail_inject_ready` close it for a
    // session that will *never* report one, mirroring
    // `dash::pane::signal_less_quiescent`.

    const IDLE_QUIET: Duration = Duration::from_secs(10);

    /// A fresh `InjectionState` with both `last_output` and `last_input`
    /// pinned to `now`, rather than left at whatever instant the constructor
    /// itself happened to read. `InjectionState::new()` stamps `last_input`
    /// with its own `Instant::now()` call, a few nanoseconds after a caller's
    /// own `let now = Instant::now()` -- close enough that most assertions
    /// never notice, but exactly the kind of sub-microsecond skew that makes
    /// a `duration_since(..) >= quiet` boundary check flaky. Pinning both
    /// fields to the same `now` the test already captured removes that
    /// skew entirely.
    fn signal_less_state(now: Instant) -> InjectionState {
        let mut state = InjectionState::new();
        state.last_output = now;
        state.last_input = now;
        state
    }

    #[test]
    fn a_fresh_signal_less_state_is_not_ready_yet() {
        let now = Instant::now();
        let state = signal_less_state(now);
        assert!(
            !signal_less_mail_ready(&state, now, IDLE_QUIET),
            "no time has passed since the state was created"
        );
    }

    #[test]
    fn a_signal_less_session_becomes_ready_once_quiet_for_the_idle_window() {
        let now = Instant::now();
        let state = signal_less_state(now);
        assert!(
            !signal_less_mail_ready(&state, now, IDLE_QUIET),
            "just produced output"
        );
        assert!(
            signal_less_mail_ready(&state, now + IDLE_QUIET, IDLE_QUIET),
            "quiet for the whole idle window, with no turn signal ever needed"
        );
    }

    #[test]
    fn a_signal_less_sessions_own_keystroke_restarts_the_quiet_window() {
        let now = Instant::now();
        let mut state = signal_less_state(now);
        let quiet_at = now + IDLE_QUIET;
        assert!(signal_less_mail_ready(&state, quiet_at, IDLE_QUIET));

        // A keystroke lands right as the pane would otherwise have gone
        // ready -- the same race `dash::pane`'s own `latest_of` fold closes
        // for a dashboard pane: measuring off output alone would still read
        // this as quiet.
        state.on_event(PumpEvent::Input(1), quiet_at);
        assert!(
            !signal_less_mail_ready(&state, quiet_at, IDLE_QUIET),
            "the operator's own keystroke must restart the quiet window"
        );
        assert!(
            signal_less_mail_ready(&state, quiet_at + IDLE_QUIET, IDLE_QUIET),
            "and it clears once quiet resumes for the full window"
        );
    }

    #[test]
    fn a_degraded_signal_less_session_is_never_ready() {
        let now = Instant::now();
        let mut state = signal_less_state(now);
        state.degraded = true;
        assert!(!signal_less_mail_ready(
            &state,
            now + IDLE_QUIET,
            IDLE_QUIET
        ));
    }

    #[test]
    fn mail_inject_ready_uses_may_inject_for_a_turn_signal_capable_adapter() {
        let now = Instant::now();
        // Idle by `signal_less_mail_ready`'s own rule (long quiet, no typing)
        // but with no turn ever reported -- `may_inject` must still say no
        // for a capable adapter, since that is the precise bug this task is
        // about for the *other* direction (claude waiting on its first turn).
        let mut state = signal_less_state(now);
        let later = now + IDLE_QUIET;
        assert!(
            !mail_inject_ready(true, &state, later, DEBOUNCE, IDLE_QUIET),
            "a capable adapter must still wait for a real turn boundary"
        );

        state.on_turn(&turn_signal(1, Verdict::Healthy));
        state.last_output = later - Duration::from_secs(10);
        assert!(mail_inject_ready(true, &state, later, DEBOUNCE, IDLE_QUIET));
    }

    #[test]
    fn mail_inject_ready_uses_the_signal_less_idle_window_for_an_incapable_adapter() {
        let now = Instant::now();
        let state = signal_less_state(now);
        assert!(
            !mail_inject_ready(false, &state, now, DEBOUNCE, IDLE_QUIET),
            "just produced output"
        );
        assert!(
            mail_inject_ready(false, &state, now + IDLE_QUIET, DEBOUNCE, IDLE_QUIET),
            "quiet for the idle window, with signals_seen still at 0 forever"
        );
        assert_eq!(state.signals_seen, 0, "sanity: no signal ever arrived");
    }

    #[test]
    fn a_new_turn_clears_the_typing_flag() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Input(1), now);
        state.on_turn(&turn_signal(4, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        assert!(may_inject(&state, now, DEBOUNCE));
        assert_eq!(state.last_turn, 4);
    }

    #[test]
    fn the_cooldown_blocks_until_a_later_turn_arrives() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.cooldown_at_signal = Some(state.signals_seen);
        assert!(
            !may_inject(&state, now, DEBOUNCE),
            "same turn as the cooldown"
        );

        state.on_turn(&turn_signal(4, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        assert!(
            may_inject(&state, now, DEBOUNCE),
            "a later turn releases it"
        );
    }

    /// The turn number is per-transcript: a relaunched session counts from one
    /// again, and a compaction rewrites the transcript so the count shrinks.
    /// A cooldown armed at the dead session's turn 30 then never cleared, and
    /// advise, compact and restart all went silent for the rest of the run.
    #[test]
    fn a_cooldown_outlives_neither_a_relaunch_nor_a_compaction() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_turn(&turn_signal(30, Verdict::Restart));
        state.cooldown_at_signal = Some(state.signals_seen);
        assert!(!cooldown_cleared(&state), "armed for the signal just seen");

        // The relaunched session's very first turn: number 1, far below 30.
        state.on_turn(&turn_signal(1, Verdict::Advise));
        assert!(
            cooldown_cleared(&state),
            "a fresh session's first turn is still progress this supervisor saw"
        );
    }

    #[test]
    fn a_degraded_supervisor_never_injects() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.degraded = true;
        assert!(!may_inject(&state, now, DEBOUNCE));
    }

    #[test]
    fn the_state_records_the_latest_verdict_and_score() {
        let mut state = InjectionState::new();
        state.on_turn(&TurnSignal {
            session_id: "s".to_string(),
            turn: 9,
            score: 91,
            verdict: Verdict::Restart,
            transcript_path: None,
        });
        assert_eq!(state.verdict, Verdict::Restart);
        assert_eq!(state.score, 91);
        assert_eq!(state.last_turn, 9);
    }

    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::supervise::Watcher;

    #[test]
    fn the_ladder_maps_verdicts_to_actions() {
        let now = Instant::now();
        let mut state = ready_state(now);

        state.verdict = Verdict::Healthy;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::None);

        state.verdict = Verdict::Advise;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Advise);

        state.verdict = Verdict::Compact;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Compact);

        state.verdict = Verdict::Restart;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Restart);
    }

    #[test]
    fn an_advisory_needs_no_injection_window() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.verdict = Verdict::Advise;
        state.on_event(PumpEvent::Input(1), now);
        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::Advise,
            "advice is written to the terminal, never typed into the agent"
        );
    }

    /// Bug (confirmed): the pump loop arms `cooldown_at_signal` after
    /// advising ("advise once per turn"), but `action_for`'s `Advise` arm
    /// never consulted it, so the same advisory reprinted on every ~100ms
    /// poll tick for the rest of the turn. Two consecutive evaluations with
    /// an unchanged `Advise` verdict, cooldown armed after the first, must
    /// not both advise.
    #[test]
    fn advise_is_not_repeated_every_poll_once_the_cooldown_is_armed() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.verdict = Verdict::Advise;
        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::Advise,
            "first tick advises"
        );

        // Mirrors what the pump loop does right after advising.
        state.cooldown_at_signal = Some(state.signals_seen);

        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::None,
            "the same turn must not advise again on every poll tick"
        );
    }

    #[test]
    fn compaction_and_restart_respect_the_injection_window() {
        let now = Instant::now();
        for verdict in [Verdict::Compact, Verdict::Restart] {
            let mut state = ready_state(now);
            state.verdict = verdict;
            state.on_event(PumpEvent::Input(1), now);
            assert_eq!(
                action_for(&state, now, DEBOUNCE),
                Action::None,
                "{verdict:?} must wait for an idle user"
            );
        }
    }

    #[test]
    fn a_degraded_supervisor_still_advises_but_never_injects() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.degraded = true;
        state.verdict = Verdict::Advise;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::None);
    }

    /// The rot advisory now lives on the `announce::Event` channel
    /// (`Event::RotAdvisory`) rather than as a wrap-local `advisory_line`
    /// free function; this pins the same content guarantees the old
    /// function's own test did.
    #[test]
    fn the_advisory_line_is_one_line_and_plain() {
        let line = Event::RotAdvisory {
            score: 47,
            tokens: 138_000,
        }
        .line();
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("47"));
        assert!(line.contains("138000") || line.contains("138"));
        assert!(
            !line.contains('\u{2014}'),
            "no em dashes in user-facing copy"
        );
    }

    // T8: mail advisory in wrap's pump, now `announce::Event::MailWaiting`.

    #[test]
    fn the_mail_advisory_line_names_the_count_and_points_at_inbox() {
        let line = Event::MailWaiting { count: 3 }.line();
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains('3'));
        assert!(line.contains("zirv ctx inbox"));
        assert!(
            !line.contains('\u{2014}'),
            "no em dashes in user-facing copy"
        );

        let singular = Event::MailWaiting { count: 1 }.line();
        assert!(!singular.contains("messages"), "got {singular}");
    }

    // T13: the live mail wake-up. The pump polls the mailbox on its own
    // cadence (`MAIL_POLL`) and, at a verified-idle moment, types **one**
    // advisory line into the wrapped agent so the session itself learns that
    // mail is waiting. Bodies never travel this way, and nothing here ever
    // consumes a message: that is `zirv ctx inbox`'s job.

    /// Builds the `(path, message)` shape `mail::list` returns, with a body
    /// the advisory must never repeat.
    fn unread_message(
        file: &str,
        from_agent: &str,
        from_session: &str,
        body: &str,
    ) -> (PathBuf, crate::commands::ctx::mail::Message) {
        (
            PathBuf::from("state").join("mail").join("-repo").join(file),
            crate::commands::ctx::mail::Message {
                from_session: from_session.to_string(),
                from_agent: from_agent.to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: body.to_string(),
            },
        )
    }

    /// The count the 5 `unread_mail_*` cases below assert on. `wrap` itself
    /// reads the full listing now (it needs each message's identity to
    /// dedupe on), so the count is derived from it rather than read by a
    /// second, separate call.
    fn unread_count(
        state: &crate::commands::ctx::state::StateDir,
        repo: &std::path::Path,
        agent: &str,
        session_short: &str,
        mail_enabled: bool,
    ) -> Option<usize> {
        unread_mail_for_session(state, repo, agent, session_short, mail_enabled)
            .map(|found| found.len())
    }

    #[test]
    fn a_new_message_at_an_idle_turn_boundary_is_injected_as_one_advisory_line() {
        let watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb2222-x",
            "the webhook route moved",
        )]);

        let action = watch.decide(&facts, true);

        assert_eq!(
            action,
            MailAction::Inject {
                count: 1,
                from_agent: "codex".to_string(),
                from_short: "bbbb2222".to_string(),
                ids: vec!["0000000001-aaaa.md".to_string()],
            }
        );
    }

    #[test]
    fn an_advisory_that_cannot_be_injected_falls_back_to_the_announcement_channel() {
        let watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "body",
        )]);

        assert_eq!(
            watch.decide(&facts, false),
            MailAction::Announce {
                count: 1,
                ids: vec!["0000000001-aaaa.md".to_string()],
            },
            "a busy child must never be typed into; the operator is told instead"
        );
    }

    #[test]
    fn an_announced_advisory_is_not_repeated_on_every_poll_while_the_child_stays_busy() {
        let mut watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "body",
        )]);

        let MailAction::Announce { ids, .. } = watch.decide(&facts, false) else {
            panic!("a busy child announces first");
        };
        watch.commit_announced(&ids);

        assert_eq!(
            watch.decide(&facts, false),
            MailAction::None,
            "the same message must not re-announce on every 2s poll"
        );
    }

    /// R5: `Announcer::emit` returns `()` and swallows both a disabled
    /// channel and a failed stderr write, so committing ids as announced
    /// right after it recorded advisories nobody ever saw -- and `announced`
    /// then suppressed every later announcement of them. On a signal-less
    /// adapter `may_inject` never becomes true, so that channel is the only
    /// surface the advisory has: it was never injected and never announced.
    #[test]
    fn an_announcement_that_never_surfaced_is_retried_at_the_next_poll() {
        let mut watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "body",
        )]);

        let MailAction::Announce { ids, .. } = watch.decide(&facts, false) else {
            panic!("a busy child announces first");
        };
        // The channel was quiet, or stderr was gone: nothing surfaced.
        watch.note_announcement(&ids, false);

        assert!(
            matches!(watch.decide(&facts, false), MailAction::Announce { .. }),
            "an advisory nobody saw must be announced again, not treated as delivered"
        );
    }

    /// The other half of R5: a landed announcement must still dedupe exactly
    /// as it did before, and must still leave the injection owed.
    #[test]
    fn an_announcement_that_landed_still_suppresses_the_next_poll_and_still_owes_an_injection() {
        let mut watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "body",
        )]);

        let MailAction::Announce { ids, .. } = watch.decide(&facts, false) else {
            panic!("a busy child announces first");
        };
        watch.note_announcement(&ids, true);

        assert_eq!(
            watch.decide(&facts, false),
            MailAction::None,
            "a landed announcement is not repeated on every 2s poll"
        );
        assert!(
            matches!(watch.decide(&facts, true), MailAction::Inject { .. }),
            "announcing never discharges the injection"
        );
    }

    #[test]
    fn an_advisory_held_back_while_the_child_was_busy_is_injected_at_a_later_poll() {
        let mut watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "body",
        )]);

        let MailAction::Announce { ids, .. } = watch.decide(&facts, false) else {
            panic!("a busy child announces first");
        };
        watch.commit_announced(&ids);

        // The child goes idle: the injection is still owed, and an
        // announcement never discharges it.
        assert!(
            matches!(
                watch.decide(&facts, true),
                MailAction::Inject { count: 1, .. }
            ),
            "the injection is retried as soon as it is safe"
        );
    }

    #[test]
    fn the_same_unread_set_is_never_advised_twice() {
        let mut watch = MailWatch::default();
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "body",
        )]);

        let MailAction::Inject { ids, .. } = watch.decide(&facts, true) else {
            panic!("an idle child is injected into");
        };
        watch.commit_injected(&ids);

        assert_eq!(
            watch.decide(&facts, true),
            MailAction::None,
            "an unread message that has already been advised must stay quiet"
        );
    }

    /// The dedupe is on message identity, not on a count: consuming one
    /// message and receiving another leaves the count at 1 the whole time,
    /// which is exactly the transition a watermark over counts cannot see.
    #[test]
    fn mail_that_arrives_after_the_advised_set_was_consumed_advises_again() {
        let mut watch = MailWatch::default();
        let first = mail_facts(&[unread_message("0000000001-aaaa.md", "codex", "bbbb", "one")]);
        let MailAction::Inject { ids, .. } = watch.decide(&first, true) else {
            panic!("the first message is advised");
        };
        watch.commit_injected(&ids);

        // `zirv ctx inbox --consume` moved the first message into read/, and
        // a new one arrived: same count, different message.
        let second = mail_facts(&[unread_message(
            "0000000002-cccc.md",
            "claude",
            "dddd",
            "two",
        )]);
        watch.forget_missing(&second);

        assert!(
            matches!(
                watch.decide(&second, true),
                MailAction::Inject { count: 1, .. }
            ),
            "a genuinely new arrival must advise even when the count did not change"
        );
    }

    /// F4 (review, PR #116): `wrap`'s production path no longer writes an
    /// advisory's line and its submitting `\r` in one burst (the same
    /// paste-fold bug F1/F2 fixed for `dash::pane`'s own injections --
    /// see `MailAction::Inject`'s handling in `pump`), but the byte-shape
    /// invariant these tests pin still holds identically across the two
    /// writes: `mail_advisory_line`'s own text (phase 1, no control bytes of
    /// its own) followed by `dash::pane::write_submit_cr`'s lone `\r`
    /// (phase 2, the only control byte either write carries). Composed here
    /// exactly the way production composes them, so these tests exercise
    /// the real byte shape rather than a copy of it.
    fn deferred_mail_advisory_bytes(count: usize, from_agent: &str, from_short: &str) -> Vec<u8> {
        let mut bytes = mail_advisory_line(count, from_agent, from_short).into_bytes();
        bytes.push(b'\r');
        bytes
    }

    #[test]
    fn a_mail_advisory_never_carries_a_message_body() {
        let facts = mail_facts(&[unread_message(
            "0000000001-aaaa.md",
            "codex",
            "bbbb",
            "do-not-type-this-body",
        )]);
        assert!(
            !format!("{facts:?}").contains("do-not-type-this-body"),
            "the body is dropped at the facts seam: {facts:?}"
        );

        let bytes = deferred_mail_advisory_bytes(1, &facts[0].from_agent, &facts[0].from_short);
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(
            !text.contains("do-not-type-this-body"),
            "no body ever reaches the pty: {text:?}"
        );
        assert!(text.contains("zirv ctx inbox"), "got {text:?}");
    }

    #[test]
    fn the_injected_advisory_is_one_line_ending_in_a_single_carriage_return() {
        let bytes = deferred_mail_advisory_bytes(2, "claude", "aaaa1111");
        let text = String::from_utf8(bytes).expect("utf8");

        assert!(text.ends_with('\r'), "a TUI submits on carriage return");
        assert_eq!(
            text.matches('\r').count(),
            1,
            "exactly one submission: {text:?}"
        );
        assert!(!text.contains('\n'), "got {text:?}");
        assert!(
            !text.contains('\u{1b}'),
            "no escape sequences reach the child: {text:?}"
        );
        assert!(text.contains("[zirv \u{25b8} mail]"), "got {text:?}");
        assert!(text.contains('2'), "got {text:?}");
        assert!(text.contains("aaaa1111"), "got {text:?}");
        assert!(text.contains("zirv ctx inbox"), "got {text:?}");
        assert!(
            !text.contains('\u{2014}'),
            "no em dashes in user-facing copy: {text:?}"
        );
    }

    /// `from_agent` is whatever the *sending* session had in
    /// `ZIRV_CTX_AGENT`: untrusted, unbounded, and (`mail::header_value`)
    /// only guaranteed to be one line, not a short or control-free one.
    #[test]
    fn a_sender_name_full_of_control_bytes_cannot_break_out_of_the_advisory_line() {
        let hostile = "evil\r/exit\r\u{1b}[2Jmore";
        let bytes = deferred_mail_advisory_bytes(1, hostile, &"x".repeat(500));
        let text = String::from_utf8(bytes).expect("utf8");

        assert_eq!(
            text.matches('\r').count(),
            1,
            "an interior carriage return would submit the line early: {text:?}"
        );
        assert!(!text.contains('\u{1b}'), "got {text:?}");
        assert!(
            text.len() < 400,
            "both identity fields are capped: {} bytes",
            text.len()
        );
    }

    #[test]
    fn mail_polling_is_skipped_entirely_when_mail_is_disabled() {
        assert!(
            !mail_polling_enabled(false, "sess0000", false),
            "mail.enabled = false must mean no mailbox read at all"
        );
        assert!(mail_polling_enabled(true, "sess0000", false));
    }

    #[test]
    fn mail_polling_is_skipped_for_a_session_with_no_registered_identity() {
        assert!(
            !mail_polling_enabled(true, "", false),
            "a session with no short id cannot be anyone's addressee"
        );
    }

    /// `--no-supervise` sets `degraded` at construction and `note_failure`
    /// sets it later: either way the promise is pure passthrough, which has
    /// to include not reading the mailbox on the session's behalf.
    #[test]
    fn a_degraded_supervisor_does_not_poll_for_mail_at_all() {
        assert!(
            !mail_polling_enabled(true, "sess0000", true),
            "a degraded session is pure passthrough"
        );
    }

    #[test]
    fn the_mailbox_is_not_read_on_every_pump_tick() {
        let now = Instant::now();
        let mut watch = MailWatch::default();
        assert!(watch.due(now), "the first tick polls");

        watch.polled(now);
        assert!(
            !watch.due(now + PUMP_POLL),
            "a 100ms tick must not read the filesystem"
        );
        assert!(watch.due(now + MAIL_POLL));
    }

    /// Finding #7: `handover::take_request` is a real file read + remove,
    /// and used to run on every ~100ms pump tick for the session's entire
    /// lifetime. `handover_poll_due` is the pure gate that now stops that --
    /// mirrors `the_mailbox_is_not_read_on_every_pump_tick` above for the
    /// mail poll's own identical cadence.
    #[test]
    fn the_handover_request_file_is_not_read_on_every_pump_tick() {
        let now = Instant::now();
        assert!(
            handover_poll_due(None, now),
            "the first tick, having never polled, must poll"
        );

        let last = Some(now);
        assert!(
            !handover_poll_due(last, now + PUMP_POLL),
            "a 100ms tick must not read the handover-request file"
        );
        assert!(
            handover_poll_due(last, now + MAIL_POLL),
            "due again once a full cadence has elapsed"
        );
    }

    #[test]
    fn polling_the_mailbox_never_consumes_a_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let msg = crate::commands::ctx::mail::Message {
            from_session: "other".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: None,
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");

        let found = unread_mail_for_session(&state, &repo, "claude", "sess0000", true)
            .expect("a readable mailbox");
        let mut watch = MailWatch::default();
        let facts = mail_facts(&found);
        if let MailAction::Inject { ids, .. } = watch.decide(&facts, true) {
            watch.commit_injected(&ids);
        }

        assert_eq!(
            crate::commands::ctx::mail::list(&state, &slug, None, None)
                .expect("list")
                .len(),
            1,
            "the message stays unread for the session's own `zirv ctx inbox`"
        );
    }

    #[test]
    fn unread_mail_count_is_zero_for_a_repo_with_no_mailbox_at_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        assert_eq!(
            unread_count(&state, &repo, "claude", "sess0000", true),
            Some(0)
        );
    }

    #[test]
    fn unread_mail_count_counts_what_store_wrote() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let msg = crate::commands::ctx::mail::Message {
            from_session: "other".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: None,
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");
        assert_eq!(
            unread_count(&state, &repo, "claude", "sess0000", true),
            Some(1)
        );
    }

    /// B3: `cfg.mail.enabled = false` must gate this the same way it gates
    /// delivery -- an operator who turned mail off must never see the wrap
    /// advisory (or the bar's mail count) either.
    #[test]
    fn mail_disabled_in_config_reports_no_unread_mail_even_when_some_is_stored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let msg = crate::commands::ctx::mail::Message {
            from_session: "other".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: None,
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");

        assert_eq!(
            unread_count(&state, &repo, "claude", "sess0000", false),
            None,
            "mail.enabled = false must silence the advisory entirely"
        );
    }

    /// B3 alignment: a message addressed to a different agent by name must
    /// not count for this session, the same filter `mail::list`'s own
    /// `for_agent` already applies to delivery and `zirv ctx inbox`.
    #[test]
    fn unread_mail_count_filters_by_the_sessions_own_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let msg = crate::commands::ctx::mail::Message {
            from_session: "other".to_string(),
            from_agent: "claude".to_string(),
            to: "codex".to_string(),
            to_session: None,
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");

        assert_eq!(
            unread_count(&state, &repo, "claude", "sess0000", true),
            Some(0),
            "addressed to codex, not this claude session"
        );
        assert_eq!(
            unread_count(&state, &repo, "codex", "sess0000", true),
            Some(1)
        );
    }

    /// `mail::list` itself treats "nothing there, or not a directory" as an
    /// empty mailbox rather than an error (see its own doc), so this is the
    /// ordinary case, indistinguishable from a repo that has never had mail.
    #[test]
    fn a_missing_or_non_directory_mailbox_reads_as_empty_not_as_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        std::fs::create_dir_all(state.mail()).expect("mkdir");
        // A plain file where `mail::list` expects a directory.
        std::fs::write(state.mail().join(&slug), "not a directory").expect("write");

        assert_eq!(
            unread_count(&state, &repo, "claude", "sess0000", true),
            Some(0)
        );
    }

    /// A genuine read error -- unlike "missing" or "not a directory", which
    /// `mail::list` already treats as empty -- is what `unread_mail_count`
    /// must swallow into `None` rather than propagate: the pump's own
    /// `if let Some(count) = unread_mail_count(...) { .. }` then does nothing
    /// at all for this turn, leaving the session untouched.
    #[cfg(unix)]
    #[test]
    fn a_mail_directory_the_process_cannot_read_is_reported_as_none() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let mailbox = state.mail().join(&slug);
        std::fs::create_dir_all(&mailbox).expect("mkdir");
        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let result = unread_count(&state, &repo, "claude", "sess0000", true);

        // Restore permissions so the tempdir can be cleaned up.
        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");

        assert_eq!(
            result, None,
            "a genuine read error must never masquerade as an empty mailbox"
        );
    }

    #[test]
    fn the_injected_command_carries_focus_instructions_and_ends_with_a_carriage_return() {
        let mut sink: Vec<u8> = Vec::new();
        inject_compact(&mut sink, "/compact").expect("inject");
        let text = String::from_utf8(sink).expect("utf8");
        assert!(text.starts_with("/compact "), "got {text:?}");
        assert!(text.contains(COMPACT_FOCUS));
        assert!(
            text.ends_with('\r'),
            "a TUI submits on carriage return: {text:?}"
        );
        assert_eq!(text.matches('\r').count(), 1, "exactly one submit");
        assert!(!text.contains('\n'), "no stray newline: {text:?}");
    }

    #[test]
    fn the_focus_text_names_what_to_preserve() {
        for needle in ["task", "file", "error", "next step"] {
            assert!(
                COMPACT_FOCUS.to_lowercase().contains(needle),
                "focus text should mention {needle}: {COMPACT_FOCUS}"
            );
        }
        assert!(!COMPACT_FOCUS.contains('\u{2014}'));
    }

    #[test]
    fn verification_succeeds_when_a_compaction_event_appears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n",
        )
        .expect("write");

        let mut watcher = Watcher::new(path.clone());
        let _ = watcher.read_appended();

        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            use std::io::Write as _;
            writeln!(
                file,
                "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"x\"}}"
            )
            .expect("append");
        });

        let adapter = ClaudeAdapter::new(None);
        let verified = verify_compaction(
            &mut watcher,
            &adapter,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("verify");
        writer.join().expect("writer thread");
        assert!(verified);
    }

    #[test]
    fn verification_gives_up_at_the_deadline_instead_of_retrying() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n",
        )
        .expect("write");
        let mut watcher = Watcher::new(path);
        let adapter = ClaudeAdapter::new(None);

        let started = Instant::now();
        let verified = verify_compaction(
            &mut watcher,
            &adapter,
            Instant::now() + Duration::from_millis(300),
        )
        .expect("verify");
        assert!(!verified);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn a_compact_verdict_at_an_idle_turn_boundary_injects_into_the_tui() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("injected.log");
        let transcript = tmp.path().join("t.jsonl");
        // A transcript that scores `compact`: marker misses plus tool failures
        // at 165k tokens, which is above the ceiling but below the restart score.
        let mut text = String::new();
        for i in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n");
            let block = if i < 2 { "[zirv] ok" } else { "sloppy" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{block}\"}}],\"usage\":{{\"input_tokens\":165000}}}}}}\n"
            ));
        }
        std::fs::write(&transcript, &text).expect("write");

        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("STUB_TUI_LOG", log.display().to_string()),
                ("STUB_TUI_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                (
                    "ZIRV_CTX_STATE_DIR",
                    tmp.path().join("state").display().to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        // A turn boundary is what unlocks injection, and the hook is what
        // reports one, so drive it exactly the way the real hook does.
        let socket = read_socket_path(&StateDir::from_root(tmp.path().join("state")), None)
            .expect("wrap must publish its socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Compact),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "compacted", Duration::from_secs(15));
        assert!(seen.contains("compacted"), "got {seen:?}");

        let injected = std::fs::read_to_string(&log).expect("injection log");
        assert!(injected.contains("/compact"), "got {injected:?}");
        assert!(
            injected.contains("Preserve"),
            "focus text was sent: {injected:?}"
        );
        assert_eq!(
            injected.lines().count(),
            1,
            "cooldown prevents a second injection"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }

    // T8/T13: the mail advisory driven end to end through a real wrapped
    // session -- announced on stderr while there is no injection window, and
    // typed into the child as one labelled line once there is.

    #[cfg(unix)]
    fn store_mail_for_cwd(state_root: &std::path::Path, body: &str) {
        let repo = std::env::current_dir().expect("cwd");
        let state = crate::commands::ctx::state::StateDir::from_root(state_root.to_path_buf());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: body.to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");
    }

    /// T13: with no turn signal reported yet there is no injection window at
    /// all (`may_inject` needs `signals_seen > 0`), so the poll arm falls back
    /// to the announcement channel rather than typing into a child it cannot
    /// prove is idle -- and it keeps the injection owed for a later poll.
    #[cfg(unix)]
    #[test]
    fn mail_with_no_injection_window_yet_is_announced_on_stderr_instead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "note");

        // `wrap`'s own stderr shares the outer pty in this harness (there is
        // no separate stderr stream to redirect it to), so it arrives on
        // `h.reader` exactly like the compact/restart tests' own log output
        // does; the point under test is the wording, not the transport.
        let seen = read_until(&mut h.reader, "zirv ctx inbox", Duration::from_secs(10));
        assert!(seen.contains("zirv ctx inbox"), "got {seen:?}");
        assert!(
            !seen.contains("[zirv \u{25b8} mail]"),
            "no turn boundary has been reported, so nothing may be typed into the child: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// N4: an interactive session (`wrap`/`chat`) is named on stderr when it
    /// is nudged, and never restarted. Unlike `exec`'s headless worker there
    /// is no relaunch to fold the nudge's own message body into, so that text
    /// has nowhere to reach the pty at all; this pins that directly rather
    /// than only by absence of an injection call.
    ///
    /// T13 narrowed what this promises. A nudge rides on an ordinary mail
    /// message, so the mail poll arm may well type its own labelled advisory
    /// line into the child -- what must never travel is the *guidance body*,
    /// which is what is asserted below.
    #[cfg(unix)]
    #[test]
    fn an_interactive_session_never_receives_a_nudges_own_message_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state_root.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        // Resolved from the registry rather than passed as an empty prefix:
        // there is exactly one live session, the wrap subprocess `spawn_wrap`
        // just started, and F6 makes `zirv ctx nudge` refuse any prefix
        // shorter than four characters -- an empty one most of all.
        //
        // P5: the record's `pid` is the *agent child's* now, not the wrap
        // subprocess's own -- which is still exactly as live here (the
        // stub TUI is up and waiting), so the `Liveness::Live` filter picks
        // it out the same way.
        let repo = std::env::current_dir().expect("cwd");
        let state = super::super::state::StateDir::from_root(state_root.clone());
        let short = super::super::sessions::list(&state)
            .into_iter()
            .find(|(_, liveness)| *liveness == super::super::sessions::Liveness::Live)
            .map(|(record, _)| record.short)
            .expect("the wrap subprocess registered a live session");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_root.display().to_string(),
        )]
        .into();
        let args = crate::commands::ctx::sessions::NudgeArgs {
            prefix: short,
            message: Some("do-not-type-this-guidance".to_string()),
            message_file: None,
        };
        let mut nudge_out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::sessions::run_nudge_with(
            &args,
            &mut nudge_out,
            &repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge the live wrap session");

        let socket =
            read_socket_path(&StateDir::from_root(state_root.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "nudged by", Duration::from_secs(10));
        assert!(seen.contains("nudged by"), "got {seen:?}");
        assert!(
            !seen.contains("do-not-type-this-guidance"),
            "the nudge's own message body must never reach the wrapped agent's pty: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// T13: the whole mail contract, end to end. At a verified-idle turn
    /// boundary the session itself is told -- one labelled advisory line,
    /// typed into the child -- and the two things that must never happen
    /// still never happen: the body does not travel, and the message is not
    /// consumed.
    #[cfg(unix)]
    #[test]
    fn a_wrapped_session_is_advised_of_mail_without_its_body_and_without_consuming_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "do-not-type-this-body");

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        // The labelled marker only ever comes from the injected line: the
        // announcement channel's own fallback wording does not carry it.
        let seen = read_until(
            &mut h.reader,
            "[zirv \u{25b8} mail]",
            Duration::from_secs(15),
        );
        assert!(
            seen.contains("[zirv \u{25b8} mail]"),
            "the session itself is told at an idle turn boundary: {seen:?}"
        );
        assert!(seen.contains("zirv ctx inbox"), "advisory fired: {seen:?}");
        assert!(
            !seen.contains("do-not-type-this-body"),
            "the mail body must never be typed into the agent: {seen:?}"
        );

        // Still unread: wrap only advises. `zirv ctx inbox --consume` is
        // what moves a message into read/, and wrap never calls it.
        let repo = std::env::current_dir().expect("cwd");
        let state_dir = crate::commands::ctx::state::StateDir::from_root(state.clone());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let unread = crate::commands::ctx::mail::list(&state_dir, &slug, None, None).expect("list");
        assert_eq!(unread.len(), 1, "wrap must never consume mail on its own");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// T13: the poll arm runs every `MAIL_POLL`, so "advise once" has to hold
    /// against a clock rather than against turn boundaries. A message already
    /// advised into the session must stay quiet for every later poll and
    /// every later turn.
    #[cfg(unix)]
    #[test]
    fn a_message_already_advised_into_the_session_is_never_advised_again() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "note");

        let socket_path =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        let socket = std::path::PathBuf::from(socket_path.trim());

        crate::commands::ctx::signal::send(&socket, &turn_signal(3, Verdict::Healthy))
            .expect("send 1");
        let first = read_until(
            &mut h.reader,
            "[zirv \u{25b8} mail]",
            Duration::from_secs(15),
        );
        assert!(
            first.contains("[zirv \u{25b8} mail]"),
            "the first idle poll advises: {first:?}"
        );

        // The phase boundary, and it has to be exact rather than timed. One
        // advisory shows up on the pty *twice*: the inner pty's own echo of
        // the injected line, and then the stub's `echo: <line>` answer to it.
        // The read above returns at whichever chunk carried the marker -- on a
        // slow runner that is the echo alone -- so the answer is still in
        // flight, and phase 2's own window is where it lands. That leftover,
        // not a second advisory, is what failed this test on CI twice.
        //
        // A sync line typed right after pins the boundary deterministically:
        // the advisory's bytes are already in the child's input queue (its
        // echo is what phase 1 just matched), the stub reads that queue in
        // order, so its answer to the sync line cannot arrive before its
        // answer to the advisory. Reading up to the sync answer therefore
        // consumes every surface of the advisory, at any runner speed.
        //
        // Asserted, not discarded: a read that quietly timed out here would
        // hand phase 2 exactly the leftover this exists to remove, and the
        // failure would then be reported as a bug in the dedupe.
        h.writer.write_all(b"sync-after-advisory\r").expect("write");
        h.writer.flush().expect("flush");
        let synced = read_until(
            &mut h.reader,
            "echo: sync-after-advisory",
            Duration::from_secs(15),
        );
        assert!(
            synced.contains("echo: sync-after-advisory"),
            "the phase boundary must be reached before phase 2 reads: {synced:?}"
        );

        // A second turn boundary, no new mail in between. Long enough for
        // several `MAIL_POLL` ticks to come and go.
        crate::commands::ctx::signal::send(&socket, &turn_signal(4, Verdict::Healthy))
            .expect("send 2");
        std::thread::sleep(MAIL_POLL * 2);
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let after = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));

        assert!(
            !after.contains("[zirv \u{25b8} mail]"),
            "an already-advised message must not be advised again: {after:?}"
        );
        assert!(
            !after.contains("zirv ctx inbox"),
            "nor fall back to the announcement channel for it: {after:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn a_mail_directory_that_cannot_be_read_leaves_the_session_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let repo = std::env::current_dir().expect("cwd");
        let state_dir = crate::commands::ctx::state::StateDir::from_root(state.clone());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let mailbox = state_dir.mail().join(&slug);
        std::fs::create_dir_all(&mailbox).expect("mkdir");
        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send");

        // The session keeps going, unaffected: no advisory (nothing readable
        // to report), no crash, no degrade.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let seen = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));

        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");

        assert!(
            seen.contains("echo: still here"),
            "the session must keep working: {seen:?}"
        );
        assert!(
            !seen.contains("zirv ctx inbox"),
            "nothing readable, so no advisory: {seen:?}"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(
            !log.contains("\"action\":\"degrade\""),
            "an unreadable mailbox must not degrade the session: {log}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    #[test]
    fn wrap_has_no_transcript_until_a_signal_names_one() {
        let mut source = TranscriptSource::new(None);
        assert_eq!(
            source.path(),
            None,
            "the agent minted its own session id, so there is nothing to derive"
        );

        source.adopt(Some("/tmp/a.jsonl"));
        assert_eq!(source.path(), Some(std::path::Path::new("/tmp/a.jsonl")));

        source.adopt(None);
        source.adopt(Some(""));
        assert_eq!(
            source.path(),
            Some(std::path::Path::new("/tmp/a.jsonl")),
            "a signal with nothing to report leaves the known path alone"
        );
    }

    #[test]
    fn an_explicit_transcript_override_outranks_every_signal() {
        let mut source = TranscriptSource::new(Some(PathBuf::from("/pinned.jsonl")));
        source.adopt(Some("/tmp/a.jsonl"));
        assert_eq!(source.path(), Some(std::path::Path::new("/pinned.jsonl")));
        source.forget();
        assert_eq!(
            source.path(),
            Some(std::path::Path::new("/pinned.jsonl")),
            "a pinned path outlives the session it was pinned for"
        );
    }

    #[test]
    fn a_relaunch_forgets_the_dead_sessions_transcript() {
        let mut source = TranscriptSource::new(None);
        source.adopt(Some("/tmp/old.jsonl"));
        source.forget();
        assert_eq!(
            source.path(),
            None,
            "the killed session's file must not be watched a moment longer"
        );

        source.adopt(Some("/tmp/new.jsonl"));
        assert_eq!(source.path(), Some(std::path::Path::new("/tmp/new.jsonl")));
    }

    /// Polls the decision log, which a supervised session writes from another
    /// process, so a test never races it.
    #[cfg(unix)]
    fn wait_for_log(state: &std::path::Path, needle: &str, timeout: Duration) -> String {
        let path = state.join("logs/decisions.jsonl");
        let deadline = Instant::now() + timeout;
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains(needle) || Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The end-to-end case every other wrap test masked by pinning
    /// `ZIRV_CTX_TRANSCRIPT`: with nothing pinned, the only way wrap can know
    /// which file to verify the compaction in is the turn signal itself.
    #[cfg(unix)]
    #[test]
    fn the_transcript_is_learned_from_the_turn_signal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let log = tmp.path().join("injected.log");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n",
        )
        .expect("write");

        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("STUB_TUI_LOG", log.display().to_string()),
                ("STUB_TUI_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_INJECT_TIMEOUT_MS", "5000".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal_for(3, Verdict::Compact, &transcript),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "compacted", Duration::from_secs(15));
        assert!(seen.contains("compacted"), "got {seen:?}");

        // Not the generic "verb":"wrap" needle: the prompt-injection log entry
        // written at session start also carries that verb, and would satisfy
        // the wait before the compaction outcome this test cares about is
        // ever appended.
        let decisions = wait_for_log(&state, "\"action\":\"inject\"", Duration::from_secs(15));
        assert!(
            decisions.contains("\"action\":\"inject\""),
            "the compaction was verified in the reported transcript: {decisions}"
        );
        assert!(
            !decisions.contains("\"action\":\"degrade\""),
            "a verified injection must not degrade the session: {decisions}"
        );
        assert!(
            decisions.contains(&transcript.display().to_string()),
            "the log names the file the signal reported: {decisions}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        wait_or_kill(&mut h.child, Duration::from_secs(5));
    }

    /// Same bug class as the exec one fixed in d3f0ede: after a relaunch the
    /// old session's transcript is dead, and the new session reports its own.
    #[cfg(unix)]
    #[test]
    fn a_restart_reads_the_reported_transcript_and_then_follows_the_new_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");

        let first = tmp.path().join("first.jsonl");
        std::fs::write(
            &first,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        // Already compacted: the relaunch stub never compacts on its own, so a
        // verified compaction can only mean wrap read this file and not the
        // one the dead session reported.
        let second = tmp.path().join("second.jsonl");
        std::fs::write(
            &second,
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"x\"}\n",
        )
        .expect("write");

        let script = fixture("relaunch-stub.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_INJECT_TIMEOUT_MS", "5000".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                ("ZIRV_CTX_AGENT_BIN", format!("sh {script}")),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket_path =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        let socket = std::path::PathBuf::from(socket_path.trim());
        crate::commands::ctx::signal::send(&socket, &turn_signal_for(5, Verdict::Restart, &first))
            .expect("send");

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(20));
        assert!(seen.contains("stub-tui ready"), "relaunched: {seen:?}");

        let handoffs = walk_md(&state.join("handoffs"));
        assert_eq!(handoffs.len(), 1, "one handoff per restart: {handoffs:?}");
        let note = std::fs::read_to_string(&handoffs[0]).expect("handoff");
        assert!(
            note.contains("wire the webhook"),
            "the handoff was distilled from the reported transcript, not from an empty read: {note}"
        );

        crate::commands::ctx::signal::send(&socket, &turn_signal_for(6, Verdict::Compact, &second))
            .expect("send");

        let decisions = wait_for_log(&state, "\"verdict\":\"compact\"", Duration::from_secs(20));
        let compaction = decisions
            .lines()
            .find(|line| line.contains("\"verdict\":\"compact\""))
            .unwrap_or_else(|| panic!("no compaction decision logged: {decisions}"));
        assert!(
            compaction.contains(&second.display().to_string()),
            "the new session's transcript is the one watched: {compaction}"
        );
        assert!(
            !compaction.contains(&first.display().to_string()),
            "the dead session's transcript must have been forgotten: {compaction}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        wait_or_kill(&mut h.child, Duration::from_secs(5));
    }

    use crate::commands::ctx::handoff::Handoff;

    #[test]
    fn the_relaunch_command_keeps_the_flags_the_user_wrapped() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let handoff = Handoff {
            task: "Wire the webhook".to_string(),
            next_step: "Write the failing test".to_string(),
            ..Handoff::default()
        };
        let command = relaunch_command(
            &adapter,
            &handoff,
            &["--model".to_string(), "opus".to_string()],
        );
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        assert!(args[0].contains("Wire the webhook"), "got {args:?}");
        assert_eq!(
            &args[1..],
            &["--model".to_string(), "opus".to_string()],
            "`wrap -- claude --model opus` must not restart a bare claude"
        );
    }

    /// Issue #84 acceptance: "cross-harness swap works in both directions...
    /// including the case where the successor has no system-prompt injection
    /// (codex) and must receive the packet on the task-prompt fallback
    /// channel." `perform_handover_swap` relaunches through this exact
    /// function (`relaunch_command`), generic over whichever `&dyn
    /// AgentAdapter` the operator asked to swap to, with no adapter-specific
    /// branching of its own. Both directions are exercised: codex receiving
    /// a handoff distilled while claude was the seat, and claude receiving
    /// one distilled while codex was. Codex's own `interactive_cmd` puts the
    /// initial prompt positionally (`adapters/codex.rs::interactive_cmd`),
    /// not behind any `-c developer_instructions=...` system-prompt flag --
    /// the same positional channel claude's own restart already uses, so
    /// this is not special-cased anywhere, only relied on.
    #[test]
    fn a_handover_carries_the_packet_on_the_task_prompt_channel_in_both_directions() {
        use crate::commands::ctx::adapters::codex::CodexAdapter;

        let handoff = Handoff {
            task: "Wire the webhook".to_string(),
            next_step: "Write the failing test".to_string(),
            ..Handoff::default()
        };

        // claude -> codex
        let codex = CodexAdapter::new(None);
        let to_codex = relaunch_command(&codex, &handoff, &[]);
        let codex_args: Vec<String> = to_codex
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            codex_args.iter().any(|a| a.contains("Wire the webhook")),
            "codex must receive the packet positionally: {codex_args:?}"
        );
        assert!(
            !codex_args
                .iter()
                .any(|a| a.contains("developer_instructions")),
            "the restart channel is the task prompt, never codex's system-prompt \
             flag: {codex_args:?}"
        );

        // codex -> claude
        let claude = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let to_claude = relaunch_command(&claude, &handoff, &[]);
        let claude_args: Vec<String> = to_claude
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            claude_args.iter().any(|a| a.contains("Wire the webhook")),
            "claude must receive the packet positionally too: {claude_args:?}"
        );
    }

    #[test]
    fn the_restart_prompt_carries_the_handoff() {
        let handoff = Handoff {
            task: "Wire the webhook".to_string(),
            next_step: "Write the failing test".to_string(),
            ..Handoff::default()
        };
        let prompt = restart_prompt(&handoff);
        assert!(prompt.contains("Wire the webhook"));
        assert!(prompt.contains("Write the failing test"));
        assert!(prompt.to_lowercase().contains("previous session"));
        assert!(!prompt.contains('\u{2014}'));
    }

    /// The compiler seam used by `run_with` must carry the bounded memory core.
    #[test]
    fn compose_launch_prompt_carries_the_memory_layer_under_its_configured_cap() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(repo.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.core_max_bytes = 40;
        let slug = crate::commands::ctx::state::repo_slug(repo.path());

        crate::commands::ctx::memory::remember(
            &state,
            &slug,
            &crate::commands::ctx::memory::Entry {
                key: "seam-fact".to_string(),
                written_by: "test".to_string(),
                written: 1,
                verified: 1,
                source: "explicit".to_string(),
                body: format!("{}TAIL_MARKER_NOT_TRUNCATED", "z".repeat(200)),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let composed = crate::commands::ctx::compile::compile(
            Some(&home),
            repo.path(),
            false,
            &cfg,
            &adapter,
            PromptRole::Orchestrator,
            &state,
            1,
        )
        .composed
        .expect("a launch still composes a prompt");

        assert!(
            composed.text.contains("seam-fact"),
            "the memory core layer must reach the composed prompt: {}",
            composed.text
        );
        assert!(
            !composed.text.contains("TAIL_MARKER_NOT_TRUNCATED"),
            "a tiny core_max_bytes must actually bound the delivered memory layer: {}",
            composed.text
        );
        assert!(
            composed.text.contains("[memory truncated:"),
            "the truncation must be visible, not silent: {}",
            composed.text
        );
    }

    #[cfg(unix)]
    #[test]
    fn quit_child_sends_the_sequence_then_escalates() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // A child that ignores everything typed at it.
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg("trap '' TERM; while true; do sleep 1; done");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut sink = pair.master.take_writer().expect("writer");
        // Nobody reads the master's output on this side of the harness, and an
        // undrained session-leader pty can stall the child's own exit teardown,
        // so a discarding reader thread keeps that path clear the same way the
        // real wrap pump does.
        let mut reader = pair.master.try_clone_reader().expect("reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });

        let started = Instant::now();
        quit_child(&mut sink, &mut child, "/exit\r", Duration::from_millis(200)).expect("quit");
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "child is gone"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn quit_child_returns_immediately_for_a_cooperative_child() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg(&script);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut sink = pair.master.take_writer().expect("writer");
        // See the comment in quit_child_sends_the_sequence_then_escalates: an
        // undrained master stalls both echoed input and the child's own exit.
        let mut reader = pair.master.try_clone_reader().expect("reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });

        std::thread::sleep(Duration::from_millis(200));
        quit_child(&mut sink, &mut child, "/exit\r", Duration::from_secs(5)).expect("quit");
        assert!(child.try_wait().expect("try_wait").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_restart_verdict_writes_a_handoff_and_relaunches_within_the_same_wrap_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        // relaunch-stub.sh, not stub-tui.sh: on this platform, stub-tui.sh's
        // /compact branch (long JSON literals in an unreached case arm)
        // reproducibly makes the process spawned on the relaunch's fresh pty
        // exit immediately, even with its bracket-tests de-fragilized (see
        // batch10-report.md for the full bisection). This test never sends
        // /compact, so a minimal stub sidesteps the quirk while exercising
        // the identical wrap code path (greet, echo, exit on quit sequence).
        let script = fixture("relaunch-stub.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                // Relaunch runs the stub again instead of a real agent.
                ("ZIRV_CTX_AGENT_BIN", format!("sh {script}")),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(5, Verdict::Restart),
        )
        .expect("send");

        // A fresh agent greets again through the same outer terminal
        // (h.reader/h.writer never change: the wrap session survives, only
        // the inner pty is replaced). The old agent's own output, including
        // its quit confirmation, is deliberately not forwarded once a
        // restart is underway (see spawn_output_thread) so it can never
        // interleave with the new generation's output on the same stdout;
        // that the old child actually quit is verified via the decision
        // log below instead of by watching for its suppressed text.
        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(20));
        assert!(seen.contains("stub-tui ready"), "relaunched: {seen:?}");

        let log = wait_for_log(&state, "\"action\":\"restart\"", Duration::from_secs(10));
        assert!(
            log.contains("\"action\":\"restart\""),
            "old child quit and relaunch succeeded: {log}"
        );

        let handoffs: Vec<_> = walk_md(&state.join("handoffs"));
        assert_eq!(handoffs.len(), 1, "one handoff per restart: {handoffs:?}");
        let note = std::fs::read_to_string(&handoffs[0]).expect("handoff");
        assert!(note.contains("wire the webhook"), "structural task: {note}");

        // The wrap session itself kept running: the user can still type into
        // the new agent through the very same outer pty used before the restart.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));
        assert!(echoed.contains("echo: still here"), "got {echoed:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        // Plain cleanup, not an assertion: everything the restart contract
        // promises was already checked above (log, handoff, echo through the
        // same outer pty). On this platform a wrap process that has been
        // through a relaunch can independently get stuck in the kernel's own
        // exit-teardown path for a session-leader pty process (`ps` reports
        // it with the documented "E" = "trying to exit" state flag; see
        // batch10-report.md). That is orthogonal to whether the restart
        // itself worked, so bound this wait rather than let an unrelated
        // platform quirk hang the test.
        wait_or_kill(&mut h.child, Duration::from_secs(5));
    }

    /// Waits for a child to exit, killing it if it has not within `timeout`.
    /// See the restart test's cleanup for why a plain `.wait()` is not safe
    /// to use unconditionally on this platform.
    #[cfg(unix)]
    fn wait_or_kill(child: &mut Box<dyn portable_pty::Child + Send + Sync>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
    }

    #[cfg(unix)]
    #[test]
    fn a_relaunch_that_cannot_spawn_degrades_the_session_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        // The initial spawn runs the wrapped argv directly (unaffected by
        // ZIRV_CTX_AGENT_BIN), so it starts fine on relaunch-stub.sh. relaunch()
        // instead goes through the adapter's interactive_cmd, which honors
        // ZIRV_CTX_AGENT_BIN: pointing it at a path that cannot exist makes
        // relaunch()'s own spawn_command fail, exactly like a real agent
        // binary going missing between sessions.
        let script = fixture("relaunch-stub.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                (
                    "ZIRV_CTX_AGENT_BIN",
                    "/nonexistent/zirv-ctx-test-agent-binary".to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(5, Verdict::Restart),
        )
        .expect("send");

        // relaunch() cannot spawn, so the session ends: no hang, no crash,
        // and the process exits through the old (already-quit) child's own
        // exit path rather than the fresh-generation one.
        wait_or_kill(&mut h.child, Duration::from_secs(10));

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"degrade\""),
            "note_failure logged: {log}"
        );
        // "Item 6 audit" (see the comment at `relaunch_error`'s
        // declaration) made `note_failure` name the *real* spawn error
        // instead of the old generic "relaunch failed" placeholder, which
        // that fallback string is now dead code for -- `relaunch_error` is
        // always `Some` by the time this arm runs. The nonexistent
        // `ZIRV_CTX_AGENT_BIN` path is what's actually stable across
        // portable-pty's own wording for "the binary is missing".
        assert!(
            log.contains("zirv-ctx-test-agent-binary"),
            "reason recorded: {log}"
        );
        assert!(
            log.contains("\"action\":\"restart-failed\""),
            "restart outcome logged: {log}"
        );
    }

    #[test]
    fn noting_a_failure_degrades_the_supervisor_once_and_for_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        let mut supervision = InjectionState::new();

        note_failure(
            &mut supervision,
            Some((&state, "sess")),
            "socket died",
            &Announcer::silent(),
        );
        assert!(supervision.degraded);

        // Even a fresh turn signal cannot re-enable injection.
        supervision.on_turn(&turn_signal(9, Verdict::Restart));
        supervision.last_output = Instant::now() - Duration::from_secs(30);
        assert_eq!(
            action_for(&supervision, Instant::now(), DEBOUNCE),
            Action::None
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(log.contains("degrade"), "got {log}");
        assert!(log.contains("socket died"), "record the reason: {log}");
    }

    #[test]
    fn note_failure_without_a_state_dir_still_degrades() {
        let mut supervision = InjectionState::new();
        note_failure(&mut supervision, None, "no state dir", &Announcer::silent());
        assert!(supervision.degraded);
    }

    #[cfg(unix)]
    #[test]
    fn an_unbindable_socket_leaves_a_fully_transparent_wrapper() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("s".repeat(120));
        let script = fixture("stub-tui.sh").display().to_string();

        let mut h = spawn_wrap(
            &[("ZIRV_CTX_STATE_DIR", long_state.display().to_string())],
            &["sh", &script],
        );
        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"), "got {seen:?}");

        h.writer.write_all(b"hello\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: hello", Duration::from_secs(10));
        assert!(
            echoed.contains("echo: hello"),
            "passthrough intact: {echoed:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Waits for `child`, killing it and returning `None` past `timeout` so
    /// a genuinely wedged child fails this one test fast instead of hanging
    /// it (and the suite behind it) forever. `h.child.wait()`'s own contract
    /// has no timeout at all -- fine for a child known to exit promptly, but
    /// `a_broken_transcript_path_never_stops_the_session` hung on it for real
    /// once the `DASH_REQUESTS_ENV` scrub above stopped short-circuiting the
    /// nesting guard before this child's own exit path was ever reached
    /// (reproduced twice, ~20+ min combined, before this fix). Mirrors
    /// `win::wait_bounded`'s identical shape for `std::process::Child`.
    #[cfg(unix)]
    fn wait_bounded(
        child: &mut dyn portable_pty::Child,
        timeout: Duration,
    ) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        let _ = child.kill();
        // The kill is not synchronous, so reap with the same bounded poll
        // rather than a blocking `wait()` -- a `wait()` here would hang this
        // helper forever on a child the kill somehow failed to actually
        // terminate, defeating the entire point of bounding it above. A
        // child left unreaped in a test process that is about to exit is
        // harmless (the OS reaps it); a wedged suite is not.
        let reap_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < reap_deadline {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_transcript_path_never_stops_the_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                (
                    "ZIRV_CTX_TRANSCRIPT",
                    "/nonexistent/dir/t.jsonl".to_string(),
                ),
                ("ZIRV_CTX_DEBOUNCE_MS", "200".to_string()),
                (
                    "ZIRV_CTX_STATE_DIR",
                    tmp.path().join("state").display().to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(tmp.path().join("state")), None).expect("path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(2, Verdict::Compact),
        )
        .expect("send");

        // The injection is attempted and cannot be verified, so wrap degrades
        // while the session continues.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: still here", Duration::from_secs(20));
        assert!(echoed.contains("echo: still here"), "got {echoed:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let status = wait_bounded(h.child.as_mut(), Duration::from_secs(10))
            .expect("the session must exit within a bounded window, not hang");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn no_supervise_skips_supervision_entirely() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(zirv_bin());
        cmd.arg("ctx");
        cmd.arg("wrap");
        cmd.arg("--agent");
        cmd.arg("claude");
        cmd.arg("--no-supervise");
        cmd.arg("--");
        cmd.arg("sh");
        cmd.arg(&script);
        cmd.env("TERM", "xterm");
        cmd.env(
            "ZIRV_CTX_STATE_DIR",
            tmp.path().join("state").display().to_string(),
        );
        // T8: hermetic against the developer's/agent's own environment --
        // see the identical comment on `spawn_wrap`'s pty harness above.
        // This test builds its own `CommandBuilder` rather than going
        // through that harness, so it needs the same scrub explicitly (see
        // `testenv::scrub_supervision_env_for_test`'s own doc comment).
        crate::commands::ctx::testenv::scrub_supervision_env_for_test(&mut cmd);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");

        let seen = read_until(&mut reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"));
        assert_eq!(
            read_socket_path(&StateDir::from_root(tmp.path().join("state")), None),
            None,
            "no socket is bound when supervision is off"
        );

        writer.write_all(b"/exit\r").expect("write");
        let status = child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// A restart is an escape from the conversation that rotted, so nothing
    /// that pins the launch back to it may survive into the relaunched argv.
    /// Before this, `wrap -- claude --continue` relaunched as `claude
    /// "<handoff>" --continue` and resumed the very session it was leaving.
    mod restart_flags {
        use super::*;
        use crate::commands::ctx::adapters;

        fn flags_for(argv: &[&str]) -> Vec<String> {
            let command: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
            let adapter = adapters::select(Some("claude"), &command, &CtxConfig::default())
                .expect("claude adapter");
            restart_launch_flags(adapter.as_ref(), &command)
        }

        #[test]
        fn a_relaunch_drops_every_flag_that_would_resume_the_rotted_session() {
            assert!(flags_for(&["claude", "--continue"]).is_empty());
            assert!(flags_for(&["claude", "-c"]).is_empty());
            assert!(flags_for(&["claude", "--fork-session"]).is_empty());
            assert!(flags_for(&["claude", "--resume", "abc123"]).is_empty());
            assert!(flags_for(&["claude", "--session-id", "abc123"]).is_empty());
        }

        /// The CLIs accept `--resume=abc` too, so stripping only the two-token
        /// spelling would leave the other behind.
        #[test]
        fn the_joined_spelling_of_a_resume_flag_is_dropped_as_well() {
            assert!(flags_for(&["claude", "--resume=abc123"]).is_empty());
            assert!(flags_for(&["claude", "--session-id=abc123"]).is_empty());
        }

        /// Everything else the operator passed has to reach the restarted
        /// child exactly as it reached the first one.
        #[test]
        fn a_relaunch_keeps_the_operator_flags_that_are_not_about_resuming() {
            assert_eq!(
                flags_for(&["claude", "--model", "opus", "--continue"]),
                vec!["--model".to_string(), "opus".to_string()]
            );
            assert_eq!(
                flags_for(&["claude", "--dangerously-skip-permissions"]),
                vec!["--dangerously-skip-permissions".to_string()]
            );
        }

        /// `relaunch_command` supplies the handoff positionally, so a
        /// positional prompt from the original argv must not come back too --
        /// the agent would read it as a second prompt.
        #[test]
        fn a_positional_prompt_is_not_replayed_into_the_relaunch() {
            assert!(flags_for(&["claude", "fix the parser"]).is_empty());
            assert_eq!(
                flags_for(&["claude", "fix the parser", "--model", "opus"]),
                vec!["--model".to_string(), "opus".to_string()]
            );
        }
    }

    /// The `ESC[6n` story: see `CURSOR_POSITION_REPORT`.
    mod cursor_position_report {
        use super::*;

        #[test]
        fn a_cursor_position_report_is_recognised_wherever_it_sits_in_a_chunk() {
            assert_eq!(find_cursor_position_report(b"\x1b[1;1R"), Some(0..6));
            assert_eq!(find_cursor_position_report(b"ab\x1b[24;80Rcd"), Some(2..10));
        }

        /// Every other escape sequence is a key the user pressed and has to
        /// reach the agent untouched.
        #[test]
        fn ordinary_keys_are_never_mistaken_for_a_cursor_report() {
            assert_eq!(find_cursor_position_report(b"\x1b[A"), None, "up arrow");
            assert_eq!(find_cursor_position_report(b"\x1b[3~"), None, "delete");
            assert_eq!(find_cursor_position_report(b"\x1b"), None, "bare escape");
            assert_eq!(find_cursor_position_report(b"\x1b[R"), None, "no row");
            assert_eq!(
                find_cursor_position_report(b"\x1b[12R"),
                None,
                "a report has two parameters"
            );
            assert_eq!(find_cursor_position_report(b"hello"), None);
        }

        /// An unarmed filter is a pure passthrough, which is the whole of its
        /// behavior on unix: no pty there sends the probe.
        #[test]
        fn an_unarmed_filter_forwards_everything() {
            let mut filter = CprFilter::default();
            let out = filter.filter(b"\x1b[1;1R", Instant::now());
            assert_eq!(out.as_ref(), b"\x1b[1;1R");
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)), "no copy");
        }

        #[cfg(not(windows))]
        #[test]
        fn arming_is_inert_off_windows() {
            let mut filter = CprFilter::default();
            filter.arm(Instant::now());
            assert_eq!(
                filter.filter(b"\x1b[1;1R", Instant::now()).as_ref(),
                b"\x1b[1;1R"
            );
        }

        #[cfg(windows)]
        #[test]
        fn an_armed_filter_swallows_exactly_one_report_and_keeps_the_rest() {
            let mut filter = CprFilter::default();
            filter.arm(Instant::now());

            // The terminal's answer to the console host's probe, with a real
            // keystroke riding along in the same read.
            let out = filter.filter(b"\x1b[24;1Rq", Instant::now());
            assert_eq!(out.as_ref(), b"q", "only the report is removed");

            // The next one is the agent's own business.
            assert_eq!(
                filter.filter(b"\x1b[2;3R", Instant::now()).as_ref(),
                b"\x1b[2;3R"
            );
        }

        #[cfg(windows)]
        #[test]
        fn an_armed_filter_stops_filtering_once_its_window_has_passed() {
            let mut filter = CprFilter::default();
            filter.arm(Instant::now() - CPR_FILTER_WINDOW - Duration::from_secs(1));
            assert_eq!(
                filter.filter(b"\x1b[1;1R", Instant::now()).as_ref(),
                b"\x1b[1;1R",
                "a late report belongs to the agent"
            );
        }
    }

    /// Windows coverage for the pty deadlock. Every one of these bounds its own
    /// wait: a regression here used to hang forever, and a hanging test in CI
    /// is indistinguishable from a slow one.
    #[cfg(windows)]
    mod win {
        use super::*;

        /// Waits for `child`, killing it and returning `None` past `timeout` so
        /// a re-deadlocked `wrap` fails the test instead of wedging the suite.
        /// Mirrors the non-Windows `wait_bounded` above, reap loop included.
        fn wait_bounded(
            child: &mut std::process::Child,
            timeout: Duration,
        ) -> Option<std::process::ExitStatus> {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(status)) => return Some(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => return None,
                }
            }
            let _ = child.kill();
            // The kill is not synchronous, so reap with the same bounded poll
            // rather than a blocking `wait()` -- see the identical comment on
            // the non-Windows `wait_bounded` for why an unbounded wait here
            // would defeat this helper's entire purpose.
            let reap_deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < reap_deadline {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            None
        }

        fn spawn_wrap(
            state: &std::path::Path,
            flags: &[&str],
            wrapped: &[&str],
        ) -> std::process::Child {
            let mut cmd = std::process::Command::new(zirv_bin());
            cmd.arg("ctx").arg("wrap");
            cmd.args(flags);
            cmd.arg("--");
            cmd.args(wrapped);
            cmd.env(crate::commands::ctx::state::STATE_ENV, state);
            // Hermetic against the *developer's* own environment (F2): these
            // tests spawn the real `zirv` binary, which reads the process
            // environment, so a suite run from inside an agent session would
            // otherwise trip the nesting guard and see exit code 1 instead of
            // whatever the test is actually about. The guard has its own
            // dedicated coverage above; here it is noise. T8: see the
            // identical fix/comment on `spawn_wrap`'s pty harness above, and
            // `testenv::scrub_supervision_env_for_test_cmd`'s own doc comment
            // for why this is a test-side scrub, not an extended production one.
            crate::commands::ctx::testenv::scrub_supervision_env_for_test_cmd(&mut cmd);
            // No terminal: this is also the CI/piped case, which is exactly
            // why the synthetic cursor report cannot be left to a real
            // terminal to send.
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            cmd.spawn().expect("spawn zirv ctx wrap")
        }

        /// The regression that shipped: portable-pty's pseudoconsole asks for a
        /// cursor position report and blocks until it gets one, so *every*
        /// wrapped command hung -- this one does nothing but exit.
        #[test]
        fn a_wrapped_command_that_exits_immediately_does_not_hang_the_wrapper() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut child =
                spawn_wrap(tmp.path(), &["--no-supervise"], &["cmd", "/c", "exit", "0"]);
            let status = wait_bounded(&mut child, Duration::from_secs(30))
                .expect("wrap must exit, not deadlock on the console host's cursor probe");
            assert_eq!(status.code(), Some(0), "the child's own exit code");
        }

        /// The wrapped command's exit code is the wrapper's, so a
        /// pseudoconsole that never ran the child cannot masquerade as success.
        #[test]
        fn a_wrapped_command_reports_its_own_failing_exit_code() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut child =
                spawn_wrap(tmp.path(), &["--no-supervise"], &["cmd", "/c", "exit", "3"]);
            let status = wait_bounded(&mut child, Duration::from_secs(30))
                .expect("wrap must exit rather than deadlock");
            assert_eq!(status.code(), Some(3));
        }

        /// Supervision used to be off for the entire run on Windows: the turn
        /// signal had no transport, so `bind` failed and `wrap` degraded before
        /// the agent had even started. The named pipe is what fixes that, and a
        /// bound server leaves the same directory entry unix does.
        #[test]
        fn a_supervised_wrap_binds_a_turn_signal_transport() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = tmp.path().join("state");
            let mut child = spawn_wrap(
                &state,
                &["--agent", "claude"],
                // Long enough to still be running when the assertion below
                // looks for the socket entry, short enough to reap itself if
                // the kill somehow misses.
                &["cmd", "/c", "ping -n 20 127.0.0.1"],
            );

            let deadline = Instant::now() + Duration::from_secs(30);
            let mut sockets = Vec::new();
            while Instant::now() < deadline && sockets.is_empty() {
                sockets = std::fs::read_dir(state.join("s"))
                    .map(|entries| entries.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                if sockets.is_empty() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = child.kill();
            let _ = child.wait();

            assert_eq!(
                sockets.len(),
                1,
                "a supervised wrap publishes exactly one turn-signal endpoint"
            );
            let published = std::fs::read_to_string(&sockets[0]).expect("read");
            assert!(
                published.starts_with(r"\\.\pipe\zirv-ctx-"),
                "the endpoint is a named pipe: {published}"
            );
        }
    }

    #[cfg(unix)]
    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }
}
