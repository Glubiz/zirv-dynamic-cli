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
/// `SEAT_MODEL_ENV` rides along for the same reason: it names *this*
/// session's seat, and a worker that inherits an orchestrator's copy would
/// have its own subagent dispatches refused by a guard describing a seat it
/// is not sitting in.
pub const SUPERVISION_ENV: [&str; 5] = [
    super::adapters::SESSION_ENV,
    super::adapters::SOCKET_ENV,
    super::adapters::SEAT_MODEL_ENV,
    super::wrap::TRANSCRIPT_ENV,
    super::adapters::LAUNCH_MODE_ENV,
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
///
/// `pub(crate)` (issue #144): also the liveness half of `agent::
/// try_join_dashboard`'s own gate, so the two readers of `DASH_REQUESTS_ENV`
/// cannot drift on what "live" means the way they did before -- this guard
/// used to be the only one of the two that checked `owner.pid` at all, so a
/// dashboard that exited abnormally left a directory `try_join_dashboard`
/// still treated as a live channel: a request was written into it, nobody
/// was listening, and the caller burned the whole ack timeout finding that
/// out.
///
/// A thin `bool` projection of [`dashboard_owner_liveness`] -- see that
/// function's own doc comment for the reason `try_join_dashboard` needs
/// instead of just this yes/no answer (fix round 1, both reviewers: a silent
/// refusal here is undiagnosable, the same complaint issue #144's own
/// acceptance criteria raised about the three "dashboard did not answer"
/// messages).
pub(crate) fn dashboard_owner_is_live(requests_dir: &Path) -> bool {
    matches!(dashboard_owner_liveness(requests_dir), OwnerLiveness::Live)
}

/// Why [`dashboard_owner_is_live`] answered the way it did for `requests_dir`
/// -- the same three-way distinction its own doc comment already draws
/// ("missing, unreadable, unparseable, or dead-pid... all mean 'no live
/// dashboard'"), just not collapsed to a bool: `agent::try_join_dashboard`
/// needs the reason to report ("dead owner pid N" vs "missing owner.pid") to
/// an operator who would otherwise see this refusal in total silence.
/// `Missing` folds the unreadable and unparseable cases in with a genuinely
/// absent file -- all three mean the same thing to a caller reporting this
/// upward: no pid was ever recorded to check liveness against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerLiveness {
    Live,
    Missing,
    Dead(u32),
}

