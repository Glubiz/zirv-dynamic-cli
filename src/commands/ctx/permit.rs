//! The machine-wide heavy-OPERATION budget (issue #155, replacing issue
//! #133's heavy-*worker* count).
//!
//! #133's incident was two concurrent cold `cargo build` + full-nextest
//! workloads bugchecking the host four times in twelve minutes. The budget
//! that answered it counted live `Verb::Exec | Verb::Dash` session records,
//! which is workload-blind in both directions: an idle worker sitting at a
//! prompt consumed the whole default budget of 1, while a `Verb::Chat`
//! orchestrator running a full nextest sweep consumed none. With the default
//! at 1, one parked delegation blocked every subsequent one -- so the
//! orchestrator did the work itself on the expensive seat, which is the spend
//! pattern issue #155 exists to remove.
//!
//! A permit is held for the DURATION OF AN ACTUAL HEAVY COMMAND and released
//! when the child exits, so an idle coordinator holds nothing and a busy one
//! holds exactly one.
//!
//! [`is_heavy`] is pure -- no fs, clock or env -- the same discipline
//! `safety::evaluate` holds, so classification is testable without touching
//! the machine. All I/O lives in [`acquire`]/[`live_count`].
//!
//! Cross-process atomicity: [`acquire`] claims one of `limit` numbered slot
//! files (`slot-0.json` .. `slot-<limit - 1>.json`) with an exclusive
//! `create_new` open -- the same collision-as-the-open-result idiom
//! `dash::spawnreq`'s own request files and `memory`'s per-key entries use.
//! `create_new` is a single atomic filesystem operation on both Windows and
//! Unix, so when two processes race for the same slot index exactly one
//! `open` succeeds; the loser moves on to the next index rather than both
//! believing they hold the budget. This replaces an earlier count-then-write
//! window that let two concurrent callers both observe a free slot and both
//! acquire, exceeding `max_heavy_operations`.
//!
//! Issues #267/#338 add a SECOND, independent writer-permit registry, one
//! record per `WorkerMode::Writing` delegated worker for its WHOLE LIFETIME
//! (not just while it runs a heavy command), plus a per-tree exclusivity rule
//! ([`WriterRefusal::TreeBusy`]) so two writers can never hold the same
//! checkout at once. `supervise.max_writers` optionally caps those records
//! machine-wide; zero leaves only per-tree exclusivity. It reuses this same
//! `create_new` contention idiom and dead-owner sweep under its own
//! `<state>/permits/writers/` directory ([`writer_permits_dir`]) -- the heavy
//! pool above is untouched by it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::sessions::is_alive;
use super::state::{self, StateDir};

/// Re-review (2026-08-27) finding 2a: how long an unparseable slot file is
/// left alone before the dead-owner sweep treats it as crash-orphaned and
/// removes it. `acquire`'s own claim is `create_new` then `write_all`, two
/// separate syscalls in a `panic = "abort"` binary, so a kill between them
/// leaves an empty or partial slot file behind -- and until now `live_
/// records` skipped such a file forever without ever removing it,
/// permanently losing that slot index. A few seconds tolerates a write
/// genuinely still in progress (every write in this module is a few hundred
/// bytes, effectively instantaneous) without reintroducing finding 2b's
/// sweep-vs-claim race on a file that is not actually orphaned.
const UNPARSEABLE_SLOT_GRACE_SECS: u64 = 5;

/// Re-review (2026-08-27) finding 2b: bounded retries for `acquire`'s own
/// post-claim verification (see `claim_is_verified`'s doc comment). Small --
/// this only ever loops more than once when a claim is genuinely lost to a
/// concurrent sweep or clobber, not under ordinary contention (which
/// `claim_any_slot`'s own `create_new` loop already resolves in one pass).
const CLAIM_VERIFY_ATTEMPTS: u32 = 3;

/// Substituted-command patterns [`is_heavy`] always checks, regardless of
/// `SuperviseConfig::heavy_command_patterns` -- see that field's own doc
/// comment for why an operator may only ADD to this set, never remove from
/// it. `cargo fmt`/`cargo --version` are deliberately absent: neither is
/// long or resource-hungry, and a permit on either would reintroduce
/// exactly the "idle thing holds the budget" failure this module exists to
/// fix.
pub const BUILTIN_HEAVY_PATTERNS: &[&str] = &[
    "cargo build*",
    "cargo test*",
    "cargo nextest*",
    "cargo clippy*",
    "cargo package*",
    "cargo publish*",
];

/// Whether `command` should hold a permit for its whole run.
///
/// Reuses `safety::normalize_segments`/`safety::glob_match` -- one matcher
/// in this codebase, not two -- so a heavy command hidden behind `sh -c` or
/// a `&&` chain is still classified: `normalize_segments` extracts the raw
/// command plus every quote-aware segment, unwrapped inline shell and
/// command substitution it can find, and each candidate is checked against
/// [`BUILTIN_HEAVY_PATTERNS`] plus `extra_patterns` with `glob_match`.
pub fn is_heavy(command: &str, extra_patterns: &[String]) -> bool {
    super::safety::normalize_segments(command)
        .iter()
        .any(|candidate| {
            BUILTIN_HEAVY_PATTERNS
                .iter()
                .copied()
                .chain(extra_patterns.iter().map(String::as_str))
                .any(|pattern| super::safety::glob_match(pattern, candidate))
        })
}

/// Issue #267: which of the two independent permit pools a [`PermitRecord`]
/// belongs to -- `Heavy` for a classified command (`is_heavy`, unchanged
/// from #133/#155), `Writer` for a `WorkerMode::Writing` delegated worker
/// holding exclusive write access to one checkout for its whole lifetime
/// (see [`acquire_writer`]). `#[serde(default)]` on the field that carries
/// this makes `Heavy` what every permit record written before this pool
/// existed deserializes as -- the only kind there was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermitKind {
    #[default]
    Heavy,
    Writer,
}

/// Issue #267: whether a delegated worker (`zirv ctx agent`) may write to
/// its checkout. `Writing` is the CLI default (`--mode` unstated) -- a wrong
/// "read-only" silently drops real edits, which is worse than a wrong
/// "writing" holding a writer-permit slot it did not need. A `Writing`
/// worker holds a writer permit ([`acquire_writer`]) for its whole
/// lifetime, in addition to a heavy permit while it runs an actual heavy
/// command -- unchanged from today. A `ReadOnly` worker never takes a
/// writer permit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerMode {
    ReadOnly,
    #[default]
    Writing,
}

