use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::CtxResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Continue,
    Stop(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    TimedOut,
    StoppedByTick(&'static str),
}

/// Budget for `OutputTap::drain_to_eof`'s one legitimate blocking read, at
/// `exec.rs`'s and `run_loop.rs`'s "final drain" call sites right after
/// `supervise_child` returns `Outcome::Exited`. Scheduling headroom, not a
/// work-completion wait: both `forward` reader threads normally disconnect
/// within a scheduling quantum of the child's own exit, so this is paid in
/// full only in the pathological case where they do not, and is otherwise a
/// negligible, one-time cost per session cycle -- nowhere near a delay an
/// operator would notice on a hot path measured in whole agent turns.
pub const FINAL_DRAIN_BUDGET: Duration = Duration::from_millis(500);

#[cfg(test)]
pub fn spawn(mut command: Command) -> CtxResult<Child> {
    Ok(command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?)
}

/// Polls the child, calling `on_tick` at every interval. Stops on child exit,
/// on the deadline, or when a tick asks to stop; in the last two cases it
/// terminates the child. On Windows that means the whole process tree rooted
/// at the child, not just the direct child: a shim launch (`cmd.exe /c
/// claude.cmd`) runs the real agent as a `node` grandchild, and killing only
/// cmd.exe would leave that grandchild alive to run alongside a freshly
/// spawned replacement -- two live sessions on one repo. See `terminate`.
pub fn supervise_child(
    child: &mut Child,
    deadline: Instant,
    poll: Duration,
    on_tick: &mut dyn FnMut() -> Tick,
) -> CtxResult<Outcome> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Outcome::Exited(status.code().unwrap_or(1)));
        }
        if Instant::now() >= deadline {
            terminate(child, Duration::from_secs(5))?;
            return Ok(Outcome::TimedOut);
        }
        if let Tick::Stop(reason) = on_tick() {
            terminate(child, Duration::from_secs(5))?;
            return Ok(Outcome::StoppedByTick(reason));
        }
        std::thread::sleep(poll);
    }
}

/// SIGTERM, then SIGKILL after the grace period. Safe to call on a child that
/// already exited.
pub fn terminate(child: &mut Child, grace: Duration) -> CtxResult<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        // SAFETY: `kill` with a pid this process owns and a valid signal number.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        // TerminateProcess (what `child.kill()` calls) kills only the direct
        // child. On an npm-installed agent that child is `cmd.exe /c
        // claude.cmd`, which runs `node`; killing cmd.exe leaves the node
        // grandchild alive (there is no Job Object). `taskkill /T` terminates
        // the whole tree rooted at the pid instead. Its arguments are fixed
        // flags plus a decimal pid, so there is no cmd.exe-reparse exposure.
        // Falls back to a direct kill if taskkill cannot be run.
        if !kill_tree(child.id()) {
            let _ = child.kill();
        }
    }

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// The `taskkill` invocation that terminates the whole process tree rooted at
/// `pid`: `/T` walks the tree, `/F` forces termination, `/PID <pid>` names the
/// root. Every element is a fixed flag or a decimal pid, so nothing here can
/// be reparsed by cmd.exe. Pure, so the wiring is testable without spawning.
#[cfg(not(unix))]
fn taskkill_args(pid: u32) -> Vec<String> {
    vec![
        "/T".to_string(),
        "/F".to_string(),
        "/PID".to_string(),
        pid.to_string(),
    ]
}

