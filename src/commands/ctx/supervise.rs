use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
        if !taskkill_tree(child.id()) {
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

/// Runs `taskkill /T /F /PID <pid>` without a shell, waiting briefly for it to
/// finish. Returns whether taskkill ran *and* reported success; `false` (it is
/// not on PATH, or it failed) tells the caller to fall back to a direct
/// `child.kill()`.
#[cfg(not(unix))]
fn taskkill_tree(pid: u32) -> bool {
    Command::new("taskkill")
        .args(taskkill_args(pid))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
pub fn spawn_tapped(
    mut command: Command,
    stdin_text: Option<String>,
) -> CtxResult<(Child, OutputTap)> {
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

    Ok((child, OutputTap { rx }))
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
        let mut child = spawn(sh("sleep 0.5")).expect("spawn");
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
        assert!(ticks >= 3, "expected several ticks in 500ms, got {ticks}");
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
        let (mut child, _tap) = spawn_tapped(sh("printf hello\\n; exit 4"), None).expect("spawn");
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
        let (mut child, tap) =
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
        let (mut child, tap) =
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
        let (mut child, tap) = spawn_tapped(sh("exit 0"), None).expect("spawn");
        let _ = child.wait();
        // Drain whatever arrived; a silent child must not block or panic.
        let _ = tap.try_lines();
        assert!(tap.try_lines().is_empty());
    }

    /// FIX B: a `Some(stdin_text)` is written to the child's stdin and then
    /// EOF'd, exactly the shape a shim-form headless prompt takes. `cat`
    /// echoes stdin to stdout, which the tap forwards -- proving the prompt
    /// reaches the child off argv. The metacharacter is carried literally,
    /// never reparsed, because it never touched a command line. Uses the same
    /// `sh` the other tests in this module rely on.
    #[test]
    fn spawn_tapped_delivers_the_prompt_on_stdin() {
        let (mut child, tap) =
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
}
