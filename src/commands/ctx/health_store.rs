//! Issue #455: the durable half of route health -- `<state>/health/
//! <harness>.json`, one file per harness.
//!
//! Everything that decides anything lives in [`super::health`] and is pure;
//! this module is only the read/write seam plus the one-line decision-log
//! record each phase change earns. Every failure here is swallowed: a health
//! file that cannot be read is `Healthy` (routing behaves exactly as it did
//! before this feature existed), and one that cannot be written is silently
//! dropped, never an error a supervised session can feel.
//!
//! There is no verb for clearing a record: deleting
//! `<state>/health/<harness>.json` resets that route by hand, and every
//! phase heals on its own anyway (see [`super::health::Phase`]).
//!
//! Every read-modify-write here holds one advisory OS lock per harness,
//! taken with `try_lock` and skipped when contended (review round 1, finding
//! 7; round 2, finding 2). Two processes legitimately fold into the
//! same record -- a dashboard pane's `cached_score` and that same pane's own
//! Stop hook -- and while row identity already stops a double COUNT, an
//! unlocked read-modify-write still loses whichever write lands second.
//! Reuses `memory.rs`'s own `BankLock` idiom, right down to borrowing
//! `group::open_lock_file` (which is `pub(crate)` precisely so a second
//! lock-file idiom does not have to re-derive the unix-mode-0600-vs-
//! portable `OpenOptions` split).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CtxResult;

use super::health::{self, Admission, HealthPolicy, Observed, RouteHealth, RouteKey, Transition};
use super::state::StateDir;

/// Records untouched for this long with a `Healthy` phase carry no
/// information at all and are ignored on read (and by [`all`]), so a
/// long-lived machine's `health/` directory cannot turn into a slow ranking
/// read. A non-healthy record is never skipped, however old -- it heals
/// through its own cooldown, not through expiry.
pub const STALE_HEALTHY_SECS: u64 = 24 * 60 * 60;

/// One route's persisted record. `#[serde(default)]` throughout, like every
/// other tolerant registry read in this codebase (`seat::Seat`,
/// `sessions::Record`): a file from an older shape still round-trips instead
/// of failing the whole read.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteHealthRecord {
    /// The key verbatim, so [`all`] never has to parse a sanitized
    /// basename back into a harness name.
    pub key: RouteKey,
    /// The last model observed on this harness. INFORMATIONAL only -- it is
    /// not part of the route identity (see `health`'s own header) and no
    /// decision reads it; `zirv ctx status` shows it so an operator can see
    /// which model was in flight when the breaker tripped.
    pub model: Option<String>,
    pub health: RouteHealth,
    pub updated_at: u64,
}

pub fn dir(state: &StateDir) -> PathBuf {
    state.root().join("health")
}

fn record_path(state: &StateDir, key: &RouteKey) -> PathBuf {
    dir(state).join(format!("{}.json", key.file_stem()))
}

fn lock_path(state: &StateDir, key: &RouteKey) -> PathBuf {
    dir(state).join(format!("{}.lock", key.file_stem()))
}

/// One advisory OS lock per harness record, held across the whole
/// read-modify-write. Same shape as `memory::BankLock`/`seat::SeatLock`.
struct HealthLock(std::fs::File);

impl Drop for HealthLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// `try_lock`, never `lock` (review round 2, finding 2): this critical
/// section is reached from `wrap`'s own Stop hook and from both headless
/// supervisors, and a supervisor may not block on another process's I/O.
/// A contended lock means a concurrent holder is folding the SAME transcript
/// rows, so skipping the fold loses nothing -- the same reasoning
/// `supervise.rs` documents for its own signal handler.
fn lock_route(state: &StateDir, key: &RouteKey) -> CtxResult<HealthLock> {
    super::state::create_private_dir_all(&dir(state))?;
    let file = super::group::open_lock_file(&lock_path(state, key))?;
    file.try_lock()?;
    Ok(HealthLock(file))
}

