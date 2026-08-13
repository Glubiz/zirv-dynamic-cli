//! Session registry: `<state>/sessions/<short8>.json`, one file per live
//! supervisor, keyed by the same short id `StateDir::socket_for` names its
//! turn-signal socket after. Best-effort throughout, matching the rest of
//! the state dir's own housekeeping: a registry write, refresh or removal
//! that fails must never fail a launch, and a listing must never fail just
//! because one file on disk is unreadable or malformed.
//!
//! `state.rs` is shared with a concurrent change adding `memory()`; this
//! module only ever calls `StateDir::sessions()` from there rather than
//! reaching into its internals, so the two changes stay independent.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::state::StateDir;

/// Mirrors `StateDir::socket_for`'s own derivation exactly: the first eight
/// ASCII-alphanumeric characters of the session id. Duplicated rather than
/// factored out of `state.rs` (the one file a concurrent change also
/// touches) -- `the_record_key_is_the_same_short_id_the_socket_is_named_
/// after` below pins the two derivations against each other so a future
/// edit to either cannot drift silently.
pub fn short_id(session: &str) -> String {
    session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect()
}

/// Which supervisor filed a record. `Chat` is `wrap`'s own orchestrator
/// launch, threaded through as a distinct verb from `chat.rs` rather than
/// derived from `PromptRole`: the two are independent facts about a session
/// (role governs prompt injection permissions; verb only names the calling
/// verb for the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Exec,
    Loop,
    Wrap,
    Chat,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Exec => "exec",
            Verb::Loop => "loop",
            Verb::Wrap => "wrap",
            Verb::Chat => "chat",
        }
    }
}

impl std::fmt::Display for Verb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub session: String,
    pub short: String,
    pub agent: String,
    pub repo: PathBuf,
    pub repo_slug: String,
    pub verb: Verb,
    pub pid: u32,
    pub started_at: u64,
}

impl Record {
    /// `pid` is always this process's own: a registry entry describes the
    /// supervisor that filed it, and every registration happens from inside
    /// that same process.
    pub fn new(session: &str, agent: &str, repo: &Path, verb: Verb) -> Self {
        Self {
            session: session.to_string(),
            short: short_id(session),
            agent: agent.to_string(),
            repo: repo.to_path_buf(),
            repo_slug: super::state::repo_slug(repo),
            verb,
            pid: std::process::id(),
            started_at: super::state::now_secs(),
        }
    }
}

fn record_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.json"))
}

/// Best-effort write: a registry that cannot be written must never be the
/// reason a launch fails, matching every other piece of state-dir
/// housekeeping in this codebase.
fn write_record(state: &StateDir, record: &Record) -> PathBuf {
    let path = record_path(state, &record.short);
    let _ = super::state::create_private_dir_all(&state.sessions());
    if let Ok(json) = serde_json::to_string_pretty(record) {
        let _ = super::state::write_private(&path, &json);
    }
    path
}

/// Registered at spawn, best-effort, and removed when the supervisor exits.
/// `Drop` covers a panic-free early return; `release()` is called explicitly
/// in every arm that leaves the supervisor loop, the same explicit-arm
/// discipline `RawGuard` follows because this binary's release profile is
/// `panic = "abort"` and Drop is therefore not guaranteed to run.
#[derive(Debug)]
pub struct SessionGuard {
    state: StateDir,
    record: Record,
    path: PathBuf,
    released: bool,
}

impl SessionGuard {
    pub fn register(state: &StateDir, record: Record) -> Self {
        let path = write_record(state, &record);
        Self {
            state: state.clone(),
            record,
            path,
            released: false,
        }
    }

    // `list`/`resolve_prefix` and their supporting types below now have
    // production call sites (`mail::run_send_with`'s `--to-session`,
    // `sessions::run_nudge_with`'s own resolution). `record()` alone is
    // still only exercised by this module's own tests -- nothing yet needs
    // the whole `Record` back out of a live guard, only its side effects.
    #[allow(dead_code)]
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// `loop`'s per-cycle refresh: a fresh session id means a fresh short
    /// id, so the previous cycle's file is removed and a new one written
    /// under the new name -- one guard for the whole supervised run, not one
    /// per cycle.
    pub fn refresh_session(&mut self, new_session: &str) {
        if self.released {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        self.record.session = new_session.to_string();
        self.record.short = short_id(new_session);
        self.record.started_at = super::state::now_secs();
        self.path = write_record(&self.state, &self.record);
    }

    /// Idempotent, like `RawGuard::restore`.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Stale,
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing; it only probes existence and
    // permission, the same check `kill -0` makes from a shell.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: `handle` is checked for null before any further call, and
    // `code` is only read after a successful `GetExitCodeProcess`.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        alive
    }
}

