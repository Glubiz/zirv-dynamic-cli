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

/// The environment variables that carry one supervised session's *identity*
/// into everything it spawns: which session id turn signals should claim,
/// which socket to post them on, and which transcript file the supervisor is
/// watching. A child that inherits these from an outer session reports its
/// own turns as if they belonged to that outer session -- which is exactly
/// how a nested launch drove the outer rot engine to a `Restart` verdict and
/// had it kill the outer agent (see `nested_session_evidence`).
///
/// Every supervisor scrubs all three off a child command builder before
/// setting whichever of them it actually owns, so "no socket of my own"
/// degrades to *unsupervised*, never to *supervised by somebody else*.
pub const SUPERVISION_ENV: [&str; 3] = [
    super::adapters::SESSION_ENV,
    super::adapters::SOCKET_ENV,
    super::wrap::TRANSCRIPT_ENV,
];

/// `portable_pty::CommandBuilder::new` seeds itself from `std::env::vars_os`,
/// so an unset key on the builder still means "inherit". Only an explicit
/// `env_remove` actually keeps the value out of the child.
pub fn scrub_supervision_env(builder: &mut portable_pty::CommandBuilder) {
    for key in SUPERVISION_ENV {
        builder.env_remove(key);
    }
}

/// The `std::process::Command` counterpart, for the headless supervisors.
pub fn scrub_supervision_env_cmd(command: &mut std::process::Command) {
    for key in SUPERVISION_ENV {
        command.env_remove(key);
    }
}

/// Set to `true` to bypass the interactive nesting guard, for the operator
/// who genuinely means to run a session inside a session. Mirrored by
/// `--allow-nested` on `wrap` and `chat`.
pub const ALLOW_NESTED_ENV: &str = "ZIRV_ALLOW_NESTED";

/// Claude Code exports both of these into every process it spawns; either one
/// alone is too weak to key on (`CLAUDECODE` is a plain flag a user could
/// export by hand), so the pair is required together.
const CLAUDE_PID_ENV: &str = "CLAUDE_PID";
const CLAUDE_CODE_ENV: &str = "CLAUDECODE";

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Why this process looks like it is already running *inside* an agent
/// session, or `None` when nothing says so. Reads the caller's `EnvLookup`
/// only, never the process environment -- and the filesystem only to ask
/// whether the one directory-valued piece of evidence
/// (`DASH_REQUESTS_ENV`) still exists (O5, below).
///
/// Interactive supervision nested inside an existing session is not merely
/// redundant, it is destructive. The nested `wrap` binds its own turn-signal
/// socket, but when that bind fails it still spawns a child -- and that child
/// inherits the *outer* `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET`, so its hooks
/// post phantom turns into the outer supervisor's rot engine until the outer
/// engine verdicts `Restart` and kills its own child: the session the user
/// was actually talking to. `SUPERVISION_ENV` scrubbing closes the inherit
/// half of that; this closes the "should we be here at all" half.
/// Whether the dashboard that owns `requests_dir` is still alive, per its
/// `owner.pid` file. The pidfile lives in the requests dir's PARENT (i.e.
/// `<state>/dash/<short>-<token>/owner.pid`) and holds the dashboard's pid as
/// decimal ASCII. A missing, unreadable, unparseable, or dead-pid pidfile all
/// mean "no live dashboard" -- so an abnormally-exited dashboard's leftover
/// requests directory never wedges a future interactive launch. Only a
/// readable pidfile naming a live process counts.
fn dashboard_owner_is_live(requests_dir: &Path) -> bool {
    if !requests_dir.is_dir() {
        return false;
    }
    let Some(parent) = requests_dir.parent() else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(parent.join("owner.pid")) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    is_alive(pid)
}

pub fn nested_session_evidence(env: super::config::EnvLookup<'_>) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    if let Some(id) = non_empty(env(super::adapters::SESSION_ENV)) {
        found.push(format!(
            "{}={}",
            super::adapters::SESSION_ENV,
            short_id(&id)
        ));
    }
    if non_empty(env(super::adapters::SOCKET_ENV)).is_some() {
        found.push(format!("{} is set", super::adapters::SOCKET_ENV));
    }
    // A dashboard pane's own child inherits this (the dashboard exports it
    // into every pane's turn_env, never into its own process environment --
    // see `dash::run_dashboard`), so a set value here means a dashboard pane
    // owns this terminal, and starting another interactive supervisor (or
    // dashboard) inside it is exactly the nested-session hazard this guard
    // exists to catch. Deliberately not added to `SUPERVISION_ENV`: a pane
    // child's own further children (e.g. a nested `zirv ctx agent`) must
    // still be able to reach the same spawn-request channel, which scrubbing
    // it there would break.
    //
    // O5: the directory has to still exist, exactly as `agent::
    // try_join_dashboard` requires before it will use the channel. The
    // dashboard removes it on quit, so a shell that survived one -- a pane
    // child still sitting at a prompt after the dashboard closed -- carries a
    // stale value naming nothing. Treating that as evidence refused a session
    // no dashboard owns any more, and the two readers of this variable
    // disagreeing about what "set" means was the bug: one channel, one
    // liveness test.
    //
    // A directory alone is not enough, though: an *abnormal* dashboard exit
    // (crash, kill) leaves the directory behind, and a surviving pane shell
    // still carrying this env would then wedge every future interactive
    // launch forever. The dashboard writes its own pid into `owner.pid` (the
    // requests dir's parent, `<state>/dash/<short>-<token>/owner.pid`), so
    // only a pidfile naming a *live* process counts as a dashboard actually
    // owning this terminal -- a stale or dead one is no evidence.
    if non_empty(env(super::dash::spawnreq::DASH_REQUESTS_ENV))
        .is_some_and(|dir| dashboard_owner_is_live(Path::new(&dir)))
    {
        found.push(format!(
            "{} is set (a dashboard pane owns this terminal)",
            super::dash::spawnreq::DASH_REQUESTS_ENV
        ));
    }
    if non_empty(env(CLAUDE_PID_ENV)).is_some() && non_empty(env(CLAUDE_CODE_ENV)).is_some() {
        found.push(format!(
            "{CLAUDE_PID_ENV} and {CLAUDE_CODE_ENV} are set (a Claude Code session owns this terminal)"
        ));
    }
    (!found.is_empty()).then(|| found.join("; "))
}