/// One held permit, as read back from `<state>/permits/`. `pub`, and its
/// fields with it (issue #162): a refusal or a wait that cannot say WHO
/// holds the budget is undiagnosable, so both `status.rs`'s occupancy line
/// and `script_runner`'s wait message read these back through
/// [`live_records`] rather than only a bare count. `label` is whatever the
/// acquiring caller wants an operator to see -- `script_runner::command::
/// heavy_permit_for` sets it to the session identity plus the classified
/// command when a session id is available, or the bare command otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermitRecord {
    pub pid: u32,
    /// Finding B5: `pid` above is the script-runner (parent `zirv`) process
    /// that called [`acquire`], not the actual heavy child it goes on to
    /// spawn (`Command::invoke` acquires the permit before `TokioCommand`
    /// spawns anything). If the parent dies first -- killed, or itself
    /// supervised and restarted -- while the real heavy build keeps running,
    /// the dead-owner sweep in [`live_records`] would free the slot while
    /// the actual work it was guarding is still live. Set once the child
    /// exists via [`HeavyPermit::set_child_pid`]; `#[serde(default)]` so a
    /// record written before the child spawns (or by an older binary) still
    /// deserializes as `None` rather than failing to parse.
    #[serde(default)]
    pub child_pid: Option<u32>,
    pub label: String,
    pub acquired_at: u64,
    /// Issue #267: which pool this record belongs to. `#[serde(default)]`
    /// (`PermitKind::default() == Heavy`) so a permit file written before
    /// this field existed still deserializes -- it could only ever have
    /// been a heavy permit.
    #[serde(default)]
    pub kind: PermitKind,
    /// Issue #267: the canonical tree a `Writer` permit holds exclusively --
    /// `None` for a `Heavy` permit, and for a `Writer` record written before
    /// this field existed (`#[serde(default)]`; no such record exists in
    /// practice, since the writer pool is new). Compared for equality via
    /// [`tree_key`], never `PartialEq` on the raw `PathBuf` directly, so two
    /// spellings of the same checkout on a case-insensitive filesystem are
    /// recognised as the same tree.
    #[serde(default)]
    pub tree: Option<PathBuf>,
}

/// `<state>/permits/slot-<n>.json`, one file per budget slot (`n` in
/// `0..limit`) rather than one per holder -- the slot's own filename is what
/// [`acquire`] contends on via `create_new`, so the numbered layout (not
/// `StateDir::sessions()`'s per-record `<pid>-<uuid>.json` naming this used
/// before) is what makes the claim atomic.
fn permits_dir(state: &StateDir) -> PathBuf {
    state.root().join("permits")
}

/// `<dir>/slot-<slot>.json` -- the one file a given budget slot lives at,
/// shared between [`acquire`] (which contends on it) and tests.
fn slot_path(dir: &Path, slot: usize) -> PathBuf {
    dir.join(format!("slot-{slot}.json"))
}

/// Creates `path` exclusively (fails if it already exists) and writes
/// `contents` in one call -- the same collision-as-the-open-result idiom
/// `dash::spawnreq::create_new_private` and `memory`'s per-key entries use,
/// duplicated locally rather than shared across modules this deep in two
/// different subsystems. Private (0600) on Unix, plain on Windows (which has
/// no equivalent bit); `create_new` itself is what supplies the atomicity on
/// both platforms.
#[cfg(unix)]
fn create_new_private(path: &Path, contents: &str) -> std::io::Result<()> {
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
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

/// One held slot in the machine-wide heavy-operation budget. Releases its
/// file on `Drop`, however the holder exits -- infallible and silent, since
/// this binary's release profile is `panic = "abort"` and a permit that
/// fails to clean up is swept by the next [`live_count`] anyway (see that
/// function's own doc comment).
#[derive(Debug)]
pub struct HeavyPermit {
    path: PathBuf,
    /// Review finding (2026-09): the writer pool's own per-tree exclusivity
    /// claim (see [`claim_tree`]), taken BEFORE this permit's pool slot and
    /// released WITH it -- `None` for every heavy-pool permit ([`acquire`]
    /// never has a tree to claim) and set by [`acquire_writer`] once its own
    /// pool-slot claim (via [`acquire_record`]) actually succeeds.
    tree_claim: Option<PathBuf>,
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(tree_claim) = &self.tree_claim {
            let _ = std::fs::remove_file(tree_claim);
        }
    }
}

impl HeavyPermit {
    /// Records the spawned heavy child's own pid on this permit (finding
    /// B5), once it exists, so [`live_records`]' dead-owner sweep can treat
    /// the slot as still held if EITHER the parent (the script-runner
    /// process that called [`acquire`]) or this child is alive -- a parent
    /// that dies first must not free a slot the real heavy work is still
    /// using.
    ///
    /// Review finding (2026-09): a `Writer` permit's paired [`tree_claim`]
    /// (see [`claim_tree`]) gets the SAME `child_pid` written to it. Without
    /// this, the tree claim's own dead-owner sweep only ever saw the
    /// (possibly already-dead) parent pid -- never the child this call is
    /// told about -- and freed the tree the moment the parent died, even
    /// while the real worker, running as the child, was still using it.
    ///
    /// [`tree_claim`]: HeavyPermit::tree_claim
    ///
    /// Best-effort and silent on any I/O or (de)serialization failure on
    /// either file, the same discipline every other write in this module
    /// holds: a file this cannot update simply keeps its previous (more
    /// conservative) liveness answer.
    pub fn set_child_pid(&self, child_pid: u32) {
        Self::write_child_pid(&self.path, child_pid);
        if let Some(tree_claim) = &self.tree_claim {
            Self::write_child_pid(tree_claim, child_pid);
        }
    }

    /// The actual read-modify-write behind [`set_child_pid`], factored out
    /// so the pool-slot file and the (optional) paired tree-claim file get
    /// identical treatment rather than two independently-drifting copies of
    /// the same four lines.
    fn write_child_pid(path: &Path, child_pid: u32) {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(mut record) = serde_json::from_str::<PermitRecord>(&contents) else {
            return;
        };
        record.child_pid = Some(child_pid);
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Whether `record`'s owner is still alive -- true if EITHER the process
/// that acquired the permit (`record.pid`) or, once set, the child it went
/// on to spawn (`record.child_pid`, via [`HeavyPermit::set_child_pid`]) is
/// still running (finding B5). Shared by every dead-owner sweep in this
/// module -- [`live_records_in`]'s own sweep and [`claim_tree`]'s tree-claim
/// sweep -- so the rule can never independently drift between the two
/// (review finding, 2026-09).
fn permit_record_is_alive(record: &PermitRecord) -> bool {
    is_alive(record.pid) || record.child_pid.is_some_and(is_alive)
}

/// Pure: whether `modified` is more than `grace_secs` older than `now`. Split
/// out of [`slot_file_is_stale`] so finding 2a's actual age arithmetic is
/// unit-testable without a real file or the real clock -- the same
/// discipline `group::is_overdue` already holds for its own age check.
fn is_stale(modified: SystemTime, now: SystemTime, grace_secs: u64) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age.as_secs() > grace_secs,
        // `modified` is in the future (a clock skew, not a crash-orphaned
        // file written moments ago) -- never treat that as stale.
        Err(_) => false,
    }
}

/// Whether an unparseable slot file at `path` is old enough to be swept
/// (finding 2a). Any I/O or clock error resolves to `false` -- not old
/// enough -- since the safer default here is leaving a file alone, not
/// deleting it on a filesystem that cannot even report its own age.
fn slot_file_is_stale(path: &Path, grace_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    is_stale(modified, SystemTime::now(), grace_secs)
}

