//! The dashboard's spawn-request channel: a capability-token directory a
//! pane's own `zirv ctx agent` invocation (inheriting `DASH_REQUESTS_ENV`
//! from its own turn_env, see `dash::mod::build_turn_env`'s call sites) can
//! write a [`SpawnRequest`] into, and poll for a matching [`SpawnAck`]. Only
//! a process that was actually told this directory's path -- by inheriting
//! it from a pane the dashboard itself spawned -- can reach it at all: the
//! directory name embeds a random token
//! (`dash::mod`'s own `spawn_token`), not anything derivable from the
//! dashboard's own public identity, so an unrelated process on the same
//! machine cannot forge a request into a dashboard it was never invited
//! into.
//!
//! A request is data, never authority. `dash::mod::fulfill_spawn_request`
//! re-checks `cfg.agents.refusal` and `adapters::select` against the live
//! configuration before ever spawning anything -- exactly the same gate an
//! operator-issued `zirv ctx agent` invocation goes through -- so a pane
//! child cannot spawn an agent this dashboard's own configuration would
//! otherwise have refused.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::state::StateDir;

/// Exported into every pane's own `turn_env`: names the directory a pane's
/// own `zirv ctx agent` invocation writes a [`SpawnRequest`] into.
/// Deliberately absent for any process outside a dashboard pane, so
/// `agent.rs`'s own headless path is unaffected when this is unset -- see
/// `sessions::nested_session_evidence`, which also treats a *set* value of
/// this variable as proof a dashboard pane owns this terminal, alongside
/// `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET`.
pub const DASH_REQUESTS_ENV: &str = "ZIRV_CTX_DASH_REQUESTS";

/// How often [`wait_for_ack`] polls for a matching ack file.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One `zirv ctx agent` invocation's ask: spawn `agent` as a fresh worker
/// pane, with `prompt` as its first-turn task text (data, never argv --
/// mirrors every other delegation path in this codebase). `requested_by` is
/// advisory only (the caller's own session short, or `"unknown"`): nothing
/// in the fulfilment path trusts it for anything but a label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub agent: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub requested_by: String,
}

/// The dashboard's answer to one [`SpawnRequest`]. `ok: false` always
/// carries `reason` (a gate refusal, an unknown agent, or a spawn failure);
/// `ok: true` always carries `short` (the freshly spawned pane's own
/// registry short id, the same address `zirv ctx nudge`/`zirv ctx send`
/// would use to reach it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAck {
    pub ok: bool,
    pub short: Option<String>,
    pub reason: Option<String>,
}

/// `<state>/dash/<dash_short>-<token>/requests`. `dash_short` is this
/// dashboard's own registry short id (see `dash::mod::run_dashboard`, which
/// derives it before the first pane ever spawns: `Record::new`'s own
/// `short` field is exactly `sessions::short_id(session)`, so nothing here
/// has to wait on an actual spawn). `token` is a 16-hex-character
/// capability token, freshly minted per dashboard launch.
pub fn request_dir_for(state: &StateDir, dash_short: &str, token: &str) -> PathBuf {
    state
        .dash()
        .join(format!("{dash_short}-{token}"))
        .join("requests")
}

#[cfg(unix)]
fn create_new_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn create_new_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

/// Writes `req` as a freshly created (never overwritten -- `create_new`, so
/// a uuid collision fails loudly rather than silently clobbering another
/// request) `req-<uuid>.json` file under `dir`, 0600 on unix via the same
/// private-file discipline `state::write_private` uses elsewhere. Returns
/// the path so the caller (`agent.rs`) can derive the request's own file
/// stem, which [`wait_for_ack`] and [`write_ack`] both key off.
pub fn write_request(dir: &Path, req: &SpawnRequest) -> CtxResult<PathBuf> {
    super::super::state::create_private_dir_all(dir)?;
    let path = dir.join(format!("req-{}.json", uuid::Uuid::new_v4()));
    let body = serde_json::to_string(req)?;
    create_new_private(&path, &body)?;
    Ok(path)
}

/// Every currently-queued request in `dir`: read, then deleted immediately
/// -- a request is claimed at most once, whatever the dashboard goes on to
/// do with it. A file that fails to parse (a torn write from a crash, or
/// some other process's stray file) is skipped and removed rather than left
/// to jam every later tick's listing forever; only `req-*.json` files are
/// considered at all, so an `ack-*.json` this same directory also holds is
/// never misread as a request.
pub fn take_requests(dir: &Path) -> Vec<(PathBuf, SpawnRequest)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_request = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("req-") && name.ends_with(".json"));
        if !is_request {
            continue;
        }
        let contents = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        let Ok(contents) = contents else { continue };
        let Ok(req) = serde_json::from_str::<SpawnRequest>(&contents) else {
            continue;
        };
        out.push((path, req));
    }
    out
}