/// The refusal message an interactive verb prints, or `None` when it may
/// start. `allow_nested` is the verb's own `--allow-nested` flag; the
/// `ZIRV_ALLOW_NESTED` environment variable is the second, equivalent
/// override (strict `true`, matching every other boolean this codebase reads
/// out of the environment).
///
/// Only the interactive verbs (`wrap`, `chat`) call this. Headless workers
/// (`exec`, `loop`, `agent`) legitimately run inside a session -- delegating
/// to one is the whole point of `zirv ctx agent` -- and they never take over
/// the shared console, so they are deliberately not gated.
pub fn nesting_refusal(
    verb: &str,
    env: super::config::EnvLookup<'_>,
    allow_nested: bool,
) -> Option<String> {
    let overridden = allow_nested
        || non_empty(env(ALLOW_NESTED_ENV))
            .is_some_and(|v| v.to_ascii_lowercase().parse::<bool>() == Ok(true));
    if overridden {
        return None;
    }
    let evidence = nested_session_evidence(env)?;
    Some(format!(
        "zirv ctx {verb}: refusing to start inside an existing agent session ({evidence}). \
         A nested interactive session can post turn signals into the outer supervisor and \
         get the outer session compacted, restarted or killed. Run it from a plain terminal, \
         or pass --allow-nested (or set {ALLOW_NESTED_ENV}=true) to override."
    ))
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
    /// A dashboard worker pane (`zirv ctx dash`'s own supervised child):
    /// distinct from `Chat`, which stays the dashboard's own orchestrator
    /// pane, so a registry row can tell "the orchestrator" from "a pane the
    /// dashboard spawned" apart.
    Dash,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Exec => "exec",
            Verb::Loop => "loop",
            Verb::Wrap => "wrap",
            Verb::Chat => "chat",
            Verb::Dash => "dash",
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
    /// NEW-3: whether this supervisor can actually *act* on a wake-up.
    ///
    /// A supervisor only claims nudge markers from its turn-signal arm, so
    /// one that never bound a `SignalServer` (`wrap --no-supervise`, or a
    /// bind that failed) can never notice a nudge -- not even to advise
    /// about it. Such a session used to be dropped from the registry
    /// entirely, which fixed the silent-nudge bug by making the session
    /// invisible: it vanished from `zirv ctx status` too, so an operator
    /// watching a bind-failed `wrap` simply could not see it was running.
    ///
    /// Recorded instead of hidden: `status` renders it as `unreachable`, and
    /// `nudge` refuses it with a reason. `#[serde(default = ...)]` returns
    /// `true` so a record written by an older build still parses as a normal
    /// reachable session.
    #[serde(default = "reachable_default")]
    pub reachable: bool,
    /// The process that registered this session, stamped by
    /// [`SessionGuard::register`] itself (unless a caller already set it) --
    /// so it is always `Some(pid)` for a record written by this build, and
    /// `None` only for one written by an older build before this field
    /// existed. A dashboard pane carries the *dashboard's* pid because
    /// `Pane::new` registers from inside the dashboard process itself; any
    /// other session simply carries the pid of whichever process actually
    /// called `register`. That is not always the dashboard even for a
    /// session a dashboard pane is morally responsible for: `zirv ctx
    /// agent`'s dashboard-refused-but-retryable fallback (`agent.rs`'s
    /// headless path, `agent.rs:126` onward into `exec::run_with`) runs in
    /// the *requester's* process -- a pane's own child shell, or a plain
    /// terminal -- never inside the dashboard's, so that fallback session
    /// registers the requester's pid and is not shown in that dashboard's
    /// sidebar. Accepted residual: pid-based ownership has no way to express
    /// "spawned on this dashboard's behalf, but from outside its process," so
    /// that session is only visible via mail (its own report-back) and `zirv
    /// ctx status`, same as any other unowned-by-this-dashboard record. The
    /// dashboard sidebar merge (`dash::assemble_sidebar`) keeps only records
    /// whose `owner_pid` matches its own pid, so a second, concurrently
    /// running dashboard's panes never bleed into this one's panel.
    /// `#[serde(default)]` so an on-disk record from an older build
    /// deserializes as `None` rather than failing to parse.
    #[serde(default)]
    pub owner_pid: Option<u32>,
}

