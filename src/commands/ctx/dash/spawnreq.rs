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
///
/// `cwd` is the repo the requester was invoked in, and it is **checked, not
/// honoured**: `dash::fulfill_spawn_request` refuses outright when it names
/// anything but the dashboard's own repo. Spawning a pane into a directory
/// the operator never opened is not something a request gets to ask for, and
/// silently ignoring the field would let a request from another repo run
/// here without either side noticing.
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
///
/// O2: `retryable` splits the two very different things an `ok: false` can
/// mean. A **policy** refusal (the agent gate, the argv guard, the pane cap)
/// is this operator's configuration saying no, and running the same task
/// headless instead would route straight around it -- so the requester must
/// fail. A **channel-level** failure (the request named another repo, the pty
/// spawn itself failed) says nothing about whether the task is allowed; the
/// headless path would have handled it, and suppressing that fallback turned a
/// recoverable mismatch into a dead delegation. Defaults to `false`, so an ack
/// written by an older build is read as the refusal it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAck {
    pub ok: bool,
    pub short: Option<String>,
    pub reason: Option<String>,
    #[serde(default)]
    pub retryable: bool,
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

/// The owner-pid file for a dashboard's spawn-request channel: the token
/// directory's own `owner.pid`, i.e. the PARENT of `requests_dir` joined with
/// `owner.pid` (`<state>/dash/<dash_short>-<token>/owner.pid`). It holds the
/// dashboard's process id as decimal ASCII text and nothing else.
///
/// The dashboard writes it at startup and `sessions::nested_session_evidence`
/// reads it: a token dir whose `owner.pid` names a dead (or missing) pid is a
/// leak from a dashboard that exited abnormally, and must NOT be read as a
/// live dashboard owning the terminal -- otherwise the leaked dir wedges every
/// future `zirv chat`. On a clean quit the whole token dir (this file with it)
/// is removed by `dash::mod::remove_request_dir`.
pub fn owner_pid_path(requests_dir: &Path) -> PathBuf {
    requests_dir
        .parent()
        .unwrap_or(requests_dir)
        .join("owner.pid")
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

/// The infix every in-progress write in this directory carries while it is
/// still being written. Both listings below skip anything containing it, so a
/// 50ms poller can never read a half-written file: the visible name only ever
/// appears via [`write_atomic_private`]'s own rename, which is atomic.
const TMP_INFIX: &str = ".tmp-";

/// Whether `name` is one of [`write_atomic_private`]'s in-flight temporaries.
/// Belt and braces: a temporary is named `<final>.tmp-<uuid>`, so it already
/// fails every `ends_with(".json")` check in this module -- but a listing that
/// only *happens* to exclude a half-written file is not the same thing as one
/// that says so.
fn is_tmp_name(name: &str) -> bool {
    name.contains(TMP_INFIX)
}

/// R10: creates `<dir>/<name>` by writing `<dir>/<name>.tmp-<uuid>` first and
/// renaming it into place. Every file in this directory is polled for by the
/// other side of the channel (`take_requests` every tick, `wait_for_ack` every
/// 100ms), and a plain create-then-write is visible under its final name while
/// still empty -- which a poller reads as a torn write and deletes. The rename
/// is atomic on both platforms, so the final name never exists in a partial
/// state. The temporary itself is still `create_new` + 0600, the same private
/// -file discipline `state::write_private` holds elsewhere, and is cleaned up
/// if the rename fails.
fn write_atomic_private(dir: &Path, name: &str, contents: &str) -> CtxResult<PathBuf> {
    super::super::state::create_private_dir_all(dir)?;
    let tmp = dir.join(format!("{name}{TMP_INFIX}{}", uuid::Uuid::new_v4()));
    create_new_private(&tmp, contents)?;
    let path = dir.join(name);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(path)
}

/// Writes `req` as a `req-<uuid>.json` file under `dir` through
/// [`write_atomic_private`], so the request only ever becomes visible to the
/// dashboard's own poller complete. Returns the path so the caller
/// (`agent.rs`) can derive the request's own file stem, which [`wait_for_ack`]
/// and [`write_ack`] both key off -- and so it can remove the request again if
/// it gives up waiting before anybody claimed it.
pub fn write_request(dir: &Path, req: &SpawnRequest) -> CtxResult<PathBuf> {
    let body = serde_json::to_string(req)?;
    write_atomic_private(dir, &format!("req-{}.json", uuid::Uuid::new_v4()), &body)
}

/// Every currently-queued request in `dir`, **claimed by rename**: each
/// `req-<uuid>.json` is renamed to its own `claim-req-<uuid>` in the same
/// operation that takes it off the queue, and only then read back. A file
/// that fails to parse (a torn write from a crash, or some other process's
/// stray file) is skipped and its claim removed rather than left to jam every
/// later tick's listing forever; only `req-*.json` files are considered at
/// all, so neither an `ack-*.json` nor a `claim-*` this same directory also
/// holds is ever misread as a request.
///
/// O6: taking used to be a *delete*, with the claim written afterwards by
/// `dash::mod::claim_batch`. Between those two writes the request existed
/// nowhere on disk -- neither queued nor claimed -- so a requester whose ack
/// timed out inside that window saw no claim, concluded nobody was listening,
/// and ran the same task headless while the dashboard was already spawning it.
/// One rename is both halves at once, so the window does not exist: the file
/// is a request until it is a claim, with no instant in between.
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
            .is_some_and(|name| {
                name.starts_with("req-") && name.ends_with(".json") && !is_tmp_name(name)
            });
        if !is_request {
            continue;
        }
        let Some(stem) = request_stem(&path) else {
            continue;
        };
        let claim = claim_path(dir, &stem);
        // The claim *is* the take. A failed rename leaves the request queued
        // for the next tick rather than consuming it into nothing.
        if std::fs::rename(&path, &claim).is_err() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&claim) else {
            let _ = std::fs::remove_file(&claim);
            continue;
        };
        let Ok(req) = serde_json::from_str::<SpawnRequest>(&contents) else {
            let _ = std::fs::remove_file(&claim);
            continue;
        };
        // The *request* path, not the claim path: every caller on both sides
        // of the channel keys its ack off `request_stem` of the name the
        // requester itself wrote.
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
    let body = serde_json::to_string(ack)?;
    write_atomic_private(dir, &format!("ack-{request_stem}.json"), &body)?;
    Ok(())
}