/// The `taskkill` command, assembled but not run. Shared by the synchronous
/// [`kill_tree`] and by the console-close handler's fire-and-poll sweep, so
/// there is exactly one place the argv and the stdio discipline are decided.
#[cfg(not(unix))]
fn taskkill_command(pid: u32) -> Command {
    let mut command = Command::new("taskkill");
    command
        .args(taskkill_args(pid))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Runs `taskkill /T /F /PID <pid>` without a shell, waiting briefly for it to
/// finish. Returns whether taskkill ran *and* reported success; `false` (it is
/// not on PATH, or it failed) tells the caller to fall back to a direct
/// `child.kill()`.
///
/// `pub(crate)` (P1): the pty seams -- `wrap::quit_child` and
/// `dash::pane::Pane::finish_shutdown` -- have only ever had portable-pty's
/// own `Child::kill()`, which is a `TerminateProcess` against the *direct*
/// child. For an npm-installed agent that direct child is `cmd.exe /c
/// claude.cmd` and the real agent is a `node` grandchild, so a "killed" pane
/// left a live agent behind holding the repo. This is the same tree-kill
/// `terminate` (exec/loop) has always used, reachable from those seams too.
/// Never a substitute for evidence of death: portable-pty 0.9.0's own
/// `kill()` inverts its success check, and taskkill's exit status says only
/// that taskkill ran -- `try_wait`/`wait_for_exit` remain the only proof.
#[cfg(not(unix))]
pub(crate) fn kill_tree(pid: u32) -> bool {
    taskkill_command(pid)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// P2: the supervised-child pid registry.
//
// The Windows console-close handler (`term::console_ctrl_handler`) restores
// the console and returns FALSE, which kills *this* process -- and nothing
// else. A pane's child lives on a ConPTY of its own and never sees
// CTRL_CLOSE_EVENT, so clicking the X on a dashboard window left every agent
// running, invisible, holding the repo. The handler needs a list of pids to
// tree-kill, and it has to be a list it can read without allocating a lock
// wait it might not survive: Windows gives a console-close handler about five
// seconds.
//
// Deliberately cross-platform (only the *consumer* is Windows-only): the
// bookkeeping is ordinary safe code, and keeping it off `cfg` means CI --
// which is Linux -- actually runs its tests.
// ---------------------------------------------------------------------------

/// Every supervised child pid this process has spawned and not yet confirmed
/// dead. Registered by [`ChildGuard::adopt`], removed by
/// [`ChildGuard::release`] (and by its `Drop`).
static SUPERVISED_PIDS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

fn supervised_pids() -> &'static Mutex<Vec<u32>> {
    SUPERVISED_PIDS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Adds `pid` to the registry, ignoring a duplicate. A poisoned lock is
/// dropped silently: a registry that cannot be written is a lost tree-kill at
/// console close, never a failed spawn.
pub(crate) fn register_child_pid(pid: u32) {
    if let Ok(mut pids) = supervised_pids().lock()
        && !pids.contains(&pid)
    {
        pids.push(pid);
    }
}

/// Removes `pid` from the registry. Idempotent.
pub(crate) fn deregister_child_pid(pid: u32) {
    if let Ok(mut pids) = supervised_pids().lock() {
        pids.retain(|held| *held != pid);
    }
}

/// The registry as it stands, for the console-close handler.
///
/// `try_lock`, never `lock`: this runs on the handler's own thread inside a
/// close window the OS will not wait past, and a handler that blocks on a
/// contended mutex hangs the window instead of killing anything. Losing the
/// snapshot degrades to today's behaviour (orphans), which is bad; hanging
/// the close is worse. A poisoned lock takes the same route.
///
/// `allow(dead_code)` off Windows rather than `cfg(windows)`: the only
/// non-test caller is `kill_registered_trees`, which *is* Windows-only, so
/// on Linux this reads as dead in the bin target and `-D warnings` fails the
/// ubuntu CI job. Gating the function itself would take the registry tests
/// with it, and running those on CI is the whole reason the registry is not
/// `cfg`'d in the first place (see this section's own header comment).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn supervised_pid_snapshot() -> Vec<u32> {
    supervised_pids()
        .try_lock()
        .map(|pids| pids.clone())
        .unwrap_or_default()
}

/// How long the console-close sweep waits for its `taskkill` children before
/// giving up. Windows allows a console control handler roughly five seconds
/// before it terminates the process anyway, so this stays well inside it.
#[cfg(windows)]
const CLOSE_KILL_BUDGET: Duration = Duration::from_millis(1_500);

/// Tree-kills every registered child pid. Called only from the Windows
/// console control handler (CTRL_CLOSE/LOGOFF/SHUTDOWN), which runs on an
/// ordinary thread -- spawning processes there is legal, unlike in a POSIX
/// signal handler, which is why the unix side deliberately has no counterpart.
///
/// Every `taskkill` is spawned first and waited on afterwards, against one
/// shared budget, so N children cost one wait rather than N.
#[cfg(windows)]
pub(crate) fn kill_registered_trees() {
    let mut killers: Vec<Child> = supervised_pid_snapshot()
        .into_iter()
        .filter_map(|pid| taskkill_command(pid).spawn().ok())
        .collect();
    let deadline = Instant::now() + CLOSE_KILL_BUDGET;
    while !killers.is_empty() && Instant::now() < deadline {
        killers.retain_mut(|killer| !matches!(killer.try_wait(), Ok(Some(_))));
        if killers.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// P3: job objects -- the kernel-enforced backstop.
//
// Every userspace kill path (P1's tree-kill, P2's console-close sweep) needs
// this process to still be running to fire. `TerminateProcess` against zirv
// itself -- `taskkill /F`, a crash, an `abort` from the release profile's
// `panic = "abort"` -- runs no user code at all. A job object with
// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` makes the *kernel* the killer: when
// zirv dies for any reason its handles close, and closing the last job handle
// terminates every process in the job.
//
// Every failure degrades silently to an inert guard. A job that cannot be
// created or assigned is exactly today's behaviour, and today's behaviour is
// not an error worth stopping a session for.
//
// Known residual: `AssignProcessToJobObject` adds *one* process. Descendants
// it creates **after** the assignment inherit the job; ones it had already
// created do not. On a shim launch (`cmd.exe /c claude.cmd` -> `node`) the
// assignment happens on the very next statement after the spawn, so it
// normally lands well before cmd.exe has started `node` -- but it is a race,
// not a guarantee, and portable-pty offers no `CREATE_SUSPENDED` seam to
// close it. This is why P1 and P2 stay: both go through `taskkill /T`, which
// walks the tree as it stands *at kill time* and so covers a grandchild the
// job missed. P3 is the backstop for the case the other two cannot reach at
// all (no user code runs), not a replacement for either.
// ---------------------------------------------------------------------------

/// An anonymous kill-on-close job object holding one child's process tree.
///
/// The handle is kept as `usize` rather than `HANDLE` for the same reason
/// `term::StashedConsole` does: a raw pointer field would make every struct
/// that stores a guard (`dash::pane::Pane`, `wrap`'s own child state) `!Send`
/// for no reason. `0` means inert -- no job was created, or assignment failed.
#[cfg(windows)]
#[derive(Debug)]
pub struct JobGuard {
    handle: usize,
}

#[cfg(windows)]
impl JobGuard {
    /// The degraded guard every failure produces: holds nothing, kills
    /// nothing, and closing it is a no-op.
    pub fn inert() -> Self {
        Self { handle: 0 }
    }

    /// Whether a real job actually holds the child. Only this module's own
    /// tests ask -- production behaviour is deliberately identical either
    /// way, since every job failure degrades to exactly the pre-P3 behaviour
    /// rather than to an error.
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.handle != 0
    }

    /// Creates an anonymous job limited to kill-on-close and assigns `pid` to
    /// it. Returns [`JobGuard::inert`] on any failure -- no job object
    /// support, an ambient job that refuses nesting, a pid that has already
    /// exited, or a missing `PROCESS_SET_QUOTA`/`PROCESS_TERMINATE` right.
    pub fn adopt(pid: u32) -> Self {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        // SAFETY: every call below takes either no pointer or a live local.
        // `job` and `process` are checked for null before use and closed on
        // every exit path; `limits` is a plain `#[repr(C)]` POD passed by
        // reference with its own size.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Self::inert();
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(job);
                return Self::inert();
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                CloseHandle(job);
                return Self::inert();
            }
            let assigned = AssignProcessToJobObject(job, process) != 0;
            CloseHandle(process);
            if !assigned {
                // Nested jobs are supported on every Windows this binary
                // targets, but an ambient job created without nesting support
                // (or one that forbids breakaway) still refuses. Degrade.
                CloseHandle(job);
                return Self::inert();
            }
            Self {
                handle: job as usize,
            }
        }
    }

    /// Closes the job handle, which is what makes the kernel reap whatever is
    /// still in the job. Idempotent, and called explicitly rather than left to
    /// `Drop` alone: the release profile is `panic = "abort"`.
    pub fn close(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        if self.handle == 0 {
            return;
        }
        let handle = self.handle;
        self.handle = 0;
        // SAFETY: a handle this guard created and has not closed before.
        unsafe {
            CloseHandle(handle as _);
        }
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        self.close();
    }
}