fn reachable_default() -> bool {
    true
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
            // Reachable unless a caller says otherwise: every headless
            // supervisor binds a socket as a matter of course, so only
            // `wrap` (which can run `--no-supervise`, or fail to bind) has
            // any reason to call `unreachable()` below.
            reachable: true,
            // Left unset here: `SessionGuard::register` stamps this
            // process's own pid on the way to disk, unless a caller has
            // already set one (see its own doc comment).
            owner_pid: None,
        }
    }

    /// Marks this record as one that can never act on a wake-up -- see the
    /// `reachable` field. Chained onto `new` at the one call site that knows
    /// whether a turn-signal socket actually bound.
    pub fn unreachable(mut self) -> Self {
        self.reachable = false;
        self
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
    /// Stamps `owner_pid` with this process's own pid before writing the
    /// record, unless the caller already set one -- see the field's own doc
    /// comment. Every registration path goes through this one function, so
    /// this is the single seam: a dashboard pane and a standalone `wrap`/
    /// `exec`/`loop` session both end up attributed to whichever process
    /// actually called this, without each caller having to remember to stamp
    /// itself. That is the dashboard's own pid for a pane (registered from
    /// inside the dashboard process) and the calling process's own pid for
    /// everything else -- including `zirv ctx agent`'s headless fallback,
    /// which runs in the *requester's* process rather than the dashboard's
    /// even when the request named one (see the field's own doc comment for
    /// that residual).
    pub fn register(state: &StateDir, mut record: Record) -> Self {
        if record.owner_pid.is_none() {
            record.owner_pid = Some(std::process::id());
        }
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

    /// Points this run's record at a new session id: `loop`'s per-cycle
    /// refresh, and `exec`'s per-restart one. One guard, and one record, for
    /// the whole supervised run.
    ///
    /// C7: `short` and the record's path are deliberately **not** refreshed
    /// with it. The short id is this supervisor's *address* -- what
    /// `resolve_prefix` hands a sender, what `send --to-session` and `zirv
    /// ctx nudge` store on a message, and what `zirv ctx status` prints for
    /// a human to type. Rotating it every cycle or restart meant a message
    /// addressed to a live session became permanently undeliverable the
    /// moment that session was replaced, which is the whole "stranded mail"
    /// class of bug: the sender resolved a real address, and the supervisor
    /// then stopped answering to it. The session *id* rotates (that is the
    /// point of a fresh session); the address it can be reached at does not.
    pub fn refresh_session(&mut self, new_session: &str) {
        if self.released {
            return;
        }
        self.record.session = new_session.to_string();
        self.record.started_at = super::state::now_secs();
        self.path = write_record(&self.state, &self.record);
    }

    /// This run's stable delivery address -- see `refresh_session`. Every
    /// mail listing a supervisor performs on its own behalf is scoped to
    /// this, never to `short_id(current session)`.
    ///
    /// Read back by `exec`'s nudge-relaunch mail listing specifically because
    /// it is the one value demonstrably unaffected by the
    /// `refresh_session` call immediately above it.
    pub fn short(&self) -> &str {
        &self.record.short
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
    // `read_dir` yields records in a filesystem-dependent order, so a caller
    // that indexes the list positionally (the dashboard sidebar re-reads it
    // every ~1s) would see rows reorder under the operator whenever an
    // unrelated session registers or exits. Sort by a stable key -- the launch
    // time, then the short id as a tiebreak -- so the ordering is deterministic
    // across refreshes regardless of how the directory happened to enumerate.
    found.sort_by(|a, b| {
        a.0.started_at
            .cmp(&b.0.started_at)
            .then_with(|| a.0.short.cmp(&b.0.short))
    });
    sweep_orphaned_markers(state, &found);
    found
}

/// C8: a wake-up marker outlives its session whenever the supervisor died
/// before its own poll could claim it (killed, crashed, or simply never
/// bound a socket to notice). Left behind, it is claimed by the *next*
/// supervisor that happens to register under the same short id -- which,
/// now that short ids are stable addresses rather than per-cycle values, is
/// a real possibility rather than a theoretical one. Swept alongside the
/// stale record it belonged to, on the same read.
///
/// Only markers with no *live* record are removed: a marker whose session is
/// alive is simply one that has not been claimed yet.
fn sweep_orphaned_markers(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("nudge") {
            continue;
        }
        let Some(short) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let has_live_record = found
            .iter()
            .any(|(record, liveness)| *liveness == Liveness::Live && record.short == short);
        if !has_live_record {
            let _ = std::fs::remove_file(&path);
        }
    }
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

/// What `claim_nudge_marker` reports when the marker carries no usable
/// sender -- an empty or unreadable file, or one written by a build that
/// predates C4.
pub const UNKNOWN_SENDER: &str = "unknown";

/// Best-effort, matching every other piece of state-dir housekeeping in this
/// module: a marker that fails to write just means the wake-up is missed and
/// the nudge's mail (already durable) is picked up at the next natural poll
/// or cycle instead of immediately.
///
/// C4: the marker's *contents* are the sender's own short id. The marker
/// used to be empty, so the supervisor that claimed it had nothing to name
/// and every announcement fell back to reporting the claiming session's own
/// id -- "nudged by <myself>", which was simply false.
fn write_nudge_marker(state: &StateDir, short: &str, from: &str) {
    let _ = super::state::create_private_dir_all(&state.sessions());
    let _ = std::fs::write(nudge_marker_path(state, short), from.as_bytes());
}

/// Atomically claims the wake-up marker for `short`, returning who sent it.
///
/// `std::fs::remove_file` is the atomic claim, the same idiom `mail::
/// claim_and_write` and `mail::consume` build on: exactly one racing
/// observer ever sees `Ok(())`, every other one sees `NotFound`, so two
/// supervisors polling the same session at once cannot both act on the same
/// wake-up. The read happens *before* the claim (there is nothing left to
/// read afterwards) and is best-effort: the remove is what decides who won,
/// so a failed read only costs the sender's name, never the wake-up itself.
pub fn claim_nudge_marker(state: &StateDir, short: &str) -> Option<String> {
    let path = nudge_marker_path(state, short);
    let contents = std::fs::read_to_string(&path).ok();
    if std::fs::remove_file(&path).is_err() {
        return None;
    }
    Some(
        contents
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| UNKNOWN_SENDER.to_string()),
    )
}

/// The `for_session` filter a supervisor passes to `mail::list` when listing
/// mail *for itself*.
///
/// `latched` is the registry short once this run has registered -- the
/// address senders actually resolved, stable for the whole run. `current` is
/// the short of whatever session is running right now, used only in the
/// window before registration (where it is, by construction, the address
/// about to be registered).
///
/// Never returns `None`. `None` means "apply no session filter at all", which
/// makes a supervisor read *and consume* every directed message in the repo,
/// including ones addressed to other sessions. That is exactly what `loop`
/// used to do, and it is why this is a named function rather than an inline
/// `unwrap_or`: reverting any seam to `None` or to `short_id(current
/// session)` should look wrong at the call site.
pub fn delivery_filter<'a>(latched: Option<&'a str>, current: &'a str) -> Option<&'a str> {
    Some(latched.unwrap_or(current))
}

/// How much of a short id `zirv ctx nudge` insists on. `resolve_prefix`
/// accepts any *unique* prefix, which is the right rule for a read-only
/// lookup but the wrong one for a write: a single mistyped character can
/// still be unique, and the nudge then wakes -- and can restart -- a session
/// the operator never meant to touch. Four characters is 16 bits of the
/// eight-hex-character short id, enough that a typo lands on "no session
/// matches" rather than on a neighbour.
pub const MIN_NUDGE_PREFIX: usize = 4;

/// The refusal for a too-short nudge target, or `None` when the prefix is
/// long enough to act on. Pure, so the rule is testable without a registry
/// on disk. A prefix that *equals* a live session's whole short id always
/// passes, however short that short id happens to be.
pub fn nudge_prefix_too_short(prefix: &str, live_shorts: &[String]) -> Option<String> {
    if prefix.chars().count() >= MIN_NUDGE_PREFIX {
        return None;
    }
    if live_shorts.iter().any(|short| short == prefix) {
        return None;
    }
    let listed = if live_shorts.is_empty() {
        "none registered".to_string()
    } else {
        live_shorts.join(", ")
    };
    Some(format!(
        "prefix too short (a nudge needs at least {MIN_NUDGE_PREFIX} characters, \
         or a session's whole short id); sessions: {listed}"
    ))
}

/// Every live session's short id, in the order `list` found them. The list a
/// refusal names back to the operator.
fn live_shorts(state: &StateDir) -> Vec<String> {
    list(state)
        .into_iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short)
        .collect()
}

