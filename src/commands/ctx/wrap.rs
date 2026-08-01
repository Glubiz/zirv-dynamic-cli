use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::adapters::AgentAdapter;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::NormalizedEvent;
use super::handoff::{self, Handoff};
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
    pub last_turn: u64,
    pub verdict: Verdict,
    pub score: u32,
    pub user_typed_since_turn: bool,
    pub last_output: Instant,
    pub cooldown_until_turn: Option<u64>,
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
            verdict: Verdict::Healthy,
            score: 0,
            user_typed_since_turn: false,
            last_output: Instant::now(),
            cooldown_until_turn: None,
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
        self.verdict = signal.verdict;
        self.score = signal.score;
        self.user_typed_since_turn = false;
    }
}

/// Both spec preconditions, and nothing else: a turn boundary has been reported
/// and the user is idle. Everything about which verdict deserves which action
/// lives in the escalation ladder, not here.
pub fn may_inject(state: &InjectionState, now: Instant, debounce: Duration) -> bool {
    !state.degraded
        && state.last_turn > 0
        && !state.user_typed_since_turn
        && now.duration_since(state.last_output) >= debounce
        && state
            .cooldown_until_turn
            .is_none_or(|turn| state.last_turn > turn)
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
        Verdict::Advise => Action::Advise,
        Verdict::Compact if may_inject(state, now, debounce) => Action::Compact,
        Verdict::Restart if may_inject(state, now, debounce) => Action::Restart,
        _ => Action::None,
    }
}