#[cfg(not(any(unix, windows)))]
fn is_alive(_pid: u32) -> bool {
    // No portable liveness check: never sweep a record this platform cannot
    // actually verify.
    true
}

/// Every record currently on disk, alongside whether its own process is
/// still alive. A stale record (its process is gone) is swept -- its file
/// removed -- as a side effect of this read, but is still reported in the
/// returned list so a caller can say what it just cleaned up. A file that
/// fails to parse is skipped outright: one malformed record must never fail
/// the whole listing.
pub fn list(state: &StateDir) -> Vec<(Record, Liveness)> {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Record>(&contents) else {
            continue;
        };
        if is_alive(record.pid) {
            found.push((record, Liveness::Live));
        } else {
            let _ = std::fs::remove_file(&path);
            found.push((record, Liveness::Stale));
        }
    }
    found
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// No live session's short id or full session id starts with the given
    /// prefix. Names every currently known live session, so the caller
    /// learns what it could have typed instead.
    NotFound { existing: Vec<String> },
    /// More than one live session matches; every candidate is named so the
    /// caller can disambiguate.
    Ambiguous(Vec<Record>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound { existing } => {
                if existing.is_empty() {
                    write!(f, "no sessions are registered")
                } else {
                    write!(
                        f,
                        "no session matches; known sessions: {}",
                        existing.join(", ")
                    )
                }
            }
            ResolveError::Ambiguous(candidates) => {
                let names: Vec<&str> = candidates.iter().map(|r| r.short.as_str()).collect();
                write!(f, "ambiguous prefix; candidates: {}", names.join(", "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolves a short-id (or full session-id) prefix to the one live record it
/// names. Only live records are candidates: a stale one has already been
/// swept from disk by the time a caller could act on it.
pub fn resolve_prefix(state: &StateDir, prefix: &str) -> Result<Record, ResolveError> {
    let live: Vec<Record> = list(state)
        .into_iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record)
        .collect();

    let matches: Vec<Record> = live
        .iter()
        .filter(|r| r.short.starts_with(prefix) || r.session.starts_with(prefix))
        .cloned()
        .collect();

    match matches.len() {
        0 => Err(ResolveError::NotFound {
            existing: live.into_iter().map(|r| r.short).collect(),
        }),
        1 => Ok(matches.into_iter().next().expect("checked len == 1")),
        _ => Err(ResolveError::Ambiguous(matches)),
    }
}

// N4: `zirv ctx nudge <prefix>`. A nudge is two independent pieces: a
// payload (an ordinary, durable, session-addressed mail message, so it is
// visible in `zirv ctx inbox` and survives however long it takes to be
// picked up) and a wake-up (an empty marker file, so a supervisor that is
// blocked on its own poll interval does not have to wait for it). The two
// are stored separately on purpose -- losing the marker (a crash between the
// two writes, or nobody claiming it before the state dir is swept) never
// loses the message, it just means the message is picked up at the next
// natural poll or cycle instead of immediately.

fn nudge_marker_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.nudge"))
}

/// Best-effort, matching every other piece of state-dir housekeeping in this
/// module: a marker that fails to write just means the wake-up is missed and
/// the nudge's mail (already durable) is picked up at the next natural poll
/// or cycle instead of immediately.
fn write_nudge_marker(state: &StateDir, short: &str) {
    let _ = super::state::create_private_dir_all(&state.sessions());
    let _ = std::fs::write(nudge_marker_path(state, short), b"");
}

/// Atomically claims the wake-up marker for `short`, if one is there.
/// `std::fs::remove_file` is the atomic claim, the same idiom `mail::
/// claim_and_write` and `mail::consume` build on: exactly one racing
/// observer ever sees `Ok(())`, every other one sees `NotFound`, so two
/// supervisors polling the same session at once cannot both act on the same
/// wake-up.
pub fn claim_nudge_marker(state: &StateDir, short: &str) -> bool {
    std::fs::remove_file(nudge_marker_path(state, short)).is_ok()
}

#[derive(Debug, clap::Args)]
pub struct NudgeArgs {
    /// Short id (or a unique prefix of one) of the live session to nudge.
    pub prefix: String,
    /// Message text. When omitted, read from `--message-file`, else from
    /// stdin.
    #[arg(long)]
    pub message: Option<String>,
    /// Path to a file holding the message text.
    #[arg(long)]
    pub message_file: Option<PathBuf>,
}

/// `--message`, else `--message-file`, else stdin -- trimmed either way, the
/// same convention `mail::resolve_message` uses.
fn resolve_nudge_message(args: &NudgeArgs, stdin: &mut dyn Read) -> CtxResult<String> {
    if let Some(text) = &args.message {
        return Ok(text.trim().to_string());
    }
    if let Some(path) = &args.message_file {
        return Ok(std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .trim()
            .to_string());
    }
    let mut buffer = String::new();
    stdin.read_to_string(&mut buffer)?;
    Ok(buffer.trim().to_string())
}

pub fn run_nudge_with<W: Write>(
    args: &NudgeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    stdin: &mut dyn Read,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    if !cfg.mail.enabled {
        // A nudge's payload is ordinary mail; without mail there is nothing
        // durable left to deliver, and the marker alone (no message to
        // explain why the session woke up) is worse than refusing outright.
        return Err(
            "zirv ctx nudge: mail is disabled (mail.enabled = false); nothing was sent".into(),
        );
    }

    let state = StateDir::resolve(env)?;
    // Only a live session is a valid nudge target: `resolve_prefix` already
    // filters to live records (a stale one was swept from disk by the time a
    // caller could act on it), so an unknown *or* dead session both surface
    // as the same `NotFound`, naming what is actually there instead.
    let record =
        resolve_prefix(&state, &args.prefix).map_err(|e| format!("zirv ctx nudge: {e}"))?;

    let body = resolve_nudge_message(args, stdin)?;
    if body.is_empty() {
        return Err(
            "zirv ctx nudge: no message given; pass --message, --message-file, or pipe one on stdin"
                .into(),
        );
    }

    let msg = super::mail::Message {
        from_session: super::mail::identity_or_unknown(env, super::adapters::SESSION_ENV),
        from_agent: super::mail::identity_or_unknown(env, super::adapters::AGENT_ENV),
        to: record.agent.clone(),
        to_session: Some(record.short.clone()),
        sent: super::state::now_secs(),
        body,
    };
    let slug = super::state::repo_slug(repo);
    super::mail::store(&state, &slug, &msg, &cfg)?;
    // Written after the mail: losing the marker (crash, or a write failure)
    // must never mean the message itself was lost, only that it is picked up
    // at the next natural poll or cycle instead of immediately.
    write_nudge_marker(&state, &record.short);

    writeln!(
        w,
        "zirv ctx nudge: queued for {} ({})",
        record.short, record.agent
    )?;
    Ok(0)
}

pub fn run_nudge<W: Write>(args: &NudgeArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_nudge_with(args, w, &repo, &env, &mut std::io::stdin())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_in(root: &Path) -> StateDir {
        StateDir::from_root(root.to_path_buf())
    }

    fn record_for(session: &str, repo: &Path, verb: Verb) -> Record {
        Record::new(session, "claude", repo, verb)
    }

    /// A pid guaranteed dead by the time it is used: a real child process,
    /// spawned and waited on, so its exit is deterministic rather than a
    /// hardcoded number that might collide with something alive on this
    /// machine.
    fn dead_pid() -> u32 {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit", "0"]);
            c
        } else {
            std::process::Command::new("true")
        };
        let mut child = cmd.spawn().expect("spawn a short-lived process");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    #[test]
    fn a_record_is_written_at_spawn_and_removed_when_the_supervisor_exits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let path = record_path(&state, &record.short);

        let guard = SessionGuard::register(&state, record);
        assert!(path.is_file(), "the record file exists right after spawn");

        drop(guard);
        assert!(
            !path.exists(),
            "the record file is gone once the supervisor exits"
        );
    }

    #[test]
    fn the_record_key_is_the_same_short_id_the_socket_is_named_after() {
        let session = "abcdef12-3456-4789-8abc-def012345678";
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());

        let socket = state.socket_for(session);
        let socket_stem = socket
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("socket file stem");

        assert_eq!(
            short_id(session),
            socket_stem,
            "the registry's own short id must match the socket's stem exactly"
        );
    }

    #[test]
    fn an_explicit_release_removes_the_record_even_though_drop_is_not_guaranteed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("22222222-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let path = record_path(&state, &record.short);

        let mut guard = SessionGuard::register(&state, record);
        assert!(path.is_file());

        guard.release();
        assert!(!path.exists(), "an explicit release removes the file");

        // Idempotent: dropping after an explicit release must not error or
        // try to remove anything a second time.
        drop(guard);
    }

    #[test]
    fn a_record_whose_process_is_gone_is_reported_stale_and_swept_on_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let mut record = record_for("33333333-2222-4333-8444-555555555555", &repo, Verb::Loop);
        record.pid = dead_pid();
        let path = record_path(&state, &record.short);
        write_record(&state, &record);
        assert!(path.is_file(), "sanity: the record was written");

        let found = list(&state);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Liveness::Stale);
        assert_eq!(found[0].0.session, record.session);

        assert!(
            !path.exists(),
            "a stale record is swept from disk as a side effect of listing"
        );
    }

    #[test]
    fn a_live_record_is_reported_live_and_kept_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        // This test process's own pid is alive for as long as the test runs.
        let record = record_for("44444444-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let path = record_path(&state, &record.short);
        write_record(&state, &record);

        let found = list(&state);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Liveness::Live);
        assert!(path.is_file(), "a live record's file is untouched");
    }

    #[test]
    fn a_malformed_record_is_skipped_rather_than_failing_the_listing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        std::fs::write(state.sessions().join("broken.json"), "{ not json").expect("write junk");

        let good = record_for("55555555-2222-4333-8444-555555555555", &repo, Verb::Exec);
        write_record(&state, &good);

        let found = list(&state);
        assert_eq!(
            found.len(),
            1,
            "the malformed file is skipped, not fatal: {found:?}"
        );
        assert_eq!(found[0].0.session, good.session);
    }

    #[test]
    fn listing_an_absent_sessions_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        assert!(list(&state).is_empty());
    }

    #[test]
    fn resolving_a_unique_prefix_returns_the_one_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("bbbbbbbb-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        write_record(&state, &record);

        let resolved = resolve_prefix(&state, "bbbb").expect("unique prefix resolves");
        assert_eq!(resolved.session, record.session);

        let resolved_full = resolve_prefix(&state, "bbbbbbbb").expect("the full short id too");
        assert_eq!(resolved_full.session, record.session);
    }

    #[test]
    fn an_ambiguous_prefix_is_an_error_that_names_every_candidate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        // Filtered-and-truncated to 8 chars, these two share the prefix
        // "aaaa" but are not otherwise identical: "aaaa1111" vs "aaaa2222".
        let one = record_for("aaaa1111-xxxx-4xxx-8xxx-xxxxxxxxxxxx", &repo, Verb::Exec);
        let two = record_for("aaaa2222-yyyy-4yyy-8yyy-yyyyyyyyyyyy", &repo, Verb::Loop);
        write_record(&state, &one);
        write_record(&state, &two);

        let err = resolve_prefix(&state, "aaaa").expect_err("two records share this prefix");
        match &err {
            ResolveError::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                let shorts: Vec<&str> = candidates.iter().map(|r| r.short.as_str()).collect();
                assert!(shorts.contains(&one.short.as_str()), "{shorts:?}");
                assert!(shorts.contains(&two.short.as_str()), "{shorts:?}");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains(&one.short), "names the first candidate: {msg}");
        assert!(
            msg.contains(&two.short),
            "names the second candidate: {msg}"
        );
    }

    #[test]
    fn an_unknown_prefix_is_an_error_that_says_which_sessions_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("cccccccc-2222-4333-8444-555555555555", &repo, Verb::Chat);
        write_record(&state, &record);

        let err = resolve_prefix(&state, "zzzz").expect_err("nothing starts with zzzz");
        match &err {
            ResolveError::NotFound { existing } => {
                assert_eq!(existing, &vec![record.short.clone()]);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(err.to_string().contains(&record.short));
    }

    #[test]
    fn a_stale_record_is_never_offered_as_a_resolution_candidate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let mut stale = record_for("dddddddd-2222-4333-8444-555555555555", &repo, Verb::Exec);
        stale.pid = dead_pid();
        write_record(&state, &stale);

        let err = resolve_prefix(&state, "dddd")
            .expect_err("the only match is stale, so effectively gone");
        assert!(matches!(err, ResolveError::NotFound { existing } if existing.is_empty()));
    }

    #[test]
    fn a_loop_keeps_one_record_and_refreshes_the_session_id_each_cycle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let first = record_for("eeeeeeee-2222-4333-8444-555555555555", &repo, Verb::Loop);
        let first_path = record_path(&state, &first.short);

        let mut guard = SessionGuard::register(&state, first);
        assert!(first_path.is_file());

        let second_session = "ffffffff-2222-4333-8444-555555555555";
        guard.refresh_session(second_session);
        let second_path = record_path(&state, &short_id(second_session));

        assert!(
            !first_path.exists(),
            "the previous cycle's file is removed, not left behind"
        );
        assert!(second_path.is_file(), "the new cycle's file is written");
        assert_eq!(guard.record().session, second_session);
        assert_eq!(
            guard.record().verb,
            Verb::Loop,
            "the verb survives a refresh"
        );

        // Only one record for the whole run at any given time.
        let found = list(&state);
        assert_eq!(found.len(), 1, "one record, not one per cycle: {found:?}");

        guard.release();
        assert!(!second_path.exists());
    }

    #[test]
    fn refreshing_after_release_is_a_harmless_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("12121212-2222-4333-8444-555555555555", &repo, Verb::Loop);
        let path = record_path(&state, &record.short);

        let mut guard = SessionGuard::register(&state, record);
        guard.release();
        assert!(!path.exists());

        guard.refresh_session("34343434-2222-4333-8444-555555555555");
        let new_path = record_path(&state, &short_id("34343434-2222-4333-8444-555555555555"));
        assert!(
            !new_path.exists(),
            "a released guard must not resurrect a record on refresh"
        );
    }

    #[test]
    fn verb_round_trips_through_json_as_a_lowercase_word() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        for verb in [Verb::Exec, Verb::Loop, Verb::Wrap, Verb::Chat] {
            let record = record_for("00000000-2222-4333-8444-555555555555", &repo, verb);
            let path = write_record(&state, &record);
            let raw = std::fs::read_to_string(&path).expect("read");
            assert!(
                raw.contains(&format!("\"{}\"", verb.as_str())),
                "verb {verb} must serialize as its lowercase word: {raw}"
            );
        }
    }

    // N4: `zirv ctx nudge`.

    fn env_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn a_nudge_stores_a_session_addressed_message_and_a_wake_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Exec);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: "abcd".to_string(),
            message: Some("please check the new failing test".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("nudge");
        assert_eq!(code, 0);

        let slug = super::super::state::repo_slug(&repo);
        let listed = super::super::mail::list(&state, &slug, None, Some(&short)).expect("list");
        assert_eq!(listed.len(), 1, "the payload is durable, ordinary mail");
        assert_eq!(listed[0].1.to_session, Some(short.clone()));
        assert_eq!(listed[0].1.body, "please check the new failing test");

        assert!(
            nudge_marker_path(&state, &short).is_file(),
            "the wake-up marker exists alongside the payload"
        );
    }

    #[test]
    fn the_marker_is_claimed_exactly_once_even_with_two_observers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        write_nudge_marker(&state, "aaaa1111");

        assert!(
            claim_nudge_marker(&state, "aaaa1111"),
            "the first observer claims it"
        );
        assert!(
            !claim_nudge_marker(&state, "aaaa1111"),
            "a second observer finds nothing left to claim"
        );
    }

    #[test]
    fn nudging_an_unknown_or_dead_session_is_an_error_that_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let repo = tmp.path().join("repo");

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: "zzzz".to_string(),
            message: Some("hello?".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("no session is registered at all");
        assert!(err.to_string().contains("no session"), "got {err}");

        // A dead session (its process gone) is swept from the registry on
        // read, so it surfaces exactly the same way as unknown -- there is
        // nothing left to disambiguate it from "never existed".
        let state = state_in(&state_dir);
        let mut dead = record_for("dddddddd-2222-4333-8444-555555555555", &repo, Verb::Exec);
        dead.pid = dead_pid();
        write_record(&state, &dead);

        let args = NudgeArgs {
            prefix: "dddd".to_string(),
            message: Some("hello?".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("the only match is dead");
        assert!(err.to_string().contains("no session"), "got {err}");
    }
}