/// Every heavy-operation permit currently held, sweeping (and never
/// including) any entry whose owning pid is no longer alive -- reusing
/// `sessions::is_alive`, the bare-pid signal-0 probe, rather than a second,
/// independently-drifting copy -- so a permit left behind by a killed or
/// crashed holder never wedges the budget forever. A directory that does not
/// exist yet, or a file that fails to read or parse, both read as "not
/// held": nothing on a fresh machine, and one malformed file must never fail
/// the whole listing.
///
/// Issue #152: `sessions::list`'s own sweep moved on to `sessions::
/// record_is_alive`, which disambiguates an `EPERM`-read pid by comparing a
/// `Record`'s stamped `start_time` against a freshly read one. `PermitRecord`
/// carries no `start_time` and deliberately is not getting one in that same
/// change -- a permit slot's failure mode is different from a session
/// record's: it is not offered for restore or addressed by a human-typed
/// prefix, so a wedged slot merely outlives its holder briefly and then
/// frees the moment that pid genuinely frees (or `is_alive`'s own `EPERM`
/// residual applies, same as before). Extending `PermitRecord` to carry a
/// start time too is a deliberate non-goal of this fix, not an oversight.
///
/// Exposed as records, not just a count (issue #162): `status.rs`'s
/// occupancy line and `script_runner`'s wait message both need to name WHO
/// holds the budget, and reading both off this same function is what makes
/// the two guaranteed to never disagree.
pub fn live_records(state: &StateDir) -> Vec<PermitRecord> {
    live_records_in(&permits_dir(state))
}

/// Issue #267: the directory-generic core of [`live_records`], shared with
/// [`live_writer_records`] -- one dead-owner sweep and one file-reading
/// discipline for both independent pools, distinguished only by which
/// directory each pool's own accessor passes in ([`permits_dir`] for the
/// heavy pool, [`writer_permits_dir`] for the writer pool). Heavy-pool
/// behaviour is byte-for-byte unchanged: [`live_records`] is this call with
/// no other logic around it, exactly like before this split.
fn live_records_in(dir: &Path) -> Vec<PermitRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
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
        let Ok(record) = serde_json::from_str::<PermitRecord>(&contents) else {
            // Finding 2a: an unparseable file is either a crash-orphaned
            // partial write (`acquire`'s own `create_new` then `write_all`
            // is two syscalls, and this binary is `panic = "abort"`) or a
            // write genuinely still in progress. Only the former is safe to
            // remove -- age it by mtime, and free the slot index once it is
            // older than a short grace window, so a crash-orphaned file does
            // not lose that slot forever while a fresh write is left alone.
            if slot_file_is_stale(&path, UNPARSEABLE_SLOT_GRACE_SECS) {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        };
        // Finding B5: live if EITHER the parent that acquired the permit or
        // the heavy child it went on to spawn is alive -- a parent that
        // exits (or is restarted by its own supervisor) first must not free
        // a slot the real heavy work is still using. `child_pid` is `None`
        // until `HeavyPermit::set_child_pid` runs, so a permit still in its
        // brief pre-spawn window falls back to the parent-only check this
        // always had.
        let alive = permit_record_is_alive(&record);
        if alive {
            found.push(record);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    found
}

/// How many heavy-operation permits are currently held -- see
/// [`live_records`] for the per-holder detail this counts.
pub fn live_count(state: &StateDir) -> usize {
    live_records(state).len()
}

/// Grants a permit when one of `limit` numbered slots can be claimed
/// exclusively, `None` when every slot is already taken. Best-effort like
/// every other piece of state-dir housekeeping in this codebase: a permit
/// that cannot be written is simply not granted rather than failing the
/// caller outright.
///
/// Atomic across processes (finding B1): earlier this counted live permits
/// and then wrote a new one, a count-then-create window in which two
/// processes could both observe a free slot and both acquire, exceeding
/// `limit`. Now the count is never trusted as the admission decision --
/// [`live_records`] is still called first, but only to sweep dead holders'
/// files out of the way; the ACTUAL decision is each candidate slot's own
/// `create_new`, a single atomic filesystem operation on both Windows and
/// Unix. Two processes racing for the same index can never both succeed:
/// exactly one `open` wins, and the loser tries the next index instead of
/// believing it holds the budget.
pub fn acquire(state: &StateDir, limit: usize, label: &str) -> Option<HeavyPermit> {
    let record = PermitRecord {
        pid: std::process::id(),
        child_pid: None,
        label: label.to_string(),
        acquired_at: state::now_secs(),
        kind: PermitKind::Heavy,
        tree: None,
    };
    acquire_record(&permits_dir(state), limit, record)
}

/// Issue #267: the directory-and-record-generic core of [`acquire`], shared
/// with [`acquire_writer`] -- one `create_new`-contention claim loop and one
/// verify-then-retry discipline (finding 2b) for both independent pools.
/// `record.pid` is trusted as the caller's own claim identity; every other
/// field travels through untouched. Heavy-pool behaviour is byte-for-byte
/// unchanged: [`acquire`] builds the identical `PermitRecord` it always did
/// (now with `kind: Heavy, tree: None` made explicit) and calls this with no
/// other logic around it.
fn acquire_record(dir: &Path, limit: usize, record: PermitRecord) -> Option<HeavyPermit> {
    if limit == 0 {
        return None;
    }
    state::create_private_dir_all(dir).ok()?;

    // Sweeps any slot whose owning pid is dead, freeing it for reuse below --
    // see `live_records`'s own doc comment. `live_records_in` is a cheap fast
    // path ONLY, not the admission decision: skipping the slot loop entirely
    // when every slot already looked live avoids `limit` doomed `create_new`
    // attempts in the common contended case. A stale or racing count here
    // can only make this function too conservative (refuse when a slot was
    // about to free up), never too permissive -- the loop below, not this
    // check, is what enforces `limit`.
    if live_records_in(dir).len() >= limit {
        return None;
    }

    let own_pid = record.pid;
    let json = serde_json::to_string_pretty(&record).ok()?;

    // Re-review (2026-08-27) finding 2b: a claim that wins `create_new` is
    // not yet trustworthy on its own. `live_records_in`'s own dead-owner
    // sweep (above, and on every other caller running concurrently) reads a
    // slot, decides its owner is dead, and removes the file with no recheck
    // -- so a concurrent `acquire`/`acquire_writer` that wins `create_new`
    // on that exact path between the sweeper's read and its `remove_file`
    // gets its brand-new permit file deleted out from under it while it
    // believes it holds the slot. Re-reading and verifying the just-written
    // record closes that: a lost claim retries the whole scan (a slot the
    // sweeper just freed, or that another holder just released, may now be
    // claimable) rather than ever proceeding with a phantom permit.
    for _ in 0..CLAIM_VERIFY_ATTEMPTS {
        let path = claim_any_slot(dir, limit, &json)?;
        if claim_is_verified(&path, own_pid) {
            return Some(HeavyPermit {
                path,
                tree_claim: None,
            });
        }
        // The file this claim just wrote is gone (swept) or holds a record
        // this call never wrote (clobbered) -- never proceed with a phantom
        // permit. Best-effort retry: loop back and scan again.
    }
    None
}

/// `<state>/permits/writers/`: the writer pool's own directory, a sibling of
/// the heavy pool's numbered slot files rather than sharing them (issue
/// #267) -- so [`live_records`]/[`live_count`]'s existing heavy-pool
/// behaviour never has to filter a writer's `writer-<n>.json` out of its own
/// `slot-<n>.json` listing. Same `<state>/permits/` root, same `create_new`
/// contention idiom, same dead-owner sweep -- just its own numbered
/// namespace.
fn writer_permits_dir(state: &StateDir) -> PathBuf {
    permits_dir(state).join("writers")
}

/// Every writer permit currently held -- the writer-pool counterpart of
/// [`live_records`], sweeping dead owners the identical way.
pub fn live_writer_records(state: &StateDir) -> Vec<PermitRecord> {
    live_records_in(&writer_permits_dir(state))
}

/// Issue #267: pure -- the comparison key for a writer permit's own tree
/// path. Case-folded on Windows and macOS, where the filesystem itself is
/// case-insensitive, so two spellings of the same checkout (`D:\repo` vs
/// `d:\REPO`) are recognised as the same tree rather than silently letting
/// two writers hold it at once; verbatim on every other platform. Callers
/// pass an already-canonicalised path (the same contract `agent::
/// validate_workdir`'s callers already hold); this does no filesystem I/O
/// of its own, so it stays testable without a real directory or a real OS
/// difference to run on.
pub fn tree_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if cfg!(any(windows, target_os = "macos")) {
        raw.to_lowercase()
    } else {
        raw.into_owned()
    }
}