fn load_record(state: &StateDir, key: &RouteKey, now: u64) -> Option<RouteHealthRecord> {
    let contents = std::fs::read_to_string(record_path(state, key)).ok()?;
    let record = serde_json::from_str::<RouteHealthRecord>(&contents).ok()?;
    (!is_stale_healthy(&record, now)).then_some(record)
}

/// One route's stored health. `Healthy` for a missing, unreadable, corrupt,
/// or aged-out-healthy record -- never an error.
pub fn load(state: &StateDir, key: &RouteKey, now: u64) -> RouteHealth {
    load_record(state, key, now)
        .map(|record| record.health)
        .unwrap_or_default()
}

fn is_stale_healthy(record: &RouteHealthRecord, now: u64) -> bool {
    record.health.phase.is_healthy() && now.saturating_sub(record.updated_at) > STALE_HEALTHY_SECS
}

fn store(state: &StateDir, key: &RouteKey, health: &RouteHealth, model: Option<&str>, now: u64) {
    let record = RouteHealthRecord {
        key: key.clone(),
        // A poll that resolved no model must not erase the last one that
        // did: this field exists to say what was in flight when the breaker
        // tripped.
        model: model
            .map(str::to_string)
            .or_else(|| load_record(state, key, now).and_then(|record| record.model)),
        health: health.clone(),
        updated_at: now,
    };
    let Ok(json) = serde_json::to_string_pretty(&record) else {
        return;
    };
    // Swallowed in both directions, and deliberately NOT written to stderr:
    // this runs inside `wrap`'s own supervision of a live PTY session, where
    // a stray line is a visible defect. A route whose health cannot be
    // persisted simply behaves as it did before this feature existed.
    let _ = super::state::create_private_dir_all(&dir(state))
        .and_then(|()| super::state::write_private(&record_path(state, key), &json));
}

/// Folds one observed provider error into `key`'s record and persists the
/// result. `Some` only when the phase actually changed.
pub fn observe_and_persist(
    state: &StateDir,
    key: &RouteKey,
    observed: &Observed,
    model: Option<&str>,
    now: u64,
    policy: &HealthPolicy,
) -> Option<Transition> {
    // Finding 7: the lock covers the load AND the store. A failure to take
    // it is swallowed like every other I/O failure here -- routing then
    // behaves as it did before this feature existed, which is strictly
    // better than a supervised session feeling an error.
    let _lock = lock_route(state, key).ok()?;
    let current = load(state, key, now);
    let (next, transition) = health::observe(&current, observed, now, policy);
    if next != current {
        store(state, key, &next, model, now);
    }
    transition
}

/// Applies an elapsed cooldown on its own, so the half-open edge is a real
/// persisted phase with its own decision-log line (finding 6). `None` when
/// nothing was due.
fn promote_and_persist(
    state: &StateDir,
    key: &RouteKey,
    model: Option<&str>,
    now: u64,
    policy: &HealthPolicy,
) -> Option<Transition> {
    if !record_path(state, key).exists() {
        return None;
    }
    let _lock = lock_route(state, key).ok()?;
    let current = load(state, key, now);
    let (next, transition) = health::promote(&current, now, policy);
    if next != current {
        store(state, key, &next, model, now);
    }
    transition
}

/// Folds one successfully completed turn into `key`'s record. Reads nothing
/// and writes nothing for a route with no stored record at all, which is the
/// steady state of every healthy session.
pub fn record_success_and_persist(
    state: &StateDir,
    key: &RouteKey,
    model: Option<&str>,
    now: u64,
    policy: &HealthPolicy,
) -> Option<Transition> {
    if !record_path(state, key).exists() {
        return None;
    }
    let _lock = lock_route(state, key).ok()?;
    let current = load(state, key, now);
    if current == RouteHealth::default() {
        return None;
    }
    let (next, transition) = health::record_success(&current, now, policy);
    if next != current {
        store(state, key, &next, model, now);
    }
    transition
}

