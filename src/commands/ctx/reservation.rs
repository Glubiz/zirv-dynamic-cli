//! Per-provider token-reservation ledger (issue #358, task T3).
//!
//! `group.rs`'s `reserved_tokens` protects one WORK GROUP's own budget:
//! two children admitted into the same group must not both be handed its
//! entire remaining ceiling before either one's spend has rolled up. This
//! module answers a different question -- how much does one PROVIDER
//! (`adapters::provider_for_agent_name`'s own vocabulary, "claude"/"codex"/
//! etc) already owe across EVERY admitted-but-unsettled delegation right
//! now, grouped or not -- so a machine-wide capacity snapshot can be built
//! without walking every group record. A group ceiling and a provider
//! reservation are taken and released independently, for the same admitted
//! child: neither subsumes the other.
//!
//! Storage mirrors `group.rs` exactly: one JSON file per provider under
//! `StateDir::reservations()`, a sibling `.lock` file guarded by the same
//! advisory OS lock `group::open_lock_file` already opens for `group.rs`
//! and `task.rs`, and every mutation is a load-modify-write under that lock.
//! Owner liveness reuses the permit pool's own discipline
//! (`permit::permit_record_is_alive`'s bare `sessions::is_alive(pid)`
//! check, not `sessions::record_is_alive`'s fuller start-time
//! disambiguation, which needs a whole `sessions::Record` this ledger has
//! no reason to carry) -- a reservation whose owning process is gone is
//! excluded from [`outstanding`] and swept the next time this provider's
//! ledger is locked for a write, exactly like a permit slot swept in
//! `permit::live_records_in`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::sessions;
use super::state::{self, StateDir, create_private_dir_all, write_private};

fn current_schema_version() -> u32 {
    1
}

/// One admitted-but-unsettled delegation's expected token spend against a
/// provider. `pid`/`pid_start_time` name the PARENT process that admitted
/// the child and therefore owns settling or releasing this reservation
/// (never the delegated worker itself, which never sees this id) -- stamped
/// once by [`reserve`], the same "caller's own process, stamped at
/// acquisition" shape `permit::PermitRecord::pid` already holds.
/// `pid_start_time` is [`sessions::process_start_secs`]'s own reading at
/// reservation time, kept for a future finer-grained disambiguator the way
/// `sessions::Record::start_time` already is for session records; today's
/// liveness check ([`outstanding`]) is deliberately the plainer
/// permit-style `sessions::is_alive(pid)` alone, so this field is not yet
/// consulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: String,
    pub session: String,
    pub pid: u32,
    #[serde(default)]
    pub pid_start_time: Option<u64>,
    pub tokens: u64,
    pub created_at: u64,
}

/// The whole on-disk ledger for one provider. `#[serde(default)]` on both
/// fields (and on the struct as a whole via [`Ledger::default`]) so a
/// missing or freshly created file, and a future build's extra fields, both
/// round-trip the same tolerant way every other state-dir record in this
/// codebase does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<Reservation>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            schema_version: current_schema_version(),
            entries: Vec::new(),
        }
    }
}

fn ledger_path(state: &StateDir, provider: &str) -> PathBuf {
    state
        .reservations()
        .join(format!("{}.json", state::provider_slug(provider)))
}

fn lock_path(state: &StateDir, provider: &str) -> PathBuf {
    state
        .reservations()
        .join(format!("{}.lock", state::provider_slug(provider)))
}

/// One advisory OS lock per provider ledger, mirroring `group.rs`'s own
/// per-group lock file exactly (same `open_lock_file`, reused directly
/// rather than reimplemented, and the same "leave the file behind on drop"
/// reasoning: deleting it can split two contenders across old and new
/// inodes, while an unlocked empty file is harmless).
struct ReservationLock(std::fs::File);

impl Drop for ReservationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock_ledger(state: &StateDir, provider: &str) -> CtxResult<ReservationLock> {
    create_private_dir_all(&state.reservations())?;
    let file = super::group::open_lock_file(&lock_path(state, provider))?;
    file.lock()?;
    Ok(ReservationLock(file))
}