pub fn advisory_line(score: u32, tokens: u64) -> String {
    format!(
        "zirv ctx: context health is slipping (score {score}, {tokens} tokens in context). A /compact soon will keep instruction-following sharp."
    )
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
        if let Some(jsonl) = watcher.read_if_changed()?
            && adapter
                .parse_events(&jsonl)
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
pub fn note_failure(state: &mut InjectionState, log_target: Option<(&StateDir, &str)>, what: &str) {
    state.degraded = true;
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
                    if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
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
    let child = pair.slave.spawn_command(builder)?;

    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    Ok((pair, child, reader, writer))
}

pub fn run_with<W: Write>(
    args: &WrapArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let (program, rest) = args
        .command
        .split_first()
        .ok_or("no command to wrap; pass it after --")?;

    let cfg = CtxConfig::load(repo, env)?;
    // Selection happens here so an unknown or unverified agent fails before the
    // terminal is touched.
    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &args.command,
        cfg.agent_bin.as_deref(),
    )?;

    let state_dir = super::state::StateDir::resolve(env)?;
    let session = super::event::SessionId::new_v4();

    let composed = super::prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &cfg.prompt,
    );
    let prompt_args = super::prompt::injection_args(adapter.as_ref(), composed.as_ref());
    super::prompt::log_injection(
        &state_dir,
        "wrap",
        session.as_str(),
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );

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
                );
                None
            }
        }
    };

    // Deliberately not derived from `session`: that id belongs to wrap, not to
    // the agent it spawns, so a derived path names a file nobody ever writes.
    let mut transcript = TranscriptSource::new(env(TRANSCRIPT_ENV).map(PathBuf::from));

    let (cols, rows) = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    let mut pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
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
    let turn_env: Vec<(String, String)> = server
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
    for (key, value) in &turn_env {
        command.env(key, value);
    }

    let mut child = pair.slave.spawn_command(command)?;

    let reader = pair.master.try_clone_reader()?;
    // One writer, shared: the stdin pump and (from Task C4) the injector both
    // need it, and `take_writer` can only be called once. Its contents (not
    // the Arc itself) get swapped out on a restart, so every holder of this
    // Arc transparently starts writing to the fresh pty.
    let writer = std::sync::Arc::new(std::sync::Mutex::new(pair.master.take_writer()?));
    let (tx, rx) = mpsc::channel::<PumpEvent>();
    // Bumped on every restart so a stale reader thread from an abandoned pty
    // never reports a false PtyClosed for the pty that replaced it.
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // PTY to stdout.
    spawn_output_thread(reader, tx.clone(), generation.clone(), 0);

    // stdin to PTY.
    let input_tx = tx.clone();
    let input_writer = std::sync::Arc::clone(&writer);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let Ok(mut sink) = input_writer.lock() else {
                        return;
                    };
                    if sink.write_all(&buf[..n]).is_err() || sink.flush().is_err() {
                        return;
                    }
                    drop(sink);
                    if input_tx.send(PumpEvent::Input(n)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Raw mode is best-effort: without a terminal (a pipe, or CI) the wrapper
    // still passes bytes through.
    let mut raw = RawGuard::enter(STDIN_FD).ok();

    let debounce = Duration::from_millis(cfg.wrap.debounce_ms);
    let inject_timeout = Duration::from_millis(cfg.wrap.inject_timeout_ms);

    // Carried into `relaunch_command` too, so a restart does not silently drop
    // the injected prompt the first command already carries.
    let relaunch_extra: Vec<String> = rest
        .iter()
        .cloned()
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
    );

    if let Some(guard) = raw.as_mut() {
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
) -> CtxResult<i32> {
    let mut last_size = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);

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
            supervision.on_turn(&signal);
        }

        match action_for(supervision, Instant::now(), debounce) {
            Action::None => {}
            Action::Advise => {
                let mut stderr = std::io::stderr();
                let _ = writeln!(stderr, "\r\n{}\r", advisory_line(supervision.score, 0));
                // Advise once per turn.
                supervision.cooldown_until_turn = Some(supervision.last_turn);
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
                supervision.cooldown_until_turn = Some(supervision.last_turn);

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

                if let Some(reason) = failure {
                    note_failure(supervision, Some((state_dir, session.as_str())), reason);
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
                supervision.cooldown_until_turn = Some(supervision.last_turn);

                // Bumped before the old child is even asked to quit: its
                // reader thread's own EOF can land at any point from here on
                // (quit_child alone may take up to `grace`), and once bumped
                // that thread's `still_current` check is already false, so a
                // pty closing on its way out can never be mistaken for the
                // fresh one that is about to replace it.
                let new_generation =
                    generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;

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

                let quit = writer
                    .lock()
                    .map_err(|_| "pty writer poisoned".to_string())
                    .and_then(|mut sink| {
                        quit_child(&mut *sink, child, adapter.quit_sequence(), grace)
                            .map_err(|e| e.to_string())
                    });

                // That session is over. Whatever it was writing is now a dead
                // file, and the replacement reports its own on its first turn.
                transcript.forget();

                let relaunched = quit.is_ok()
                    && match relaunch(adapter, repo, &note, extra, turn_env, last_size) {
                        Ok((fresh_pair, fresh_child, fresh_reader, fresh_writer)) => {
                            spawn_output_thread(
                                fresh_reader,
                                tx.clone(),
                                generation.clone(),
                                new_generation,
                            );
                            if let Ok(mut sink) = writer.lock() {
                                *sink = fresh_writer;
                            }
                            *pair = fresh_pair;
                            *child = fresh_child;
                            true
                        }
                        Err(_) => false,
                    };

                if !relaunched {
                    note_failure(
                        supervision,
                        Some((state_dir, session.as_str())),
                        "relaunch failed",
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
            let _ = pair.master.resize(PtySize {
                rows: size.1,
                cols: size.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        std::thread::sleep(PUMP_POLL);
    }
}

pub fn run<W: Write>(args: &WrapArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    pub(crate) fn spawn_wrap(extra_env: &[(&str, String)], wrapped: &[&str]) -> Harness {
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
        let err = run_with(&args, &mut out, tmp.path(), &|_| None).expect_err("nothing to wrap");
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
        state.cooldown_until_turn = Some(3);
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

    #[test]
    fn the_advisory_line_is_one_line_and_plain() {
        let line = advisory_line(47, 138_000);
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("47"));
        assert!(line.contains("138000") || line.contains("138"));
        assert!(
            !line.contains('\u{2014}'),
            "no em dashes in user-facing copy"
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
        let _ = watcher.read_if_changed();

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

        note_failure(&mut supervision, Some((&state, "sess")), "socket died");
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
        note_failure(&mut supervision, None, "no state dir");
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