/// Whether `harness` may be given work right now -- one small file read,
/// then the pure verdict. A route is one harness, so this is the whole
/// question (see `health`'s own header).
pub fn harness_admission(
    state: &StateDir,
    harness: &str,
    now: u64,
    policy: &HealthPolicy,
) -> Admission {
    if !policy.enabled {
        return Admission::Allow;
    }
    let key = RouteKey::new(harness);
    match health::admission(&load(state, &key, now), now, policy) {
        // Named, because this reason travels into standalone human lines
        // (`rollover`'s park message, `zirv ctx status`'s exclusions).
        Admission::Deny { reason } => Admission::Deny {
            reason: format!("{harness}: {reason}"),
        },
        other => other,
    }
}

/// Every stored route with something to say -- an aged-out `Healthy` record
/// is skipped, so this is the set `zirv ctx status` renders. Sorted by
/// harness so callers (and their tests) see a stable order.
pub fn all(state: &StateDir, now: u64) -> Vec<RouteHealthRecord> {
    let Ok(entries) = std::fs::read_dir(dir(state)) else {
        return Vec::new();
    };
    let mut records: Vec<RouteHealthRecord> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let contents = std::fs::read_to_string(entry.path()).ok()?;
            let record = serde_json::from_str::<RouteHealthRecord>(&contents).ok()?;
            if record.key.harness.is_empty() || is_stale_healthy(&record, now) {
                return None;
            }
            Some(record)
        })
        .collect();
    records.sort_by(|a, b| a.key.harness.cmp(&b.key.harness));
    records
}

/// One supervised poll's worth of health observation, folded into
/// `harness`'s record.
///
/// `errors` is this poll's newly parsed provider errors, in order, each
/// carrying the transcript row's own time and id; `turn_succeeded` says the
/// poll also saw a completed assistant turn after the last of them.
/// `incremental` says the poll read APPENDED bytes rather than re-reading
/// the transcript from its start -- a row with no timestamp of its own is
/// observed only then (finding 2), because otherwise there is nothing to
/// distinguish a fresh failure from a months-old one being re-parsed.
///
/// Exactly one decision-log line is appended per phase change -- never one
/// per observation.
#[allow(clippy::too_many_arguments)]
pub fn observe_poll(
    state: &StateDir,
    harness: &str,
    model: Option<&str>,
    errors: &[Observed],
    turn_succeeded: bool,
    incremental: bool,
    now: u64,
    policy: &HealthPolicy,
    session: &str,
    verb: &str,
) {
    if !policy.enabled {
        return;
    }
    let key = RouteKey::new(harness);
    // Finding 6: before anything else, so an elapsed cooldown becomes a
    // persisted `HalfOpen` with its own `health-half-open` line rather than
    // a phase only ever derived on the fly and never recorded.
    if let Some(transition) = promote_and_persist(state, &key, model, now, policy) {
        log_transition(state, &key, &transition, now, session, verb);
    }
    for observed in errors {
        if observed.at.is_none() && !incremental {
            continue;
        }
        if let Some(transition) = observe_and_persist(state, &key, observed, model, now, policy) {
            log_transition(state, &key, &transition, now, session, verb);
        }
    }
    if turn_succeeded
        && let Some(transition) = record_success_and_persist(state, &key, model, now, policy)
    {
        log_transition(state, &key, &transition, now, session, verb);
    }
}