/// Why [`acquire_writer`] refused. `TreeBusy` names the label of whichever
/// live writer already holds the requested tree -- diagnosable the same way
/// [`PermitRecord::label`] already makes a heavy-permit wait diagnosable
/// (issue #162). `PoolExhausted` is the ordinary bounded-pool refusal every
/// `acquire_record` caller can hit, independent of which tree was asked
/// for. Both are retryable: the same request typically succeeds once the
/// other writer finishes, or immediately with a fresh `--worktree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterRefusal {
    TreeBusy { holder_label: String },
    PoolExhausted,
}

/// Issues #267/#338: the one diagnostic rendering shared by headless and
/// dashboard worker admission. A tree refusal keeps the original holder and
/// `--worktree` guidance. A machine-wide refusal names its operator controls
/// and every live holder, including each holder's tree, so a cross-repository
/// collision is diagnosable without a separate status call.
pub(crate) fn describe_writer_refusal(
    refusal: &WriterRefusal,
    state: &StateDir,
    max_writers: usize,
    tree: &Path,
) -> String {
    match refusal {
        WriterRefusal::TreeBusy { holder_label } => format!(
            "writer-busy: another writing worker already holds {} ({holder_label}); retry once \
             it finishes, or pass --worktree for an isolated checkout",
            tree.display()
        ),
        WriterRefusal::PoolExhausted => {
            let holders = live_writer_records(state);
            let mut description = format!(
                "writer-busy: the writer-permit pool ({} of {} in use) is full; raise \
                 supervise.max_writers or ZIRV_CTX_SUPERVISE_MAX_WRITERS (0 lifts the \
                 machine-wide cap), or retry once a writer finishes",
                holders.len(),
                max_writers
            );
            for holder in holders {
                let holder_tree = holder
                    .tree
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(unknown tree)".to_string());
                description.push_str(&format!(
                    "\n  pid {} -- {} -- {holder_tree}",
                    holder.pid, holder.label
                ));
            }
            description
        }
    }
}

/// `<state>/permits/writers/trees/`: a subdirectory of the writer pool's own
/// slot directory, deliberately separate from the `slot-<n>.json` files
/// [`live_records_in`] scans there -- a tree claim (see [`claim_tree`]) is
/// not a pool slot, and [`live_writer_records`] must keep returning exactly
/// the slots it always did (`std::fs::read_dir` is
/// not recursive, so this subdirectory's files never appear in that
/// listing).
fn tree_claims_dir(state: &StateDir) -> PathBuf {
    writer_permits_dir(state).join("trees")
}

/// A stable (same input, same output, same running binary), filesystem-safe
/// name for `key` (already [`tree_key`]-normalised) -- a raw path cannot
/// double as a filename directly (a Windows drive path's own `:`, arbitrary
/// length, separators), so this hashes it instead. `DefaultHasher` is not
/// guaranteed stable across Rust versions/toolchains, but every process
/// racing for the same tree here is running the identical compiled binary,
/// which is the only stability this needs.
fn tree_claim_hash(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// `<dir>/tree-<hash>.json` -- the one file a given tree's exclusivity claim
/// lives at, shared between [`claim_tree`] (which contends on it) and tests.
fn tree_claim_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("tree-{}.json", tree_claim_hash(key)))
}

/// Review finding (2026-09): makes "is another live writer already holding
/// this tree?" and "claim it" ONE atomic filesystem operation, closing a
/// race the former read-then-create check in [`acquire_writer`] left open --
/// with `supervise.max_writers > 1`, two concurrent [`acquire_writer`] calls
/// for the SAME tree could both pass a `live_records_in` scan (neither's
/// record had been written yet) and then both go on to claim a DIFFERENT
/// pool slot, defeating "one writer per tree" even though the pool itself
/// had room for both.
///
/// `record` is the exact [`PermitRecord`] [`acquire_writer`] is about to try
/// to write to the pool as well -- same `pid`/`label`/`tree`, so this claim
/// file is swept by the identical dead-owner rule every other permit file
/// in this module already gets, rather than a second, independently-drifting
/// copy of that logic. [`CLAIM_VERIFY_ATTEMPTS`] only matters when the claim
/// this call is racing against belongs to a dead owner or is still mid-write
/// (finding 2a's own grace window, reused here); ordinary contention for a
/// genuinely live tree resolves to [`WriterRefusal::TreeBusy`] on the very
/// first attempt.
fn claim_tree(dir: &Path, key: &str, record: &PermitRecord) -> Result<PathBuf, WriterRefusal> {
    let _ = state::create_private_dir_all(dir);
    let path = tree_claim_path(dir, key);
    let Ok(json) = serde_json::to_string_pretty(record) else {
        return Err(WriterRefusal::PoolExhausted);
    };
    for _ in 0..CLAIM_VERIFY_ATTEMPTS {
        match create_new_private(&path, &json) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // The file this call just lost the race for may already be
                // gone again by the time this reads it (freed concurrently)
                // -- that case falls through and simply retries.
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    match serde_json::from_str::<PermitRecord>(&contents) {
                        Ok(existing) => {
                            let alive = permit_record_is_alive(&existing);
                            if alive {
                                return Err(WriterRefusal::TreeBusy {
                                    holder_label: existing.label,
                                });
                            }
                            // The recorded owner is dead -- sweep the stale
                            // claim and retry, the same dead-owner discipline
                            // every other permit file in this module already
                            // applies.
                            let _ = std::fs::remove_file(&path);
                        }
                        Err(_) => {
                            // Finding 2a's own unparseable-file grace window,
                            // reused here: only remove it once it is old
                            // enough that a write genuinely still in progress
                            // is implausible.
                            if slot_file_is_stale(&path, UNPARSEABLE_SLOT_GRACE_SECS) {
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                }
            }
            // Any other I/O error is not fatal to the whole attempt -- retry,
            // same discipline as `claim_any_slot`.
            Err(_) => {}
        }
    }
    // Every retry exhausted without ever confirming the tree free. The only
    // way this happens is persistent contention (another live claim, or a
    // dead one this call keeps losing the sweep race for) -- either way,
    // refusing is the one outcome that can never let two writers share a
    // tree, so this reports the same refusal ordinary contention would.
    Err(WriterRefusal::TreeBusy {
        holder_label: "(unknown -- contended claim)".to_string(),
    })
}

