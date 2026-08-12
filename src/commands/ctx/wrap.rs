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
fn answer_inherit_cursor_probe(writer: &mut (dyn Write + Send)) {
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
        Self {
            last_turn: 0,
            signals_seen: 0,
            verdict: Verdict::Healthy,
            score: 0,
            user_typed_since_turn: false,
            last_output: Instant::now(),
            cooldown_at_signal: None,
            degraded: false,
        }
    }

    pub fn on_event(&mut self, event: PumpEvent, now: Instant) {
        match event {
            PumpEvent::Output(_) => self.last_output = now,
            PumpEvent::Input(_) => self.user_typed_since_turn = true,
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

/// Sent as arguments to the adapter's compaction command. PreCompact hooks
/// cannot add instructions to a compaction, so this is the only channel for them.
pub const COMPACT_FOCUS: &str = "Preserve the current task and its acceptance criteria, the file paths touched so far, any unresolved errors or failing tests, and the exact next step. Drop resolved tangents and full file dumps.";

pub const TRANSCRIPT_ENV: &str = "ZIRV_CTX_TRANSCRIPT";
pub const SOCKET_PATH_FILE: &str = "socket-path";

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

/// Whether the mailbox has grown since the last check: the only thing that
/// should ever trigger a fresh advisory. Equal or shrinking (a message was
/// consumed elsewhere, or the read failed and fell back to the last known
/// count) never re-fires one.
pub fn mail_grew(previous: usize, current: usize) -> bool {
    current > previous
}

/// Unread mail for `repo`, filtered to what this session's own harness would
/// see (`mail::list`'s `for_agent`, the same filter delivery and `zirv ctx
/// inbox` already use -- a message addressed to a different agent by name
/// must not count here either). `None` when mail is disabled outright
/// (`cfg.mail.enabled = false`, honored exactly like delivery does: an
/// operator who turned mail off must never be told mail is waiting) or on any
/// read error. Called from two throttled call sites -- the turn-signal arm,
/// bounded by how often the agent reports a turn boundary, and (T12b) the
/// bar's own 1s redraw throttle, never the raw byte-pump path -- so an
/// unreadable mail directory (permissions, a stray non-directory file at that
/// path) is silently ignored rather than degrading or interrupting the
/// session: mail is advisory, and a wrapped session must never be made worse
/// by it.
fn unread_mail_count(
    state: &super::state::StateDir,
    repo: &Path,
    agent: &str,
    mail_enabled: bool,
) -> Option<usize> {
    if !mail_enabled {
        return None;
    }
    super::mail::list(state, &super::state::repo_slug(repo), Some(agent))
        .ok()
        .map(|found| found.len())
}

pub fn inject_compact(sink: &mut dyn Write, compact_command: &str) -> CtxResult<()> {
    // A TUI submits on carriage return, not newline.
    write!(sink, "{compact_command} {COMPACT_FOCUS}\r")?;
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

    // Ctrl-C twice is the conventional escape hatch before force.
    let _ = write!(sink, "\x03\x03");
    let _ = sink.flush();
    if wait_for_exit(child, Instant::now() + grace)? {
        return Ok(());
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
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
    let mut builder = CommandBuilder::new(command.get_program());
    for arg in command.get_args() {
        builder.arg(arg);
    }
    builder.cwd(repo);
    // Without this the fresh session has no socket to report turn boundaries
    // on, and supervision would silently end at the first restart.
    for (key, value) in turn_env {
        builder.env(key, value);
    }

    // Before the spawn, and before anything else touches this pty: on Windows
    // the console host will not service the child at all until it is answered.
    // A restart opens a fresh pseudoconsole, so it re-deadlocks without this.
    let mut writer = pair.master.take_writer()?;
    answer_inherit_cursor_probe(&mut *writer);

    let child = pair.slave.spawn_command(builder)?;

    let reader = pair.master.try_clone_reader()?;

    Ok((pair, child, reader, writer))
}

/// `role` is a caller-supplied parameter rather than a `WrapArgs` field: it is
/// not something a user ever types on the `wrap` command line, only something
/// another verb (`zirv ctx chat`) decides on the caller's behalf. Every
/// existing `wrap` caller (the `wrap` verb itself, and every relaunch inside
/// `pump`) passes `PromptRole::Worker`; `chat` is the one caller that passes
/// `PromptRole::Orchestrator`.
/// `session` lets a caller that already generated a session id for its own
/// purposes (`chat.rs`'s launch banner, printed before this function is ever
/// called) hand it in rather than have two different ids exist for the same
/// launch. `None` (every caller but `chat`) keeps today's behavior: a fresh
/// id minted here.
pub fn run_with<W: Write>(
    args: &WrapArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    role: PromptRole,
    session: Option<super::event::SessionId>,
) -> CtxResult<i32> {
    if args.command.is_empty() {
        return Err("no command to wrap; pass it after --".into());
    }

    let cfg = CtxConfig::load(repo, env)?;
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
    let adapter = adapters::select(agent_name, &args.command, &cfg)?;

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
    let composed = super::prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        skip_injection,
        &cfg.prompt,
        role,
    );
    // The wrapped command's own argv may already carry the adapter's
    // system-prompt flag; merge it in rather than letting `prompt_args` below
    // silently override it with a second occurrence.
    let (launch_command, composed) =
        super::prompt::merge_command_line_prompt(adapter.as_ref(), &args.command, composed, None);
    let prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        &launch_command,
        composed.as_ref(),
        &state_dir,
        session.as_str(),
    );
    super::prompt::log_injection(
        &state_dir,
        "wrap",
        session.as_str(),
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    ));
    // Stripping the user's own --append-system-prompt (see
    // merge_command_line_prompt) can empty the argv even though args.command
    // itself was not empty at the top of this function, e.g. `wrap -- --
    // append-system-prompt foo` with nothing else. That must be an error, not
    // a panic on a hot path where release is panic = "abort".
    let (program, rest) = launch_command
        .split_first()
        .ok_or("no command to wrap; pass it after --")?;

    let mut supervision = InjectionState::new();
    supervision.degraded = args.no_supervise;

    let server = if args.no_supervise {
        None
    } else {
        match super::signal::SignalServer::bind(&state_dir.socket_for(session.as_str())) {
            Ok(server) => {
                // Publish the path so `zirv ctx status` and tests can find it.
                let _ = super::state::create_private_dir_all(state_dir.root());
                let _ = super::state::write_private(
                    &state_dir.root().join(SOCKET_PATH_FILE),
                    &server.path().display().to_string(),
                );
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

    let mut command = CommandBuilder::new(program);
    for arg in rest {
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
    for (key, value) in &turn_env {
        command.env(key, value);
    }

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
        cfg.mail.enabled,
        stdout_lock.clone(),
        (cols, rows),
    );
    if bar.chrome.bar {
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
    let relaunch_extra: Vec<String> = restart_launch_flags(adapter.as_ref(), &launch_command)
        .into_iter()
        .chain(prompt_args.iter().cloned())
        .collect();

    let exit = pump(
        &mut child,
        &rx,
        &mut pair,
        &mut supervision,
        server.as_ref(),
        adapter.as_ref(),
        &writer,
        &mut transcript,
        &state_dir,
        &session,
        debounce,
        inject_timeout,
        repo,
        cfg.handoff.tail_items,
        &cfg.handoff.model,
        Duration::from_secs(cfg.handoff.timeout_secs),
        QUIT_GRACE,
        tx,
        generation,
        &relaunch_extra,
        &turn_env,
        &cpr_filter,
        &announcer,
        &mut bar,
    );

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
            writeln!(w, "zirv ctx wrap: {e}")?;
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
    mail_enabled: bool,
    stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
    rows: u16,
    cols: u16,
}

impl BarRuntime {
    fn new(
        chrome: super::chrome::Chrome,
        harness: String,
        mail_enabled: bool,
        stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
        size: (u16, u16),
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
            mail_enabled,
            stdout_lock,
            cols: size.0,
            rows: size.1,
        }
    }

    fn active(&self) -> bool {
        self.chrome.bar && !self.disabled
    }
}

/// The 1s redraw throttle: usage and mail are read from disk only when this
/// has elapsed, never on the byte-pump path.
const BAR_THROTTLE: Duration = Duration::from_secs(1);

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
    if let Ok(_guard) = bar.stdout_lock.lock() {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(sequence.as_bytes());
        let _ = stdout.flush();
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

    let windows = super::window::load(state_dir);
    let usage_percent = windows
        .five_hour
        .iter()
        .chain(windows.seven_day.iter())
        .map(|w| w.used_percentage)
        .fold(None, |acc: Option<f64>, p| {
            Some(acc.map_or(p, |a| a.max(p)))
        });
    let unread_mail = unread_mail_count(state_dir, repo, &bar.harness, bar.mail_enabled);

    let state = super::chrome::BarState {
        harness: bar.harness.clone(),
        score: (supervision.signals_seen > 0).then_some(supervision.score),
        verdict: (supervision.signals_seen > 0).then_some(supervision.verdict),
        usage_percent,
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

#[allow(clippy::too_many_arguments)]
fn pump(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    rx: &mpsc::Receiver<PumpEvent>,
    pair: &mut portable_pty::PtyPair,
    supervision: &mut InjectionState,
    server: Option<&super::signal::SignalServer>,
    adapter: &dyn AgentAdapter,
    writer: &std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    transcript: &mut TranscriptSource,
    state_dir: &super::state::StateDir,
    session: &super::event::SessionId,
    debounce: Duration,
    inject_timeout: Duration,
    repo: &Path,
    tail_items: usize,
    distiller_model: &str,
    distiller_timeout: Duration,
    grace: Duration,
    tx: mpsc::Sender<PumpEvent>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    extra: &[String],
    turn_env: &[(String, String)],
    cpr_filter: &std::sync::Arc<std::sync::Mutex<CprFilter>>,
    announcer: &Announcer,
    bar: &mut BarRuntime,
) -> CtxResult<i32> {
    let mut last_size = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    // Read only from the turn-signal arm below, never from a byte-pump or
    // per-tick path: see `unread_mail_count`.
    let mut mail_seen = 0usize;

    loop {
        if let Some(status) = child.try_wait()? {
            // Let the reader thread flush whatever is still buffered.
            while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
            return Ok(status.exit_code() as i32);
        }

        while let Ok(event) = rx.try_recv() {
            if event == PumpEvent::PtyClosed {
                let status = child.wait()?;
                return Ok(status.exit_code() as i32);
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

            // Advisory only: wrap never consumes mail (it stays unread for
            // `zirv ctx inbox`) and never writes to the pty, only to stderr.
            // A read error (including no mailbox at all) leaves `mail_seen`
            // where it was, so it neither advises nor errors.
            if let Some(count) =
                unread_mail_count(state_dir, repo, adapter.name(), bar.mail_enabled)
                && mail_grew(mail_seen, count)
            {
                announcer.emit(&Event::MailWaiting { count });
                mail_seen = count;
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
                let injected = writer
                    .lock()
                    .map_err(|_| "pty writer poisoned".to_string())
                    .and_then(|mut sink| {
                        let command = adapter.compact_command().unwrap_or("/compact");
                        inject_compact(&mut *sink, command).map_err(|e| e.to_string())
                    });

                // Arm the cooldown before verifying so a failed verification
                // cannot turn into a retry loop.
                supervision.cooldown_at_signal = Some(supervision.signals_seen);

                // No transcript means no verification is possible, and a
                // deadline spent polling a file nobody writes would block the
                // pump for nothing.
                let failure = match (injected, transcript.path()) {
                    (Err(_), _) => Some("compact injection failed"),
                    (Ok(()), None) => Some("no transcript reported, compaction unverifiable"),
                    (Ok(()), Some(path)) => {
                        let seen = verify_compaction(
                            &mut Watcher::new(path.to_path_buf()),
                            adapter,
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
            Action::Restart => {
                supervision.cooldown_at_signal = Some(supervision.signals_seen);

                let jsonl = transcript
                    .path()
                    .map(|path| std::fs::read_to_string(path).unwrap_or_default())
                    .unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, tail_items);
                let (note, source) = handoff::distill_or_structural(
                    adapter,
                    distiller_model,
                    &ctx,
                    distiller_timeout,
                );
                let stored = handoff::store(state_dir, repo, session.as_str(), &note);

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

                let relaunched = match (new_generation, quit.is_ok()) {
                    (Some(new_generation), true) => {
                        match relaunch(adapter, repo, &note, extra, turn_env, last_size) {
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
                                true
                            }
                            Err(_) => false,
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
                    note_failure(
                        supervision,
                        Some((state_dir, session.as_str())),
                        "relaunch failed",
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
                    return Ok(status.exit_code() as i32);
                }
            }
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
                bar.disabled = true;
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

pub fn run<W: Write>(args: &WrapArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env, PromptRole::Worker, None)
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
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn wrap");
        drop(pair.slave);
        Harness {
            reader: pair.master.try_clone_reader().expect("reader"),
            writer: pair.master.take_writer().expect("writer"),
            child,
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

    #[test]
    fn wrap_needs_a_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: None,
            no_supervise: false,
            command: Vec::new(),
            simple: false,
        };
        let mut out = Vec::new();
        let err = run_with(
            &args,
            &mut out,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
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
        };
        let mut out = Vec::new();
        let err = run_with(
            &args,
            &mut out,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
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
        };
        let mut out = Vec::new();
        let err = run_with(
            &args,
            &mut out,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
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
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[("ZIRV_CTX_STATE_DIR", state.path().display().to_string())],
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

    #[test]
    fn mail_grew_only_fires_on_a_genuine_increase() {
        assert!(mail_grew(0, 1));
        assert!(mail_grew(2, 5));
        assert!(!mail_grew(3, 3), "unchanged is not growth");
        assert!(
            !mail_grew(3, 1),
            "a shrink (consumed elsewhere) is not growth"
        );
    }

    #[test]
    fn unread_mail_count_is_zero_for_a_repo_with_no_mailbox_at_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        assert_eq!(unread_mail_count(&state, &repo, "claude", true), Some(0));
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
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");
        assert_eq!(unread_mail_count(&state, &repo, "claude", true), Some(1));
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
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");

        assert_eq!(
            unread_mail_count(&state, &repo, "claude", false),
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
            sent: 1,
            body: "note".to_string(),
        };
        crate::commands::ctx::mail::store(&state, &slug, &msg, &CtxConfig::default())
            .expect("store");

        assert_eq!(
            unread_mail_count(&state, &repo, "claude", true),
            Some(0),
            "addressed to codex, not this claude session"
        );
        assert_eq!(unread_mail_count(&state, &repo, "codex", true), Some(1));
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

        assert_eq!(unread_mail_count(&state, &repo, "claude", true), Some(0));
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

        let result = unread_mail_count(&state, &repo, "claude", true);

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
        let socket =
            std::fs::read_to_string(tmp.path().join("state/socket-path")).unwrap_or_default();
        assert!(
            !socket.trim().is_empty(),
            "wrap must publish its socket path"
        );
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

    // T8: mail advisory driven end to end through a real wrapped session.

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
                sent: 1,
                body: body.to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");
    }

    #[cfg(unix)]
    #[test]
    fn a_wrapped_session_is_told_about_new_mail_on_stderr_only() {
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

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        // `wrap`'s own stderr shares the outer pty in this harness (there is
        // no separate stderr stream to redirect it to), so it arrives on
        // `h.reader` exactly like the compact/restart tests' own log output
        // does; the point under test is the wording, not the transport.
        let seen = read_until(&mut h.reader, "zirv ctx inbox", Duration::from_secs(10));
        assert!(seen.contains("zirv ctx inbox"), "got {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn wrap_never_consumes_or_injects_mail() {
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

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "zirv ctx inbox", Duration::from_secs(10));
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
        let unread = crate::commands::ctx::mail::list(&state_dir, &slug, None).expect("list");
        assert_eq!(unread.len(), 1, "wrap must never consume mail on its own");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn the_advisory_fires_at_most_once_per_turn_boundary() {
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

        let socket_path = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
        let socket = std::path::PathBuf::from(socket_path.trim());

        crate::commands::ctx::signal::send(&socket, &turn_signal(3, Verdict::Healthy))
            .expect("send 1");
        let first = read_until(&mut h.reader, "zirv ctx inbox", Duration::from_secs(10));
        assert!(
            first.contains("zirv ctx inbox"),
            "the first turn advises: {first:?}"
        );

        // A second turn boundary, no new mail in between.
        crate::commands::ctx::signal::send(&socket, &turn_signal(4, Verdict::Healthy))
            .expect("send 2");
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let after = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));

        assert!(
            !after.contains("zirv ctx inbox"),
            "a second turn boundary with no new mail must not advise again: {after:?}"
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

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
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

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
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

        let socket_path = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
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

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
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

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
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

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
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
        assert!(log.contains("relaunch failed"), "reason recorded: {log}");
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

        let socket = std::fs::read_to_string(tmp.path().join("state/socket-path")).expect("path");
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
        let status = h.child.wait().expect("wait");
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
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");

        let seen = read_until(&mut reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"));
        assert!(
            !tmp.path().join("state/socket-path").exists(),
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
            let _ = child.wait();
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