#[derive(Debug, clap::Args)]
pub struct NudgeArgs {
    /// Short id (or a unique prefix of one, at least four characters) of the
    /// live session to nudge.
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
    // Checked before `resolve_prefix`, not after: a one- or two-character
    // prefix is very often *unique* on a machine running a single session, so
    // resolution would happily succeed and nudge (and potentially restart) a
    // session the operator only half-typed.
    if let Some(refusal) = nudge_prefix_too_short(&args.prefix, &live_shorts(&state)) {
        return Err(format!("zirv ctx nudge: {refusal}").into());
    }
    // Only a live session is a valid nudge target: `resolve_prefix` already
    // filters to live records (a stale one was swept from disk by the time a
    // caller could act on it), so an unknown *or* dead session both surface
    // as the same `NotFound`, naming what is actually there instead.
    let record =
        resolve_prefix(&state, &args.prefix).map_err(|e| format!("zirv ctx nudge: {e}"))?;

    // NEW-3: a supervisor with no turn-signal socket claims no wake-up
    // markers, so it cannot act on a nudge *or* advise about one -- the
    // marker would simply sit on disk until swept. Refused with the reason,
    // rather than accepted into a silence the operator has no way to
    // distinguish from a bug. The mail path is still open to them: plain
    // `zirv ctx send` stores a message the session's *next* run will read.
    if !record.reachable {
        return Err(format!(
            "zirv ctx nudge: session {} ({}) is not reachable for nudges -- it is running \
             without a turn-signal socket (`--no-supervise`, or the socket failed to bind), \
             so it never checks for wake-ups and would not even show an advisory. \
             Use `zirv ctx send --to-session {}` to leave a message for its next run.",
            record.short, record.verb, record.short
        )
        .into());
    }

    let body = resolve_nudge_message(args, stdin)?;
    if body.is_empty() {
        return Err(
            "zirv ctx nudge: no message given; pass --message, --message-file, or pipe one on stdin"
                .into(),
        );
    }

    let from_session = super::mail::identity_or_unknown(env, super::adapters::SESSION_ENV);
    let msg = super::mail::Message {
        from_session: from_session.clone(),
        from_agent: super::mail::identity_or_unknown(env, super::adapters::AGENT_ENV),
        to: record.agent.clone(),
        to_session: Some(record.short.clone()),
        sent: super::state::now_secs(),
        body,
    };
    // C1: the *target's* repo slug, not the sender's cwd. The registry is
    // machine-wide, so `resolve_prefix` happily returns a session running in
    // another checkout -- and storing its mail under the sender's slug filed
    // the message in a mailbox that session never reads. A nudge that
    // resolves a session must deliver into that session's own repo.
    // M2: `store_to` -- `record.repo_slug` may name another checkout, whose
    // mailbox this session's `cfg.mail.keep`/`max_message_bytes` must not
    // govern (see `mail::limits_for`).
    super::mail::store_to(
        &state,
        &record.repo_slug,
        &super::state::repo_slug(repo),
        &msg,
        &cfg,
    )?;
    // Written after the mail: losing the marker (crash, or a write failure)
    // must never mean the message itself was lost, only that it is picked up
    // at the next natural poll or cycle instead of immediately.
    //
    // C4: carries the sender's own short id so the woken supervisor can name
    // who nudged it instead of reporting itself.
    write_nudge_marker(&state, &record.short, &short_id(&from_session));

    // C1: names the repo it was actually delivered into, so a cross-repo
    // nudge is visible as one rather than looking like a local delivery that
    // silently went somewhere else.
    writeln!(
        w,
        "zirv ctx nudge: queued for {} ({}, {}) in {}",
        record.short, record.agent, record.verb, record.repo_slug
    )?;
    // N6: an interactive session is only ever *advised* of a nudge -- it is
    // never restarted and never typed into, and it never receives message
    // bodies. Saying so here is the difference between "nothing happened,
    // the nudge is broken" and "the operator on the other end has to go read
    // it", which is the actual contract.
    if matches!(record.verb, Verb::Wrap | Verb::Chat) {
        writeln!(
            w,
            "zirv ctx nudge: {} is an interactive session; the guidance is delivered as \
             inbox mail plus an on-screen advisory, never typed into the agent",
            record.short
        )?;
    }
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
    fn a_record_without_owner_pid_deserializes_as_unowned() {
        // A record written by a build that predates `owner_pid` has no such
        // key in its JSON at all -- not `null`, simply absent -- which is
        // what `#[serde(default)]` (rather than a required field) exists to
        // survive.
        let json = r#"{
            "session": "11111111-2222-4333-8444-555555555555",
            "short": "11111111",
            "agent": "claude",
            "repo": "/repo",
            "repo_slug": "-repo",
            "verb": "exec",
            "pid": 1,
            "started_at": 0,
            "reachable": true
        }"#;
        let record: Record = serde_json::from_str(json).expect("deserialize");
        assert_eq!(record.owner_pid, None);
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

    /// Finding 3: `owner_pid` used to be stamped only by `dash/pane.rs`, so
    /// every *other* registration path -- a standalone `wrap`/`exec`/`loop`
    /// session in particular -- was written with `owner_pid: None`, an owner
    /// no dashboard could ever match, even though the registering process
    /// itself is a perfectly good owner to record. Moving the stamp into
    /// `register` itself fixes that uniformly: every registration is now
    /// attributed to whichever process actually called it. (This does not
    /// reach `zirv ctx agent`'s dashboard-refused-but-retryable fallback,
    /// which runs in the *requester's* process rather than the dashboard's
    /// even when it was dispatched on that dashboard's behalf -- a separate,
    /// accepted residual; see `owner_pid`'s own doc comment.)
    #[test]
    fn register_stamps_owner_pid_with_the_current_process_unless_already_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        let record = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        assert_eq!(record.owner_pid, None, "unstamped before registration");
        let guard = SessionGuard::register(&state, record);
        assert_eq!(
            guard.record().owner_pid,
            Some(std::process::id()),
            "register stamps the current process's own pid"
        );

