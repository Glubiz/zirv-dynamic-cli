//! Restart-chain breaker and named respawn failure classes (issue #310, 3b).
//!
//! `run_loop::backoff_for` and `exec.rs`'s own `restarts >= max_restarts`
//! check are both per-PROCESS caps: they reset the moment a fresh `zirv ctx
//! exec`/`loop` invocation starts, so a session that dies, gets relaunched by
//! an operator or a script, and dies again immediately loops forever across
//! process boundaries with no memory of the pattern. This module chains
//! inter-boot gaps ACROSS process boundaries, persisted under the state dir
//! (one JSON record per chain key, capped at [`MAX_STORED_BOOTS`] boots),
//! mirroring `objective.rs`'s own storage idiom: one file per record, written
//! via `create_private_dir_all` + `write_private`, with the actual trip
//! decision kept pure (`evaluate`/`push_boot`/`counts_by_class` never touch
//! the clock or the filesystem) -- I/O lives only in [`load`]/[`store`] and
//! the [`record_boot_and_evaluate`] wrapper built on them.
//!
//! Each boot is tagged with a [`FailureClass`] so a usage-limit death (issue
//! #227's codex-at-capacity case) never spends the `crash` budget: `evaluate`
//! only ever looks at boots of the ONE class it was asked about, so a boot of
//! a different class neither advances nor resets that class's own count.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::state::{StateDir, create_private_dir_all, write_private};

pub const SCHEMA_VERSION: u32 = 1;

/// How many boots a chain record keeps, oldest dropped first. High enough
/// that `evaluate`'s trailing-window check always has real history to look
/// at, low enough that a machine running one repository for months does not
/// accumulate an unbounded file. Mirrors Hermes's own `_MAX_STORED_BOOTS`
/// (50).
pub const MAX_STORED_BOOTS: usize = 50;

/// Named respawn failure classes (issue #310). Each gets its own counter
/// (`counts_by_class`) and its own trailing-window trip check (`evaluate`),
/// so classes never share a budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A genuine crash or unexpected non-zero exit -- also the bucket a
    /// rot/timeout-triggered restart falls into today, since neither is a
    /// vendor-side condition the way `UsageLimit`/`AuthBlocked` are.
    Crash,
    /// This module's own 3a stall detector gave up after the grace period.
    Stalled,
    /// A vendor-reported usage limit or capacity error (issue #227's codex
    /// at-capacity death is the reference case this must not be confused
    /// with `Crash`).
    UsageLimit,
    /// The vendor rejected credentials/authentication outright.
    AuthBlocked,
    /// The agent violated its own expected output protocol in a way rot
    /// scoring alone does not already classify as a plain crash.
    Protocol,
    /// A configured token/tool-call budget was exhausted.
    Budget,
}

/// One respawn, chronologically ordered by construction (callers only ever
/// append via [`push_boot`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boot {
    pub at_secs: u64,
    pub class: FailureClass,
    /// A `zirv ctx loop` cycle's own planned, scheduled fresh launch (or any
    /// other intentional respawn) -- excluded from every chain check
    /// entirely, matching issue #310's "planned `zirv ctx loop` cycles are
    /// tagged and excluded" requirement.
    #[serde(default)]
    pub planned: bool,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainRecord {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub boots: Vec<Boot>,
}

impl Default for ChainRecord {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            boots: Vec::new(),
        }
    }
}

fn record_path(state: &StateDir, key: &str) -> PathBuf {
    state.restart_chains().join(format!("{key}.json"))
}

/// `Ok(None)` both for a missing file and for one that fails to parse -- a
/// caller cannot tell "never recorded" apart from "malformed" anyway. A file
/// that fails to parse is left on disk: reading must never destroy an
/// operator's state to make itself succeed. Mirrors `objective::load`
/// exactly.
pub fn load(state: &StateDir, key: &str) -> CtxResult<Option<ChainRecord>> {
    let Ok(contents) = std::fs::read_to_string(record_path(state, key)) else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&contents).ok())
}

/// Writes a chain record. Matches `objective::store`'s own
/// private-dir-then-atomic-write shape.
pub fn store(state: &StateDir, key: &str, record: &ChainRecord) -> CtxResult<()> {
    create_private_dir_all(&state.restart_chains())?;
    let json = serde_json::to_string_pretty(record)?;
    write_private(&record_path(state, key), &json)?;
    Ok(())
}