/// Issue #267: grants a writer permit for `tree` -- one checkout a `--mode
/// writing` delegated worker holds exclusively for its whole lifetime,
/// never only for the duration of one heavy command the way a heavy permit
/// is. Two independent refusals:
///
/// 1. [`WriterRefusal::TreeBusy`] -- some other LIVE writer permit already
///    holds [`tree_key`]'s own claim on this tree (enforced atomically by
///    [`claim_tree`], taken before anything else below). This is the "never
///    two writers in one worktree" rule (design section 3); `--worktree`
///    sidesteps it entirely by naming a fresh, never-before-seen tree.
/// 2. [`WriterRefusal::PoolExhausted`] -- when `limit` is positive, every one
///    of its slots is already claimed by writers in OTHER trees. Zero means
///    there is no machine-wide cap; the holder record is still written, with
///    an effective limit of [`usize::MAX`], so status and refusal diagnostics
///    continue to list every writer. A tree claim not backed by a pool slot is
///    released immediately, so a refused request never wedges the tree for
///    one that never launched.
///
/// Reuses [`acquire_record`] for the pool-slot claim, so a writer permit
/// survives a crash and is swept exactly like a heavy one (same
/// `create_new` contention files, same dead-owner sweep) -- just under
/// [`writer_permits_dir`] instead of [`permits_dir`].
pub fn acquire_writer(
    state: &StateDir,
    limit: usize,
    label: &str,
    tree: &Path,
) -> Result<HeavyPermit, WriterRefusal> {
    let dir = writer_permits_dir(state);
    let key = tree_key(tree);
    let record = PermitRecord {
        pid: std::process::id(),
        child_pid: None,
        label: label.to_string(),
        acquired_at: state::now_secs(),
        kind: PermitKind::Writer,
        tree: Some(tree.to_path_buf()),
    };
    let claim_path = claim_tree(&tree_claims_dir(state), &key, &record)?;
    let effective_limit = if limit == 0 { usize::MAX } else { limit };
    match acquire_record(&dir, effective_limit, record) {
        Some(mut permit) => {
            permit.tree_claim = Some(claim_path);
            Ok(permit)
        }
        None => {
            // Nothing was actually granted -- release the tree claim taken
            // above so it never outlives the request that never launched.
            let _ = std::fs::remove_file(&claim_path);
            Err(WriterRefusal::PoolExhausted)
        }
    }
}

/// One pass over every slot index, returning the path this call manages to
/// claim via `create_new` -- `None` once every index in `0..limit` is
/// already taken. Split out of [`acquire`] so finding 2b's verify-then-retry
/// loop can re-run a full scan on a lost claim, rather than only retrying
/// the single index that was lost.
fn claim_any_slot(dir: &Path, limit: usize, json: &str) -> Option<PathBuf> {
    for slot in 0..limit {
        let path = slot_path(dir, slot);
        match create_new_private(&path, json) {
            Ok(()) => return Some(path),
            // Someone else already holds this slot (the expected, common
            // case under contention) -- try the next index.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // Any other I/O error on this one slot (e.g. a transient
            // permission hiccup) is not fatal to the whole attempt either --
            // best-effort, same as every other state-dir write in this
            // module.
            Err(_) => continue,
        }
    }
    None
}

