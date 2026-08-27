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

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::sessions::is_alive;
use super::state::{self, StateDir};

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
pub struct HeavyPermit {
    path: PathBuf,
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl HeavyPermit {
    /// Records the spawned heavy child's own pid on this permit (finding
    /// B5), once it exists, so [`live_records`]' dead-owner sweep can treat
    /// the slot as still held if EITHER the parent (the script-runner
    /// process that called [`acquire`]) or this child is alive -- a parent
    /// that dies first must not free a slot the real heavy work is still
    /// using. Best-effort and silent on any I/O or (de)serialization
    /// failure, the same discipline every other write in this module holds:
    /// if this permit's own file cannot be updated, the pre-existing
    /// parent-pid liveness check still applies, so this can only ever make
    /// the sweep MORE conservative, never less.
    pub fn set_child_pid(&self, child_pid: u32) {
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let Ok(mut record) = serde_json::from_str::<PermitRecord>(&contents) else {
            return;
        };
        record.child_pid = Some(child_pid);
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

/// Every heavy-operation permit currently held, sweeping (and never
/// including) any entry whose owning pid is no longer alive -- reusing
/// `sessions::is_alive`, the same liveness probe `sessions::list` sweeps
/// dead session records with, rather than a second, independently-drifting
/// copy -- so a permit left behind by a killed or crashed holder never
/// wedges the budget forever. A directory that does not exist yet, or a
/// file that fails to read or parse, both read as "not held": nothing on a
/// fresh machine, and one malformed file must never fail the whole listing.
///
/// Exposed as records, not just a count (issue #162): `status.rs`'s
/// occupancy line and `script_runner`'s wait message both need to name WHO
/// holds the budget, and reading both off this same function is what makes
/// the two guaranteed to never disagree.
pub fn live_records(state: &StateDir) -> Vec<PermitRecord> {
    let Ok(entries) = std::fs::read_dir(permits_dir(state)) else {
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
            continue;
        };
        // Finding B5: live if EITHER the parent that acquired the permit or
        // the heavy child it went on to spawn is alive -- a parent that
        // exits (or is restarted by its own supervisor) first must not free
        // a slot the real heavy work is still using. `child_pid` is `None`
        // until `HeavyPermit::set_child_pid` runs, so a permit still in its
        // brief pre-spawn window falls back to the parent-only check this
        // always had.
        let alive = is_alive(record.pid) || record.child_pid.is_some_and(is_alive);
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
    if limit == 0 {
        return None;
    }
    let dir = permits_dir(state);
    state::create_private_dir_all(&dir).ok()?;

    // Sweeps any slot whose owning pid is dead, freeing it for reuse below --
    // see `live_records`'s own doc comment. `live_count` is a cheap fast
    // path ONLY, not the admission decision: skipping the slot loop entirely
    // when every slot already looked live avoids `limit` doomed `create_new`
    // attempts in the common contended case. A stale or racing count here
    // can only make this function too conservative (refuse when a slot was
    // about to free up), never too permissive -- the loop below, not this
    // check, is what enforces `limit`.
    if live_count(state) >= limit {
        return None;
    }

    let record = PermitRecord {
        pid: std::process::id(),
        child_pid: None,
        label: label.to_string(),
        acquired_at: state::now_secs(),
    };
    let json = serde_json::to_string_pretty(&record).ok()?;

    for slot in 0..limit {
        let path = slot_path(&dir, slot);
        match create_new_private(&path, &json) {
            Ok(()) => return Some(HeavyPermit { path }),
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
}