        let mut explicit = record_for("22222222-3333-4444-8555-666666666666", &repo, Verb::Exec);
        explicit.owner_pid = Some(999);
        let guard = SessionGuard::register(&state, explicit);
        assert_eq!(
            guard.record().owner_pid,
            Some(999),
            "an owner the caller already set is left alone"
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

    /// MED: `list` sorts by a stable key (`started_at`, then `short`) rather
    /// than returning records in filesystem enumeration order, so a caller
    /// that indexes positionally (the dashboard sidebar) sees a deterministic
    /// ordering across refreshes. The `started_at` values here are chosen so
    /// the correct order is neither the shorts' alphabetical order nor any
    /// plausible directory order, pinning `started_at` as the primary key.
    #[test]
    fn list_returns_records_in_a_stable_sorted_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        // (session, started_at): sorted by started_at gives ccc, bbb, aaa --
        // the reverse of the shorts' own alphabetical order.
        let seeds = [
            ("cccccccc-2222-4333-8444-555555555555", 100u64),
            ("aaaaaaaa-2222-4333-8444-555555555555", 300u64),
            ("bbbbbbbb-2222-4333-8444-555555555555", 200u64),
        ];
        for (session, started_at) in seeds {
            let mut record = record_for(session, &repo, Verb::Exec);
            record.started_at = started_at;
            write_record(&state, &record);
        }

        let order: Vec<String> = list(&state).into_iter().map(|(r, _)| r.short).collect();
        assert_eq!(
            order,
            vec![
                "cccccccc".to_string(),
                "bbbbbbbb".to_string(),
                "aaaaaaaa".to_string(),
            ],
            "records come back ordered by started_at, deterministically"
        );
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

    /// C7: a refresh rotates the session *id* but keeps the record's short
    /// id -- the supervisor's stable delivery address -- and therefore its
    /// file. Rotating the address was what stranded mail addressed to a live
    /// session the moment the next cycle or restart replaced it.
    #[test]
    fn a_loop_keeps_one_record_and_refreshes_the_session_id_each_cycle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let first = record_for("eeeeeeee-2222-4333-8444-555555555555", &repo, Verb::Loop);
        let stable_short = first.short.clone();
        let record_file = record_path(&state, &stable_short);

        let mut guard = SessionGuard::register(&state, first);
        assert!(record_file.is_file());

        let second_session = "ffffffff-2222-4333-8444-555555555555";
        guard.refresh_session(second_session);

        assert!(
            record_file.is_file(),
            "the record stays under its original short id -- that is its address"
        );
        assert_eq!(
            guard.short(),
            stable_short,
            "the delivery address survives a refresh"
        );
        assert!(
            !record_path(&state, &short_id(second_session)).exists(),
            "no second file appears under the new session's own short id"
        );
        assert_eq!(
            guard.record().session,
            second_session,
            "the session id itself does rotate"
        );
        assert_eq!(
            guard.record().verb,
            Verb::Loop,
            "the verb survives a refresh"
        );

        // Only one record for the whole run at any given time.
        let found = list(&state);
        assert_eq!(found.len(), 1, "one record, not one per cycle: {found:?}");

        guard.release();
        assert!(!record_file.exists());
    }

    /// The point of the stable address, stated as the delivery property it
    /// exists to protect: a sender resolves a live session, addresses a
    /// message at the short id it got, and the supervisor still finds that
    /// message after its session has rotated underneath it.
    #[test]
    fn directed_mail_survives_a_session_rotation_and_still_reaches_the_supervisor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = state_in(&tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);

        let record = record_for("aaaa1111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let mut guard = SessionGuard::register(&state, record);

        // A sender resolves the live session and addresses it, exactly as
        // `send --to-session` / `nudge` do.
        let addressed = super::super::sessions::resolve_prefix(&state, "aaaa1111")
            .expect("the live session resolves");
        let msg = super::super::mail::Message {
            from_session: "bbbb2222".to_string(),
            from_agent: "codex".to_string(),
            to: "claude".to_string(),
            to_session: Some(addressed.short.clone()),
            sent: super::super::state::now_secs(),
            body: "the webhook route moved".to_string(),
        };
        super::super::mail::store(&state, &slug, &msg, &cfg).expect("store");

        // ... and then the supervisor restarts, minting a fresh session id.
        guard.refresh_session("cccc3333-2222-4333-8444-555555555555");

        let delivered =
            super::super::mail::list(&state, &slug, Some("claude"), Some(guard.short()))
                .expect("list");
        assert_eq!(
            delivered.len(),
            1,
            "the message must still reach the supervisor it was addressed to"
        );
        assert_eq!(delivered[0].1.body, "the webhook route moved");

        // And it is still *only* reachable by that address: a different
        // supervisor must not pick it up.
        let other = super::super::mail::list(&state, &slug, Some("claude"), Some("zzzz9999"))
            .expect("list");
        assert!(
            other.is_empty(),
            "directed mail stays directed: {:?}",
            other
        );
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
        for verb in [Verb::Exec, Verb::Loop, Verb::Wrap, Verb::Chat, Verb::Dash] {
            let record = record_for("00000000-2222-4333-8444-555555555555", &repo, verb);
            let path = write_record(&state, &record);
            let raw = std::fs::read_to_string(&path).expect("read");
            assert!(
                raw.contains(&format!("\"{}\"", verb.as_str())),
                "verb {verb} must serialize as its lowercase word: {raw}"
            );
        }
    }

    #[test]
    fn verb_dash_serializes_lowercase() {
        assert_eq!(Verb::Dash.as_str(), "dash");
        let json = serde_json::to_string(&Verb::Dash).expect("serialize");
        assert_eq!(json, "\"dash\"");
        let back: Verb = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Verb::Dash);
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
        write_nudge_marker(&state, "aaaa1111", "bbbb2222");