/// Re-review (2026-08-27) finding 2b: re-reads `path` right after this call
/// claimed it and confirms the record on disk is still the one this call
/// itself wrote (by `pid`). `false` means the claim was lost between the
/// `write_all` inside [`create_new_private`] and this check -- the file is
/// gone (a concurrent dead-owner sweep won the race) or was overwritten with
/// someone else's record (not possible under `create_new`'s own atomicity
/// today, but cheap to rule out here rather than assumed forever) -- either
/// way, [`acquire`] must not treat this as a held permit.
fn claim_is_verified(path: &Path, expected_pid: u32) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(record) = serde_json::from_str::<PermitRecord>(&contents) else {
        return false;
    };
    record.pid == expected_pid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_orphan_permit(state: &StateDir, label: &str, pid: u32) {
        let dir = permits_dir(state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid,
            child_pid: None,
            label: label.to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        };
        let path = dir.join(format!("{pid}-{}.json", uuid::Uuid::new_v4()));
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        state::write_private(&path, &json).expect("write");
    }

    /// Issue #155, Phase 5(e): the budget must count WORK, not sessions. The
    /// old rule counted `Verb::Exec | Verb::Dash` records, so an idle worker
    /// consumed the whole default budget of 1 -- which meant one parked
    /// delegation blocked every subsequent one, and the orchestrator did the
    /// work itself on the expensive seat.
    #[test]
    fn heavy_classification_is_about_the_command_not_the_session() {
        let none: Vec<String> = Vec::new();
        for heavy in [
            "cargo build",
            "cargo build --release",
            "cargo test --verbose -- --test-threads=1",
            "cargo nextest run --no-fail-fast",
            "cargo clippy --all-targets -- -D warnings",
            "cargo package",
            "cargo publish --dry-run",
        ] {
            assert!(is_heavy(heavy, &none), "{heavy} must hold a permit");
        }
        for light in [
            "git status",
            "cargo --version",
            "cargo fmt -- --check",
            "ls",
            "rg TODO src/",
            "echo cargo build",
        ] {
            assert!(!is_heavy(light, &none), "{light} must not hold a permit");
        }
    }

    /// Operator patterns ADD to the built-in set; they never replace it. A
    /// repo layer may only add, which is narrowing.
    #[test]
    fn configured_patterns_extend_the_builtin_set() {
        let extra = vec!["npm run build*".to_string()];
        assert!(is_heavy("npm run build --workspaces", &extra));
        assert!(is_heavy("cargo build", &extra), "built-ins still apply");
        assert!(!is_heavy("npm run lint", &extra));
    }

    /// The permit itself: bounded, released on drop, and never held by an
    /// idle process.
    #[test]
    fn a_permit_is_bounded_and_released_on_drop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let first = acquire(&state, 1, "cargo build").expect("the first permit is granted");
        assert_eq!(live_count(&state), 1);
        assert!(
            acquire(&state, 1, "cargo nextest run").is_none(),
            "the budget of 1 must refuse a second concurrent heavy operation"
        );

        drop(first);
        assert_eq!(live_count(&state), 0);
        assert!(
            acquire(&state, 1, "cargo build").is_some(),
            "the slot is free again"
        );
    }

    /// Finding B1: acquisition used to be count-then-create with no lock, so
    /// two racing callers could both observe a free slot and both acquire,
    /// exceeding `limit`. Races `threads` real OS threads against a budget of
    /// 1 with a `Barrier` to line them up as close to simultaneously as this
    /// process can manage, and asserts the OUTCOME rather than the timing:
    /// however the race actually interleaves, `create_new`'s own atomicity
    /// must mean exactly one acquisition ever succeeds -- never zero (the old
    /// code could never grant none when a slot was free) and never more than
    /// one (the bug this test exists to catch).
    #[test]
    fn concurrent_acquisitions_never_exceed_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        const THREADS: usize = 16;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));

        // Held for the whole race so every granted permit is still live when
        // counted -- an ungated `Drop` racing the count below would make a
        // momentarily-too-low reading look like a pass for the wrong reason.
        let held: Vec<Option<HeavyPermit>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let state = state.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        acquire(&state, 1, &format!("racer-{i}"))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("racer thread must not panic"))
                .collect()
        });

        let granted = held.iter().filter(|permit| permit.is_some()).count();
        assert_eq!(
            granted, 1,
            "a budget of 1 must grant exactly one permit even when {THREADS} threads race for it"
        );
    }

    /// A permit whose owning process is gone must not wedge the budget
    /// forever -- the same dead-owner sweep `sessions::list` already performs
    /// for session records, and `dash`'s `owner.pid` sweep for request dirs.
    #[test]
    fn a_permit_left_by_a_dead_process_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dead_pid = crate::commands::ctx::testenv::dead_pid();
        write_orphan_permit(&state, "cargo build", dead_pid);
        assert_eq!(
            live_count(&state),
            0,
            "a dead owner's permit does not count"
        );
        assert!(acquire(&state, 1, "cargo build").is_some());
    }

    /// Finding B5: `pid` on a `PermitRecord` names the script-runner
    /// (parent) process that called `acquire`, not the actual heavy child it
    /// goes on to spawn. If the parent dies first while the real heavy child
    /// is still running, the sweep must not free the slot just because the
    /// PARENT is gone -- it must also check `child_pid`. Simulates that
    /// exact shape: a dead recorded `pid` (the gone parent) alongside a
    /// `child_pid` of this very test process (guaranteed alive for the
    /// duration of the test).
    #[test]
    fn a_permit_stays_live_on_a_dead_parent_if_its_child_is_still_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dead_pid = crate::commands::ctx::testenv::dead_pid();
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid: dead_pid,
            child_pid: Some(std::process::id()),
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        state::write_private(&slot_path(&dir, 0), &json).expect("write");

        assert_eq!(
            live_count(&state),
            1,
            "a live child must keep the slot even though the recorded parent pid is dead"
        );
    }

    /// `HeavyPermit::set_child_pid` is the only way `child_pid` is ever set
    /// in production (`Command::invoke`, once the real child is spawned) --
    /// proves it actually persists to the same file `live_records` reads
    /// back, not just to an in-memory copy.
    #[test]
    fn set_child_pid_persists_to_the_permit_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let permit = acquire(&state, 1, "cargo build").expect("permit granted");

        permit.set_child_pid(4242);

        let records = live_records(&state);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].child_pid,
            Some(4242),
            "the child pid must be readable back through live_records, not just held in memory"
        );
    }

    /// Issue #162: a refusal or a wait that cannot say WHO holds the budget
    /// is undiagnosable. `live_records` must report the label each holder
    /// was acquired with, not just how many there are.
    #[test]
    fn live_records_reports_each_holders_own_label() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let _held =
            acquire(&state, 1, "session ab12cd34: cargo nextest run").expect("permit granted");

        let records = live_records(&state);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].label, "session ab12cd34: cargo nextest run");
        assert_eq!(records[0].pid, std::process::id());
    }

    /// Pure arithmetic underlying finding 2a's sweep decision, with no file
    /// or real clock involved: strictly older than `grace_secs` is stale,
    /// exactly at it is not yet, and a `modified` time in the future (clock
    /// skew, not a crash-orphaned file) is never stale.
    #[test]
    fn is_stale_marks_only_strictly_past_the_grace_window() {
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert!(
            !is_stale(base, base + std::time::Duration::from_secs(5), 5),
            "exactly at the grace window is not yet stale"
        );
        assert!(
            is_stale(base, base + std::time::Duration::from_secs(6), 5),
            "one second past the grace window is stale"
        );
        assert!(
            !is_stale(base, base - std::time::Duration::from_secs(1), 5),
            "a modified time in the future must never be treated as stale"
        );
    }

    /// Finding 2a: a crash-orphaned slot file (`acquire`'s own `create_new`
    /// then `write_all` is two syscalls in a `panic = "abort"` binary) older
    /// than the grace window must be swept, freeing its slot index -- before
    /// this fix `live_records` skipped an unparseable file forever without
    /// ever removing it, permanently losing that slot.
    #[test]
    fn an_old_unparseable_slot_file_is_swept_and_the_slot_becomes_claimable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        std::fs::write(&path, "").expect("write empty (unparseable) slot file");

        let old =
            SystemTime::now() - std::time::Duration::from_secs(UNPARSEABLE_SLOT_GRACE_SECS + 5);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for mtime")
            .set_modified(old)
            .expect("backdate mtime");

        assert_eq!(
            live_count(&state),
            0,
            "an unparseable file must never count as a held permit"
        );
        assert!(
            !path.exists(),
            "a crash-orphaned slot file older than the grace window must be swept"
        );
        assert!(
            acquire(&state, 1, "cargo build").is_some(),
            "the swept slot index must become claimable again"
        );
    }

    /// The other half of finding 2a: an unparseable file with a fresh mtime
    /// (just written) must NOT be swept -- it may be a write genuinely still
    /// in progress, and sweeping it out from under an in-flight `acquire`
    /// would reintroduce a phantom-permit race of its own.
    #[test]
    fn a_fresh_unparseable_slot_file_is_not_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        std::fs::write(&path, "").expect("write empty (unparseable) slot file");

        assert_eq!(live_count(&state), 0, "still not a held permit");
        assert!(
            path.exists(),
            "a fresh unparseable file must not be swept -- it may be a write still in progress"
        );
    }

    /// Finding 2b: `claim_is_verified` is true only when the record actually
    /// on disk right now is the one this call itself wrote.
    #[test]
    fn claim_is_verified_true_for_a_freshly_written_own_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        let pid = std::process::id();
        let json = serde_json::to_string_pretty(&PermitRecord {
            pid,
            child_pid: None,
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        })
        .expect("serialize");
        create_new_private(&path, &json).expect("write");

        assert!(claim_is_verified(&path, pid));
    }

    /// Finding 2b: a claim whose file is gone by the time of the verify read
    /// -- exactly what a concurrent dead-owner sweep winning the race would
    /// leave behind -- must never verify.
    #[test]
    fn claim_is_verified_false_once_the_file_is_gone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0); // never written

        assert!(!claim_is_verified(&path, std::process::id()));
    }

    /// Finding 2b: a claim whose file holds a record this call never wrote
    /// (a different pid) must never verify either -- "wrong", not only
    /// "gone".
    #[test]
    fn claim_is_verified_false_when_the_record_names_a_different_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        let json = serde_json::to_string_pretty(&PermitRecord {
            pid: std::process::id().wrapping_add(1),
            child_pid: None,
            label: "someone else's claim".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        })
        .expect("serialize");
        create_new_private(&path, &json).expect("write");

        assert!(!claim_is_verified(&path, std::process::id()));
    }

    /// Finding 2b: exercises `acquire`'s own verify-then-rescan retry at the
    /// level of its building blocks -- a claim whose file vanishes between
    /// the write and the verify (simulating the dead-owner sweep race
    /// `claim_is_verified`'s doc comment describes) must not be trusted, and
    /// the freed index must be claimable again on the very next scan.
    #[test]
    fn a_claim_whose_file_vanishes_before_verification_is_not_trusted_and_the_slot_reclaims() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let own_pid = std::process::id();
        let json = serde_json::to_string_pretty(&PermitRecord {
            pid: own_pid,
            child_pid: None,
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        })
        .expect("serialize");

        let claimed = claim_any_slot(&dir, 1, &json).expect("the only slot is free");
        // Simulate a concurrent dead-owner sweep winning the race between
        // the write inside `claim_any_slot` and this check.
        std::fs::remove_file(&claimed).expect("simulate the sweep");
        assert!(
            !claim_is_verified(&claimed, own_pid),
            "a vanished claim must never verify"
        );

        let reclaimed = claim_any_slot(&dir, 1, &json).expect("the freed slot claims again");
        assert_eq!(reclaimed, claimed, "the same (only) slot index is reused");
    }

    /// Issue #267: a `PermitRecord` written before the writer pool existed
    /// (no `kind`/`tree` fields at all) must still deserialise, and must
    /// read as the only kind there ever was.
    #[test]
    fn a_permit_record_written_before_writer_pools_existed_still_deserialises_as_heavy() {
        let old = r#"{"pid":123,"label":"cargo build","acquired_at":1700000000}"#;
        let record: PermitRecord = serde_json::from_str(old).expect("older records still parse");
        assert_eq!(record.kind, PermitKind::Heavy);
        assert_eq!(record.tree, None);
    }

    /// Issue #267: pure case-folding, no filesystem involved -- the same
    /// path spelled with different case is the same tree only on a
    /// case-insensitive filesystem (Windows/macOS).
    #[test]
    fn tree_key_case_folds_only_on_windows_and_macos() {
        let a = tree_key(Path::new("/Repo/Foo"));
        let b = tree_key(Path::new("/repo/foo"));
        if cfg!(any(windows, target_os = "macos")) {
            assert_eq!(a, b, "case must be folded on this platform");
        } else {
            assert_ne!(a, b, "case must be preserved on this platform");
        }
    }

    /// Design section 3: a second `writing` worker into a tree that already
    /// has a live writer is refused, even when the pool itself has room for
    /// more (`limit` of 2 here) -- tree exclusivity is a separate rule from
    /// the bounded pool, not merely a side effect of a pool of 1.
    #[test]
    fn a_second_writer_in_the_same_tree_is_refused_while_the_first_is_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let _held = acquire_writer(&state, 2, "worker-a", &tree).expect("first writer granted");
        let err = acquire_writer(&state, 2, "worker-b", &tree)
            .expect_err("a second writer in the same tree must be refused");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            }
        );
    }

    /// The other half of the same rule: a DIFFERENT tree must never be
    /// refused just because some other tree already has a live writer.
    #[test]
    fn a_writer_in_a_different_tree_is_granted_even_while_another_tree_is_busy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held_a = acquire_writer(&state, 2, "worker-a", &tree_a).expect("granted");
        assert!(
            acquire_writer(&state, 2, "worker-b", &tree_b).is_ok(),
            "a different tree must not be refused by another tree's writer"
        );
    }

    /// Issue #338: zero removes only the machine-wide bound. Writers in
    /// different trees are both recorded, while the atomic tree claim still
    /// refuses a second writer in either occupied tree.
    #[test]
    fn zero_writer_limit_allows_different_trees_but_keeps_tree_exclusivity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held_a = acquire_writer(&state, 0, "worker-a", &tree_a).expect("first granted");
        let _held_b = acquire_writer(&state, 0, "worker-b", &tree_b).expect("second granted");
        let err = acquire_writer(&state, 0, "worker-c", &tree_a)
            .expect_err("the occupied tree must still be exclusive");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            }
        );

        let records = live_writer_records(&state);
        assert_eq!(records.len(), 2, "both live writers must remain visible");
        assert!(records.iter().any(|record| record.label == "worker-a"));
        assert!(records.iter().any(|record| record.label == "worker-b"));
    }

    /// Issue #338: zero is the opt-out, not a removal of the configurable
    /// machine-wide policy. An explicit bound of one preserves the prior
    /// cross-tree refusal.
    #[test]
    fn explicit_writer_limit_one_preserves_the_machine_wide_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held = acquire_writer(&state, 1, "worker-a", &tree_a).expect("first granted");
        let err = acquire_writer(&state, 1, "worker-b", &tree_b)
            .expect_err("the configured machine-wide bound must be enforced");
        assert_eq!(err, WriterRefusal::PoolExhausted);
    }

    /// Issue #338: an exhausted machine-wide pool names both operator
    /// controls and every live holder, including the repository tree that
    /// status alone previously made hard to correlate with the refusal.
    #[test]
    fn pool_exhaustion_description_names_the_controls_count_and_holders() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held = acquire_writer(&state, 1, "worker-a", &tree_a).expect("first granted");
        let description =
            describe_writer_refusal(&WriterRefusal::PoolExhausted, &state, 1, &tree_b);

        assert!(description.starts_with("writer-busy:"));
        assert!(description.contains("1 of 1 in use"));
        assert!(description.contains("supervise.max_writers"));
        assert!(description.contains("ZIRV_CTX_SUPERVISE_MAX_WRITERS"));
        assert!(description.contains("0 lifts the machine-wide cap"));
        assert!(description.contains(&format!(
            "pid {} -- worker-a -- {}",
            std::process::id(),
            tree_a.display()
        )));
    }

    /// The writer pool's own bound is independent of the heavy pool's --
    /// exhausting one must never affect the other, mirroring `a_permit_is_
    /// bounded_and_released_on_drop` for the heavy pool.
    #[test]
    fn the_writer_pool_is_bounded_independently_of_the_heavy_pool() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held_a = acquire_writer(&state, 1, "worker-a", &tree_a).expect("granted");
        let err = acquire_writer(&state, 1, "worker-b", &tree_b)
            .expect_err("a writer pool of 1 is exhausted by the first writer");
        assert_eq!(err, WriterRefusal::PoolExhausted);

        assert_eq!(
            live_count(&state),
            0,
            "the heavy pool must be untouched by writer acquisitions"
        );
        assert!(
            acquire(&state, 1, "cargo build").is_some(),
            "the heavy pool is independent of the writer pool"
        );
    }

    /// A writer permit left by a dead process must not wedge its tree
    /// forever -- the writer-pool counterpart of `a_permit_left_by_a_dead_
    /// process_is_swept`.
    #[test]
    fn a_writer_permit_left_by_a_dead_process_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let dead_pid = crate::commands::ctx::testenv::dead_pid();

        let dir = writer_permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid: dead_pid,
            child_pid: None,
            label: "worker-a".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        state::write_private(&slot_path(&dir, 0), &json).expect("write");

        assert_eq!(
            live_writer_records(&state).len(),
            0,
            "a dead owner's writer permit does not count"
        );
        assert!(
            acquire_writer(&state, 1, "worker-b", &tree).is_ok(),
            "the tree is free again once the dead owner's permit is swept"
        );
    }

    /// Review finding (2026-09): with `max_writers = 2` (room in the pool for
    /// both), a second `acquire_writer` for the SAME tree must still be
    /// refused as `TreeBusy` -- before the fix, the tree-exclusivity check
    /// was a plain read-then-create with no atomicity of its own, so two
    /// concurrent callers could each see no live writer for the tree yet and
    /// both go on to claim a different (free) pool slot.
    #[test]
    fn two_writers_for_the_same_tree_never_both_succeed_even_with_a_free_slot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let _first = acquire_writer(&state, 2, "worker-a", &tree).expect("first writer granted");
        let err = acquire_writer(&state, 2, "worker-b", &tree)
            .expect_err("a second writer for the same tree must be refused");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            },
            "the free slot must not let a second writer share the tree"
        );
    }

    /// The atomic tree claim must not regress the existing "different trees
    /// never block each other" rule.
    #[test]
    fn different_trees_still_both_succeed_under_the_atomic_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _a = acquire_writer(&state, 2, "worker-a", &tree_a).expect("granted");
        let _b = acquire_writer(&state, 2, "worker-b", &tree_b).expect("granted");
    }

    /// Dropping the first writer frees the tree claim for a third caller --
    /// the permit guard must release both the pool slot AND the tree claim
    /// together, or the tree would stay wedged after the slot alone freed.
    #[test]
    fn dropping_a_writer_frees_the_tree_for_a_new_caller() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let first = acquire_writer(&state, 2, "worker-a", &tree).expect("first writer granted");
        assert!(acquire_writer(&state, 2, "worker-b", &tree).is_err());

        drop(first);

        let third = acquire_writer(&state, 2, "worker-c", &tree)
            .expect("dropping the first writer must free the tree claim too, not just the slot");
        drop(third);
    }

    /// A stale tree claim left by a dead process must not wedge its tree
    /// forever -- mirrors `a_writer_permit_left_by_a_dead_process_is_swept`,
    /// but targets the NEW per-tree claim file directly rather than a slot
    /// file, proving `claim_tree`'s own dead-owner sweep (not just the pool
    /// slot's) reclaims it.
    #[test]
    fn a_stale_tree_claim_from_a_dead_pid_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let dead_pid = crate::commands::ctx::testenv::dead_pid();

        let key = tree_key(&tree);
        let dir = tree_claims_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid: dead_pid,
            child_pid: None,
            label: "worker-a".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        create_new_private(&tree_claim_path(&dir, &key), &json).expect("write stale claim");

        assert!(
            acquire_writer(&state, 1, "worker-b", &tree).is_ok(),
            "a dead owner's tree claim must be swept, freeing the tree"
        );
    }

    /// `live_writer_records` must keep returning exactly the pool slots it
    /// always did -- a tree claim (living under its own
    /// `writers/trees/` subdirectory) must never be double-counted as a
    /// second writer.
    #[test]
    fn live_writer_count_counts_slots_not_tree_claims() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let _held = acquire_writer(&state, 2, "worker-a", &tree).expect("granted");
        assert_eq!(
            live_writer_records(&state).len(),
            1,
            "one writer holds one slot -- the tree claim file must not also be counted"
        );
    }

    /// Review finding (2026-09): `set_child_pid` must propagate the SAME
    /// `child_pid` onto the paired tree claim, not just the pool slot --
    /// proven by acquiring a real writer permit, calling `set_child_pid`,
    /// and reading the tree claim file back off disk directly (`live_
    /// writer_records`/`set_child_pid_persists_to_the_permit_file` already
    /// cover the pool-slot half of this for the heavy pool).
    #[test]
    fn set_child_pid_also_persists_to_the_paired_tree_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let permit = acquire_writer(&state, 1, "worker-a", &tree).expect("granted");
        permit.set_child_pid(4242);

        let key = tree_key(&tree);
        let claim_path = tree_claim_path(&tree_claims_dir(&state), &key);
        let contents = std::fs::read_to_string(&claim_path).expect("read tree claim");
        let record: PermitRecord = serde_json::from_str(&contents).expect("parse");
        assert_eq!(
            record.child_pid,
            Some(4242),
            "the tree claim must carry the same child pid as the pool slot"
        );
    }

    /// Review finding (2026-09), acceptance: with the child pid propagated
    /// (as `set_child_pid` now does), a tree claim whose recorded PARENT pid
    /// is dead but whose CHILD pid is alive must not be swept -- mirrors
    /// `a_permit_stays_live_on_a_dead_parent_if_its_child_is_still_alive`'s
    /// own shape, but for the tree-claim file's own sweep in `claim_tree`
    /// rather than `live_records_in`'s. A second `acquire_writer` for the
    /// same tree, even with room in the pool (`max_writers = 2`), must still
    /// be refused as `TreeBusy`.
    #[test]
    fn a_tree_claim_stays_live_on_a_dead_parent_if_its_child_is_still_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let dead_pid = crate::commands::ctx::testenv::dead_pid();

        let dir = tree_claims_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let key = tree_key(&tree);
        let record = PermitRecord {
            pid: dead_pid,
            child_pid: Some(std::process::id()),
            label: "worker-a".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        create_new_private(&tree_claim_path(&dir, &key), &json).expect("write");

        let err = acquire_writer(&state, 2, "worker-b", &tree)
            .expect_err("a live child must keep the tree claim even though the parent pid is dead");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            },
            "the tree claim must not be swept just because the recorded parent pid is dead"
        );
    }
}
