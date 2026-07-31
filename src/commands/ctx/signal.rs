// Consumed by the hook verb added in a later task of this plan; nothing calls
// this yet, so dead_code is silenced module-wide until then.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::rot::Verdict;

/// macOS `sockaddr_un.sun_path` is 104 bytes. Fail early with a readable error
/// instead of an opaque OS error from inside a supervisor.
pub const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSignal {
    pub session_id: String,
    pub turn: u64,
    pub score: u32,
    pub verdict: Verdict,
}

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
        use std::io::BufRead;

        check_len(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let (tx, rx) = std::sync::mpsc::channel();

        // The accept loop lives for the process. A foreground supervisor owns
        // the socket for its whole run, so there is nothing to join.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let reader = std::io::BufReader::new(stream);
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

/// The Windows counterpart keeps the two branches type-identical, even though
/// the fields are never read on that platform: `wrap` degrades to polling
/// instead of holding a live `SignalServer`.
#[cfg(not(unix))]
#[derive(Debug)]
#[allow(dead_code)]
pub struct SignalServer {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<TurnSignal>,
}

#[cfg(not(unix))]
impl SignalServer {
    pub fn bind(_path: &Path) -> CtxResult<Self> {
        Err("turn signals need unix domain sockets; supervision degrades to polling".into())
    }

    pub fn try_recv(&self) -> Option<TurnSignal> {
        self.rx.try_recv().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(not(unix))]
pub fn send(_path: &Path, _signal: &TurnSignal) -> CtxResult<()> {
    Err("turn signals need unix domain sockets".into())
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
        }
    }

    #[test]
    fn signals_round_trip_through_json() {
        let json = serde_json::to_string(&sample(3)).expect("serialize");
        assert!(json.contains("\"verdict\":\"compact\""), "got {json}");
        let back: TurnSignal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, sample(3));
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
}