        assert_eq!(
            claim_nudge_marker(&state, "aaaa1111").as_deref(),
            Some("bbbb2222"),
            "the first observer claims it, and learns who sent it"
        );
        assert_eq!(
            claim_nudge_marker(&state, "aaaa1111"),
            None,
            "a second observer finds nothing left to claim"
        );
    }

    /// C4: the marker carries the *sender's* short id. Every emitter used to
    /// pass its own id into `Event::Nudge`, so the announcement always read
    /// "nudged by <myself>".
    #[test]
    fn claiming_a_marker_reports_who_sent_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());

        write_nudge_marker(&state, "target01", "sender99");
        assert_eq!(
            claim_nudge_marker(&state, "target01").as_deref(),
            Some("sender99")
        );

        // An empty marker (a pre-C4 writer, or a truncated write) still
        // claims -- the wake-up matters more than the attribution.
        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        std::fs::write(nudge_marker_path(&state, "target01"), b"").expect("write");
        assert_eq!(
            claim_nudge_marker(&state, "target01").as_deref(),
            Some(UNKNOWN_SENDER)
        );

        // Whitespace is not a sender either.
        std::fs::write(
            nudge_marker_path(&state, "target01"),
            b"  
",
        )
        .expect("write");
        assert_eq!(
            claim_nudge_marker(&state, "target01").as_deref(),
            Some(UNKNOWN_SENDER)
        );
    }

    /// C8: a marker whose session is gone is swept with the record, on the
    /// same read. Left behind, it would be claimed by whichever supervisor
    /// next registers under that short id -- much likelier now that a short
    /// id is a stable address rather than a per-cycle value.
    #[test]
    fn orphaned_markers_are_swept_with_their_records() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        // One live session with an unclaimed marker, one dead session with
        // one, and one marker whose record never existed at all.
        let live = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let live_short = live.short.clone();
        write_record(&state, &live);
        write_nudge_marker(&state, &live_short, "sender01");

        let mut dead = record_for("22222222-2222-4333-8444-555555555555", &repo, Verb::Exec);
        dead.pid = dead_pid();
        let dead_short = dead.short.clone();
        write_record(&state, &dead);
        write_nudge_marker(&state, &dead_short, "sender02");

        write_nudge_marker(&state, "99999999", "sender03");

        let _ = list(&state);

        assert!(
            nudge_marker_path(&state, &live_short).is_file(),
            "a live session's unclaimed marker is left alone"
        );
        assert!(
            !nudge_marker_path(&state, &dead_short).exists(),
            "a dead session's marker is swept with its record"
        );
        assert!(
            !nudge_marker_path(&state, "99999999").exists(),
            "a marker with no record at all is swept too"
        );
    }

    // F6: a unique-but-mistyped prefix must not be actionable.

    #[test]
    fn a_nudge_prefix_shorter_than_four_characters_is_refused_with_the_session_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Chat);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        // "abc" is *unique* here -- exactly the shape that used to resolve and
        // nudge the wrong session on a typo.
        let args = NudgeArgs {
            prefix: "abc".to_string(),
            message: Some("wake up".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("three characters is not enough to nudge on");
        let msg = err.to_string();
        assert!(msg.contains("prefix too short"), "got {msg}");
        assert!(msg.contains(&short), "names what could be typed: {msg}");

        // Nothing was queued and nothing was woken.
        let slug = super::super::state::repo_slug(&repo);
        assert!(
            super::super::mail::list(&state, &slug, None, Some(&short))
                .expect("list")
                .is_empty(),
            "a refused nudge must not store a message"
        );
        assert!(!nudge_marker_path(&state, &short).exists());

        // Four characters, and the whole short id, both still work.
        let args = NudgeArgs {
            prefix: "abcd".to_string(),
            message: Some("wake up".to_string()),
            message_file: None,
        };
        let code = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("four characters is enough");
        assert_eq!(code, 0);
    }

    #[test]
    fn the_minimum_prefix_rule_still_admits_a_whole_short_id() {
        // Pure rule, no registry: a session whose entire short id is shorter
        // than the minimum is still addressable by that whole id, and only by
        // it.
        let shorts = vec!["ab".to_string()];
        assert_eq!(nudge_prefix_too_short("ab", &shorts), None);
        assert!(nudge_prefix_too_short("a", &shorts).is_some());
        assert_eq!(nudge_prefix_too_short("abcd", &[]), None);
        let refusal = nudge_prefix_too_short("x", &[]).expect("refused");
        assert!(refusal.contains("none registered"), "got {refusal}");
    }

    // NEW-2: the delivery address each supervisor lists its own mail under.
    // Reverting a seam to `None` (loop's old behavior) or to
    // `short_id(current session)` (exec's old behavior) has to break
    // something, not just quietly change routing.

    #[test]
    fn the_delivery_filter_is_never_an_unfiltered_listing() {
        // `None` means "no session filter at all", which makes a supervisor
        // read *and consume* other sessions' directed mail.
        assert!(delivery_filter(Some("aaaa1111"), "bbbb2222").is_some());
        assert!(delivery_filter(None, "bbbb2222").is_some());
    }

    #[test]
    fn the_loop_delivery_address_is_the_registry_short() {
        // Before the first cycle registers there is nothing latched, so the
        // address is the short about to be registered.
        assert_eq!(delivery_filter(None, "cycle001"), Some("cycle001"));

        // From then on the latched registry short wins over whatever short
        // the current cycle happens to have minted.
        assert_eq!(
            delivery_filter(Some("cycle001"), "cycle002"),
            Some("cycle001"),
            "a loop must keep answering to the address a sender resolved,              not to this cycle's own fresh id"
        );
        assert_eq!(
            delivery_filter(Some("cycle001"), "cycle009"),
            Some("cycle001"),
            "and must keep doing so however many cycles later"
        );
    }

    /// The exec seam, expressed against a real guard: the address a sender
    /// resolved has to keep working after the session underneath it rotates.
    #[test]
    fn the_exec_delivery_address_survives_session_rotation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("aaaa1111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let launch_short = record.short.clone();
        let mut guard = SessionGuard::register(&state, record);

        assert_eq!(
            delivery_filter(Some(guard.short()), &launch_short),
            Some(launch_short.as_str()),
            "at launch the guard's address and the launch short agree"
        );

        // A restart mints a fresh session id...
        let restarted = "bbbb2222-2222-4333-8444-555555555555";
        guard.refresh_session(restarted);
        let rotated_short = short_id(restarted);
        assert_ne!(
            rotated_short, launch_short,
            "sanity: the session's own short really did change"
        );

        // ...and the delivery address does not follow it.
        assert_eq!(
            delivery_filter(Some(guard.short()), &rotated_short),
            Some(launch_short.as_str()),
            "mail addressed before the restart must still reach this run"
        );
    }

    // F2: the nesting guard.

    #[test]
    fn nested_session_evidence_names_every_signal_it_found() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        assert_eq!(
            nested_session_evidence(&|k| empty.get(k).cloned()),
            None,
            "a plain terminal is not nested"
        );

        let env = env_map(&[(
            super::super::adapters::SESSION_ENV,
            "abcdef12-3456-4789-8abc-def012345678",
        )]);
        let evidence = nested_session_evidence(&|k| env.get(k).cloned()).expect("nested");
        assert!(evidence.contains("ZIRV_CTX_SESSION"), "got {evidence}");
        assert!(
            evidence.contains("abcdef12"),
            "names the outer session: {evidence}"
        );

        let env = env_map(&[(super::super::adapters::SOCKET_ENV, "/tmp/sock")]);
        let evidence = nested_session_evidence(&|k| env.get(k).cloned()).expect("nested");
        assert!(evidence.contains("ZIRV_CTX_SOCKET"), "got {evidence}");

        // Either Claude Code marker alone is too weak; the pair is not.
        let only_flag = env_map(&[("CLAUDECODE", "1")]);
        assert_eq!(
            nested_session_evidence(&|k| only_flag.get(k).cloned()),
            None
        );
        let pair = env_map(&[("CLAUDECODE", "1"), ("CLAUDE_PID", "4242")]);
        let evidence = nested_session_evidence(&|k| pair.get(k).cloned()).expect("nested");
        assert!(evidence.contains("Claude Code"), "got {evidence}");

        // An exported-but-empty variable is not evidence of anything.
        let blank = env_map(&[(super::super::adapters::SESSION_ENV, "  ")]);
        assert_eq!(nested_session_evidence(&|k| blank.get(k).cloned()), None);
    }

    /// A pane's own child inherits `DASH_REQUESTS_ENV` from the dashboard
    /// that spawned it (see `dash::run_dashboard`'s own turn_env assembly);
    /// this pins that the guard actually fires on it, the same as it does
    /// for `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET` above. The dashboard's own
    /// startup never has this set in its own process environment -- it only
    /// ever exports it into a pane's turn_env, never its own -- so this is
    /// evidence a *pane* owns the terminal, never a self-trip on the
    /// dashboard's own launch.
    #[test]
    fn dash_requests_env_trips_the_nested_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let requests = tmp
            .path()
            .join("dash")
            .join("aaaa1111-0123")
            .join("requests");
        std::fs::create_dir_all(&requests).expect("mkdir");
        // A live dashboard writes its own pid into `owner.pid`; this test
        // process stands in for that live dashboard.
        std::fs::write(
            requests.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        let dir = requests.display().to_string();
        let env = env_map(&[(
            super::super::dash::spawnreq::DASH_REQUESTS_ENV,
            dir.as_str(),
        )]);
        let evidence =
            nested_session_evidence(&|k| env.get(k).cloned()).expect("a pane owns this terminal");
        assert!(
            evidence.contains(super::super::dash::spawnreq::DASH_REQUESTS_ENV),
            "got {evidence}"
        );
        assert!(evidence.contains("dashboard pane"), "got {evidence}");
    }

    /// O5: the dashboard removes its request directory on quit, so a shell
    /// that outlived one carries a value naming nothing. `agent::
    /// try_join_dashboard` has always required the directory to exist before
    /// it will use the channel; this guard must agree, or a survivor process
    /// is refused an interactive session on the strength of a dashboard that
    /// is gone.
    #[test]
    fn a_stale_dash_requests_path_is_not_evidence_of_a_live_dashboard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let gone = tmp
            .path()
            .join("dash")
            .join("aaaa1111-0123")
            .join("requests")
            .display()
            .to_string();
        let env = env_map(&[(
            super::super::dash::spawnreq::DASH_REQUESTS_ENV,
            gone.as_str(),
        )]);
        assert_eq!(nested_session_evidence(&|k| env.get(k).cloned()), None);
    }

    /// MED (read side of the leaked-spawn-request-dir wedge): a requests
    /// directory that still exists is evidence a dashboard owns the terminal
    /// only when its `owner.pid` names a live process. Missing, or naming a
    /// dead pid, is no evidence -- an abnormally-exited dashboard must not
    /// wedge every future interactive launch.
    #[test]
    fn only_a_live_dashboard_owner_pidfile_counts_as_a_dashboard_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let make = |name: &str| {
            let requests = tmp.path().join("dash").join(name).join("requests");
            std::fs::create_dir_all(&requests).expect("mkdir");
            requests
        };
        let env_for = |requests: &Path| {
            env_map(&[(
                super::super::dash::spawnreq::DASH_REQUESTS_ENV,
                requests.to_str().expect("utf8"),
            )])
        };

        // Missing owner.pid: a directory alone is no evidence.
        let missing = make("aaaa1111-0001");
        let env = env_for(&missing);
        assert_eq!(
            nested_session_evidence(&|k| env.get(k).cloned()),
            None,
            "a requests dir with no owner.pid does not wedge the terminal"
        );

        // owner.pid naming a dead process: a crashed dashboard is no evidence.
        let dead = make("bbbb2222-0002");
        std::fs::write(
            dead.parent().expect("parent").join("owner.pid"),
            dead_pid().to_string(),
        )
        .expect("write owner.pid");
        let env = env_for(&dead);
        assert_eq!(
            nested_session_evidence(&|k| env.get(k).cloned()),
            None,
            "a dead dashboard's leftover pidfile does not wedge the terminal"
        );

        // owner.pid naming a live process (this one): a real dashboard owns it.
        let live = make("cccc3333-0003");
        std::fs::write(
            live.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        let env = env_for(&live);
        let evidence = nested_session_evidence(&|k| env.get(k).cloned())
            .expect("a live dashboard owner is evidence");
        assert!(evidence.contains("dashboard pane"), "got {evidence}");
    }

    #[test]
    fn allow_nested_overrides_the_guard() {
        let env = env_map(&[(
            super::super::adapters::SESSION_ENV,
            "abcdef12-3456-4789-8abc-def012345678",
        )]);
        let lookup = |k: &str| env.get(k).cloned();
        assert!(
            nesting_refusal("chat", &lookup, false).is_some(),
            "nested by default"
        );
        assert_eq!(
            nesting_refusal("chat", &lookup, true),
            None,
            "--allow-nested is an override"
        );

        let env = env_map(&[
            (
                super::super::adapters::SESSION_ENV,
                "abcdef12-3456-4789-8abc-def012345678",
            ),
            (ALLOW_NESTED_ENV, "true"),
        ]);
        assert_eq!(
            nesting_refusal("chat", &|k| env.get(k).cloned(), false),
            None,
            "ZIRV_ALLOW_NESTED=true is the second override"
        );

        // Strict, like every other boolean read out of the environment here.
        let env = env_map(&[
            (
                super::super::adapters::SESSION_ENV,
                "abcdef12-3456-4789-8abc-def012345678",
            ),
            (ALLOW_NESTED_ENV, "maybe"),
        ]);
        assert!(nesting_refusal("chat", &|k| env.get(k).cloned(), false).is_some());
    }

    #[test]
    fn the_refusal_names_the_verb_the_evidence_and_the_override() {
        let env = env_map(&[(super::super::adapters::SOCKET_ENV, "/tmp/sock")]);
        let msg = nesting_refusal("wrap", &|k| env.get(k).cloned(), false).expect("refused");
        assert!(msg.starts_with("zirv ctx wrap:"), "got {msg}");
        assert!(msg.contains("ZIRV_CTX_SOCKET"), "got {msg}");
        assert!(msg.contains("--allow-nested"), "got {msg}");
        assert!(msg.contains(ALLOW_NESTED_ENV), "got {msg}");
    }

    // F3: no child ever inherits another session's identity.

    #[test]
    fn scrubbing_removes_every_supervision_variable_from_a_pty_builder() {
        let mut builder = portable_pty::CommandBuilder::new("echo");
        for key in SUPERVISION_ENV {
            builder.env(key, "inherited-from-the-outer-session");
            assert!(builder.get_env(key).is_some(), "sanity: {key} was set");
        }
        scrub_supervision_env(&mut builder);
        for key in SUPERVISION_ENV {
            assert_eq!(builder.get_env(key), None, "{key} must not reach the child");
        }
    }

    #[test]
    fn scrubbing_removes_every_supervision_variable_from_a_process_command() {
        let mut command = std::process::Command::new("echo");
        scrub_supervision_env_cmd(&mut command);
        let removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str())
            .collect();
        for key in SUPERVISION_ENV {
            assert!(removed.contains(&key), "{key} must be removed: {removed:?}");
        }
    }

    /// C1: `resolve_prefix` searches the machine-wide registry, so a nudge
    /// can land on a session running in a different checkout. Storing its
    /// payload under the *sender's* repo slug filed it in a mailbox that
    /// session never reads -- the wake-up fired, the message never arrived.
    #[test]
    fn a_cross_repo_nudge_delivers_into_the_targets_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);

        let sender_repo = tmp.path().join("sender-repo");
        let target_repo = tmp.path().join("target-repo");
        let record = record_for(
            "abcdef12-3456-4789-8abc-def012345678",
            &target_repo,
            Verb::Exec,
        );
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: short.clone(),
            message: Some("please check the new failing test".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_nudge_with(
            &args,
            &mut out,
            &sender_repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge");
        assert_eq!(code, 0);

        let delivered = super::super::mail::list(
            &state,
            &super::super::state::repo_slug(&target_repo),
            None,
            Some(&short),
        )
        .expect("list");
        assert_eq!(
            delivered.len(),
            1,
            "the payload must land in the repo the target session actually reads"
        );
        assert_eq!(delivered[0].1.body, "please check the new failing test");

        assert!(
            super::super::mail::list(
                &state,
                &super::super::state::repo_slug(&sender_repo),
                None,
                None
            )
            .expect("list")
            .is_empty(),
            "nothing is filed under the sender's own repo"
        );

        // C1: the confirmation names the resolved target and where it went.
        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains(&short), "names the session: {printed}");
        assert!(
            printed.contains(&super::super::state::repo_slug(&target_repo)),
            "names the repo it was delivered into: {printed}"
        );
    }

    /// N6: an interactive target is never restarted and never typed into, and
    /// never receives message bodies. Saying so is the difference between
    /// "the nudge is broken" and "a human has to go read it".
    #[test]
    fn nudging_an_interactive_session_says_it_is_advisory_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);

        for verb in [Verb::Chat, Verb::Wrap] {
            let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, verb);
            let short = record.short.clone();
            let mut guard = SessionGuard::register(&state, record);

            let args = NudgeArgs {
                prefix: short.clone(),
                message: Some("look at this".to_string()),
                message_file: None,
            };
            let mut out = Vec::new();
            let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
            run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
                .expect("nudge");
            let printed = String::from_utf8(out).expect("utf8");
            assert!(
                printed.contains("interactive session"),
                "{verb} must be called out as interactive: {printed}"
            );
            assert!(
                printed.contains("inbox"),
                "and must say where the guidance actually shows up: {printed}"
            );
            guard.release();
        }

        // A headless target says nothing of the sort -- it really does act
        // on the guidance.
        let record = record_for("bbbbbbbb-3456-4789-8abc-def012345678", &repo, Verb::Exec);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);
        let args = NudgeArgs {
            prefix: short,
            message: Some("look at this".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("nudge");
        assert!(
            !String::from_utf8(out)
                .expect("utf8")
                .contains("interactive"),
            "a headless worker is not advisory-only"
        );
    }

    // NEW-3: a supervisor with no turn-signal socket stays *visible* but is
    // refused as a nudge target, rather than being hidden from the registry
    // (which cured the silent nudge by making the session invisible).

    #[test]
    fn an_unsupervised_wrap_is_not_a_nudge_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let record =
            record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Wrap).unreachable();
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        // Still listed -- an operator must be able to see it running.
        let listed = list(&state);
        assert_eq!(listed.len(), 1, "an unreachable session is not hidden");
        assert!(!listed[0].0.reachable);
        assert_eq!(listed[0].1, Liveness::Live);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: short.clone(),
            message: Some("please look at this".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("a session with no signal socket cannot act on a nudge");
        let msg = err.to_string();
        assert!(msg.contains("not reachable"), "got {msg}");
        assert!(
            msg.contains("turn-signal socket"),
            "must say why, not just that: {msg}"
        );
        assert!(
            msg.contains("zirv ctx send"),
            "must offer the thing that does work: {msg}"
        );

        // Nothing was queued and no marker was left behind to be claimed by
        // whatever registers under this address next.
        let slug = super::super::state::repo_slug(&repo);
        assert!(
            super::super::mail::list(&state, &slug, None, Some(&short))
                .expect("list")
                .is_empty()
        );
        assert!(!nudge_marker_path(&state, &short).exists());
    }

    /// A record written by a build that predates the field must still parse,
    /// and must be treated as an ordinary reachable session.
    #[test]
    fn a_record_without_the_reachable_field_parses_as_reachable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        let legacy = format!(
            r#"{{"session":"abcdef12-3456-4789-8abc-def012345678","short":"abcdef12",
               "agent":"claude","repo":"/work/repo","repo_slug":"-work-repo",
               "verb":"wrap","pid":{},"started_at":1700000000}}"#,
            std::process::id()
        );
        std::fs::write(state.sessions().join("abcdef12.json"), legacy).expect("write");

        let found = list(&state);
        assert_eq!(found.len(), 1, "the legacy record still parses: {found:?}");
        assert!(
            found[0].0.reachable,
            "an older record has no opinion, so it is treated as reachable"
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