/// Unix needs none of this: portable-pty does `setsid` + `TIOCSCTTY`, so
/// closing the pty master SIGHUPs the child's whole session, and an orphaned
/// process group is reaped by the same mechanism. The type exists so every
/// caller compiles unchanged.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct JobGuard;

#[cfg(not(windows))]
impl JobGuard {
    pub fn inert() -> Self {
        Self
    }

    pub fn is_active(&self) -> bool {
        false
    }

    pub fn adopt(_pid: u32) -> Self {
        Self
    }

    pub fn close(&mut self) {}
}

/// Everything a supervised child's *lifetime* owns beyond the `Child` handle
/// itself: its membership in the console-close pid registry (P2) and the job
/// object that reaps its tree if zirv dies without running any code (P3).
///
/// One guard per live child. A restart drops the old one and adopts a fresh
/// one, so the replaced child's tree cannot outlive the supervisor that
/// replaced it.
#[derive(Debug)]
pub struct ChildGuard {
    pid: Option<u32>,
    job: JobGuard,
}

impl ChildGuard {
    /// The no-op guard: a child whose pid this platform (or this pty backend)
    /// cannot report gets one, and so does every caller that has nothing to
    /// adopt yet.
    pub fn inert() -> Self {
        Self {
            pid: None,
            job: JobGuard::inert(),
        }
    }

    /// Registers `pid` for the console-close sweep and puts it in a
    /// kill-on-close job. `None` -- portable-pty could not report a pid --
    /// degrades to [`ChildGuard::inert`].
    pub fn adopt(pid: Option<u32>) -> Self {
        let Some(pid) = pid else {
            return Self::inert();
        };
        register_child_pid(pid);
        Self {
            pid: Some(pid),
            job: JobGuard::adopt(pid),
        }
    }

    /// Whether a kernel job actually backs this guard (Windows only; always
    /// false elsewhere). Test-only, like [`JobGuard::is_active`].
    #[allow(dead_code)]
    pub fn job_is_active(&self) -> bool {
        self.job.is_active()
    }

    /// The pid this guard holds, if any. Test-only: production just holds the
    /// guard and releases it.
    #[allow(dead_code)]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Deregisters the pid and closes the job handle. Called on *confirmed*
    /// exit, where closing the job kills nothing because there is nothing left
    /// to kill. Idempotent, and called explicitly in exit arms because the
    /// release profile is `panic = "abort"` and `Drop` is no safety net.
    pub fn release(&mut self) {
        if let Some(pid) = self.pid.take() {
            deregister_child_pid(pid);
        }
        self.job.close();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Bytes a transcript grew by since the previous poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    /// Whole lines only: everything up to and including the last newline.
    pub lines: String,
    /// The line the agent is still writing, if any. Reported separately and
    /// read again next poll, so nothing is ever folded in half-written.
    pub partial: String,
    /// The transcript was truncated or rewritten rather than appended to, so
    /// `lines` starts at byte zero again and anything derived from the earlier
    /// bytes is void.
    pub restarted: bool,
}

/// How much of the consumed region is fingerprinted on every poll. Both reads
/// are O(1), which is the whole point: an append must not cost the file.
const HEAD_BYTES: u64 = 4096;
const TAIL_BYTES: u64 = 256;

/// Fingerprint of the region a `Watcher` has already handed out: the first and
/// last bytes of it, plus its length. Cheap enough to recompute every poll and
/// enough to catch a transcript that was rewritten rather than appended to.
fn consumed_fingerprint(file: &mut std::fs::File, offset: u64) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};

    if offset == 0 {
        return Ok(0);
    }
    let mut read_at = |start: u64, len: u64| -> std::io::Result<String> {
        let mut buffer = vec![0u8; len as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    };
    let head = read_at(0, HEAD_BYTES.min(offset))?;
    let tail_len = TAIL_BYTES.min(offset);
    let tail = read_at(offset - tail_len, tail_len)?;
    Ok(super::event::input_hash(&format!("{offset}|{head}|{tail}")))
}

/// Polls a growing transcript and reports only what was appended since the
/// last poll, so a per-turn scoring pass costs the turn rather than the
/// session. Length and mtime together decide whether anything changed at all
/// (a same-length rewrite, possible right after a compaction rewrites the
/// transcript in place, would otherwise read as "unchanged"), and a
/// fingerprint of the already-consumed region decides whether the byte offset
/// still means anything. Any doubt reports `restarted` and re-reads from the
/// beginning: a wrong delta is worse than a slow poll.
pub struct Watcher {
    path: PathBuf,
    len: u64,
    mtime: Option<SystemTime>,
    offset: u64,
    consumed: u64,
}