fn log_transition(
    state: &StateDir,
    key: &RouteKey,
    transition: &Transition,
    now: u64,
    session: &str,
    verb: &str,
) {
    let detail = format!("{}: {}", key.label(), transition.reason());
    let _ = super::log::append(
        state,
        &super::log::Decision {
            ts: now,
            session,
            verb,
            verdict: transition.verdict(),
            score: 0,
            action: "route-health",
            detail: &detail,
            observed_at: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::ProviderErrorClass;
    use crate::commands::ctx::health::Phase;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        (dir, state)
    }

    fn transport(at: u64) -> Observed {
        Observed::new(
            ProviderErrorClass::Transport,
            Some(at),
            Some(format!("row-{at}")),
        )
    }

    #[test]
    fn a_record_round_trips_and_a_corrupt_file_reads_as_healthy() {
        let (_guard, state) = state();
        let key = RouteKey::new("claude");
        let policy = HealthPolicy::default();

        for at in [1_000, 1_001, 1_002] {
            observe_and_persist(&state, &key, &transport(at), Some("opus"), at, &policy);
        }
        let stored = load(&state, &key, 1_002);
        assert!(matches!(stored.phase, Phase::Open { .. }), "{stored:?}");
        assert_eq!(stored.observations.len(), 3);
        assert!(
            health::admission(&load(&state, &key, 1_002), 1_002, &policy)
                .denied()
                .is_some()
        );
        let record = all(&state, 1_002).pop().expect("one record");
        assert_eq!(
            record.model.as_deref(),
            Some("opus"),
            "the last model seen is kept for status, not for identity"
        );

        std::fs::write(super::record_path(&state, &key), "{ not json").expect("write");
        assert_eq!(load(&state, &key, 1_002), RouteHealth::default());
        assert_eq!(
            harness_admission(&state, "claude", 1_002, &policy),
            Admission::Allow
        );
    }

    #[test]
    fn observe_and_persist_returns_a_transition_only_on_a_phase_change() {
        let (_guard, state) = state();
        let key = RouteKey::new("codex");
        let policy = HealthPolicy::default();

        assert_eq!(
            observe_and_persist(&state, &key, &transport(10), None, 10, &policy),
            None,
            "the first failure is only a suspicion"
        );
        assert_eq!(
            observe_and_persist(&state, &key, &transport(11), None, 11, &policy),
            None
        );
        assert!(
            observe_and_persist(&state, &key, &transport(12), None, 12, &policy).is_some(),
            "the third failure opens the breaker"
        );
        assert_eq!(
            observe_and_persist(&state, &key, &transport(13), None, 13, &policy),
            None,
            "an already-open breaker does not re-announce"
        );
    }

    /// Finding 4: two supervisors legitimately read the same rows. The
    /// second pass must not count them again, or the breaker opens after
    /// two real failures instead of the configured three.
    #[test]
    fn re_observing_the_same_row_ids_is_a_no_op() {
        let (_guard, state) = state();
        let key = RouteKey::new("claude");
        let policy = HealthPolicy::default();

        for at in [1_000, 1_001] {
            observe_and_persist(&state, &key, &transport(at), None, at, &policy);
        }
        // The same two rows again, as a second reader would see them.
        for at in [1_000, 1_001] {
            assert_eq!(
                observe_and_persist(&state, &key, &transport(at), None, 1_002, &policy),
                None
            );
        }
        let stored = load(&state, &key, 1_002);
        assert_eq!(stored.observations.len(), 2, "{stored:?}");
        assert!(!matches!(stored.phase, Phase::Open { .. }), "{stored:?}");
    }

    #[test]
    fn a_success_recovers_a_route_and_is_a_no_op_without_a_record() {
        let (_guard, state) = state();
        let key = RouteKey::new("claude");
        let policy = HealthPolicy::default();

        assert_eq!(
            record_success_and_persist(&state, &key, None, 100, &policy),
            None,
            "no stored record means nothing to recover"
        );

        observe_and_persist(&state, &key, &transport(100), None, 100, &policy);
        assert!(record_success_and_persist(&state, &key, None, 110, &policy).is_some());
        assert!(load(&state, &key, 110).phase.is_healthy());
    }

    #[test]
    fn harness_admission_denies_only_the_harness_whose_breaker_is_open() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();

        for at in [1_000, 1_001, 1_002] {
            observe_and_persist(
                &state,
                &RouteKey::new("claude"),
                &Observed::new(ProviderErrorClass::Server, Some(at), Some(format!("s{at}"))),
                None,
                at,
                &policy,
            );
        }
        observe_and_persist(
            &state,
            &RouteKey::new("codex"),
            &Observed::new(
                ProviderErrorClass::Other,
                Some(1_000),
                Some("o1".to_string()),
            ),
            None,
            1_000,
            &policy,
        );

        let denied = harness_admission(&state, "claude", 1_002, &policy);
        let reason = denied.denied().expect("claude is denied");
        assert!(reason.starts_with("claude: "), "{reason}");
        assert_eq!(
            harness_admission(&state, "codex", 1_002, &policy),
            Admission::Allow
        );

        let disabled = HealthPolicy {
            enabled: false,
            ..HealthPolicy::default()
        };
        assert_eq!(
            harness_admission(&state, "claude", 1_002, &disabled),
            Admission::Allow
        );
    }

    #[test]
    fn all_skips_an_aged_out_healthy_record() {
        let (_guard, state) = state();
        let key = RouteKey::new("claude");
        store(&state, &key, &RouteHealth::default(), None, 1_000);
        assert_eq!(all(&state, 1_000).len(), 1);
        assert!(all(&state, 1_000 + STALE_HEALTHY_SECS + 1).is_empty());
    }

    #[test]
    fn observe_poll_logs_one_decision_per_transition() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();

        observe_poll(
            &state,
            "claude",
            Some("opus"),
            &[transport(1_998), transport(1_999), transport(2_000)],
            false,
            true,
            2_000,
            &policy,
            "sess",
            "wrap",
        );
        let log = std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE))
            .expect("decision log");
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\"health-open\""))
                .count(),
            1,
            "{log}"
        );
    }

    /// Finding 2: a poll that re-read the transcript from offset 0 hands
    /// over rows with no time of their own. Folding those in with the
    /// current clock is exactly how a healthy harness got its breaker
    /// opened by months-old history.
    #[test]
    fn undated_rows_are_only_observed_on_an_incremental_poll() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();
        let undated = [
            Observed::new(ProviderErrorClass::Transport, None, None),
            Observed::new(ProviderErrorClass::Transport, None, None),
            Observed::new(ProviderErrorClass::Transport, None, None),
        ];

        observe_poll(
            &state, "claude", None, &undated, false, false, 5_000, &policy, "sess", "exec",
        );
        assert_eq!(
            load(&state, &RouteKey::new("claude"), 5_000),
            RouteHealth::default(),
            "a full re-parse must record nothing it cannot date"
        );

        observe_poll(
            &state, "claude", None, &undated, false, true, 5_000, &policy, "sess", "exec",
        );
        assert!(
            matches!(
                load(&state, &RouteKey::new("claude"), 5_000).phase,
                Phase::Open { .. }
            ),
            "an incremental poll's undated rows are genuinely new"
        );
    }
    /// Finding 6: the half-open edge must be a persisted phase with its own
    /// decision-log line, not a verdict derived on the fly that no
    /// `zirv ctx status` ever showed and no log ever recorded.
    #[test]
    fn an_elapsed_cooldown_is_persisted_and_logged_as_half_open() {
        let (_guard, state) = state();
        let key = RouteKey::new("claude");
        let policy = HealthPolicy::default();

        for at in [1_000, 1_001, 1_002] {
            observe_and_persist(&state, &key, &transport(at), None, at, &policy);
        }
        assert!(matches!(
            load(&state, &key, 1_002).phase,
            Phase::Open { .. }
        ));

        // One poll after the cooldown carrying no errors and a completed
        // turn -- `turn_succeeded` then `incremental`.
        let later = 1_002 + 300;
        observe_poll(
            &state,
            "claude",
            None,
            &[],
            true,
            true,
            later,
            &policy,
            "sess",
            "wrap",
        );

        let log = std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE))
            .expect("decision log");
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\"health-half-open\""))
                .count(),
            1,
            "{log}"
        );
        // The same poll's success then heals it, which is its own line.
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\"health-recovered\""))
                .count(),
            1,
            "{log}"
        );
        assert!(load(&state, &key, later).phase.is_healthy());
    }
}
