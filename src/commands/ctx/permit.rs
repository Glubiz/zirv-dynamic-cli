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
//! Cross-process atomicity is deliberately NOT closed here: the count-then-
//! write window is the same TOCTOU today's heavy-worker gate documents, the
//! budget exists to keep concurrency low rather than enforce an exact
//! ceiling, and closing it needs a cross-process lock this state directory
//! has never had.

use std::path::PathBuf;

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
    pub label: String,
    pub acquired_at: u64,
}

/// `<state>/permits/<pid>-<uuid>.json`, one file per held permit -- mirrors
/// `StateDir::sessions()`'s own per-record layout.
fn permits_dir(state: &StateDir) -> PathBuf {
    state.root().join("permits")
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
        if is_alive(record.pid) {
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

/// Grants a permit when fewer than `limit` are currently live, `None`
/// otherwise. Best-effort like every other piece of state-dir housekeeping
/// in this codebase: a permit that cannot be written is simply not granted
/// rather than failing the caller outright.
pub fn acquire(state: &StateDir, limit: usize, label: &str) -> Option<HeavyPermit> {
    if live_count(state) >= limit {
        return None;
    }
    let dir = permits_dir(state);
    state::create_private_dir_all(&dir).ok()?;
    let path = dir.join(format!(
        "{}-{}.json",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let record = PermitRecord {
        pid: std::process::id(),
        label: label.to_string(),
        acquired_at: state::now_secs(),
    };
    let json = serde_json::to_string_pretty(&record).ok()?;
    state::write_private(&path, &json).ok()?;
    Some(HeavyPermit { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_orphan_permit(state: &StateDir, label: &str, pid: u32) {
        let dir = permits_dir(state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid,
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