impl Watcher {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            len: 0,
            mtime: None,
            offset: 0,
            consumed: 0,
        }
    }

    /// Picks up at a position a previous process left off at (see the
    /// persisted scoring checkpoint). The first poll validates it against the
    /// file itself, so a bad position costs a re-read, never a wrong answer.
    pub fn resuming(path: PathBuf, offset: u64, consumed: u64) -> Self {
        Self {
            path,
            len: 0,
            mtime: None,
            offset,
            consumed,
        }
    }

    /// The offset and fingerprint another process needs to resume here.
    pub fn position(&self) -> (u64, u64) {
        (self.offset, self.consumed)
    }

    /// `None` when the transcript is missing or has not changed since the last
    /// poll.
    pub fn read_appended(&mut self) -> CtxResult<Option<Appended>> {
        use std::io::{Read, Seek, SeekFrom};

        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Ok(None);
        };
        let (len, mtime) = (meta.len(), meta.modified().ok());
        if len == self.len && mtime == self.mtime {
            return Ok(None);
        }
        // Same length but a new mtime is a rewrite, not an append: there is no
        // delta to read, only different bytes in the same place.
        let mut restarted = len == self.len || len < self.offset;
        self.len = len;
        self.mtime = mtime;

        let mut file = std::fs::File::open(&self.path)?;
        if !restarted {
            restarted = consumed_fingerprint(&mut file, self.offset)? != self.consumed;
        }
        if restarted {
            self.offset = 0;
        }

        let mut buffer = Vec::new();
        file.seek(SeekFrom::Start(self.offset))?;
        file.read_to_end(&mut buffer)?;
        let split = buffer
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        let appended = Appended {
            lines: String::from_utf8_lossy(&buffer[..split]).into_owned(),
            partial: String::from_utf8_lossy(&buffer[split..]).into_owned(),
            restarted,
        };

        self.offset += split as u64;
        self.consumed = consumed_fingerprint(&mut file, self.offset)?;
        Ok(Some(appended))
    }
}

/// Runs an `on_failure` hook the way the script runner runs commands.
pub fn run_shell(command: &str, cwd: &Path) -> CtxResult<i32> {
    let mut shell = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.arg("-Command").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    let status = shell.current_dir(cwd).status()?;
    Ok(status.code().unwrap_or(1))
}

/// Whole lines of a supervised child's output, for matching against known
/// notices. The bytes are always forwarded onward first: tapping must never
/// change what the operator sees.
pub struct OutputTap {
    rx: std::sync::mpsc::Receiver<String>,
}

impl OutputTap {
    pub fn try_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    /// Drains everything already queued, then blocks -- but only as long as
    /// it actually takes -- for `forward`'s reader thread(s) to either
    /// deliver more lines or disconnect (stdout's and stderr's threads both
    /// finished, i.e. real EOF), whichever comes first, up to `budget`.
    ///
    /// `try_lines` alone is a pure, instantaneous, non-blocking drain with
    /// no synchronization against those threads: a child that prints its
    /// last line and exits immediately can still have that line in flight
    /// -- the reader thread not yet scheduled to read it out of the pipe --
    /// at the exact instant `child.wait()`/`try_wait()` observes the OS-level
    /// exit, independently of this process's own thread scheduling. This is
    /// the seam the "final drain" right after `supervise_child` returns
    /// (`exec.rs`, `run_loop.rs`) needs, and `try_lines` alone does not
    /// provide despite each call site's own comment claiming to close the
    /// race: it is exactly as non-blocking as every other call to
    /// `try_lines`, so it can lose the same race it was written to close.
    ///
    /// Bounded so a child that genuinely has nothing further to say is
    /// never held up by more than `budget` -- and in practice far less,
    /// since both reader threads normally disconnect within a scheduling
    /// quantum of the child's own exit, at which point `recv_timeout`
    /// returns immediately rather than waiting out the rest of the budget.
    pub fn drain_to_eof(&self, budget: Duration) -> Vec<String> {
        let mut lines = self.try_lines();
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return lines;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => lines.push(line),
                // Disconnected (both forward threads finished; nothing more
                // will ever arrive) or genuinely timed out -- either way,
                // this is everything drain_to_eof is ever going to get.
                Err(_) => return lines,
            }
        }
    }
}

/// FIX 2a: refuse a launch that would let a downstream argv element be
/// re-parsed by cmd.exe -- the `cmd.exe /c <shim>` form `resolve_program`
/// produces on Windows for an npm-installed `.cmd`. Extracts the already-
/// resolved program and arguments from the assembled `Command` and defers to
/// the one metacharacter policy in `adapters::guard_cmd_shim_reparse`. A
/// no-op off Windows and for any non-shim program.
///
/// L: `pub(crate)` (not just this module's own `spawn_tapped` chokepoint) so
/// `handoff::run_model` -- the judgment/distiller child, spawned directly
/// rather than through `spawn_tapped` -- can reach the same guard at its own
/// spawn seam, matching every other place a `Command` an adapter built is
/// actually spawned.
pub(crate) fn guard_cmd_shim_reparse(command: &Command) -> CtxResult<()> {
    let program = command.get_program().to_string_lossy().to_string();
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    super::adapters::guard_cmd_shim_reparse(&program, &args).map_err(Into::into)
}