/// Pure: appends one boot, capping the stored list at `cap` newest (oldest
/// dropped first).
pub fn push_boot(mut record: ChainRecord, boot: Boot, cap: usize) -> ChainRecord {
    record.boots.push(boot);
    if record.boots.len() > cap {
        let excess = record.boots.len() - cap;
        record.boots.drain(0..excess);
    }
    record
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainVerdict {
    Ok,
    /// The trailing `boots` unplanned respawns of the class asked about were
    /// all within the configured gap of one another: do not auto-resume,
    /// report instead.
    Tripped {
        boots: u32,
    },
}

/// Pure: whether the trailing `max_restarts` UNPLANNED boots of `class` are
/// all within `max_gap_secs` of the previous one in the chain. A boot of a
/// different class, or a planned one, does not participate at all -- it
/// neither advances nor resets this count, which is what keeps a
/// usage-limit death from ever spending the crash budget (issue #310,
/// referencing #227). `max_restarts == 0` never trips (an operator setting
/// the cap to zero has turned the breaker off, not asked for an
/// immediate trip).
pub fn evaluate(
    record: &ChainRecord,
    class: FailureClass,
    max_restarts: u32,
    max_gap_secs: u64,
) -> ChainVerdict {
    if max_restarts == 0 {
        return ChainVerdict::Ok;
    }
    let relevant: Vec<&Boot> = record
        .boots
        .iter()
        .filter(|b| !b.planned && b.class == class)
        .collect();
    let need = max_restarts as usize;
    if relevant.len() < need {
        return ChainVerdict::Ok;
    }
    let tail = &relevant[relevant.len() - need..];
    let within_gap = tail
        .windows(2)
        .all(|pair| pair[1].at_secs.saturating_sub(pair[0].at_secs) <= max_gap_secs);
    if within_gap {
        ChainVerdict::Tripped {
            boots: max_restarts,
        }
    } else {
        ChainVerdict::Ok
    }
}

/// Pure: independent per-class counts over the WHOLE stored window (not only
/// the trailing window `evaluate` checks), planned boots excluded -- what
/// issue #310's "each its own counter" acceptance criterion asks for
/// directly. Surfaced by `zirv ctx status`'s own per-session restart-chain
/// line.
pub fn counts_by_class(record: &ChainRecord) -> BTreeMap<FailureClass, u32> {
    let mut counts = BTreeMap::new();
    for boot in record.boots.iter().filter(|b| !b.planned) {
        *counts.entry(boot.class).or_insert(0) += 1;
    }
    counts
}

/// I/O wrapper: load the chain (a missing/unreadable one reads as empty),
/// append this boot, store it back, then evaluate the freshly updated record
/// against `class`. The one seam a caller (`exec.rs`) actually needs.
/// Best-effort like every other piece of state-dir housekeeping in this
/// codebase: a store failure never blocks the restart decision itself, it
/// only means this boot silently did not count toward the breaker.
pub fn record_boot_and_evaluate(
    state: &StateDir,
    key: &str,
    class: FailureClass,
    planned: bool,
    now_secs: u64,
    max_restarts: u32,
    max_gap_secs: u64,
) -> ChainVerdict {
    let existing = load(state, key).ok().flatten().unwrap_or_default();
    let updated = push_boot(
        existing,
        Boot {
            at_secs: now_secs,
            class,
            planned,
        },
        MAX_STORED_BOOTS,
    );
    let verdict = evaluate(&updated, class, max_restarts, max_gap_secs);
    let _ = store(state, key, &updated);
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(at_secs: u64, class: FailureClass) -> Boot {
        Boot {
            at_secs,
            class,
            planned: false,
        }
    }

    fn record_of(boots: Vec<Boot>) -> ChainRecord {
        ChainRecord {
            schema_version: SCHEMA_VERSION,
            boots,
        }
    }

    #[test]
    fn fewer_than_max_restarts_never_trips() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(100, FailureClass::Crash),
        ]);
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 3, 300),
            ChainVerdict::Ok
        );
    }

    #[test]
    fn three_boots_with_gaps_at_exactly_the_boundary_trips() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(300, FailureClass::Crash),
            boot(600, FailureClass::Crash),
        ]);
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 3, 300),
            ChainVerdict::Tripped { boots: 3 },
            "a gap of exactly max_gap_secs is still within it"
        );
    }

    #[test]
    fn a_gap_one_second_past_the_boundary_does_not_trip() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(300, FailureClass::Crash),
            boot(601, FailureClass::Crash),
        ]);
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 3, 300),
            ChainVerdict::Ok
        );
    }

    #[test]
    fn two_boots_never_trips_regardless_of_gap() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(1, FailureClass::Crash),
        ]);
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 3, 300),
            ChainVerdict::Ok,
            "3 boots are required, not merely 2 close ones"
        );
    }

    #[test]
    fn max_restarts_zero_never_trips() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(1, FailureClass::Crash),
            boot(2, FailureClass::Crash),
        ]);
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 0, 300),
            ChainVerdict::Ok
        );
    }

    /// A usage-limit death must never spend the crash budget: interleaved
    /// boots of the other class neither trip nor reset the class actually
    /// being asked about.
    #[test]
    fn a_usage_limit_death_never_burns_the_crash_budget() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(50, FailureClass::UsageLimit),
            boot(100, FailureClass::Crash),
            boot(150, FailureClass::UsageLimit),
            boot(200, FailureClass::Crash),
        ]);
        // Only 3 Crash boots exist (0, 100, 200), gaps of 100s each: trips.
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 3, 300),
            ChainVerdict::Tripped { boots: 3 }
        );
        // Only 2 UsageLimit boots exist: never enough to trip on its own.
        assert_eq!(
            evaluate(&record, FailureClass::UsageLimit, 3, 300),
            ChainVerdict::Ok
        );
    }

    #[test]
    fn planned_loop_cycles_are_excluded_entirely() {
        let record = record_of(vec![
            Boot {
                at_secs: 0,
                class: FailureClass::Crash,
                planned: true,
            },
            Boot {
                at_secs: 10,
                class: FailureClass::Crash,
                planned: true,
            },
            Boot {
                at_secs: 20,
                class: FailureClass::Crash,
                planned: true,
            },
        ]);
        assert_eq!(
            evaluate(&record, FailureClass::Crash, 3, 300),
            ChainVerdict::Ok,
            "planned boots must never count toward the breaker"
        );
    }

    #[test]
    fn counts_by_class_are_independent() {
        let record = record_of(vec![
            boot(0, FailureClass::Crash),
            boot(1, FailureClass::Crash),
            boot(2, FailureClass::UsageLimit),
            boot(3, FailureClass::Stalled),
        ]);
        let counts = counts_by_class(&record);
        assert_eq!(counts.get(&FailureClass::Crash), Some(&2));
        assert_eq!(counts.get(&FailureClass::UsageLimit), Some(&1));
        assert_eq!(counts.get(&FailureClass::Stalled), Some(&1));
        assert_eq!(counts.get(&FailureClass::AuthBlocked), None);
    }

    #[test]
    fn counts_by_class_excludes_planned_boots() {
        let record = record_of(vec![Boot {
            at_secs: 0,
            class: FailureClass::Crash,
            planned: true,
        }]);
        assert!(counts_by_class(&record).is_empty());
    }

    #[test]
    fn push_boot_caps_at_the_configured_stored_maximum() {
        let mut record = ChainRecord::default();
        for i in 0..60 {
            record = push_boot(record, boot(i, FailureClass::Crash), 50);
        }
        assert_eq!(record.boots.len(), 50);
        assert_eq!(
            record.boots.first().unwrap().at_secs,
            10,
            "oldest dropped first"
        );
        assert_eq!(record.boots.last().unwrap().at_secs, 59);
    }

    #[test]
    fn load_of_a_missing_file_is_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(load(&state, "some-repo").expect("load"), None);
    }

    #[test]
    fn store_then_load_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let record = record_of(vec![boot(0, FailureClass::Crash)]);
        store(&state, "some-repo", &record).expect("store");
        assert_eq!(load(&state, "some-repo").expect("load"), Some(record));
    }

    #[test]
    fn record_boot_and_evaluate_persists_and_trips_across_calls() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let key = "some-repo";

        assert_eq!(
            record_boot_and_evaluate(&state, key, FailureClass::Crash, false, 0, 3, 300),
            ChainVerdict::Ok
        );
        assert_eq!(
            record_boot_and_evaluate(&state, key, FailureClass::Crash, false, 100, 3, 300),
            ChainVerdict::Ok
        );
        assert_eq!(
            record_boot_and_evaluate(&state, key, FailureClass::Crash, false, 200, 3, 300),
            ChainVerdict::Tripped { boots: 3 }
        );

        let stored = load(&state, key).expect("load").expect("present");
        assert_eq!(stored.boots.len(), 3);
    }

    #[test]
    fn a_malformed_file_reads_as_none_rather_than_erroring() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create_private_dir_all(&state.restart_chains()).expect("mkdir");
        std::fs::write(record_path(&state, "some-repo"), "not json").expect("write");
        assert_eq!(load(&state, "some-repo").expect("load"), None);
    }
}