/// A missing or unparseable file both read as an empty ledger -- a file that
/// fails to parse is left on disk (a caller cannot tell "never existed"
/// apart from "malformed" anyway, and reading must never destroy state to
/// make itself succeed), matching `group::load`'s own tolerance.
fn load(state: &StateDir, provider: &str) -> Ledger {
    let Ok(contents) = std::fs::read_to_string(ledger_path(state, provider)) else {
        return Ledger::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn save(state: &StateDir, provider: &str, ledger: &Ledger) -> CtxResult<()> {
    create_private_dir_all(&state.reservations())?;
    let json = serde_json::to_string_pretty(ledger)?;
    write_private(&ledger_path(state, provider), &json)?;
    Ok(())
}

/// Permit-style liveness: alive exactly when the owning process still is,
/// per `sessions::is_alive`'s bare signal-0 (Unix) / `OpenProcess` (Windows)
/// probe -- the same check `permit::permit_record_is_alive` bases its own
/// parent-pid half on. No `sessions::Record` exists for a reservation's
/// owner, so the fuller start-time disambiguation `sessions::record_is_alive`
/// offers is not available here.
fn is_owner_alive(entry: &Reservation) -> bool {
    sessions::is_alive(entry.pid)
}

/// Drops every entry whose owner is no longer alive. Called at the top of
/// every locked mutation below, so a dead owner's reservation is swept the
/// next time this provider's ledger is written for any reason -- mirroring
/// `permit::live_records_in`'s own sweep-on-read, just deferred to the next
/// WRITE rather than every read, since [`outstanding`]/[`entries`] are
/// read-only and must not take the lock just to prune.
fn prune_dead(ledger: &mut Ledger) {
    ledger.entries.retain(is_owner_alive);
}

/// Admits one more expected token spend against `provider`'s ledger,
/// stamping the calling process's own pid and start time -- the parent that
/// admitted the child, exactly as `permit::acquire`'s `PermitRecord::pid`
/// names the process that called it, never the delegated worker. Zero-token
/// reservations are valid and persisted like any other: an unbounded budget
/// still occupies a slot in the ledger, the same way `group::admit_child`
/// still increments `admitted_children` for a child with no group ceiling.
pub fn reserve(
    state: &StateDir,
    provider: &str,
    session: &str,
    tokens: u64,
    now: u64,
) -> CtxResult<Reservation> {
    let _lock = lock_ledger(state, provider)?;
    let mut ledger = load(state, provider);
    prune_dead(&mut ledger);
    let pid = std::process::id();
    let reservation = Reservation {
        id: uuid::Uuid::new_v4().to_string(),
        session: session.to_string(),
        pid,
        pid_start_time: sessions::process_start_secs(pid),
        tokens,
        created_at: now,
    };
    ledger.entries.push(reservation.clone());
    save(state, provider, &ledger)?;
    Ok(reservation)
}

/// Finding #11 (issue #358 review): like [`reserve`], but atomic against a
/// CONCURRENT reservation on the same provider ledger. Placement (`agent::
/// run_with`'s own routing, `dash::fulfill_spawn_request`) is computed
/// against a `CapacitySnapshot` taken BEFORE this call, outside any lock --
/// two admissions racing the same provider can both read "room enough" from
/// that stale snapshot and both call [`reserve`], jointly over-committing
/// it. Here the "is there room" check and the reservation itself happen
/// under the SAME lock acquisition (the one [`lock_ledger`] already
/// serializes every mutation through), so only one of two racing callers
/// against a tight `limit_tokens` can ever win.
///
/// `limit_tokens` is the caller's own ceiling -- typically the provider's
/// projected headroom for its binding window, converted to a raw token
/// count via that window's configured budget (`pace.five_hour_budget_
/// tokens`/`seven_day_budget_tokens`) -- checked as `outstanding + tokens <=
/// limit`. `None` disables the check entirely (no configured budget to
/// convert headroom against, or a caller that does not want one), in which
/// case this behaves exactly like [`reserve`] wrapped in `Ok`.
///
/// Returns `Ok(Ok(reservation))` on success, or `Ok(Err(outstanding))` --
/// the ledger's own current outstanding total, for the caller to report or
/// retry a placement against -- when admitting `tokens` would push the
/// ledger over `limit_tokens`. Nothing is written to the ledger on the
/// `Err` branch; the caller is expected to retry placement excluding this
/// provider and reserve there instead, never to refuse the delegation
/// outright.
pub fn reserve_within(
    state: &StateDir,
    provider: &str,
    session: &str,
    tokens: u64,
    limit_tokens: Option<u64>,
    now: u64,
) -> CtxResult<Result<Reservation, u64>> {
    let _lock = lock_ledger(state, provider)?;
    let mut ledger = load(state, provider);
    prune_dead(&mut ledger);
    let outstanding = ledger
        .entries
        .iter()
        .fold(0u64, |sum, entry| sum.saturating_add(entry.tokens));
    if let Some(limit) = limit_tokens
        && outstanding.saturating_add(tokens) > limit
    {
        return Ok(Err(outstanding));
    }
    let pid = std::process::id();
    let reservation = Reservation {
        id: uuid::Uuid::new_v4().to_string(),
        session: session.to_string(),
        pid,
        pid_start_time: sessions::process_start_secs(pid),
        tokens,
        created_at: now,
    };
    ledger.entries.push(reservation.clone());
    save(state, provider, &ledger)?;
    Ok(Ok(reservation))
}

/// Removes `id`'s reservation exactly once, returning the tokens it had
/// reserved -- `Ok(None)` both for an id that never existed and for one
/// already settled/released, so a caller cannot double-count a reservation
/// it (or a racing rollback) already resolved.
///
/// `actual_tokens` mirrors `group::settle_reservation`'s own `(reserved,
/// actual)` pair -- call sites settle both the group's reservation and this
/// one with the identical figures in the same breath -- but this ledger
/// tracks only OUTSTANDING reservations, not a running settled total per
/// provider, so the actual figure has nothing to roll up into here beyond
/// removing the entry.
pub fn settle(
    state: &StateDir,
    provider: &str,
    id: &str,
    actual_tokens: u64,
) -> CtxResult<Option<u64>> {
    let _ = actual_tokens;
    let _lock = lock_ledger(state, provider)?;
    let mut ledger = load(state, provider);
    prune_dead(&mut ledger);
    let Some(pos) = ledger.entries.iter().position(|entry| entry.id == id) else {
        save(state, provider, &ledger)?;
        return Ok(None);
    };
    let removed = ledger.entries.remove(pos);
    save(state, provider, &ledger)?;
    Ok(Some(removed.tokens))
}

/// Removes `id`'s reservation without rolling anything up -- the launch it
/// was reserved for never actually happened. `Ok(true)` when an entry was
/// actually removed, `Ok(false)` for an unknown or already-resolved id, the
/// same "tell the caller whether anything changed" contract [`settle`]
/// gives via its `Option`.
pub fn release(state: &StateDir, provider: &str, id: &str) -> CtxResult<bool> {
    let _lock = lock_ledger(state, provider)?;
    let mut ledger = load(state, provider);
    prune_dead(&mut ledger);
    let before = ledger.entries.len();
    ledger.entries.retain(|entry| entry.id != id);
    let removed = ledger.entries.len() != before;
    save(state, provider, &ledger)?;
    Ok(removed)
}

/// The sum of every live reservation's tokens against `provider` right now
/// -- a dead owner's entry is excluded from the sum here but only actually
/// swept from disk on the next locked write ([`prune_dead`]'s own doc
/// comment), since this is a read and must not take the lock just to prune.
/// `now` is a parameter, not `state::now_secs()`, the same testable seam
/// `group::is_overdue`/`admit_child` already take one for, even though
/// today's liveness check ([`is_owner_alive`]) needs no age comparison of
/// its own.
///
/// Read by `fallback::capacity_snapshot`'s own per-provider builder (issue
/// #358, task T5), which is the single place a machine-wide capacity picture
/// is assembled.
pub fn outstanding(state: &StateDir, provider: &str, now: u64) -> u64 {
    let _ = now;
    load(state, provider)
        .entries
        .iter()
        .filter(|entry| is_owner_alive(entry))
        .fold(0u64, |sum, entry| sum.saturating_add(entry.tokens))
}

/// Every reservation currently on disk for `provider`, unfiltered by
/// liveness -- for `status`-style callers that want to show a dead-owner
/// entry too (an operator diagnosing why a slot has not yet freed) rather
/// than only the subset [`outstanding`] counts. Not yet called from any
/// non-test code, the same forward-declared shape as [`outstanding`].
#[allow(dead_code)]
pub fn entries(state: &StateDir, provider: &str) -> Vec<Reservation> {
    load(state, provider).entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ledger_path(state: &StateDir, provider: &str) -> PathBuf {
        ledger_path(state, provider)
    }

    #[test]
    fn reserve_then_outstanding_equals_the_reserved_tokens() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let reservation =
            reserve(&state, "claude", "sess-a", 1_000, 1_700_000_000).expect("reserve");

        assert_eq!(reservation.session, "sess-a");
        assert_eq!(reservation.tokens, 1_000);
        assert_eq!(reservation.pid, std::process::id());
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 1_000);
    }

    #[test]
    fn settling_a_reservation_removes_it_exactly_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let reservation =
            reserve(&state, "claude", "sess-a", 5_000, 1_700_000_000).expect("reserve");

        let settled = settle(&state, "claude", &reservation.id, 4_200).expect("settle");
        assert_eq!(settled, Some(5_000), "the reserved amount is returned");
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 0);

        let settled_again = settle(&state, "claude", &reservation.id, 100).expect("settle again");
        assert_eq!(
            settled_again, None,
            "a second settle of the same id is a no-op"
        );
        assert_eq!(
            outstanding(&state, "claude", 1_700_000_100),
            0,
            "outstanding is unchanged by the second settle"
        );
    }

    #[test]
    fn release_removes_a_reservation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let reservation =
            reserve(&state, "codex", "sess-b", 2_000, 1_700_000_000).expect("reserve");

        assert!(release(&state, "codex", &reservation.id).expect("release"));
        assert_eq!(outstanding(&state, "codex", 1_700_000_100), 0);
        assert!(
            !release(&state, "codex", &reservation.id).expect("release again"),
            "releasing an already-released id reports nothing changed"
        );
    }

    /// A reservation whose owning process is dead is excluded from
    /// `outstanding` immediately, and physically removed from the file the
    /// next time this provider's ledger is locked for a write.
    #[test]
    fn a_dead_owner_reservation_is_excluded_and_pruned_on_the_next_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dead_pid = super::super::testenv::dead_pid();

        {
            let _lock = lock_ledger(&state, "claude").expect("lock");
            let mut ledger = load(&state, "claude");
            ledger.entries.push(Reservation {
                id: "dead-1".to_string(),
                session: "sess-dead".to_string(),
                pid: dead_pid,
                pid_start_time: Some(1),
                tokens: 9_999,
                created_at: 1_700_000_000,
            });
            save(&state, "claude", &ledger).expect("seed dead entry");
        }

        assert_eq!(
            outstanding(&state, "claude", 1_700_000_100),
            0,
            "a dead owner's reservation must not count"
        );
        assert_eq!(
            entries(&state, "claude").len(),
            1,
            "outstanding alone must not prune the file"
        );

        // Any locked write prunes it.
        reserve(&state, "claude", "sess-live", 10, 1_700_000_200).expect("reserve");
        let remaining = entries(&state, "claude");
        assert_eq!(remaining.len(), 1, "the dead entry is gone after a write");
        assert_eq!(remaining[0].session, "sess-live");
    }

    #[test]
    fn two_providers_are_independent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        reserve(&state, "claude", "sess-a", 100, 1_700_000_000).expect("reserve claude");
        reserve(&state, "codex", "sess-b", 200, 1_700_000_000).expect("reserve codex");

        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 100);
        assert_eq!(outstanding(&state, "codex", 1_700_000_100), 200);
        assert_ne!(
            sample_ledger_path(&state, "claude"),
            sample_ledger_path(&state, "codex")
        );
    }

    /// `state::provider_slug` folds case and separators, so two harness
    /// names that map to the same underlying provider slug share a ledger --
    /// e.g. two adapter labels that both slug down to the same provider.
    #[test]
    fn two_provider_spellings_that_share_a_slug_share_the_ledger() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        reserve(&state, "Anthropic", "sess-a", 50, 1_700_000_000).expect("reserve");
        reserve(&state, "anthropic", "sess-b", 75, 1_700_000_000).expect("reserve");

        assert_eq!(
            outstanding(&state, "ANTHROPIC", 1_700_000_100),
            125,
            "every spelling folds to the same slug and therefore the same ledger"
        );
    }

    /// Finding-B1-style race (see `group.rs`'s own concurrent admission
    /// test): 8 threads racing `reserve` against the same provider must each
    /// get their own distinct entry, and the ledger's own sum must be exact
    /// -- no reservation silently lost to a concurrent load-modify-write.
    #[test]
    fn concurrent_reserve_from_eight_threads_yields_eight_distinct_entries_and_the_exact_sum() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        const THREADS: u64 = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS as usize));

        let ids: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let state = state.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        reserve(
                            &state,
                            "claude",
                            &format!("sess-{i}"),
                            10 + i,
                            1_700_000_000,
                        )
                        .expect("reserve")
                        .id
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("racer thread must not panic"))
                .collect()
        });

        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            THREADS as usize,
            "every reservation id is distinct"
        );
        assert_eq!(entries(&state, "claude").len(), THREADS as usize);

        let expected_sum: u64 = (0..THREADS).map(|i| 10 + i).sum();
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), expected_sum);
    }

    #[test]
    fn a_zero_token_reservation_still_counts_as_an_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let reservation = reserve(&state, "claude", "sess-a", 0, 1_700_000_000).expect("reserve");

        assert_eq!(entries(&state, "claude").len(), 1);
        assert_eq!(
            outstanding(&state, "claude", 1_700_000_100),
            0,
            "zero tokens sum to zero, but the slot is still occupied"
        );
        assert!(
            settle(&state, "claude", &reservation.id, 0)
                .expect("settle")
                .is_some()
        );
    }

    #[test]
    fn a_missing_ledger_file_reads_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 0);
        assert!(entries(&state, "claude").is_empty());
    }

    /// A ledger written before `schema_version`/`pid_start_time` existed
    /// still deserialises, defaulting to the current schema version and no
    /// start time -- the same `#[serde(default)]` back-compat contract every
    /// other state-dir record in this codebase holds.
    #[test]
    fn an_older_ledger_shape_still_deserialises() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create_private_dir_all(&state.reservations()).expect("mkdir");
        let old =
            r#"{"entries":[{"id":"r-1","session":"sess-a","pid":123,"tokens":10,"created_at":1}]}"#;
        state::write_private(&ledger_path(&state, "claude"), old).expect("write old shape");

        let ledger = load(&state, "claude");
        assert_eq!(ledger.schema_version, 1);
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].pid_start_time, None);
    }

    #[test]
    fn reserve_within_admits_under_the_limit_and_refuses_over_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let first = reserve_within(&state, "claude", "sess-a", 60, Some(100), 1_700_000_000)
            .expect("reserve_within")
            .expect("60 of 100 fits");
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 60);

        let refused = reserve_within(&state, "claude", "sess-b", 60, Some(100), 1_700_000_000)
            .expect("reserve_within");
        assert_eq!(
            refused,
            Err(60),
            "60 + 60 exceeds the 100-token limit; the outstanding total is reported, nothing \
             written"
        );
        assert_eq!(
            outstanding(&state, "claude", 1_700_000_100),
            60,
            "a refused reserve_within must not mutate the ledger"
        );

        let second = reserve_within(&state, "claude", "sess-c", 40, Some(100), 1_700_000_000)
            .expect("reserve_within")
            .expect("60 + 40 == 100, exactly at the limit, still fits");
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 100);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn reserve_within_with_no_limit_never_refuses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        reserve_within(&state, "claude", "sess-a", 1_000_000, None, 1_700_000_000)
            .expect("reserve_within")
            .expect("no limit means no refusal, however large");
        assert_eq!(outstanding(&state, "claude", 1_700_000_100), 1_000_000);
    }

    /// Finding #11 (issue #358 review): the actual race the whole function
    /// exists to close. 8 threads each try to reserve 60% of a shared
    /// 100-token limit on the same provider at once -- only ONE can ever
    /// fit (60 + 60 > 100), so at most one of the 8 may succeed. A bare
    /// `outstanding()` read followed by a separate `reserve()` call would
    /// let several of these racers all read "40 tokens of room" against the
    /// stale pre-race total and all admit, jointly blowing through the
    /// limit -- exactly `group.rs`'s own Finding-B1 admission race, one
    /// level up.
    #[test]
    fn concurrent_reserve_within_eight_threads_admits_at_most_one_of_eight_racers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        const THREADS: u64 = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS as usize));

        let outcomes: Vec<Result<Reservation, u64>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let state = state.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        reserve_within(
                            &state,
                            "claude",
                            &format!("sess-{i}"),
                            60,
                            Some(100),
                            1_700_000_000,
                        )
                        .expect("reserve_within")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("racer thread must not panic"))
                .collect()
        });

        let admitted = outcomes.iter().filter(|o| o.is_ok()).count();
        assert_eq!(
            admitted, 1,
            "60-token reservations against a 100-token limit: at most one of eight racers may \
             ever fit, and the barrier guarantees at least one tries"
        );
        assert_eq!(
            outstanding(&state, "claude", 1_700_000_100),
            60,
            "the ledger's own total must match exactly the one admitted reservation"
        );
        assert_eq!(
            entries(&state, "claude").len(),
            1,
            "no refused racer may have left a stray entry behind"
        );
    }
}