pub(crate) fn dashboard_owner_liveness(requests_dir: &Path) -> OwnerLiveness {
    if !requests_dir.is_dir() {
        return OwnerLiveness::Missing;
    }
    let Some(parent) = requests_dir.parent() else {
        return OwnerLiveness::Missing;
    };
    let Ok(contents) = std::fs::read_to_string(parent.join("owner.pid")) else {
        return OwnerLiveness::Missing;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return OwnerLiveness::Missing;
    };
    if is_alive(pid) {
        OwnerLiveness::Live
    } else {
        OwnerLiveness::Dead(pid)
    }
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
    /// Issue #139: the `safety::policy_fingerprint` of the LAUNCH-TIME
    /// snapshot this session was pinned to (the same fingerprint value
    /// written to `POLICY_FINGERPRINT_ENV`/read back by `evaluate_with_
    /// attestation_evidence`), when the launch computed one at all. `None`
    /// for a record written by an older build, a launch that never
    /// attempted attestation (no adapter support, or the fingerprint could
    /// not be computed), or any session type this field is not yet threaded
    /// through to. `status.rs` compares this against a freshly loaded
    /// policy's own fingerprint for `record.repo` to surface a "policy
    /// snapshot stale" line -- see `Modules/Ctx Subsystem.md`.
    /// `#[serde(default)]` so an on-disk record from an older build
    /// deserializes as `None` rather than failing to parse.
    #[serde(default)]
    pub safety_policy_sha256: Option<String>,
    /// Issue #169: the `prompt::PromptRole` label (`"orchestrator"`,
    /// `"sub-orchestrator"` or `"worker"`) this session was ACTUALLY spawned
    /// with, stamped once by the server that spawned it (`Pane::spawn`,
    /// `wrap::run_with`) -- never by anything the session itself later
    /// claims. Plain `String`, not the `prompt::PromptRole` type itself: this
    /// module has no reason to depend on `prompt.rs`, and every other
    /// plain-vocabulary field here (`agent`, `roster::RosterPane::role`)
    /// already follows the same "label string, not an enum" convention.
    ///
    /// Read by `dash::mod::parent_role_for` (via [`load_record`]) for a
    /// requesting session this dashboard hosts no pane for -- an operator's
    /// own terminal, or a headless coordinator -- which is the only place a
    /// role can be recovered for such a session at all. `None` for a record
    /// written by an older build, or any session type this was never threaded
    /// through to; that reader then falls back to the verb (`Verb::Chat` is
    /// an orchestrator seat, anything else a worker), never to a wider role
    /// than the session could already have had.
    #[serde(default)]
    pub role: Option<String>,
    /// Issue #152: epoch seconds the process that registered this session
    /// itself started, stamped once by [`Record::new`] via
    /// [`process_start_secs`]. Exists so `record_is_alive` can tell the
    /// original process apart from an unrelated one the OS later recycles
    /// this record's `pid` to -- `is_alive`'s own `EPERM` branch has no way
    /// to make that distinction with the pid alone (see its doc comment).
    /// `None` for a record written by an older build, a non-unix platform
    /// (`process_start_secs` has no reader there), or any environment
    /// `process_start_secs` could not read (no `ps` on `PATH`, refused,
    /// unparsable output) -- every one of those degrades `record_is_alive`
    /// back to today's EPERM-is-alive behavior, never to a false "dead".
    /// `#[serde(default)]` so an on-disk record from an older build
    /// deserializes as `None` rather than failing to parse, the same
    /// back-compat pattern `owner_pid` already established.
    #[serde(default)]
    pub start_time: Option<u64>,
    /// Issue #243: the `screen::ScreenReport::summary` of the most
    /// recent scoring cycle that flagged something in the transcript bytes it
    /// ingested. `None` for a clean cycle, a record written before this field
    /// existed, or no scoring cycle yet -- `status.rs`'s own reader.
    #[serde(default)]
    pub last_screening: Option<String>,
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
            // Left unset here too: only a caller that actually resolved a
            // launch-time policy snapshot (and its fingerprint) has
            // anything to record -- see `with_safety_policy_sha256`.
            safety_policy_sha256: None,
            // Left unset here too: only a caller that actually knows the
            // role it spawned (`with_role`) has anything to record.
            role: None,
            // Issue #152: this process's own start time, read the same way
            // `record_is_alive` will later re-read whoever holds this pid --
            // see the field's own doc comment. `None` wherever
            // `process_start_secs` cannot tell, which callers other than
            // `record_is_alive` never need to know about.
            start_time: process_start_secs(std::process::id()),
            last_screening: None,
        }
    }

    /// Marks this record as one that can never act on a wake-up -- see the
    /// `reachable` field. Chained onto `new` at the one call site that knows
    /// whether a turn-signal socket actually bound.
    pub fn unreachable(mut self) -> Self {
        self.reachable = false;
        self
    }

    /// Issue #139: stamps the launch-time safety-policy fingerprint (see the
    /// field's own doc comment), chained onto `new` at whichever call site
    /// already resolved one for this launch. `None` is a legitimate value
    /// (leaves the field unset, the same as never calling this at all) so a
    /// caller that only sometimes has a fingerprint (e.g. attestation is
    /// disabled, or fingerprinting failed) does not need its own branch.
    pub fn with_safety_policy_sha256(mut self, fingerprint: Option<String>) -> Self {
        self.safety_policy_sha256 = fingerprint;
        self
    }

    /// Issue #169: stamps the role (a `prompt::PromptRole::label()` string)
    /// this session was actually spawned with, chained onto `new` at the
    /// call site that resolved one. Forgery-proof by construction: the
    /// caller is the server that decided what to spawn (`Pane::spawn`'s own
    /// `PaneSpec::role`, `wrap::run_with`'s own `role` parameter), never
    /// anything read back from the session's own request.
    pub fn with_role(mut self, role: &str) -> Self {
        self.role = Some(role.to_string());
        self
    }

    /// Issue #186 hardening: preserves a supervisor's already-established
    /// delivery address while the underlying vendor session changes. This is
    /// only used by Zirv's own cross-harness continuation path; callers must
    /// supply the short id of the logical supervisor that is being continued.
    pub fn with_stable_short(mut self, short: &str) -> Self {
        if !short.is_empty() {
            self.short = short.to_string();
        }
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

/// Issue #243: read-modify-write of `short`'s own registry record
/// with a fresh screening summary. Best-effort like every other registry
/// write here -- a hook process is fresh every turn, never the supervisor
/// that holds the live `SessionGuard`, so this is the only way to update a
/// field on a record already on disk.
pub fn set_last_screening(state: &StateDir, short: &str, summary: Option<String>) {
    let Ok(text) = std::fs::read_to_string(record_path(state, short)) else {
        return;
    };
    let Ok(mut record) = serde_json::from_str::<Record>(&text) else {
        return;
    };
    record.last_screening = summary;
    write_record(state, &record);
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

    /// Points this run's record at the pid of the agent child the supervisor
    /// actually spawned, rather than at the supervisor's own pid.
    ///
    /// P5: `Record::new` stamps `std::process::id()`, which for `wrap` is
    /// zirv's pid -- so a `wrap` record stayed "alive" (and stayed offered
    /// for restore, and stayed nudge-targetable) for exactly as long as the
    /// *wrapper* lived, whether or not the agent underneath it was still
    /// there. `dash::pane::Pane::spawn` has always stamped the real child pid
    /// for the same reason; this is that same override, on the one seam
    /// `wrap` has for it. Called right after the spawn, and again after every
    /// relaunch: a record left pointing at a replaced child's dead pid would
    /// be swept by `list` and the live session would vanish from `zirv ctx
    /// status`.
    ///
    /// `owner_pid` is deliberately untouched -- it answers "which process
    /// filed this record", which is still zirv's own, and is what
    /// `dash::assemble_sidebar` scopes its panel by. Like every other write
    /// here, best-effort: `short` and the record's path do not move (see
    /// `refresh_session`), so a failed write costs a stale pid, never an
    /// address.
    ///
    /// Review round 2 finding 1 (issue #152): `start_time` MUST move with
    /// `pid`, not just `pid` alone. `record_is_alive`'s `EPERM` branch
    /// compares whoever currently holds `record.pid` against `record.
    /// start_time` -- leaving the old value in place after repointing `pid`
    /// at a fresh child would compare the CHILD's real start time against
    /// the SUPERVISOR's, which is a guaranteed mismatch (the child always
    /// starts meaningfully after the supervisor that goes on to spawn it).
    /// That is not a hypothetical: it is exactly the everyday sandboxed
    /// case issue #146 was written for -- a live child, probed with `EPERM`
    /// -- and would have `list` delete a perfectly live pane's record.
    /// Re-reading via `process_start_secs(pid)` keeps the two in lockstep;
    /// `None` (no usable `ps`) degrades `start_time` to `None` too, which
    /// is `record_is_alive`'s own "cannot tell" case, never a false mismatch.
    pub fn adopt_child_pid(&mut self, pid: u32) {
        if self.released || self.record.pid == pid {
            return;
        }
        self.record.pid = pid;
        self.record.start_time = process_start_secs(pid);
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

/// The three possible outcomes of a `kill(pid, 0)` signal-0 probe, named so
/// `record_is_alive` (issue #152) can react to the middle one -- `EPERM` --
/// differently from the other two, which `is_alive` folds together (`EPERM`
/// reads as alive there, same as `CanSignal` -- see its own doc comment).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalProbe {
    CanSignal,
    NoSuchProcess,
    PermissionDenied,
}

#[cfg(unix)]
fn probe_signal(pid: u32) -> SignalProbe {
    // SAFETY: signal 0 sends nothing; it only probes existence and
    // permission, the same check `kill -0` makes from a shell.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return SignalProbe::CanSignal;
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
        SignalProbe::PermissionDenied
    } else {
        SignalProbe::NoSuchProcess
    }
}

/// Signal-0 liveness probe (`kill -0`, the same check a shell's own `kill -0
/// <pid>` makes: existence and permission only, nothing is actually sent).
///
/// Issue #146: a plain `kill() == 0` check reads a non-zero return as "dead"
/// outright, which conflates two very different errno values. `ESRCH` (no
/// such process) genuinely does mean dead. `EPERM` means the process exists
/// but this caller lacks permission to signal it -- exactly what a sandboxed
/// `zirv ctx send`/`zirv ctx nudge`, running as a Bash-tool child inside a
/// dash pane, gets when probing the very sessions it is trying to reach.
/// Treating that as "dead" made `list` sweep every live record as `Stale`,
/// so `resolve_prefix` saw zero `Live` candidates and every send/nudge
/// failed with "no sessions are registered" -- issue #146's exact symptom,
/// with genuinely live sessions sitting right there in the registry.
///
/// `pub(crate)`: the single liveness check shared by this whole module
/// (`dashboard_owner_liveness`, `short_is_live` before issue #152, `list`
/// before issue #152) and, since issue #145/#146's fix, by `dash::mod` too
/// (`sweep_stale_token_dirs` and its own discovery scan) -- which used to
/// carry an independent, identically EPERM-blind copy of this exact check
/// rather than importing this one.
///
/// Documented trade-off, not a bug: reading `EPERM` as alive means a pid the
/// kernel has recycled to an unrelated, foreign-uid process keeps a stale
/// session/dashboard record alive until that pid frees again, since this
/// bare pid-only check has no start-time (or any other) disambiguator to
/// tell the original process apart from its replacement. Issue #152
/// addresses this for the one caller that actually has more than a bare pid
/// to work with: a `Record` also carries a `start_time`, and `record_is_alive`
/// below uses it to make exactly that distinction for `short_is_live` and
/// `list`, which now call it instead of this function. This function keeps
/// its original bare-pid, EPERM-is-alive contract unchanged -- callers with
/// no `Record` (`supervise`, `permit`, `dashboard_owner_liveness`,
/// `dash::mod`'s sweeps) still have no disambiguator available and keep
/// today's behavior exactly.
#[cfg(unix)]
pub(crate) fn is_alive(pid: u32) -> bool {
    probe_signal(pid) != SignalProbe::NoSuchProcess
}

#[cfg(windows)]
pub(crate) fn is_alive(pid: u32) -> bool {
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
pub(crate) fn is_alive(_pid: u32) -> bool {
    // No portable liveness check: never sweep a record this platform cannot
    // actually verify.
    true
}

/// How far apart a record's stamped `start_time` and a freshly read one may
/// be before they are considered two different processes rather than the
/// same one read twice.
///
/// Review round 2 finding 2 (issue #152): 10s was too tight. This
/// disambiguator's whole job is to catch a pid the OS recycled to an
/// unrelated process well AFTER the original session died -- in practice
/// hours or days later, since a pid only frees once the original process is
/// long gone and the kernel's pid counter wraps back around to it. It has no
/// business firing on ordinary clock noise: an NTP correction or a manual
/// clock change between registration and a later check can plausibly move
/// either `now_secs()` reading by more than a few seconds without any
/// process having changed at all, and `record_is_alive`'s `EPERM` branch is
/// exactly the sandboxed-probe path issue #146 exists for -- a genuinely
/// live, unrelated-uid session, not a recycled one. 300s (5 minutes) absorbs
/// realistic NTP steps, `ps -o etime=` rounding, and this reader's own
/// now-minus-age derivation slack, while still being a small fraction of the
/// "hours or days" gap the actual recycled-pid failure mode produces. Never
/// sweep a live record on a difference the clock environment alone could
/// plausibly explain.
///
/// Compare `RECYCLED_PID_TOLERANCE_SECS` (`kill`'s own recycled-pid guard,
/// above): both absorb clock/reading slack, but they answer different
/// questions at different magnitudes and must not be unified. `kill`'s guard
/// compares a target's own freshly-read age against `registered_at` -- a
/// ONE-SIDED "is this process younger than its own record" heuristic, where
/// even a few seconds of slack (5s) is enough margin because a genuine
/// session's process always predates its record by a wide, predictable
/// margin (registration happens moments after the process starts). This
/// disambiguator instead compares two INDEPENDENT start-time readings of the
/// same claimed process, taken at different times, against each other --
/// exactly the kind of comparison a clock step disturbs, and with no
/// registration-order assumption to lean on, hence the much wider 300s.
///
/// Only `record_is_alive`'s `#[cfg(unix)]` branch reads this outside of
/// tests -- see its own `#[cfg_attr]`, matching `parse_etime`'s identical
/// non-unix dead-code allowance below.
#[cfg_attr(not(unix), allow(dead_code))]
const START_TIME_TOLERANCE_SECS: u64 = 300;

/// Pure: whether a mismatch between a record's stamped `start_time` and a
/// freshly read one is large enough to mean "a different process now holds
/// this pid" -- issue #152's disambiguator for `is_alive`'s `EPERM` branch
/// (see its own doc comment on the trade-off this closes for `Record`-based
/// liveness).
///
/// Either side missing degrades to "cannot tell", which must read as NOT
/// disambiguating -- i.e. still alive -- per `record_is_alive`'s contract: a
/// record from a build or platform that cannot stamp/read a start time keeps
/// today's EPERM-is-alive behavior exactly, and must never read as falsely
/// dead just because one side of the comparison is missing.
///
/// Only called from the `#[cfg(unix)]` `record_is_alive` and from tests; the
/// non-unix `record_is_alive` never reaches it, mirroring `parse_etime`'s own
/// `#[cfg_attr(not(unix), allow(dead_code))]`.
#[cfg_attr(not(unix), allow(dead_code))]
fn start_time_disambiguates_dead(recorded: Option<u64>, current: Option<u64>) -> bool {
    match (recorded, current) {
        (Some(recorded), Some(current)) => recorded.abs_diff(current) > START_TIME_TOLERANCE_SECS,
        _ => false,
    }
}

/// Epoch seconds the process holding `pid` started, if this platform and
/// environment can tell -- built directly on [`process_age_secs`] (`now -
/// age`), so it inherits that reader's exact "cannot tell" cases (`ps`
/// missing/refused, unparsable output) with no cfg split of its own:
/// `process_age_secs` is already `None` on every non-unix target, which is
/// exactly issue #152's own scope note -- Windows liveness stays on its
/// existing `OpenProcess`/`GetExitCodeProcess` mechanism, unaffected, since
/// `record_is_alive` never calls this off unix.
///
/// `pub(crate)`: every place `Record::pid` is ever repointed at a different
/// process after `Record::new` -- `SessionGuard::adopt_child_pid` here, and
/// `dash::pane::Pane::spawn`'s own `record.pid = child_pid` -- must re-derive
/// `start_time` for the NEW pid in the same breath, or `record_is_alive`
/// compares the new process against the old one's start time and reads a
/// guaranteed, false mismatch (review round 2 finding 1, issue #152).
pub(crate) fn process_start_secs(pid: u32) -> Option<u64> {
    let age = process_age_secs(pid)?;
    Some(super::state::now_secs().saturating_sub(age))
}

/// [`is_alive`], sharpened for a [`Record`]: unlike a bare pid, a record also
/// carries the `start_time` its own process stamped at registration, which
/// is exactly the disambiguator `is_alive`'s own doc comment says a bare
/// signal-0 probe cannot have -- issue #152.
///
/// Unix: a `kill(pid, 0)` that can signal the process answers alive
/// unconditionally (this caller reached it, full stop -- no reason to doubt
/// a start time on top of that), and `ESRCH` answers dead unconditionally,
/// identical to `is_alive`. Only `EPERM` -- "exists, but not one I may
/// signal" -- gets a second opinion: whether the process now holding this pid
/// started around the same time this record's own process did, or
/// meaningfully later (the kernel recycled the pid to something unrelated
/// after the original exited). Missing or unreadable start times on either
/// side degrade to alive, exactly matching `EPERM`'s treatment before this
/// existed.
///
/// Non-unix: identical to `is_alive(record.pid)` -- issue #152's acceptance
/// leaves the Windows liveness mechanism unchanged.
///
/// `short_is_live` and `list`'s sweep are this function's only callers;
/// every other liveness check in this module and `dash::mod` has no
/// `Record` to read a start time from and keeps calling bare `is_alive`.
#[cfg(unix)]
pub fn record_is_alive(record: &Record) -> bool {
    match probe_signal(record.pid) {
        SignalProbe::CanSignal => true,
        SignalProbe::NoSuchProcess => false,
        SignalProbe::PermissionDenied => {
            !start_time_disambiguates_dead(record.start_time, process_start_secs(record.pid))
        }
    }
}

#[cfg(not(unix))]
pub fn record_is_alive(record: &Record) -> bool {
    is_alive(record.pid)
}

/// Whether the registry still holds a record for `short` whose pid is alive.
///
/// P4: a dashboard that was killed (rather than quit) leaves both a restore
/// roster *and* the sessions its panes registered -- and on Windows, before
/// the job-object backstop, those panes' agents genuinely outlived it.
/// Restoring such a candidate spawns a second agent onto a conversation the
/// first one is still holding. Reads the record file directly rather than
/// going through `list`, which sweeps as a side effect: this is a question,
/// not a cleanup. A missing, unreadable or malformed record answers `false`
/// -- nothing to collide with, so the restore may proceed.
pub fn short_is_live(state: &StateDir, short: &str) -> bool {
    load_record(state, short).is_some_and(|record| record_is_alive(&record))
}

/// One registry record, read straight off disk by its short id -- a question,
/// never a cleanup: unlike [`list`], nothing is swept and no liveness is
/// judged here, so a caller that only wants what was RECORDED about a session
/// (`Record::role`, issue #169) does not have to walk, and mutate, the whole
/// registry to find it. `None` for a missing, unreadable or malformed record.
pub fn load_record(state: &StateDir, short: &str) -> Option<Record> {
    std::fs::read_to_string(record_path(state, short))
        .ok()
        .and_then(|contents| serde_json::from_str::<Record>(&contents).ok())
}

/// Every record currently on disk, alongside whether its own process is
/// still alive. A stale record (its process is gone) is swept -- its file
/// removed -- as a side effect of this read, but is still reported in the
/// returned list so a caller can say what it just cleaned up. A file that
/// fails to parse is skipped outright: one malformed record must never fail
/// the whole listing.
pub fn list(state: &StateDir) -> Vec<(Record, Liveness)> {
    let mut found = Vec::new();
    // Issue #99 (2026-08-23): an absent `sessions/` directory used to make
    // this whole function return immediately, before `sweep_orphan_endpoints`
    // below ever ran. That is exactly the state a fresh install, or a
    // machine where every registry record has already been cleaned up some
    // other way, is in -- precisely the case a stray `*.sock` file left by an
    // older zirv build (which predates the registry entirely) needs the
    // sweep to still run. `state.sessions()` missing now only means "no
    // records to list", not "skip every other sweep this function does".
    if let Ok(entries) = std::fs::read_dir(state.sessions()) {
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
            if record_is_alive(&record) {
                found.push((record, Liveness::Live));
            } else {
                let _ = std::fs::remove_file(&path);
                found.push((record, Liveness::Stale));
            }
        }
        // `read_dir` yields records in a filesystem-dependent order, so a
        // caller that indexes the list positionally (the dashboard sidebar
        // re-reads it every ~1s) would see rows reorder under the operator
        // whenever an unrelated session registers or exits. Sort by a stable
        // key -- the launch time, then the short id as a tiebreak -- so the
        // ordering is deterministic across refreshes regardless of how the
        // directory happened to enumerate.
        found.sort_by(|a, b| {
            a.0.started_at
                .cmp(&b.0.started_at)
                .then_with(|| a.0.short.cmp(&b.0.short))
        });
    }
    sweep_orphaned_markers(state, &found);
    sweep_orphan_endpoints(state, &found);
    found
}

/// C9 (issue #99, 2026-08-23): an orphaned turn-signal endpoint -- a
/// `*.sock` file in `state.sockets()` with no matching live session record.
/// `SignalServer::bind` writes one for every supervised session (a real Unix
/// domain socket, or on Windows a marker file naming the pipe), and only
/// `Drop for SignalServer` removes it, which never runs for a killed or
/// crashed process (this binary's release profile is `panic = "abort"`).
/// Left behind, these accumulate and `zirv ctx status` lists every one of
/// them forever as `(no record)` (`status.rs`'s own `sessions_lines`).
///
/// Only a marker whose endpoint fails a connection probe
/// (`signal::probe`) is removed: one that still answers belongs to a
/// supervisor that is alive but was never (or no longer) recorded in the
/// registry -- an older build, or a registry write that failed -- and must
/// stay both on disk and listed. `found` is the same list `list` just
/// computed, so this never calls back into `list` itself, and every record
/// with a live entry is skipped outright without ever touching the network
/// (matching `sweep_orphaned_markers`'s own "only markers with no live
/// record" rule immediately above).
fn sweep_orphan_endpoints(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.sockets()) else {
        return;
    };
    let live: std::collections::BTreeSet<&str> = found
        .iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short.as_str())
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let Some(short) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if live.contains(short) {
            continue;
        }
        if !super::signal::probe(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
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

/// Extends a [`ResolveError`]'s own display text with where the registry was
/// actually checked -- the state dir's `sessions()` subdirectory -- and
/// whether `ZIRV_CTX_STATE_DIR` pinned it or the platform default was used.
///
/// Issue #146: "no sessions are registered" alone gives no way to tell "the
/// registry really is empty" apart from "this call resolved a different
/// state dir than the one the session actually registered under" -- which is
/// exactly the shape of the EPERM-blind liveness bug this same issue fixed
/// (see `is_alive`'s own doc comment): a caller and the supervisor it means
/// to reach can end up looking at different state dirs, or the same dir
/// while one of them can no longer confirm the other alive, and either way
/// the operator sees the identical unhelpful message. Appended, not
/// substituted: the existing `{err}` text stays the prefix, so anything
/// already asserting on it keeps passing (`send`/`nudge`'s own call sites).
pub fn resolve_error_with_diagnostics(
    err: &ResolveError,
    state: &StateDir,
    env: EnvLookup<'_>,
) -> String {
    let state_env_note = match non_empty(env(super::state::STATE_ENV)) {
        Some(_) => format!("{} is set", super::state::STATE_ENV),
        None => format!(
            "{} is not set (using the platform default state dir)",
            super::state::STATE_ENV
        ),
    };
    format!(
        "{err} (registry checked at {}; {state_env_note})",
        state.sessions().display()
    )
}

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

/// Best-effort low-latency notification used after durable mail storage.
/// The marker carries no authority and may be lost without losing mail; live
/// supervisors consume it through the same turn-boundary path as an explicit
/// `zirv ctx nudge`.
pub(crate) fn notify_mail(state: &StateDir, short: &str, from: &str) {
    write_nudge_marker(state, short, from);
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

/// How much of a short id a write against a resolved session (`zirv ctx
/// nudge`, `zirv ctx kill`) insists on. `resolve_prefix` accepts any *unique*
/// prefix, which is the right rule for a read-only lookup but the wrong one
/// for a write: a single mistyped character can still be unique, and the
/// action then lands on -- wakes, restarts, or outright kills -- a session
/// the operator never meant to touch. Four characters is 16 bits of the
/// eight-hex-character short id, enough that a typo lands on "no session
/// matches" rather than on a neighbour.
pub const MIN_TARGET_PREFIX: usize = 4;

/// The refusal for a too-short target prefix, or `None` when it is long
/// enough to act on. Pure, so the rule is testable without a registry on
/// disk. A prefix that *equals* a live session's whole short id always
/// passes, however short that short id happens to be. `verb` names the
/// action in the refusal text (`"nudge"`, `"kill"`).
pub fn prefix_too_short(verb: &str, prefix: &str, live_shorts: &[String]) -> Option<String> {
    if prefix.chars().count() >= MIN_TARGET_PREFIX {
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
        "prefix too short (a {verb} needs at least {MIN_TARGET_PREFIX} characters, \
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
    if let Some(refusal) = prefix_too_short("nudge", &args.prefix, &live_shorts(&state)) {
        return Err(format!("zirv ctx nudge: {refusal}").into());
    }
    // Only a live session is a valid nudge target: `resolve_prefix` already
    // filters to live records (a stale one was swept from disk by the time a
    // caller could act on it), so an unknown *or* dead session both surface
    // as the same `NotFound`, naming what is actually there instead.
    let record = resolve_prefix(&state, &args.prefix).map_err(|e| {
        format!(
            "zirv ctx nudge: {}",
            resolve_error_with_diagnostics(&e, &state, env)
        )
    })?;

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
    // never restarted and never receives message bodies. The supervisor may
    // type a one-line advisory into the agent at a verified-idle boundary,
    // but the guidance body itself always waits in the inbox. Saying so here
    // is the difference between "nothing happened, the nudge is broken" and
    // "the agent will be pointed at its inbox", which is the actual contract.
    if matches!(record.verb, Verb::Wrap | Verb::Chat) {
        writeln!(
            w,
            "zirv ctx nudge: {} is an interactive session; the guidance is delivered as \
             inbox mail plus a one-line advisory (typed in only at a verified-idle \
             boundary), never the message body itself",
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

// Issue #166: `zirv ctx kill <prefix>`. A worker that has exhausted its
// token budget, or is simply wedged, cannot notice a `nudge` -- that path
// depends on the target reading its own mail and waking itself. `kill` needs
// none of that: it is a plain OS-level SIGTERM/SIGKILL against the
// registered pid, which works whether or not the process on the other end
// can still act on anything.

/// How long `kill` waits after SIGTERM before escalating to SIGKILL --
/// `supervise::terminate`'s own grace window for an owned `Child`.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, clap::Args)]
pub struct KillArgs {
    /// Short id (or a unique prefix of one, at least four characters) of the
    /// session to terminate. On unix a target whose pid the OS has since
    /// recycled to an unrelated process is deregistered without being
    /// signalled; where the check cannot run (Windows, or no `ps` on PATH)
    /// the registered pid is signalled as-is.
    pub prefix: String,
}

/// How much later than its own registration the process holding a session's
/// pid may have started before this reads as a recycled pid. A genuine
/// session's process always predates the record that describes it, so any
/// positive slack is pure margin -- against a coarse `ps` reading, a clock
/// that stepped between the two, and second-boundary rounding.
///
/// Compare `START_TIME_TOLERANCE_SECS` (`record_is_alive`'s own
/// disambiguator, issue #152): both absorb the same kinds of measurement
/// slack, but at deliberately different magnitudes because they answer
/// different questions. This constant backs a ONE-SIDED "is the process
/// younger than its own record" heuristic (`pid_looks_recycled`), where a
/// genuine session's process predates its record by a wide, predictable
/// margin, so a few seconds (5s) of slack is already generous margin.
/// `START_TIME_TOLERANCE_SECS` instead backs a comparison of two
/// INDEPENDENT start-time readings of the same claimed process taken at
/// different times -- exactly what an NTP correction or manual clock change
/// between the two readings can disturb -- with no registration-order
/// assumption to lean on, hence its much wider 300s. Do not unify these.
const RECYCLED_PID_TOLERANCE_SECS: u64 = 5;

/// Pure: whether the process currently holding a session's pid started
/// AFTER that session was registered -- i.e. the original process died and
/// the OS handed its pid to something unrelated.
///
/// Security review round 2 (Finding 7): `resolve_prefix` reports such a
/// session as live (`is_alive` is a signal-0 probe with no way to tell one
/// process from another at the same pid -- see its own doc comment), so
/// `kill` would SIGTERM/SIGKILL a stranger's process. `age_secs` is the
/// target's own elapsed running time, `registered_at` the record's
/// `started_at`, both in seconds.
fn pid_looks_recycled(registered_at: u64, age_secs: u64, now: u64) -> bool {
    now.saturating_sub(age_secs) > registered_at.saturating_add(RECYCLED_PID_TOLERANCE_SECS)
}

/// Pure: POSIX `ps -o etime=` output (`[[dd-]hh:]mm:ss`) as seconds. `None`
/// for anything that does not parse, which every caller reads as "cannot
/// tell" rather than as any particular age.
///
/// Only called from the `#[cfg(unix)]` `process_age_secs` and from tests; the
/// non-unix `process_age_secs` never reaches it.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_etime(raw: &str) -> Option<u64> {
    let (days, clock) = match raw.trim().split_once('-') {
        Some((days, clock)) => (days.trim().parse::<u64>().ok()?, clock),
        None => (0, raw.trim()),
    };
    let mut fields = clock.split(':').rev();
    let seconds = fields.next()?.trim().parse::<u64>().ok()?;
    let minutes = fields.next()?.trim().parse::<u64>().ok()?;
    let hours = match fields.next() {
        Some(hours) => hours.trim().parse::<u64>().ok()?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// How long the process holding `pid` has been running, in seconds.
///
/// POSIX `ps -o etime=`, deliberately rather than a platform-specific
/// interface (`/proc/<pid>/stat` plus `btime`, `sysctl KERN_PROC_PID`): this
/// answers one question, on one code path, and a platform-abstraction layer
/// for it would be more machinery than the question is worth. `None` --
/// `ps` missing, refused (a sandbox), or output this cannot parse -- means
/// "cannot tell", and every caller then behaves exactly as it did before this
/// check existed rather than refusing to act.
#[cfg(unix)]
fn process_age_secs(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_etime(&String::from_utf8_lossy(&output.stdout))
}

/// No portable start-time probe on this platform, so a recycled pid is
/// indistinguishable from the session's own -- see [`run_kill_with`]'s doc
/// comment for the residual this leaves.
#[cfg(not(unix))]
fn process_age_secs(_pid: u32) -> Option<u64> {
    None
}

/// Terminates a registered session's process outright -- SIGTERM, escalating
/// to SIGKILL after `KILL_GRACE` -- and deregisters it. Unlike `nudge`, this
/// never depends on the target being able to notice or act on anything: it
/// is a plain process signal, not mail. Freeing whatever machine-wide
/// heavy-operation permit the session's pid held (`permit::live_records`) is
/// a direct consequence of the pid actually dying, not a separate step here.
///
/// Only a live session is a valid kill target: `resolve_prefix` already
/// filters to live records (a stale one was already swept from disk, and
/// deregistered, by the time a caller could act on it -- see `list`'s own
/// doc comment), so an unknown *or* already-dead session both surface as the
/// same `NotFound`, the identical contract `run_nudge_with` already has for
/// this exact case -- there is nothing left to kill, and nothing left to
/// deregister either, since the sweep already did that.
///
/// Security review round 2 (Finding 7): "live" there is a signal-0 probe of
/// a pid, and a pid the OS recycled before any sweep noticed answers it just
/// as a live session would -- so this used to SIGTERM/SIGKILL an unrelated
/// process that merely inherited the number. The target's own start time is
/// checked against the record's `started_at` first ([`process_age_secs`],
/// [`pid_looks_recycled`]): a process younger than the session that claims it
/// is a recycled pid, and is deregistered without ever being signalled.
///
/// Residual, deliberately not closed here: the check needs a start time, and
/// on Windows (and anywhere `ps` is missing or refused) there is none to be
/// had without a platform-abstraction layer this one code path does not
/// justify. There the registered pid is signalled as before, and a recycled
/// pid is still reachable -- narrow, since it also requires the original
/// process to have died and the pid counter to have wrapped before any
/// `sessions::list` swept the record.
pub fn run_kill_with<W: Write>(args: &KillArgs, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    if let Some(refusal) = prefix_too_short("kill", &args.prefix, &live_shorts(&state)) {
        return Err(format!("zirv ctx kill: {refusal}").into());
    }
    let record = resolve_prefix(&state, &args.prefix).map_err(|e| {
        format!(
            "zirv ctx kill: {}",
            resolve_error_with_diagnostics(&e, &state, env)
        )
    })?;

    // Finding 7: the pid may no longer be this session's. Deregister it --
    // the session it described is gone either way -- but signal nothing.
    if process_age_secs(record.pid)
        .is_some_and(|age| pid_looks_recycled(record.started_at, age, super::state::now_secs()))
    {
        let _ = std::fs::remove_file(record_path(&state, &record.short));
        writeln!(
            w,
            "zirv ctx kill: {} ({}, {}) is gone -- pid {} now belongs to a process that started \
             after the session registered, so nothing was signalled; deregistered it from the \
             session registry",
            record.short, record.agent, record.verb, record.pid
        )?;
        return Ok(0);
    }

    let confirmed_dead = super::supervise::terminate_pid(record.pid, KILL_GRACE);
    // Best-effort, matching every other piece of state-dir housekeeping in
    // this module: whether or not the process could be confirmed dead, there
    // is no reason left to keep the record around -- `kill` is the operator
    // saying this session is done, not asking to retry.
    let _ = std::fs::remove_file(record_path(&state, &record.short));

    if confirmed_dead {
        writeln!(
            w,
            "zirv ctx kill: terminated {} ({}, {}, pid {}) and deregistered it",
            record.short, record.agent, record.verb, record.pid
        )?;
    } else {
        writeln!(
            w,
            "zirv ctx kill: sent SIGTERM/SIGKILL to {} ({}, {}, pid {}) but could not confirm \
             it exited; deregistered it from the session registry regardless",
            record.short, record.agent, record.verb, record.pid
        )?;
    }
    Ok(0)
}

pub fn run_kill<W: Write>(args: &KillArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    run_kill_with(args, w, &env)
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

    use super::super::testenv::dead_pid;

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
    fn a_record_without_start_time_deserializes_as_unset() {
        // Same back-compat pattern as `owner_pid` above, for issue #152's
        // new field: a record written by a build that predates `start_time`
        // has no such key in its JSON at all, which `#[serde(default)]`
        // exists to survive.
        let json = format!(
            r#"{{
            "session": "22222222-2222-4333-8444-555555555555",
            "short": "22222222",
            "agent": "claude",
            "repo": "/repo",
            "repo_slug": "-repo",
            "verb": "exec",
            "pid": {},
            "started_at": 0,
            "reachable": true
        }}"#,
            std::process::id()
        );
        let record: Record = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record.start_time, None);
        // The degrade rule end to end: a record with nothing to compare
        // against must never read as falsely dead. `pid` here is this very
        // test process's own -- always alive -- so this exercises the whole
        // `record_is_alive` path, not just the comparator in isolation.
        assert!(
            record_is_alive(&record),
            "no start_time to compare -- must degrade to alive, never false-dead"
        );
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

    /// P5: `wrap` files its record before it has a child, so `Record::new`
    /// stamps zirv's own pid; `adopt_child_pid` re-points it at the agent the
    /// supervisor actually spawned, exactly as `dash::pane::Pane::spawn`
    /// already does at its own registration. The record's *address* (`short`,
    /// and therefore its path) must not move with it -- that is what mail and
    /// `zirv ctx nudge` resolve against.
    #[test]
    fn adopt_child_pid_repoints_the_record_at_the_agent_without_moving_its_address() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let short = record.short.clone();
        let path = record_path(&state, &short);

        let mut guard = SessionGuard::register(&state, record);
        assert_eq!(
            guard.record().pid,
            std::process::id(),
            "before the spawn there is no child pid to record"
        );

        guard.adopt_child_pid(4242);
        assert_eq!(guard.record().pid, 4242);
        assert_eq!(guard.short(), short, "the delivery address does not move");
        assert!(path.is_file(), "and neither does the record's own path");

        let on_disk: Record =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(on_disk.pid, 4242, "the override reached disk");
        assert_eq!(
            on_disk.owner_pid,
            Some(std::process::id()),
            "owner_pid still answers 'which process filed this', untouched"
        );
    }

    /// Review round 2 finding 1 (issue #152): `adopt_child_pid` must re-stamp
    /// `start_time` for the NEW pid in the same breath it repoints `pid`
    /// itself, or the very next `EPERM` liveness probe against a perfectly
    /// live pane compares the CHILD's real start time against whatever the
    /// record's `start_time` was left at (the supervisor's own, from
    /// `Record::new`) and reads a guaranteed, false mismatch -- deleting a
    /// live session's record. Pid 1 stands in for "a real process this
    /// caller cannot signal", the same real-`EPERM` source the other pid-1
    /// tests in this module use, since forcing an `EPERM` against a pid this
    /// test owns outright is not possible.
    #[cfg(unix)]
    #[test]
    fn adopt_child_pid_re_stamps_start_time_so_the_new_pid_reads_live() {
        // SAFETY: `geteuid` takes no arguments and only reads process state.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, kill(1, 0) succeeds outright");
            return;
        }
        // SAFETY: signal 0 sends nothing; it only probes existence and
        // permission -- see finding 5's own note on why `geteuid` alone is
        // not a sufficient guard (a rootless/namespaced sandbox can still
        // let this uid signal pid 1 outright).
        if unsafe { libc::kill(1, 0) } == 0 {
            eprintln!("skipping: kill(1, 0) succeeds outright in this sandbox");
            return;
        }
        if process_start_secs(1).is_none() {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to check");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let mut record = record_for("77777777-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        // Issue #218 fix round, defect 3: the 2s tolerance below (needed to
        // absorb `process_start_secs`'s own `now - elapsed` straddle) also
        // means a record that started life within 2s of pid 1's own start
        // time -- entirely possible in a fresh container, where pid 1 and
        // this test process can both be seconds old -- would pass the
        // tolerance check even if `adopt_child_pid` never re-stamped
        // `start_time` at all. Pin the record to an ancient sentinel first,
        // so the assertion below can only pass if `adopt_child_pid` actually
        // moved `start_time`, not merely if it happened to already be close.
        record.start_time = Some(1);
        let mut guard = SessionGuard::register(&state, record);

        guard.adopt_child_pid(1);

        // `process_start_secs` derives its answer as `now - elapsed`, so two
        // calls a few instructions apart can straddle a second boundary and
        // land one second off each other -- issue #218's flake. Compare
        // within a small tolerance instead of exact equality. That tolerance
        // still catches the real regression: pid 1's start time is the
        // machine's boot time, while a pinned, unmoved `start_time` would
        // still read this test process's own (recent) start -- hours or more
        // away from pid 1's, never within 2 seconds of it.
        let actual = guard.record().start_time;
        let expected = process_start_secs(1);
        assert_ne!(
            actual,
            Some(1),
            "adopt_child_pid must re-stamp start_time, not leave the ancient sentinel in place \
             (expected live={expected:?})"
        );
        match (actual, expected) {
            (Some(actual), Some(expected)) => {
                let diff = actual.abs_diff(expected);
                assert!(
                    diff <= 2,
                    "start_time must move with pid, not stay pinned to the supervisor's own \
                     (adopted={actual}, live={expected}, diff={diff}s)"
                );
            }
            _ => panic!(
                "start_time must move with pid, not stay pinned to the supervisor's own \
                 (adopted={actual:?}, live={expected:?})"
            ),
        }
        assert!(
            record_is_alive(guard.record()),
            "the repointed record must read live, not falsely dead from a stale start_time"
        );
    }

    /// P5, the restart window: the pid a `wrap` record names must never be a
    /// *dead* one, not even briefly.
    ///
    /// `list` sweeps any record whose pid is gone, and it runs on other
    /// processes' schedules -- `zirv ctx status`, `nudge`, `send
    /// --to-session`, a dashboard's ~1s registry refresh. So during a rot
    /// restart, where the old child is killed long before a replacement
    /// exists, `pump`'s restart arm parks the record on zirv's own pid first
    /// and adopts the fresh child's only once there is one. This pins that
    /// three-step sequence, which is all `pump` does to the guard: there is
    /// no seam to drive the pump's own restart arm from a unit test, and the
    /// two calls it makes are exactly these.
    #[test]
    fn a_restart_never_leaves_the_record_naming_a_dead_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("44444444-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let path = record_path(&state, &record.short);
        let mut guard = SessionGuard::register(&state, record);

        let on_disk = || -> Record {
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse")
        };

        // 1. First spawn: the record names the agent child.
        guard.adopt_child_pid(4242);
        assert_eq!(on_disk().pid, 4242);

        // 2. Restart begins -- the child is about to be killed, so the record
        //    is parked on this process, which is alive by construction.
        guard.adopt_child_pid(std::process::id());
        assert_eq!(on_disk().pid, std::process::id());
        assert!(
            is_alive(on_disk().pid),
            "a concurrent `list` mid-restart must not sweep this record"
        );

        // 3. Respawn: back onto the fresh child.
        guard.adopt_child_pid(5353);
        assert_eq!(on_disk().pid, 5353);
        assert_eq!(
            on_disk().short,
            guard.short(),
            "and the delivery address never moved through any of it"
        );
    }

    /// Idempotent and inert after release, like every other guard write here:
    /// a relaunch calls it once per fresh child, and a released guard must not
    /// resurrect the record file it just removed.
    #[test]
    fn adopt_child_pid_is_a_no_op_on_a_released_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("33333333-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let path = record_path(&state, &record.short);

        let mut guard = SessionGuard::register(&state, record);
        guard.release();
        guard.adopt_child_pid(4242);
        assert!(!path.exists(), "a released record stays released");
    }

    /// P4's production probe. A record naming *this* test process is live by
    /// construction; an absurd pid is not; and no record at all answers
    /// "nothing to collide with", so the restore may proceed.
    #[test]
    fn short_is_live_answers_from_the_record_the_short_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        let mut alive = record_for("aaaaaaaa-2222-4333-8444-555555555555", &repo, Verb::Dash);
        alive.pid = std::process::id();
        let alive_short = alive.short.clone();
        let _alive_path = write_record(&state, &alive);

        let mut dead = record_for("bbbbbbbb-2222-4333-8444-555555555555", &repo, Verb::Dash);
        // Far above any pid Windows or Linux hands out, so it cannot collide
        // with a real process on the machine running these tests.
        dead.pid = 4_000_000_003;
        let dead_short = dead.short.clone();
        let _dead_path = write_record(&state, &dead);

        assert!(short_is_live(&state, &alive_short));
        assert!(!short_is_live(&state, &dead_short));
        assert!(
            !short_is_live(&state, "nosuchid"),
            "no record means nothing to collide with"
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

    /// The Windows named-pipe namespace is machine-global (unlike a unix
    /// domain socket, scoped to its own tempdir), so a hardcoded or
    /// small-space-derived short id in a test that touches
    /// `signal::probe`/`SignalServer::bind` risks colliding with an
    /// unrelated live pipe -- including one this very test binary leaked
    /// earlier in the same run (`Drop for SignalServer` on Windows only
    /// removes the marker file; the acceptor thread and its pipe instance
    /// keep answering for the rest of the process's life). A fresh random
    /// UUID per call, the same generator every real session id already uses
    /// (`event.rs`), keeps this from ever landing on a name anything else in
    /// the process could already own.
    fn unique_endpoint_session() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Issue #99 (2026-08-23): a `*.sock` marker with no matching session
    /// record and nothing listening behind it is a leftover from a killed or
    /// crashed supervisor (`Drop for SignalServer` never ran). `list`'s own
    /// sweep must remove it rather than let it accumulate forever.
    #[test]
    fn a_dead_endpoint_marker_with_no_record_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sockets()).expect("mkdir");
        let path = state.socket_for(&unique_endpoint_session());
        std::fs::write(&path, "leftover, nothing is listening").expect("write leftover marker");
        assert!(path.exists(), "sanity: the leftover marker exists");

        assert!(list(&state).is_empty(), "no registry record exists");
        assert!(
            !path.exists(),
            "a dead endpoint with no record must be swept: {}",
            path.display()
        );
    }

    /// A marker whose endpoint still answers belongs to a supervisor that is
    /// alive but simply has no registry record (an older build, or a
    /// registry write that failed) -- it must stay on disk and stay listed,
    /// not be swept just because nothing filed a `Record` for it.
    #[cfg(unix)]
    #[test]
    fn a_live_endpoint_marker_with_no_record_is_kept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let path = state.socket_for(&unique_endpoint_session());
        let _server = crate::commands::ctx::signal::SignalServer::bind(&path).expect("bind");
        assert!(path.exists(), "sanity: the live endpoint exists");

        assert!(list(&state).is_empty(), "no registry record exists");
        assert!(
            path.exists(),
            "a live endpoint with no record must be kept: {}",
            path.display()
        );
    }

    /// A live *registered* session's own endpoint marker must never be swept
    /// as a side effect of sweeping everyone else's orphans.
    #[cfg(unix)]
    #[test]
    fn a_live_registered_sessions_endpoint_marker_is_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let session = unique_endpoint_session();
        let record = record_for(&session, &repo, Verb::Wrap);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let path = state.socket_for(&session);
        let _server = crate::commands::ctx::signal::SignalServer::bind(&path).expect("bind");
        assert!(path.exists(), "sanity: the endpoint exists");

        let found = list(&state);
        assert!(
            found
                .iter()
                .any(|(r, liveness)| r.short == short && *liveness == Liveness::Live),
            "the registered session is still listed as live: {found:?}"
        );
        assert!(
            path.exists(),
            "a live registered session's own marker must be untouched: {}",
            path.display()
        );
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
        assert_eq!(prefix_too_short("nudge", "ab", &shorts), None);
        assert!(prefix_too_short("nudge", "a", &shorts).is_some());
        assert_eq!(prefix_too_short("nudge", "abcd", &[]), None);
        let refusal = prefix_too_short("nudge", "x", &[]).expect("refused");
        assert!(refusal.contains("none registered"), "got {refusal}");
    }

    /// Issue #146: the same diagnostic extension as `mail::run_send_with`'s
    /// own regression test -- a prefix long enough to pass the minimum-length
    /// guard but matching no live session must name the state dir it was
    /// actually checked against, not just say "no sessions are registered".
    #[test]
    fn an_unresolvable_nudge_prefix_names_the_state_dir_it_was_checked_against() {
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
            prefix: "dead0000".to_string(),
            message: Some("wake up".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("no session is registered under this empty state dir");

        let msg = err.to_string();
        assert!(
            msg.contains("no sessions are registered"),
            "the existing message text stays the prefix: {msg}"
        );
        let state = state_in(&state_dir);
        assert!(
            msg.contains(&state.sessions().display().to_string()),
            "must name the registry path actually checked: {msg}"
        );
        assert!(
            msg.contains(super::super::state::STATE_ENV),
            "must say whether ZIRV_CTX_STATE_DIR was set: {msg}"
        );
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

    /// Fix round 1 (issue #144): `dashboard_owner_is_live`'s three cases,
    /// exposed directly through `dashboard_owner_liveness` rather than only
    /// through the collapsed bool -- `agent::try_join_dashboard` needs the
    /// `Dead(pid)`/`Missing` distinction to report a refusal it used to give
    /// in total silence.
    #[test]
    fn dashboard_owner_liveness_distinguishes_missing_dead_and_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let make = |name: &str| {
            let requests = tmp.path().join("dash").join(name).join("requests");
            std::fs::create_dir_all(&requests).expect("mkdir");
            requests
        };

        let missing = make("aaaa1111-0001");
        assert_eq!(dashboard_owner_liveness(&missing), OwnerLiveness::Missing);

        let dead = make("bbbb2222-0002");
        let dead_pid_value = dead_pid();
        std::fs::write(
            dead.parent().expect("parent").join("owner.pid"),
            dead_pid_value.to_string(),
        )
        .expect("write owner.pid");
        assert_eq!(
            dashboard_owner_liveness(&dead),
            OwnerLiveness::Dead(dead_pid_value)
        );

        let live = make("cccc3333-0003");
        std::fs::write(
            live.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        assert_eq!(dashboard_owner_liveness(&live), OwnerLiveness::Live);
    }

    /// Issue #146: `is_alive` must read `EPERM` (the process exists, this
    /// caller just cannot signal it) as alive, not dead. Pid 1 is owned by
    /// root, exists on every unix box, and -- for a non-root caller, which is
    /// what CI and a sandboxed `zirv ctx send` both run as -- `kill(1, 0)`
    /// returns exactly `EPERM`. Skipped only for the (rare, unsandboxed) case
    /// of running as root, where `kill(1, 0)` succeeds outright and the
    /// assertion holds anyway for a different reason -- so root does not
    /// need its own branch, only its own explanatory skip.
    #[cfg(unix)]
    #[test]
    fn eperm_against_a_real_process_reads_as_alive_not_dead() {
        // SAFETY: `geteuid` takes no arguments and only reads process state.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, kill(1, 0) succeeds outright");
            return;
        }
        assert!(
            is_alive(1),
            "pid 1 exists and is owned by root; a non-root caller's kill(1, 0) is EPERM, which \
             must read as alive"
        );
    }

    /// The other half: a pid that has genuinely exited (spawned, waited on)
    /// must still read as dead. `EPERM` must not have swallowed `ESRCH` too.
    #[test]
    fn a_waited_on_pid_reads_as_dead() {
        assert!(!is_alive(dead_pid()));
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

    fn sh(script: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    /// Issue #166: `zirv ctx kill` must work against a session that has no
    /// way to notice anything -- unlike `nudge`, this never depends on the
    /// target reading mail or waking itself. Ends the real process by pid and
    /// deregisters the record, both without a `SessionGuard` of its own: a
    /// separate `kill` invocation only ever has the registry record, never
    /// the original guard.
    #[test]
    fn kill_terminates_a_live_sessions_process_and_deregisters_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let mut child = sh("sleep 30")
            .spawn()
            .expect("spawn a real process to kill");
        let pid = child.id();
        // `run_kill_with`'s own liveness check is `is_alive` (`kill(pid,
        // 0)`), not `Child::try_wait` -- this test is the process's real
        // parent, so it must reap concurrently or the killed process would
        // sit as a zombie (still visible to `kill(pid, 0)`) until this test's
        // own `wait()` ran. See `supervise::terminate_pid`'s own tests for
        // the same reasoning.
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        let mut record = record_for("eeeeeeee-2222-4333-8444-555555555555", &repo, Verb::Exec);
        record.pid = pid;
        let short = record.short.clone();
        let path = record_path(&state, &short);
        write_record(&state, &record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = KillArgs {
            prefix: short.clone(),
        };
        let mut out = Vec::new();
        let code =
            run_kill_with(&args, &mut out, &|k| env.get(k).cloned()).expect("kill must succeed");
        assert_eq!(code, 0);
        reaper.join().expect("reaper thread");

        assert!(!path.exists(), "the record is deregistered");
        assert!(
            list(&state).is_empty(),
            "nothing left to resolve a future nudge or kill against"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&short), "names what it killed: {text}");
    }

    /// Security review round 2 (Finding 7): the POSIX `ps -o etime=` shapes,
    /// and nothing else read as an age.
    #[test]
    fn parse_etime_reads_the_posix_elapsed_time_format() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("       01:30"), Some(90));
        assert_eq!(parse_etime("02:03:04"), Some(7_384));
        assert_eq!(parse_etime("3-04:05:06\n"), Some(273_906));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("7"), None, "a bare number is not an etime");
        assert_eq!(parse_etime("a:bc"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
    }

    /// The pure half of the recycled-pid guard: only a process that started
    /// AFTER the session that claims its pid is a stranger. A process older
    /// than its own record is the ordinary case (a supervisor registers
    /// itself moments after it starts, and a dashboard registers a pane's
    /// record long after the dashboard process itself began), and must never
    /// be mistaken for a recycled one.
    #[test]
    fn pid_looks_recycled_only_flags_a_process_younger_than_its_own_record() {
        let now = 1_700_000_000;
        assert!(
            !pid_looks_recycled(now - 60, 3_600, now),
            "a process much older than its record is the ordinary case"
        );
        assert!(
            !pid_looks_recycled(now, 0, now),
            "and one that started in the same instant is not a stranger either"
        );
        assert!(
            !pid_looks_recycled(now - RECYCLED_PID_TOLERANCE_SECS, 0, now),
            "the tolerance absorbs a coarse reading and a stepped clock"
        );
        assert!(
            pid_looks_recycled(now - 3_600, 5, now),
            "a process seconds old under an hour-old record is a recycled pid"
        );
    }

    /// Issue #152's own pure comparator: only a start-time pair that both
    /// exist AND disagree by more than the tolerance means "a different
    /// process now holds this pid". No process spawning -- everything here
    /// is plain arithmetic.
    #[test]
    fn start_time_disambiguates_dead_only_flags_a_mismatch_beyond_tolerance() {
        assert!(
            !start_time_disambiguates_dead(Some(1_000), Some(1_000)),
            "identical start times are obviously the same process"
        );
        assert!(
            !start_time_disambiguates_dead(Some(1_000), Some(1_000 + START_TIME_TOLERANCE_SECS)),
            "the tolerance absorbs a coarse reading and a stepped clock"
        );
        assert!(
            start_time_disambiguates_dead(Some(1_000), Some(1_000 + START_TIME_TOLERANCE_SECS + 1)),
            "beyond the tolerance is a different process"
        );
        assert!(
            start_time_disambiguates_dead(Some(10_000), Some(1_000)),
            "the mismatch is symmetric -- either side reading later or earlier than the other \
             still means two different processes"
        );
        assert!(
            !start_time_disambiguates_dead(None, Some(1_000)),
            "no recorded start time -- cannot tell, degrade to alive"
        );
        assert!(
            !start_time_disambiguates_dead(Some(1_000), None),
            "no freshly-read start time -- cannot tell, degrade to alive"
        );
        assert!(
            !start_time_disambiguates_dead(None, None),
            "neither side known -- cannot tell, degrade to alive"
        );
    }

    /// `process_start_secs` smoke test: this test process's own start time
    /// is readable, and reading it twice gives (very close to) the same
    /// answer -- it is not approximated freshly relative to "now" in a way
    /// that would drift materially between two calls a moment apart.
    ///
    /// Review round 2 finding 4: exact equality was too strict. Each read is
    /// an independent `now_secs() - ps_etime_derived_age` derivation, and
    /// `ps`'s own `etime` field is whole-second/whole-minute granularity
    /// (rounding differently depending on exactly when within that window
    /// each `ps` invocation lands), so two reads a moment apart can
    /// legitimately land a second or two either side of each other without
    /// anything about the process's real start time having changed. `2`
    /// comfortably covers that rounding while still catching a reader that
    /// is actually broken (drifting with "now" rather than anchored to the
    /// process).
    #[cfg(unix)]
    #[test]
    fn process_start_secs_of_this_process_is_stable_across_two_reads() {
        let Some(first) = process_start_secs(std::process::id()) else {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to read");
            return;
        };
        let Some(second) = process_start_secs(std::process::id()) else {
            eprintln!("skipping: `ps` became unusable between the two reads");
            return;
        };
        assert!(
            first.abs_diff(second) <= 2,
            "the same process read twice must report nearly the same start time: {first} vs \
             {second}"
        );
    }

    /// The one branch a bare `is_alive` cannot get right for a `Record`:
    /// `EPERM` alone cannot tell the session's own process apart from an
    /// unrelated one the OS later recycled its pid to (issue #152). Forcing
    /// a real `EPERM` against a process this test spawns itself is not
    /// possible -- a caller always has permission to signal its own child,
    /// so `kill(pid, 0)` on one always succeeds (see the next test for that
    /// branch instead). Pid 1 is the same real-`EPERM` source
    /// `eperm_against_a_real_process_reads_as_alive_not_dead` above already
    /// relies on: it exists, is owned by root, and (for a non-root caller)
    /// always answers `EPERM`.
    #[cfg(unix)]
    #[test]
    fn record_is_alive_disambiguates_an_eperm_pid_by_start_time() {
        // SAFETY: `geteuid` takes no arguments and only reads process state.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, kill(1, 0) succeeds outright");
            return;
        }
        // Review round 2 finding 5: `geteuid() == 0` alone is not a reliable
        // enough guard. Under `docker run --user <uid>`, or any other setup
        // where this test's own uid happens to already own pid 1 (a
        // namespaced/rootless container's pid 1 is not always root's), a
        // non-root euid can still get `kill(1, 0) == 0` -- `CanSignal`, not
        // `EPERM` -- which would make this test's `!record_is_alive`
        // assertion below deterministically false regardless of start_time.
        // Ask the same question `probe_signal` itself would, directly.
        // SAFETY: signal 0 sends nothing; it only probes existence and
        // permission.
        if unsafe { libc::kill(1, 0) } == 0 {
            eprintln!("skipping: kill(1, 0) succeeds outright in this sandbox");
            return;
        }
        let Some(pid1_start) = process_start_secs(1) else {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to check");
            return;
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");

        let mut mismatched = record_for("33333333-2222-4333-8444-555555555555", &repo, Verb::Exec);
        mismatched.pid = 1;
        mismatched.start_time = Some(pid1_start.saturating_sub(3 * 3_600));
        assert!(
            !record_is_alive(&mismatched),
            "pid 1's own start time is hours off from the record's -- a different process now \
             holds it"
        );

        let mut matching = mismatched.clone();
        matching.start_time = Some(pid1_start);
        assert!(
            record_is_alive(&matching),
            "start times agree -- still the same process"
        );

        let mut unknown = mismatched.clone();
        unknown.start_time = None;
        assert!(
            record_is_alive(&unknown),
            "no recorded start time to compare -- degrade to EPERM's old alive answer"
        );
    }

    /// `record_is_alive`'s `kill(pid, 0)` success branch is unconditional by
    /// design (issue #152): a caller that can actually signal the process
    /// needs no second opinion from a start time, however far off a
    /// stale/fabricated one is. Proven through `list`'s own sweep -- the one
    /// production call site this matters for -- against a real, owned, live
    /// child process, which is exactly why the `EPERM` branch above has to
    /// be tested against pid 1 instead: this kind of process can never
    /// produce a real `EPERM` to exercise it.
    #[cfg(unix)]
    #[test]
    fn a_real_live_owned_pid_stays_live_even_with_a_wildly_wrong_start_time() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let mut child = sh("sleep 30").spawn().expect("spawn a stand-in process");
        let pid = child.id();

        let mut record = record_for("55555555-2222-4333-8444-555555555555", &repo, Verb::Exec);
        record.pid = pid;
        record.start_time = Some(super::super::state::now_secs().saturating_sub(3 * 3_600));
        let short = record.short.clone();
        let path = record_path(&state, &short);
        write_record(&state, &record);

        assert!(
            record_is_alive(&record),
            "kill(pid, 0) succeeds for our own child regardless of start_time"
        );
        let found = list(&state);
        let (_, liveness) = found
            .iter()
            .find(|(r, _)| r.short == short)
            .expect("the record is still listed");
        assert_eq!(*liveness, Liveness::Live);
        assert!(path.exists(), "list must not have swept a live record");

        let _ = child.kill();
        let _ = child.wait();
    }

    /// The whole point of the guard, end to end on a real process: `kill`
    /// must not SIGTERM a stranger that merely inherited a dead session's
    /// pid. Simulated the only way a test can without waiting for the OS to
    /// wrap its pid counter -- a genuinely fresh process, under a record
    /// that claims to predate it by an hour.
    #[cfg(unix)]
    #[test]
    fn kill_deregisters_a_recycled_pid_without_signalling_it() {
        if process_age_secs(std::process::id()).is_none() {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to check");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let mut child = sh("sleep 30").spawn().expect("spawn a stand-in process");
        let pid = child.id();

        let mut record = record_for("cccccccc-2222-4333-8444-555555555555", &repo, Verb::Exec);
        record.pid = pid;
        record.started_at = super::super::state::now_secs() - 3_600;
        let short = record.short.clone();
        let path = record_path(&state, &short);
        write_record(&state, &record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = KillArgs {
            prefix: short.clone(),
        };
        let mut out = Vec::new();
        let code = run_kill_with(&args, &mut out, &|k| env.get(k).cloned()).expect("kill runs");

        assert_eq!(code, 0);
        assert!(!path.exists(), "the dead session is still deregistered");
        assert!(
            is_alive(pid),
            "but the unrelated process holding its pid must be left alone"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("nothing was signalled"),
            "and the operator is told why: {text}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// `resolve_prefix`'s own contract, reused verbatim by `kill`: an unknown
    /// prefix and a prefix whose only match already died (and was therefore
    /// already swept and deregistered by `list`) surface identically -- there
    /// is nothing left to kill or deregister either way.
    #[test]
    fn killing_an_unknown_or_dead_session_is_an_error_that_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);

        let args = KillArgs {
            prefix: "zzzz".to_string(),
        };
        let mut out = Vec::new();
        let err = run_kill_with(&args, &mut out, &|k| env.get(k).cloned())
            .expect_err("no session is registered at all");
        assert!(err.to_string().contains("no session"), "got {err}");

        let state = state_in(&state_dir);
        let mut dead = record_for("dddddddd-2222-4333-8444-555555555555", &repo, Verb::Loop);
        dead.pid = dead_pid();
        write_record(&state, &dead);

        let args = KillArgs {
            prefix: "dddd".to_string(),
        };
        let mut out = Vec::new();
        let err = run_kill_with(&args, &mut out, &|k| env.get(k).cloned())
            .expect_err("the only match is already dead");
        assert!(err.to_string().contains("no session"), "got {err}");
    }

    /// Same guard `nudge` already has, on the same shared rule: a kill is at
    /// least as destructive as a nudge (it ends the process outright), so a
    /// mistyped one- or two-character prefix must not resolve just because it
    /// happens to be unique on this machine.
    #[test]
    fn a_kill_prefix_shorter_than_four_characters_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Chat);
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = KillArgs {
            prefix: "abc".to_string(),
        };
        let mut out = Vec::new();
        let err = run_kill_with(&args, &mut out, &|k| env.get(k).cloned())
            .expect_err("three characters is not enough to kill on");
        assert!(err.to_string().contains("prefix too short"), "got {err}");
    }
}
