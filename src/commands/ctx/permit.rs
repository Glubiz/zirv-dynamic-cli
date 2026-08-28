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

    let own_pid = std::process::id();
    let record = PermitRecord {
        pid: own_pid,
        child_pid: None,
        label: label.to_string(),
        acquired_at: state::now_secs(),
    };
    let json = serde_json::to_string_pretty(&record).ok()?;

    // Re-review (2026-08-27) finding 2b: a claim that wins `create_new` is
    // not yet trustworthy on its own. `live_records`'s own dead-owner sweep
    // (above, and on every other caller running concurrently) reads a slot,
    // decides its owner is dead, and removes the file with no recheck -- so
    // a concurrent `acquire` that wins `create_new` on that exact path
    // between the sweeper's read and its `remove_file` gets its brand-new
    // permit file deleted out from under it while it believes it holds the
    // slot. Re-reading and verifying the just-written record closes that:
    // a lost claim retries the whole scan (a slot the sweeper just freed, or
    // that another holder just released, may now be claimable) rather than
    // ever proceeding with a phantom permit.
    for _ in 0..CLAIM_VERIFY_ATTEMPTS {
        let path = claim_any_slot(&dir, limit, &json)?;
        if claim_is_verified(&path, own_pid) {
            return Some(HeavyPermit { path });
        }
        // The file this claim just wrote is gone (swept) or holds a record
        // this call never wrote (clobbered) -- never proceed with a phantom
        // permit. Best-effort retry: loop back and scan again.
    }
    None
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
}
