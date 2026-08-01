// Consumed by the exec verb added in a later task of this plan; nothing calls
// this yet outside tests, so dead_code is silenced module-wide until then,
// matching config.rs/state.rs/log.rs/event.rs/handoff.rs.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

pub fn spawn(mut command: Command) -> CtxResult<Child> {
    Ok(command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?)
}

/// Polls the child, calling `on_tick` at every interval. Stops on child exit,
/// on the deadline, or when a tick asks to stop; kills the child in the last two
/// cases so no supervisor ever leaks a process.
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
        let _ = child.kill();
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

/// Polls a growing transcript. Returns the whole file whenever its length
/// changed, because scoring needs the full turn history, not just the delta.
pub struct Watcher {
    path: PathBuf,
    len: u64,
}

impl Watcher {
    pub fn new(path: PathBuf) -> Self {
        Self { path, len: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_if_changed(&mut self) -> CtxResult<Option<String>> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Ok(None);
        };
        if meta.len() == self.len {
            return Ok(None);
        }
        self.len = meta.len();
        Ok(Some(std::fs::read_to_string(&self.path)?))
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

/// Like `spawn`, but the child's stdout and stderr are piped so they can be
/// matched. Each stream is forwarded to this process's corresponding stream
/// unchanged, line by line.
pub fn spawn_tapped(mut command: Command) -> CtxResult<(Child, OutputTap)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

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

    #[test]
    fn terminate_is_safe_on_an_already_dead_child() {
        let mut child = spawn(sh("exit 0")).expect("spawn");
        let _ = child.wait();
        terminate(&mut child, Duration::from_millis(50)).expect("terminate must be idempotent");
    }

    #[test]
    fn the_watcher_reports_content_only_when_it_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        assert_eq!(
            watcher
                .read_if_changed()
                .expect("missing file is not an error"),
            None
        );

        std::fs::write(&path, "line one\n").expect("write");
        assert_eq!(
            watcher.read_if_changed().expect("read"),
            Some("line one\n".to_string())
        );
        assert_eq!(watcher.read_if_changed().expect("read"), None, "unchanged");

        std::fs::write(&path, "line one\nline two\n").expect("append");
        assert_eq!(
            watcher.read_if_changed().expect("read"),
            Some("line one\nline two\n".to_string()),
            "the whole file, since scoring needs the full history"
        );
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
        let (mut child, _tap) = spawn_tapped(sh("printf hello\\n; exit 4")).expect("spawn");
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
        let (mut child, tap) = spawn_tapped(sh("printf 'one\\ntwo\\n'; exit 0")).expect("spawn");
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
        let (mut child, tap) = spawn_tapped(sh("printf 'oops\\n' >&2; exit 0")).expect("spawn");
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
        let (mut child, tap) = spawn_tapped(sh("exit 0")).expect("spawn");
        let _ = child.wait();
        // Drain whatever arrived; a silent child must not block or panic.
        let _ = tap.try_lines();
        assert!(tap.try_lines().is_empty());
    }
}
