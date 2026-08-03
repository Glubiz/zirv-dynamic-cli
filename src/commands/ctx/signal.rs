use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::rot::Verdict;

/// macOS `sockaddr_un.sun_path` is 104 bytes. Fail early with a readable error
/// instead of an opaque OS error from inside a supervisor.
#[cfg(unix)]
pub const MAX_SOCKET_PATH: usize = 100;

/// Per connection, so one noisy client cannot make a supervisor buffer without
/// bound. A turn signal is a few hundred bytes.
pub const MAX_SIGNAL_BYTES: u64 = 64 * 1024;

/// Per read, so one silent client cannot hold the accept loop forever and
/// starve every later signal.
pub const CLIENT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSignal {
    pub session_id: String,
    pub turn: u64,
    pub score: u32,
    pub verdict: Verdict,
    /// Where the agent is writing this session's transcript. The agent mints
    /// its own session id, so a supervisor cannot derive this: the hook that
    /// runs inside the agent is the only party that knows it. Optional so a
    /// signal from an older sender still parses.
    #[serde(default)]
    pub transcript_path: Option<String>,
}

#[cfg(unix)]
fn check_len(path: &Path) -> CtxResult<()> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(format!(
            "socket path is too long ({len} bytes, limit {MAX_SOCKET_PATH}): {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
pub struct SignalServer {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<TurnSignal>,
}

#[cfg(unix)]
impl SignalServer {
    pub fn bind(path: &Path) -> CtxResult<Self> {
        use std::io::{BufRead, Read};

        check_len(path)?;
        if let Some(parent) = path.parent() {
            super::state::create_private_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let (tx, rx) = std::sync::mpsc::channel();

        // The accept loop lives for the process. A foreground supervisor owns
        // the socket for its whole run, so there is nothing to join.
        //
        // Connections are handled one at a time on purpose (a supervisor sees
        // one hook at a time), which is exactly why both bounds below matter:
        // without them a client that connects and then says nothing, or one
        // that streams without ever sending a newline, would keep every later
        // turn signal from ever being accepted.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT));
                let reader = std::io::BufReader::new(stream.take(MAX_SIGNAL_BYTES));
                for line in reader.lines().map_while(Result::ok) {
                    let Ok(signal) = serde_json::from_str::<TurnSignal>(&line) else {
                        continue;
                    };
                    if tx.send(signal).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            path: path.to_path_buf(),
            rx,
        })
    }

    pub fn try_recv(&self) -> Option<TurnSignal> {
        self.rx.try_recv().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for SignalServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
pub fn send(path: &Path, signal: &TurnSignal) -> CtxResult<()> {
    use std::io::Write;

    check_len(path)?;
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    writeln!(stream, "{}", serde_json::to_string(signal)?)?;
    stream.flush()?;
    Ok(())
}

/// Windows has no unix domain sockets, so the same surface rides a named pipe
/// (`\\.\pipe\zirv-ctx-<session>`) instead. Everything above this line is
/// unchanged: `bind` still takes the state dir's socket path, `path` still
/// returns it, and the hook still receives that path in its environment and
/// hands it straight back to `send`, which derives the same pipe name from it.
///
/// The path itself is still created, as an ordinary file: `zirv ctx status`
/// lists supervised sessions by reading that directory, and a live supervisor
/// on Windows has to show up there the same way it does on unix.
#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::{CLIENT_READ_TIMEOUT, CtxResult, MAX_SIGNAL_BYTES, TurnSignal};

    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
        HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OVERLAPPED, PIPE_ACCESS_INBOUND, ReadFile,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

    pub const PIPE_PREFIX: &str = r"\\.\pipe\";
    /// Win32's own limit on a pipe name, prefix included. Checked up front so
    /// an over-long state dir fails the way an over-long `sun_path` does on
    /// unix, with a readable message instead of an opaque OS error.
    pub const MAX_PIPE_NAME: usize = 256;

    const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
    /// How long `send` keeps retrying a pipe that exists but has no free
    /// instance (the accept loop is between connections). Microseconds in
    /// practice; the budget only matters when there is no server at all, and
    /// every caller of `send` already ignores the error.
    const CONNECT_RETRY: Duration = Duration::from_secs(1);
    const POLL: Duration = Duration::from_millis(10);

    /// The pipe a socket path names. A path that is already a pipe name is
    /// returned as-is, so a value that has been through `SignalServer::path`
    /// and the hook's environment round-trips.
    pub fn pipe_name(path: &Path) -> CtxResult<String> {
        let raw = path.to_string_lossy();
        if raw.starts_with(PIPE_PREFIX) || raw.starts_with(r"\\?\pipe\") {
            return Ok(raw.into_owned());
        }
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .ok_or_else(|| format!("socket path names no session: {}", path.display()))?;
        let name = format!("{PIPE_PREFIX}zirv-ctx-{stem}");
        if name.chars().count() > MAX_PIPE_NAME {
            return Err(format!(
                "socket path is too long ({} bytes, limit {MAX_PIPE_NAME}): {name}",
                name.chars().count()
            )
            .into());
        }
        Ok(name)
    }

    fn wide(name: &str) -> Vec<u16> {
        std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A manual-reset event used as the completion signal for one overlapped
    /// I/O operation. Windows events are fine to signal and wait on from a
    /// different thread than the one that created them.
    struct Event(OwnedHandle);

    impl Event {
        fn create() -> CtxResult<Self> {
            // SAFETY: every argument is null/0: an anonymous, manual-reset,
            // initially-unsignaled event. The returned handle is checked below.
            let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if handle.is_null() {
                return Err(format!(
                    "could not create event: {}",
                    std::io::Error::last_os_error()
                )
                .into());
            }
            // SAFETY: a valid, exclusively owned event handle that nothing else has.
            Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle as _) }))
        }

        fn raw(&self) -> HANDLE {
            self.0.as_raw_handle() as HANDLE
        }
    }

    /// One listening instance, opened for overlapped I/O so its connect can be
    /// posted without blocking the calling thread (see `create_and_post_connect`).
    /// Every read on an accepted connection has to stay overlapped too. Windows
    /// rejects a `ReadFile` on a `FILE_FLAG_OVERLAPPED` handle unless it supplies
    /// an `OVERLAPPED`, so `drain` below cannot fall back to a plain read once
    /// the connect side needs this mode.
    fn create_instance(name: &str) -> CtxResult<OwnedHandle> {
        let wide = wide(name);
        // SAFETY: `wide` is NUL-terminated and outlives the call; the security
        // attributes pointer is explicitly null (default DACL: this user only).
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                PIPE_BUFFER_BYTES,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "could not create {name}: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        // SAFETY: a valid, exclusively owned pipe handle that nothing else has.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
    }

    /// A `ConnectNamedPipe` already posted on a freshly created instance. A
    /// client that connects, writes, and fully disconnects before the server
    /// has ever called `ConnectNamedPipe` on that instance makes the call fail
    /// with `ERROR_PIPE_CLOSING`, silently losing the connection and its data.
    /// `create_and_post_connect` closes that window to zero by posting the
    /// connect synchronously, on the same call that creates the instance,
    /// before any client could know the instance exists.
    pub struct PendingConnect {
        pipe: OwnedHandle,
        event: Event,
        overlapped: Box<OVERLAPPED>,
        already_connected: bool,
    }

    // SAFETY: `PendingConnect` exclusively owns its pipe, event, and boxed
    // `OVERLAPPED`; handing one to another thread (`spawn_acceptor`) transfers
    // that ownership outright, it is never aliased or touched concurrently
    // from two threads at once.
    unsafe impl Send for PendingConnect {}

    /// Creates a fresh instance of `name` and posts its connect immediately.
    pub fn create_and_post_connect(name: &str) -> CtxResult<PendingConnect> {
        let pipe = create_instance(name)?;
        let event = Event::create()?;
        // SAFETY: zero-initialized is the documented way to prepare an
        // `OVERLAPPED` before its first use.
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        overlapped.hEvent = event.raw();

        // SAFETY: `pipe` is live for the call; `overlapped` is heap-allocated
        // and kept alive inside the returned `PendingConnect` for as long as
        // the operation can still be pending, which `OVERLAPPED` requires.
        let ok =
            unsafe { ConnectNamedPipe(pipe.as_raw_handle() as _, overlapped.as_mut() as *mut _) };
        if ok != 0 {
            // Completed synchronously: vanishingly rare (would need another
            // client to connect in the microseconds since `create_instance`
            // returned), but not an error.
            return Ok(PendingConnect {
                pipe,
                event,
                overlapped,
                already_connected: true,
            });
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(code) if code == ERROR_IO_PENDING as i32 => Ok(PendingConnect {
                pipe,
                event,
                overlapped,
                already_connected: false,
            }),
            Some(code) if code == ERROR_PIPE_CONNECTED as i32 => Ok(PendingConnect {
                pipe,
                event,
                overlapped,
                already_connected: true,
            }),
            _ => Err(format!(
                "connect failed on {name}: {}",
                std::io::Error::last_os_error()
            )
            .into()),
        }
    }

    /// Blocks until `pending`'s connect completes, however long that takes.
    pub fn wait_connect(pending: PendingConnect) -> CtxResult<OwnedHandle> {
        if !pending.already_connected {
            // SAFETY: `pending.event` outlives the call.
            unsafe { WaitForSingleObject(pending.event.raw(), INFINITE) };
            let mut transferred: u32 = 0;
            // SAFETY: `pending.pipe`/`overlapped` are both still live; `bWait`
            // is `FALSE` because the event above already signaled completion.
            let ok = unsafe {
                GetOverlappedResult(
                    pending.pipe.as_raw_handle() as _,
                    pending.overlapped.as_ref() as *const _ as *mut _,
                    &mut transferred,
                    0,
                )
            };
            if ok == 0 {
                return Err(format!(
                    "connect did not complete: {}",
                    std::io::Error::last_os_error()
                )
                .into());
            }
        }
        Ok(pending.pipe)
    }

    /// One overlapped read, waiting up to `timeout` for either data or EOF.
    /// `Ok(None)` means the timeout elapsed with nothing read and the
    /// connection is still open; `Ok(Some(0))` is a clean EOF/disconnect.
    fn read_bounded(
        pipe: &OwnedHandle,
        buf: &mut [u8],
        event: &Event,
        timeout: Duration,
    ) -> CtxResult<Option<usize>> {
        // SAFETY: zero-initialized is the documented way to prepare an
        // `OVERLAPPED` before its first use.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.raw();

        // SAFETY: `pipe`, `buf`, and `overlapped` are all live for the call.
        let ok = unsafe {
            ReadFile(
                pipe.as_raw_handle() as _,
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        if ok != 0 {
            let mut transferred: u32 = 0;
            // SAFETY: the read already completed synchronously; this only
            // recovers the byte count, it does not wait.
            unsafe {
                GetOverlappedResult(pipe.as_raw_handle() as _, &overlapped, &mut transferred, 0)
            };
            return Ok(Some(transferred as usize));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(code) if code == ERROR_IO_PENDING as i32 => {}
            Some(code)
                if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32 =>
            {
                return Ok(Some(0));
            }
            _ => {
                return Err(format!("read failed: {}", std::io::Error::last_os_error()).into());
            }
        }

        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `event` outlives the call.
        let waited = unsafe { WaitForSingleObject(event.raw(), timeout_ms) };
        if waited == WAIT_TIMEOUT {
            // SAFETY: cancels only this operation, on this handle.
            unsafe { CancelIoEx(pipe.as_raw_handle() as _, &overlapped) };
            let mut transferred: u32 = 0;
            // Waits out the cancellation so the kernel is done writing into
            // `overlapped` before it goes out of scope.
            unsafe {
                GetOverlappedResult(pipe.as_raw_handle() as _, &overlapped, &mut transferred, 1)
            };
            return Ok(None);
        }
        let mut transferred: u32 = 0;
        // SAFETY: the event is signaled, so the operation has completed.
        let ok = unsafe {
            GetOverlappedResult(pipe.as_raw_handle() as _, &overlapped, &mut transferred, 0)
        };
        if ok == 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(code)
                    if code == ERROR_BROKEN_PIPE as i32
                        || code == ERROR_PIPE_NOT_CONNECTED as i32 =>
                {
                    Ok(Some(0))
                }
                _ => Err(
                    format!("read did not complete: {}", std::io::Error::last_os_error()).into(),
                ),
            };
        }
        Ok(Some(transferred as usize))
    }

    /// Drains one connection into `emit`, newline-delimited, bounded both ways:
    /// `MAX_SIGNAL_BYTES` of content and `CLIENT_READ_TIMEOUT` of silence. The
    /// timeout restarts whenever the client makes progress, because each read
    /// gets its own fresh `CLIENT_READ_TIMEOUT` budget -- only a read that
    /// times out with nothing at all abandons the connection.
    pub fn drain(pipe: OwnedHandle, mut emit: impl FnMut(TurnSignal) -> bool) {
        let Ok(event) = Event::create() else {
            return;
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut budget = MAX_SIGNAL_BYTES;
        let mut chunk = [0u8; 4096];

        loop {
            let want = chunk.len().min(budget as usize);
            if want == 0 {
                // Past the byte budget: whatever else this client has to say is
                // abandoned rather than buffered.
                return;
            }
            let read = match read_bounded(&pipe, &mut chunk[..want], &event, CLIENT_READ_TIMEOUT) {
                Ok(Some(0)) | Ok(None) | Err(_) => return,
                Ok(Some(read)) => read,
            };
            budget -= read as u64;
            pending.extend_from_slice(&chunk[..read]);

            while let Some(at) = pending.iter().position(|byte| *byte == b'\n') {
                let line = pending.drain(..=at).collect::<Vec<u8>>();
                let Ok(text) = std::str::from_utf8(&line) else {
                    continue;
                };
                let Ok(signal) = serde_json::from_str::<TurnSignal>(text.trim_end()) else {
                    continue;
                };
                if !emit(signal) {
                    return;
                }
            }
        }
    }

    /// Opens the client end, waiting out the moment between connections when
    /// every instance is busy.
    pub fn connect(name: &str) -> std::io::Result<std::fs::File> {
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

        let deadline = Instant::now() + CONNECT_RETRY;
        loop {
            let error = match std::fs::OpenOptions::new().write(true).open(name) {
                Ok(file) => return Ok(file),
                Err(error) => error,
            };
            let transient = matches!(
                error.raw_os_error(),
                Some(code)
                    if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32
            );
            if !transient || Instant::now() >= deadline {
                return Err(error);
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub struct SignalServer {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<TurnSignal>,
}

/// Waits out an already-posted connect on a dedicated thread and pushes the
/// accepted handle onto `accepted`, retrying with a freshly posted connect on
/// a genuine failure. `pending`'s connect was posted synchronously by the
/// caller before this thread was even spawned (see `win::create_and_post_connect`
/// and the module doc on `win::PendingConnect`), so there is no window in
/// which a client could connect, write, and disconnect unobserved -- only the
/// (unbounded) wait for a real client is deferred to this thread.
#[cfg(windows)]
fn spawn_acceptor(
    name: String,
    pending: win::PendingConnect,
    accepted: std::sync::mpsc::Sender<std::os::windows::io::OwnedHandle>,
) {
    std::thread::spawn(move || {
        let mut pending = pending;
        loop {
            match win::wait_connect(pending) {
                Ok(pipe) => {
                    let _ = accepted.send(pipe);
                    return;
                }
                Err(_) => {
                    let Ok(fresh) = win::create_and_post_connect(&name) else {
                        return;
                    };
                    pending = fresh;
                }
            }
        }
    });
}

#[cfg(windows)]
impl SignalServer {
    pub fn bind(path: &Path) -> CtxResult<Self> {
        let name = win::pipe_name(path)?;
        if let Some(parent) = path.parent() {
            super::state::create_private_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        // The connect is posted before `bind` returns, so a hook that fires
        // immediately after the agent is spawned always finds it in flight.
        let first = win::create_and_post_connect(&name)?;
        // Not the transport, just the same directory entry unix leaves behind:
        // `ctx status` enumerates supervised sessions from it.
        super::state::write_private(path, &name)?;

        let (tx, rx) = std::sync::mpsc::channel();
        // Accepted-but-not-yet-drained connections, in acceptance order. A
        // dedicated acceptor thread per instance (see `spawn_acceptor`) feeds
        // this queue; a single drainer thread below empties it serially, which
        // is what keeps signals in the order they were sent.
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();

        let listening = name.clone();
        spawn_acceptor(listening.clone(), first, accepted_tx.clone());

        std::thread::spawn(move || {
            for pipe in accepted_rx {
                // Posted before the drain starts, not after it finishes, so
                // the next instance's connect is never delayed by how long
                // this one takes -- and posted synchronously right here,
                // before handing off to a new acceptor thread, so there is no
                // gap in which a fast client could connect and disconnect
                // before any `ConnectNamedPipe` was ever in flight for it.
                let Ok(next) = win::create_and_post_connect(&listening) else {
                    return;
                };
                spawn_acceptor(listening.clone(), next, accepted_tx.clone());

                let mut alive = true;
                win::drain(pipe, |signal| {
                    alive = tx.send(signal).is_ok();
                    alive
                });
                if !alive {
                    return;
                }
            }
        });

        Ok(Self {
            path: path.to_path_buf(),
            rx,
        })
    }

    pub fn try_recv(&self) -> Option<TurnSignal> {
        self.rx.try_recv().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(windows)]
impl Drop for SignalServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
pub fn send(path: &Path, signal: &TurnSignal) -> CtxResult<()> {
    use std::io::Write;

    let name = win::pipe_name(path)?;
    let mut pipe = win::connect(&name)?;
    writeln!(pipe, "{}", serde_json::to_string(signal)?)?;
    pipe.flush()?;
    Ok(())
}

/// Neither transport exists here, so `wrap` reports the failure once and runs
/// as pure passthrough for the rest of the session (`InjectionState::degraded`).
#[cfg(not(any(unix, windows)))]
#[derive(Debug)]
pub struct SignalServer {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<TurnSignal>,
}

#[cfg(not(any(unix, windows)))]
impl SignalServer {
    pub fn bind(_path: &Path) -> CtxResult<Self> {
        Err("turn signals need a unix domain socket or a Windows named pipe".into())
    }

    pub fn try_recv(&self) -> Option<TurnSignal> {
        self.rx.try_recv().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(not(any(unix, windows)))]
pub fn send(_path: &Path, _signal: &TurnSignal) -> CtxResult<()> {
    Err("turn signals need a unix domain socket or a Windows named pipe".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::rot::Verdict;

    fn sample(turn: u64) -> TurnSignal {
        TurnSignal {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            turn,
            score: 64,
            verdict: Verdict::Compact,
            transcript_path: Some("/tmp/t.jsonl".to_string()),
        }
    }

    /// Waits for one signal, so a test never hangs on a supervisor that is
    /// stuck instead of failing.
    #[cfg(any(unix, windows))]
    fn recv_within(server: &SignalServer, timeout: std::time::Duration) -> Option<TurnSignal> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Some(signal) = server.try_recv() {
                return Some(signal);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn signals_round_trip_through_json() {
        let json = serde_json::to_string(&sample(3)).expect("serialize");
        assert!(json.contains("\"verdict\":\"compact\""), "got {json}");
        let back: TurnSignal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, sample(3));
    }

    /// The supervisor cannot derive the agent's own transcript path, so the
    /// signal has to carry it.
    #[test]
    fn a_signal_carries_the_transcript_the_agent_is_writing() {
        let json = serde_json::to_string(&sample(3)).expect("serialize");
        assert!(json.contains("/tmp/t.jsonl"), "got {json}");

        let legacy: TurnSignal = serde_json::from_str(
            "{\"session_id\":\"s\",\"turn\":1,\"score\":0,\"verdict\":\"healthy\"}",
        )
        .expect("a sender that omits the field still parses");
        assert_eq!(legacy.transcript_path, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_bound_server_receives_sent_signals_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.sock");
        let server = SignalServer::bind(&path).expect("bind");
        assert_eq!(server.path(), path.as_path());
        assert!(path.exists(), "the socket file is created");

        for turn in 1..=3 {
            send(&path, &sample(turn)).expect("send");
        }

        let mut received = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while received.len() < 3 && std::time::Instant::now() < deadline {
            if let Some(signal) = server.try_recv() {
                received.push(signal.turn);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        assert_eq!(received, vec![1, 2, 3]);
    }

    #[cfg(unix)]
    #[test]
    fn try_recv_is_non_blocking_when_nothing_arrived() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = SignalServer::bind(&dir.path().join("q.sock")).expect("bind");
        assert!(server.try_recv().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn sending_to_a_dead_socket_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = send(&dir.path().join("missing.sock"), &sample(1)).expect_err("no listener");
        assert!(!err.to_string().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rebinding_replaces_a_stale_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale.sock");
        std::fs::write(&path, "leftover").expect("write");
        let _server = SignalServer::bind(&path).expect("bind over a stale file");
        send(&path, &sample(9)).expect("send");
    }

    #[cfg(unix)]
    #[test]
    fn an_over_long_socket_path_fails_with_a_clear_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let long = dir.path().join("x".repeat(MAX_SOCKET_PATH + 20));
        let err = SignalServer::bind(&long).expect_err("too long");
        assert!(
            err.to_string().contains("too long"),
            "message should say why: {err}"
        );
    }

    /// A hook that connects and then wedges (or a stray `nc`) must not be able
    /// to silence every later turn boundary.
    #[cfg(unix)]
    #[test]
    fn a_silent_client_cannot_starve_the_signals_behind_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("silent.sock");
        let server = SignalServer::bind(&path).expect("bind");

        let silent = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        // Let the accept loop pick the silent client up before the real one.
        std::thread::sleep(std::time::Duration::from_millis(200));

        send(&path, &sample(7)).expect("send");
        let received = recv_within(&server, std::time::Duration::from_secs(15));
        drop(silent);
        assert_eq!(
            received.map(|s| s.turn),
            Some(7),
            "the silent connection must time out instead of blocking the loop"
        );
    }

    /// The read is bounded, so a client that streams without ever sending a
    /// newline is abandoned rather than buffered without limit.
    #[cfg(unix)]
    #[test]
    fn a_flooding_client_is_abandoned_at_the_byte_limit() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("flood.sock");
        let server = SignalServer::bind(&path).expect("bind");

        {
            let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("connect");
            let mut junk = vec![b'x'; MAX_SIGNAL_BYTES as usize + 1];
            junk.push(b'\n');
            let _ = stream.write_all(&junk);
            // Past the budget, so this must never be read.
            let _ = writeln!(
                stream,
                "{}",
                serde_json::to_string(&sample(42)).expect("serialize")
            );
            let _ = stream.flush();
        }

        send(&path, &sample(9)).expect("send");
        let received = recv_within(&server, std::time::Duration::from_secs(15));
        assert_eq!(
            received.map(|s| s.turn),
            Some(9),
            "the flooded connection's trailing signal must be dropped, not queued ahead of a well-behaved client"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_server_removes_the_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone.sock");
        {
            let _server = SignalServer::bind(&path).expect("bind");
            assert!(path.exists());
        }
        assert!(!path.exists());
    }

    /// Windows coverage for the named-pipe transport. Every test here bounds
    /// its own wait: the whole point of this transport is that a supervisor
    /// never blocks on it, so a regression has to fail rather than hang.
    #[cfg(windows)]
    mod win {
        use super::super::win::{MAX_PIPE_NAME, PIPE_PREFIX, pipe_name};
        use super::*;
        use std::time::Duration;

        /// The pipe namespace is machine-global, so two tests that picked the
        /// same session id would share a pipe. Unique per test, per process.
        fn unique_session() -> String {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            format!(
                "t{}x{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            )
        }

        fn socket_path(dir: &std::path::Path) -> std::path::PathBuf {
            dir.join(format!("{}.sock", unique_session()))
        }

        #[test]
        fn a_socket_path_names_a_pipe_under_the_zirv_prefix() {
            let name =
                pipe_name(std::path::Path::new(r"C:\state\sockets\abcdef12.sock")).expect("a name");
            assert_eq!(name, format!(r"{PIPE_PREFIX}zirv-ctx-abcdef12"));
        }

        /// `path()` goes into the hook's environment and comes back to `send`,
        /// so both spellings have to resolve to the same pipe.
        #[test]
        fn a_pipe_name_survives_a_round_trip_through_the_hook_environment() {
            let from_path =
                pipe_name(std::path::Path::new(r"C:\state\sockets\abcdef12.sock")).expect("name");
            let again = pipe_name(std::path::Path::new(&from_path)).expect("name");
            assert_eq!(from_path, again, "an already-resolved pipe name is stable");
        }

        #[test]
        fn an_over_long_socket_path_fails_with_a_clear_message() {
            let dir = tempfile::tempdir().expect("tempdir");
            let long = dir
                .path()
                .join(format!("{}.sock", "x".repeat(MAX_PIPE_NAME)));
            let err = SignalServer::bind(&long).expect_err("too long");
            assert!(
                err.to_string().contains("too long"),
                "message should say why: {err}"
            );
        }

        #[test]
        fn a_bound_server_receives_sent_signals_in_order() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = socket_path(dir.path());
            let server = SignalServer::bind(&path).expect("bind");
            assert_eq!(server.path(), path.as_path());
            assert!(path.exists(), "ctx status still sees a supervised session");

            for turn in 1..=3 {
                send(&path, &sample(turn)).expect("send");
            }

            let mut received = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while received.len() < 3 && std::time::Instant::now() < deadline {
                if let Some(signal) = server.try_recv() {
                    received.push(signal.turn);
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            assert_eq!(received, vec![1, 2, 3]);
        }

        /// The pump calls this on every ~100ms tick, so it must never block.
        #[test]
        fn try_recv_is_non_blocking_when_nothing_arrived() {
            let dir = tempfile::tempdir().expect("tempdir");
            let server = SignalServer::bind(&socket_path(dir.path())).expect("bind");
            let started = std::time::Instant::now();
            assert!(server.try_recv().is_none());
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "try_recv blocked for {:?}",
                started.elapsed()
            );
        }

        #[test]
        fn sending_to_a_dead_socket_is_an_error_not_a_hang() {
            let dir = tempfile::tempdir().expect("tempdir");
            let started = std::time::Instant::now();
            let err = send(&socket_path(dir.path()), &sample(1)).expect_err("no listener");
            assert!(!err.to_string().is_empty());
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "send should give up quickly, took {:?}",
                started.elapsed()
            );
        }

        #[test]
        fn rebinding_replaces_a_stale_socket_file() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = socket_path(dir.path());
            std::fs::write(&path, "leftover").expect("write");
            let _server = SignalServer::bind(&path).expect("bind over a stale file");
            send(&path, &sample(9)).expect("send");
        }

        #[test]
        fn dropping_the_server_removes_the_socket_file() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = socket_path(dir.path());
            {
                let _server = SignalServer::bind(&path).expect("bind");
                assert!(path.exists());
            }
            assert!(!path.exists());
        }

        /// A hook that connects and then wedges must not be able to silence
        /// every later turn boundary: the drain is bounded, so the accept loop
        /// comes back.
        #[test]
        fn a_silent_client_cannot_starve_the_signals_behind_it() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = socket_path(dir.path());
            let server = SignalServer::bind(&path).expect("bind");

            let name = pipe_name(&path).expect("name");
            let silent = std::fs::OpenOptions::new()
                .write(true)
                .open(&name)
                .expect("connect");
            // Let the accept loop pick the silent client up before the real one.
            std::thread::sleep(Duration::from_millis(200));

            send(&path, &sample(7)).expect("send");
            let received = recv_within(&server, Duration::from_secs(15));
            drop(silent);
            assert_eq!(
                received.map(|s| s.turn),
                Some(7),
                "the silent connection must time out instead of blocking the loop"
            );
        }

        /// The read is bounded, so a client that streams without ever sending a
        /// newline is abandoned rather than buffered without limit.
        #[test]
        fn a_flooding_client_is_abandoned_at_the_byte_limit() {
            use std::io::Write as _;

            let dir = tempfile::tempdir().expect("tempdir");
            let path = socket_path(dir.path());
            let server = SignalServer::bind(&path).expect("bind");
            let name = pipe_name(&path).expect("name");

            {
                let mut pipe = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&name)
                    .expect("connect");
                let mut junk = vec![b'x'; MAX_SIGNAL_BYTES as usize + 1];
                junk.push(b'\n');
                let _ = pipe.write_all(&junk);
                // Past the budget, so this must never be read.
                let _ = writeln!(
                    pipe,
                    "{}",
                    serde_json::to_string(&sample(42)).expect("serialize")
                );
                let _ = pipe.flush();
            }

            send(&path, &sample(9)).expect("send");
            let received = recv_within(&server, Duration::from_secs(15));
            assert_eq!(
                received.map(|s| s.turn),
                Some(9),
                "the flooded connection's trailing signal must be dropped, not queued ahead of a well-behaved client"
            );
        }
    }
}