/// The file stem `write_request`'s own return path carries -- `"req-<uuid>"`
/// -- shared by every caller (`agent.rs`, and `dash::mod`'s own request
/// handler working from `take_requests`' returned paths) that needs to
/// derive the same ack filename from a request path.
pub fn request_stem(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// Writes the ack for the request whose file stem was `request_stem` --
/// `"ack-<stem>.json"`, in the same directory the request itself lived in,
/// so [`wait_for_ack`]'s caller (who already knows its own request's stem)
/// can find it deterministically without listing the directory.
pub fn write_ack(dir: &Path, request_stem: &str, ack: &SpawnAck) -> CtxResult<()> {
    super::super::state::create_private_dir_all(dir)?;
    let path = dir.join(format!("ack-{request_stem}.json"));
    let body = serde_json::to_string(ack)?;
    super::super::state::write_private(&path, &body)?;
    Ok(())
}

/// Polls `dir` for `ack-<request_stem>.json`, up to `timeout`, sleeping
/// [`POLL_INTERVAL`] between checks. Consumes (deletes) the file once found,
/// so a stale ack can never be misread by a later, unrelated wait on the
/// same stem. `None` on timeout, or on an ack file that fails to parse.
pub fn wait_for_ack(dir: &Path, request_stem: &str, timeout: Duration) -> Option<SpawnAck> {
    let path = dir.join(format!("ack-{request_stem}.json"));
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let _ = std::fs::remove_file(&path);
            return serde_json::from_str::<SpawnAck>(&contents).ok();
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("requests");
        (tmp, dir)
    }

    fn sample_request() -> SpawnRequest {
        SpawnRequest {
            agent: "claude".to_string(),
            prompt: "fix the failing tests".to_string(),
            cwd: PathBuf::from("/repo"),
            requested_by: "abcd1234".to_string(),
        }
    }

    #[test]
    fn request_dir_for_nests_under_dash_short_and_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = request_dir_for(&state, "aaaa1111", "0123456789abcdef");
        assert_eq!(
            dir,
            tmp.path()
                .join("dash")
                .join("aaaa1111-0123456789abcdef")
                .join("requests")
        );
    }

    #[test]
    fn a_request_and_its_ack_round_trip() {
        let (_tmp, dir) = dir();
        let req = sample_request();

        let path = write_request(&dir, &req).expect("write_request");
        assert!(path.is_file(), "the request file exists on disk");

        let taken = take_requests(&dir);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].1, req);
        assert!(!path.exists(), "take_requests deletes the file it read");

        let stem = request_stem(&path).expect("stem");
        let ack = SpawnAck {
            ok: true,
            short: Some("bbbb2222".to_string()),
            reason: None,
        };
        write_ack(&dir, &stem, &ack).expect("write_ack");

        let got = wait_for_ack(&dir, &stem, Duration::from_secs(1)).expect("ack arrives");
        assert_eq!(got, ack);
    }

    #[test]
    fn take_requests_skips_and_removes_malformed_json() {
        let (_tmp, dir) = dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let bad = dir.join("req-not-json.json");
        std::fs::write(&bad, "{ this is not valid json").expect("write bad file");

        let taken = take_requests(&dir);
        assert!(taken.is_empty(), "a malformed request yields nothing");
        assert!(!bad.exists(), "the malformed file is still removed");
    }

    #[test]
    fn take_requests_ignores_ack_files_and_a_missing_directory() {
        let (_tmp, dir) = dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("ack-req-x.json"), r#"{"ok":true}"#).expect("write");
        assert!(take_requests(&dir).is_empty());

        let absent = dir.join("does-not-exist");
        assert!(take_requests(&absent).is_empty());
    }

    #[test]
    fn wait_for_ack_times_out_when_nothing_ever_answers() {
        let (_tmp, dir) = dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let got = wait_for_ack(&dir, "req-never-answered", Duration::from_millis(150));
        assert!(got.is_none());
    }

    #[test]
    fn write_request_never_overwrites_an_existing_file() {
        let (_tmp, dir) = dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("req-collision.json");
        std::fs::write(&path, "already here").expect("pre-create");

        // `create_new_private` refuses to clobber an existing file; simulate
        // the collision directly against the private writer rather than
        // hoping for an actual uuid collision.
        let err = create_new_private(&path, "{}").expect_err("must not overwrite");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }
}