/// `claim-<request_stem>`: the name [`take_requests`] renames a request to the
/// moment it takes it, before any fulfilment work starts. Deliberately
/// extensionless so it can never be mistaken for a `req-*.json` or an
/// `ack-*.json` by either side's own directory listing. Its contents are the
/// request's own JSON (that is what was renamed), but nothing ever parses them:
/// the file's existence is the whole signal.
fn claim_path(dir: &Path, request_stem: &str) -> PathBuf {
    dir.join(format!("claim-{request_stem}"))
}

/// Withdraws a claim: called when fulfilment *failed* outright, so the claim
/// no longer stands for anything (R6). A requester that timed out reads a
/// lingering claim as "the dashboard has this, the answer is just slow" and
/// reports success -- for a pane that will never exist. A claim is kept when
/// the spawn itself succeeded and only the ack write failed: there a pane
/// genuinely does exist, and headless double-running it would be the worse
/// outcome. Best-effort: an already-absent claim is not an error.
pub fn remove_claim(dir: &Path, request_stem: &str) {
    let _ = std::fs::remove_file(claim_path(dir, request_stem));
}

/// Whether some dashboard has claimed this request.
///
/// F2: no longer consulted by the requester. Reading the claim and *then*
/// acting on that reading is check-then-act against a dashboard whose claim is
/// a rename of the request file itself, so `agent.rs` now makes its own
/// `remove_file` of that same file the decision -- exactly one of the two
/// operations can win, with no window in between. Kept as the claim
/// protocol's own observable, which is what its tests (here and in
/// `dash::mod`) assert against.
#[cfg(test)]
pub fn is_claimed(dir: &Path, request_stem: &str) -> bool {
    claim_path(dir, request_stem).exists()
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
    fn owner_pid_path_is_the_token_dir_sibling_of_requests() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let requests = request_dir_for(&state, "aaaa1111", "0123456789abcdef");
        assert_eq!(
            owner_pid_path(&requests),
            tmp.path()
                .join("dash")
                .join("aaaa1111-0123456789abcdef")
                .join("owner.pid")
        );
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
        assert!(
            !path.exists(),
            "take_requests takes the request off the queue"
        );

        let stem = request_stem(&path).expect("stem");
        let ack = SpawnAck {
            ok: true,
            short: Some("bbbb2222".to_string()),
            reason: None,
            retryable: false,
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
        assert!(
            !is_claimed(&dir, "req-not-json"),
            "and its claim is withdrawn rather than left standing for a request nobody can read"
        );
    }

    /// O6: taking a request and claiming it are the same rename, so there is no
    /// instant in which the request is neither queued nor claimed -- the window
    /// a requester's ack timeout used to fall into and double-run the task.
    #[test]
    fn taking_a_request_claims_it_in_the_same_operation() {
        let (_tmp, dir) = dir();
        let path = write_request(&dir, &sample_request()).expect("write_request");
        let stem = request_stem(&path).expect("stem");
        assert!(
            !is_claimed(&dir, &stem),
            "nothing is claimed before the take"
        );

        let taken = take_requests(&dir);

        assert_eq!(taken.len(), 1);
        assert!(!path.exists(), "the request file is gone");
        assert!(
            is_claimed(&dir, &stem),
            "and it is claimed already -- both are one rename"
        );
        assert!(
            take_requests(&dir).is_empty(),
            "a claimed request is never taken a second time"
        );
    }

    /// The requester's own timeout cleanup runs against a path the dashboard
    /// may already have renamed away: an absent file is not an error.
    #[test]
    fn removing_an_already_taken_request_is_not_an_error() {
        let (_tmp, dir) = dir();
        let path = write_request(&dir, &sample_request()).expect("write_request");
        assert_eq!(take_requests(&dir).len(), 1);

        let err = std::fs::remove_file(&path).expect_err("already renamed away");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
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

    /// Writes one request and takes it, so the directory holds exactly one
    /// claim. Returns that claim's request stem.
    fn claim_one(dir: &Path) -> String {
        let path = write_request(dir, &sample_request()).expect("write_request");
        assert_eq!(take_requests(dir).len(), 1);
        request_stem(&path).expect("stem")
    }

    /// F10: a claim exists from the moment a request is taken and is neither a
    /// request nor an ack, so neither side's own listing may pick it up.
    #[test]
    fn a_claim_is_visible_to_the_requester_and_invisible_to_take_requests() {
        let (_tmp, dir) = dir();
        let stem = claim_one(&dir);

        assert!(is_claimed(&dir, &stem));
        assert!(
            !is_claimed(&dir, "req-other"),
            "a claim only covers its own request"
        );
        assert!(
            take_requests(&dir).is_empty(),
            "a claim file must never be read back as a request"
        );
        assert!(
            wait_for_ack(&dir, &stem, Duration::from_millis(50)).is_none(),
            "a claim is not an ack"
        );
    }

    #[test]
    fn wait_for_ack_times_out_when_nothing_ever_answers() {
        let (_tmp, dir) = dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let got = wait_for_ack(&dir, "req-never-answered", Duration::from_millis(150));
        assert!(got.is_none());
    }

    /// R10: every write in this directory lands through a rename, so a 50ms
    /// poller never sees a file under its final name while it is still being
    /// written -- and nothing is left behind under the temporary one.
    #[test]
    fn every_write_renames_into_place_and_leaves_no_temporary_behind() {
        let (_tmp, dir) = dir();
        let path = write_request(&dir, &sample_request()).expect("write_request");
        assert!(path.is_file());
        write_ack(
            &dir,
            "req-x",
            &SpawnAck {
                ok: true,
                short: Some("bbbb2222".to_string()),
                reason: None,
                retryable: false,
            },
        )
        .expect("write_ack");

        let lingering: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|name| is_tmp_name(name))
            .collect();
        assert!(lingering.is_empty(), "got {lingering:?}");

        // And the renamed files are all readable as themselves.
        let stem = request_stem(&path).expect("stem");
        assert_eq!(take_requests(&dir).len(), 1);
        assert!(is_claimed(&dir, &stem));
        assert!(wait_for_ack(&dir, "req-x", Duration::from_millis(50)).is_some());
    }

    /// A temporary left by a crashed writer is not a request: it must be
    /// neither returned nor consumed by a listing that walks past it.
    #[test]
    fn take_requests_ignores_a_lingering_temporary_file() {
        let (_tmp, dir) = dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let stray = dir.join(format!("req-abc.json{TMP_INFIX}0123"));
        std::fs::write(
            &stray,
            r#"{"agent":"claude","prompt":"x","cwd":"/repo","requested_by":"y"}"#,
        )
        .expect("write stray");

        assert!(
            take_requests(&dir).is_empty(),
            "a temporary is not a request"
        );
        assert!(
            stray.exists(),
            "and it is left for its own writer to finish or clean up, not consumed"
        );
    }

    /// R6: a claim can be withdrawn, so a fulfilment that failed outright
    /// stops telling a timed-out requester that a pane is on its way.
    #[test]
    fn remove_claim_withdraws_a_claim() {
        let (_tmp, dir) = dir();
        let stem = claim_one(&dir);
        assert!(is_claimed(&dir, &stem));
        remove_claim(&dir, &stem);
        assert!(!is_claimed(&dir, &stem));
        // Idempotent: withdrawing twice is not an error.
        remove_claim(&dir, &stem);
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