/// Like `spawn`, but the child's stdout and stderr are piped so they can be
/// matched. Each stream is forwarded to this process's corresponding stream
/// unchanged, line by line.
///
/// `stdin_text` (FIX B) is the headless prompt when it must be delivered on
/// **stdin** rather than as an argv token: on a Windows `cmd.exe /c <shim>`
/// launch, an argv prompt would be reparsed by cmd.exe, so the prompt (and any
/// folded mail) travels on stdin instead -- the same mechanism the distiller
/// uses. `None` keeps stdin nulled, exactly as before, which is what every
/// off-shim launch (and every `sh`-based fake-agent test) gets.
///
/// The third return value is the child's [`ChildGuard`] (P2/P3): every
/// `exec`/`loop` launch reaches this one chokepoint, so registering the pid
/// and adopting the job here is what makes those two supervisors' children
/// impossible to orphan without every call site having to remember. Hold it
/// for as long as the child may run -- dropping it closes the job, and on
/// Windows closing a kill-on-close job is itself the kill.
pub fn spawn_tapped(
    mut command: Command,
    stdin_text: Option<String>,
) -> CtxResult<(Child, OutputTap, ChildGuard)> {
    // FIX 2a (command-injection defense): both std::process supervisors
    // (`exec`, `loop`) reach every spawn -- first launch and every restart --
    // through here, so this is their single chokepoint for the cmd.exe
    // argv-reparse guard. A no-op off Windows and for any non-shim program.
    guard_cmd_shim_reparse(&command)?;
    let stdin_mode = if stdin_text.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = command
        .stdin(stdin_mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Adopted before anything else can fail: from here on the child is
    // reapable by the console-close sweep and by the kernel, whatever this
    // process does next.
    let guard = ChildGuard::adopt(Some(child.id()));

    // Write the prompt on its own thread, then drop the handle so the child
    // sees EOF -- exactly `handoff::run_model`'s stdin discipline. A write
    // failure (the child exited before draining) is not surfaced here: the
    // supervisor already reports an early, unsuccessful exit through the
    // child's own status, which is the more useful of the two reports.
    if let Some(text) = stdin_text
        && let Some(mut stdin) = child.stdin.take()
    {
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }

    let (tx, rx) = std::sync::mpsc::channel::<String>();

    if let Some(stdout) = child.stdout.take() {
        forward(stdout, tx.clone(), false);
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, tx, true);
    }

    Ok((child, OutputTap { rx }, guard))
}

fn forward<R: std::io::Read + Send + 'static>(
    stream: R,
    tx: std::sync::mpsc::Sender<String>,
    is_stderr: bool,
) {
    use std::io::BufRead;

    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if is_stderr {
                let mut sink = std::io::stderr();
                let _ = writeln!(sink, "{line}");
            } else {
                let mut sink = std::io::stdout();
                let _ = writeln!(sink, "{line}");
                let _ = sink.flush();
            }
            if tx.send(line).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[test]
    fn a_clean_exit_is_reported_with_its_code() {
        let mut child = spawn(sh("exit 0")).expect("spawn");
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::Exited(0));
    }

    #[test]
    fn a_failing_exit_code_is_preserved() {
        let mut child = spawn(sh("exit 7")).expect("spawn");
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::Exited(7));
    }

    #[test]
    fn a_deadline_kills_the_child_and_reports_a_timeout() {
        let mut child = spawn(sh("sleep 30")).expect("spawn");
        let started = Instant::now();
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_millis(200),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "must not wait for the child"
        );
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "child was reaped"
        );
    }

    #[test]
    fn a_tick_can_stop_the_run_and_name_its_reason() {
        let mut child = spawn(sh("sleep 30")).expect("spawn");
        let mut ticks = 0;
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(20),
            &mut || {
                ticks += 1;
                if ticks >= 2 {
                    Tick::Stop("rot")
                } else {
                    Tick::Continue
                }
            },
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::StoppedByTick("rot"));
        assert!(ticks >= 2);
    }

    #[test]
    fn ticks_fire_at_the_poll_interval() {
        let mut child = spawn(sh("sleep 1")).expect("spawn");
        let mut ticks = 0;
        supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(50),
            &mut || {
                ticks += 1;
                Tick::Continue
            },
        )
        .expect("supervise");
        assert!(
            ticks >= 3,
            "expected several ticks before exit, got {ticks}"
        );
    }

    #[test]
    fn terminate_stops_a_child_that_ignores_sigterm() {
        let mut child = spawn(sh("trap '' TERM; sleep 30")).expect("spawn");
        let started = Instant::now();
        terminate(&mut child, Duration::from_millis(150)).expect("terminate");
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "child is gone"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// HIGH-1: the Windows terminate path kills the whole process tree by
    /// pid, so a shim's `node` grandchild is not orphaned. The kill itself is
    /// awkward to assert deterministically; the arg wiring it is built from is
    /// pure, so pin that instead.
    #[cfg(not(unix))]
    #[test]
    fn taskkill_args_terminate_the_whole_tree_by_pid() {
        assert_eq!(
            taskkill_args(4242),
            ["/T", "/F", "/PID", "4242"].map(String::from),
            "the tree flag, the force flag, then the numeric pid -- nothing a shell could reparse"
        );
    }

    #[test]
    fn terminate_is_safe_on_an_already_dead_child() {
        let mut child = spawn(sh("exit 0")).expect("spawn");
        let _ = child.wait();
        terminate(&mut child, Duration::from_millis(50)).expect("terminate must be idempotent");
    }

    /// Force the mtime forward explicitly rather than relying on real time to
    /// advance: some filesystems have coarse (e.g. one second) mtime
    /// resolution, which would make these tests flaky otherwise.
    fn bump_mtime(path: &Path) {
        let bumped = std::time::SystemTime::now() + Duration::from_secs(2);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(bumped)
            .expect("set_modified");
    }

    #[test]
    fn the_watcher_reports_content_only_when_it_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        assert_eq!(
            watcher
                .read_appended()
                .expect("missing file is not an error"),
            None
        );

        std::fs::write(&path, "line one\n").expect("write");
        let first = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(first.lines, "line one\n");
        assert!(!first.restarted);
        assert_eq!(watcher.read_appended().expect("read"), None, "unchanged");

        std::fs::write(&path, "line one\nline two\n").expect("append");
        let second = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(
            second.lines, "line two\n",
            "only the appended bytes, not the whole file again"
        );
        assert!(!second.restarted);
    }

    /// The performance claim, asserted: pass two reads the delta, not the file.
    #[test]
    fn a_later_poll_reads_only_the_bytes_that_were_appended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let bulk: String = (0..500).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        std::fs::write(&path, &bulk).expect("write");

        let mut watcher = Watcher::new(path.clone());
        let first = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(first.lines.len(), bulk.len());
        assert_eq!(watcher.position().0, bulk.len() as u64);

        let tail = "{\"n\":500}\n";
        std::fs::write(&path, format!("{bulk}{tail}")).expect("append");
        let second = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(
            second.lines.len(),
            tail.len(),
            "turn N must cost the turn, not the session"
        );
    }

    /// A half-written line is scored but never committed: the offset stops at
    /// the last newline so the next poll sees the whole line.
    #[test]
    fn a_partial_line_is_reported_separately_and_read_again_when_complete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        std::fs::write(&path, "done\nhal").expect("write");
        let first = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(first.lines, "done\n");
        assert_eq!(first.partial, "hal");
        assert_eq!(watcher.position().0, 5);

        std::fs::write(&path, "done\nhalf\n").expect("finish the line");
        let second = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(second.lines, "half\n");
        assert!(second.partial.is_empty());
    }

    #[test]
    fn a_same_length_rewrite_is_detected_via_mtime() {
        // Right after a compaction the transcript can be rewritten in place at
        // the same byte length. Length alone would read that as unchanged.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        std::fs::write(&path, "aaaa\n").expect("write");
        assert_eq!(
            watcher
                .read_appended()
                .expect("read")
                .expect("changed")
                .lines,
            "aaaa\n"
        );

        std::fs::write(&path, "bbbb\n").expect("rewrite, same length");
        bump_mtime(&path);

        let rewritten = watcher.read_appended().expect("read").expect("changed");
        assert_eq!(
            rewritten.lines, "bbbb\n",
            "a same-length rewrite must not be mistaken for unchanged"
        );
        assert!(rewritten.restarted, "and it voids the byte offset");
    }

    #[test]
    fn a_truncated_transcript_restarts_from_the_beginning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        std::fs::write(&path, "one\ntwo\nthree\n").expect("write");
        let _ = watcher.read_appended().expect("read");

        std::fs::write(&path, "one\n").expect("truncate");
        let after = watcher.read_appended().expect("read").expect("changed");
        assert!(after.restarted);
        assert_eq!(after.lines, "one\n");
        assert_eq!(watcher.position().0, 4);
    }

    /// A rewrite that happens to be longer keeps the offset in range, so only
    /// the fingerprint of the consumed region can catch it.
    #[test]
    fn a_longer_rewrite_is_caught_by_the_consumed_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        std::fs::write(&path, "one\ntwo\n").expect("write");
        let _ = watcher.read_appended().expect("read");

        std::fs::write(&path, "different\nhistory\nentirely\n").expect("rewrite, longer");
        let after = watcher.read_appended().expect("read").expect("changed");
        assert!(after.restarted, "the earlier bytes are gone");
        assert_eq!(after.lines, "different\nhistory\nentirely\n");
    }

    #[test]
    fn a_resumed_watcher_reads_only_what_arrived_since_that_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");

        std::fs::write(&path, "one\ntwo\n").expect("write");
        let mut first = Watcher::new(path.clone());
        let _ = first.read_appended().expect("read");
        let (offset, consumed) = first.position();

        std::fs::write(&path, "one\ntwo\nthree\n").expect("append");
        let mut resumed = Watcher::resuming(path.clone(), offset, consumed);
        let after = resumed.read_appended().expect("read").expect("changed");
        assert_eq!(after.lines, "three\n");
        assert!(!after.restarted);
    }

    #[test]
    fn a_resumed_position_that_no_longer_fits_the_file_is_discarded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "fresh\nhistory\n").expect("write");

        let mut resumed = Watcher::resuming(path.clone(), 8, 12345);
        let after = resumed.read_appended().expect("read").expect("changed");
        assert!(after.restarted, "a fingerprint that does not match is void");
        assert_eq!(after.lines, "fresh\nhistory\n");
    }

    #[test]
    fn run_shell_reports_the_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(run_shell("exit 0", dir.path()).expect("run"), 0);
        assert_eq!(run_shell("exit 9", dir.path()).expect("run"), 9);
    }

    #[test]
    fn run_shell_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_shell("touch marker", dir.path()).expect("run");
        assert!(dir.path().join("marker").exists());
    }

    #[test]
    fn a_tapped_child_still_reports_its_exit_code() {
        let (mut child, _tap, _guard) =
            spawn_tapped(sh("printf hello\\n; exit 4"), None).expect("spawn");
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::Exited(4));
    }

    #[test]
    fn tapped_lines_reach_the_matcher() {
        let (mut child, tap, _guard) =
            spawn_tapped(sh("printf 'one\\ntwo\\n'; exit 0"), None).expect("spawn");
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.len() < 2 && Instant::now() < deadline {
            seen.extend(tap.try_lines());
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.wait();
        assert!(seen.iter().any(|l| l.contains("one")), "got {seen:?}");
        assert!(seen.iter().any(|l| l.contains("two")), "got {seen:?}");
    }

    #[test]
    fn stderr_is_tapped_too_because_notices_can_land_there() {
        let (mut child, tap, _guard) =
            spawn_tapped(sh("printf 'oops\\n' >&2; exit 0"), None).expect("spawn");
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.is_empty() && Instant::now() < deadline {
            seen.extend(tap.try_lines());
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.wait();
        assert!(seen.iter().any(|l| l.contains("oops")), "got {seen:?}");
    }

    #[test]
    fn try_lines_is_empty_when_nothing_was_written() {
        let (mut child, tap, _guard) = spawn_tapped(sh("exit 0"), None).expect("spawn");
        let _ = child.wait();
        // Drain whatever arrived; a silent child must not block or panic.
        let _ = tap.try_lines();
        assert!(tap.try_lines().is_empty());
    }

    /// End-to-end version of `drain_to_eof_waits_for_a_line_that_is_still_in_
    /// flight`, against a *real* spawned child and `spawn_tapped`'s own
    /// reader threads rather than a hand-built channel -- printing its one
    /// line and exiting in the same breath, exactly `fake-codex-agent.sh`'s
    /// own "limit" mode. `child.wait()` (blocking, real exit observed) races
    /// the reader thread for real here; looped so a single lucky scheduling
    /// slice cannot hide a regression.
    #[test]
    fn drain_to_eof_catches_a_real_childs_last_line_even_though_it_already_exited() {
        for _ in 0..20 {
            let (mut child, tap, _guard) =
                spawn_tapped(sh("printf 'limit hit\\n'; exit 1"), None).expect("spawn");
            let status = child.wait().expect("wait");
            assert_eq!(status.code(), Some(1));
            let lines = tap.drain_to_eof(FINAL_DRAIN_BUDGET);
            assert!(
                lines.iter().any(|l| l.contains("limit hit")),
                "the child's last line must survive `child.wait()` already having observed the \
                 exit: got {lines:?}"
            );
        }
    }

    /// The race this whole family of tests exists to pin: `try_lines` is a
    /// pure, instantaneous, non-blocking drain (`while let Ok(_) =
    /// rx.try_recv()`), with no synchronization against `forward`'s reader
    /// thread having actually reached EOF. A line that is genuinely on its
    /// way -- the sender is about to send it, a beat from now -- is
    /// invisible to `try_lines` called before that beat elapses. This is
    /// exactly the mechanism a child that prints its last line and exits
    /// immediately hits against `child.wait()`: `try_wait`/`wait` observe
    /// the OS-level exit independently of whether this process's own reader
    /// thread has been scheduled yet to drain the pipe. Built with a plain
    /// channel rather than a real spawned child so the delay is exact and
    /// deterministic instead of racing real thread-scheduling luck -- this
    /// is the seam `exec.rs`'s and `run_loop.rs`'s own "final drain" calls
    /// (right after `supervise_child` returns) sit on top of.
    #[test]
    fn try_lines_can_miss_a_line_that_has_not_arrived_yet() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tap = OutputTap { rx };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let _ = tx.send("You've hit your session limit".to_string());
        });

        assert!(
            tap.try_lines().is_empty(),
            "the line has not been sent yet -- try_lines must not block waiting for it"
        );
    }

    /// The fix: `drain_to_eof` blocks only as long as it actually takes for
    /// the tap's sender(s) to either deliver more lines or disconnect (both
    /// `forward` reader threads finished), not the full `budget` -- so a
    /// line that arrives a beat late, exactly the race above, is still
    /// caught, and a child with nothing further to say is not delayed by
    /// more than it takes the threads to notice EOF and exit.
    #[test]
    fn drain_to_eof_waits_for_a_line_that_is_still_in_flight() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tap = OutputTap { rx };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let _ = tx.send("You've hit your session limit".to_string());
            // Dropping `tx` here (end of closure) is what lets a caller
            // with nothing further to wait for return early instead of
            // paying the rest of the budget.
        });

        let started = Instant::now();
        let lines = tap.drain_to_eof(Duration::from_millis(500));
        assert_eq!(lines, vec!["You've hit your session limit".to_string()]);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "must return as soon as the sender disconnects, not wait out the full budget: {:?}",
            started.elapsed()
        );
    }

    /// The common case -- a child that had nothing more to say -- must not
    /// pay anything close to the full budget: both reader threads finish
    /// and drop their sender almost immediately once the child's pipes hit
    /// EOF, and `drain_to_eof` must notice that disconnect and return, not
    /// wait the budget out on principle.
    #[test]
    fn drain_to_eof_returns_promptly_once_the_sender_disconnects_with_nothing_sent() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tap = OutputTap { rx };
        drop(tx);

        let started = Instant::now();
        let lines = tap.drain_to_eof(Duration::from_secs(5));
        assert!(lines.is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "an already-disconnected tap must return near-instantly: {:?}",
            started.elapsed()
        );
    }

    /// Safety bound: this is still the hot supervision path, so a sender
    /// that (pathologically) never sends and never disconnects must not
    /// hang `drain_to_eof` forever -- it returns once `budget` elapses,
    /// with whatever it collected (nothing, here).
    #[test]
    fn drain_to_eof_is_bounded_when_the_sender_never_disconnects() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tap = OutputTap { rx };
        // Kept alive for the whole test so the receiver can never observe a
        // disconnect -- the one condition that must not hang this call.
        let _keep_alive = tx;

        let started = Instant::now();
        let lines = tap.drain_to_eof(Duration::from_millis(100));
        assert!(lines.is_empty());
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "must actually wait out the budget, not give up early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must not overrun the budget by much: {elapsed:?}"
        );
    }

    /// FIX B: a `Some(stdin_text)` is written to the child's stdin and then
    /// EOF'd, exactly the shape a shim-form headless prompt takes. `cat`
    /// echoes stdin to stdout, which the tap forwards -- proving the prompt
    /// reaches the child off argv. The metacharacter is carried literally,
    /// never reparsed, because it never touched a command line. Uses the same
    /// `sh` the other tests in this module rely on.
    #[test]
    fn spawn_tapped_delivers_the_prompt_on_stdin() {
        let (mut child, tap, _guard) =
            spawn_tapped(sh("cat"), Some("refactor foo() & bar()\n".to_string())).expect("spawn");
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if child.try_wait().expect("wait").is_some() {
                break;
            }
            seen.extend(tap.try_lines());
            std::thread::sleep(Duration::from_millis(20));
        }
        seen.extend(tap.try_lines());
        let _ = child.wait();
        assert!(
            seen.iter().any(|l| l.contains("refactor foo() & bar()")),
            "the prompt reached the child verbatim on stdin: {seen:?}"
        );
    }

    // P2/P3: the supervised-child pid registry and the guard that owns a
    // child's membership in it. The console-close *sweep* itself needs a real
    // console and a real external killer (see the recipe at the top of
    // `term.rs`); the bookkeeping it reads is ordinary safe code and is
    // pinned here, on every platform, so CI runs it.
    //
    // The registry is process-global and shared with every other test in this
    // binary, so these tests only ever assert about pids they invented
    // themselves -- never about the snapshot's length.

    /// Pids used by the registry tests. Deliberately absurd, so they cannot
    /// collide with a real child some concurrently running test spawned.
    const FAKE_PID_A: u32 = 4_000_000_001;
    const FAKE_PID_B: u32 = 4_000_000_002;

    #[test]
    fn a_registered_pid_shows_up_in_the_snapshot_and_leaves_on_deregistration() {
        register_child_pid(FAKE_PID_A);
        assert!(
            supervised_pid_snapshot().contains(&FAKE_PID_A),
            "the console-close sweep has to be able to see it"
        );

        deregister_child_pid(FAKE_PID_A);
        assert!(
            !supervised_pid_snapshot().contains(&FAKE_PID_A),
            "a confirmed-dead child must not be tree-killed again at close"
        );
    }

    /// A restart re-registers the same slot; a double registration would make
    /// the close sweep spawn two `taskkill`s for one pid.
    #[test]
    fn registering_the_same_pid_twice_records_it_once() {
        register_child_pid(FAKE_PID_B);
        register_child_pid(FAKE_PID_B);
        assert_eq!(
            supervised_pid_snapshot()
                .iter()
                .filter(|pid| **pid == FAKE_PID_B)
                .count(),
            1
        );
        deregister_child_pid(FAKE_PID_B);
        // Idempotent: every exit arm calls `release`, and `Drop` may call it
        // again right after.
        deregister_child_pid(FAKE_PID_B);
        assert!(!supervised_pid_snapshot().contains(&FAKE_PID_B));
    }

    /// The guard is what production actually uses: adopt registers, release
    /// deregisters, and a second release is a no-op.
    #[test]
    fn a_child_guard_registers_on_adopt_and_deregisters_on_release() {
        let pid = FAKE_PID_A + 10;
        let mut guard = ChildGuard::adopt(Some(pid));
        assert_eq!(guard.pid(), Some(pid));
        assert!(supervised_pid_snapshot().contains(&pid));

        guard.release();
        assert_eq!(guard.pid(), None);
        assert!(!supervised_pid_snapshot().contains(&pid));
        guard.release();
        assert_eq!(guard.pid(), None, "release is idempotent");
    }

    /// `panic = "abort"` means `Drop` is no safety net, but it is still the
    /// backstop for every ordinary scope exit -- and `spawn_tapped`'s callers
    /// lean on it at the end of each `exec`/`loop` cycle.
    #[test]
    fn dropping_a_child_guard_deregisters_its_pid() {
        let pid = FAKE_PID_A + 11;
        {
            let _guard = ChildGuard::adopt(Some(pid));
            assert!(supervised_pid_snapshot().contains(&pid));
        }
        assert!(!supervised_pid_snapshot().contains(&pid));
    }

    /// A pty backend that cannot report a pid (`Child::process_id() ->
    /// None`) must degrade to a guard that holds nothing, not to a panic or a
    /// bogus registration.
    #[test]
    fn a_pidless_child_gets_an_inert_guard() {
        let mut guard = ChildGuard::adopt(None);
        assert_eq!(guard.pid(), None);
        assert!(!guard.job_is_active());
        guard.release();
    }

    #[test]
    fn an_inert_job_guard_is_never_active_and_closes_harmlessly() {
        let mut job = JobGuard::inert();
        assert!(!job.is_active());
        job.close();
        job.close();
    }

    /// Off Windows there is no job object at all: portable-pty's `setsid` +
    /// `TIOCSCTTY` already ties the child's session to the pty, so `adopt`
    /// must compile and no-op rather than pretend to hold anything.
    #[cfg(not(windows))]
    #[test]
    fn job_objects_are_a_windows_only_no_op_elsewhere() {
        let job = JobGuard::adopt(std::process::id());
        assert!(!job.is_active());
    }

    /// P3 end to end: a real child, in a real kill-on-close job, dies when
    /// the last handle to that job closes -- which is what happens when zirv
    /// is killed with `taskkill /F` or aborts, running no code of its own.
    ///
    /// Spawned with `std::process::Command` (not a pty) so the only thing
    /// under test is the job. `is_alive` is `sessions`' own probe.
    #[cfg(windows)]
    #[test]
    fn closing_a_kill_on_close_job_kills_the_child_it_holds() {
        // `waitfor` blocks on a signal that is never raised: a child that
        // exits only when something kills it, with no console interaction and
        // no dependency on a shell builtin. It ships in System32, but a
        // stripped image (or a `PATH` that does not include System32) must
        // skip rather than fail -- an environment probe is not the thing
        // under test.
        let Ok(mut child) = Command::new("waitfor")
            .arg("/T")
            .arg("120")
            .arg("zirvJobObjectProbe")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            eprintln!("skipping: waitfor.exe is not available on this machine");
            return;
        };
        let pid = child.id();

        let job = JobGuard::adopt(pid);
        // An ambient job that refuses nesting is the documented degradation,
        // not a failure -- but the probe child still has to be reaped either
        // way, so the cleanup below is shared rather than duplicated into an
        // early return.
        let active = job.is_active();
        drop(job);

        let mut reaped = false;
        if active {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if child.try_wait().expect("try_wait").is_some() {
                    reaped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        if !reaped {
            let _ = child.kill();
        }
        let _ = child.wait();

        if !active {
            eprintln!("skipping: this process could not create or assign a job object");
            return;
        }
        assert!(
            reaped,
            "closing the last kill-on-close job handle must reap the child"
        );
    }
}
