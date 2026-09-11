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

use super::config::CtxConfig;
use super::health::{
    self, Admission, HealthPolicy, LatencySample, Observed, Phase, RouteHealth, RouteKey,
    Transition,
};
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

/// Folds one poll's dated turn outcomes into `key`'s record. Unlike
/// [`record_success_and_persist`], this DOES create a record for a route that
/// has none: the rolling error rate needs its denominator from the first
/// healthy turn onwards, or a route's first three failures would read as a
/// 100% error rate.
pub fn record_samples_and_persist(
    state: &StateDir,
    key: &RouteKey,
    successes: &[u64],
    latencies: &[LatencySample],
    model: Option<&str>,
    now: u64,
    policy: &HealthPolicy,
) -> Option<Transition> {
    // Finding 9: a poll with no samples of its own still has to reach a
    // degrade verdict once a record exists -- its numerator is whatever the
    // errors folded a moment ago left behind. Only a route with no record at
    // all and nothing to add is skipped.
    if successes.is_empty() && latencies.is_empty() && !record_path(state, key).exists() {
        return None;
    }
    let _lock = lock_route(state, key).ok()?;
    let current = load(state, key, now);
    let (next, transition) = health::record_samples(&current, successes, latencies, now, policy);
    if next != current {
        store(state, key, &next, model, now);
    }
    transition
}

/// Claims `harness`'s single half-open recovery trial for `claimant`.
///
/// Returns what the CALLER may do: `Trial` when it won the claim (and the
/// claim is now persisted), a `Deny` naming the current holder when another
/// caller got there first, and the ordinary [`harness_admission`] verdict for
/// every other phase -- so a caller can hand the result straight to the same
/// `denied()` check it already applies.
///
/// Never blocks. A contended lock is a DENIAL, not a pass-through
/// (finding 3): the holder is in the middle of its own read-modify-write, so
/// an unlocked read can still see an unclaimed `HalfOpen` that is about to
/// be claimed -- returning `Trial` on that reading handed out a second
/// trial with no claim persisted anywhere, which is the exact race this
/// whole mechanism exists to prevent. The caller re-plans or refuses
/// instead.
pub fn claim_trial(
    state: &StateDir,
    harness: &str,
    claimant: &str,
    now: u64,
    policy: &HealthPolicy,
) -> Admission {
    if !policy.enabled {
        return Admission::Allow;
    }
    let key = RouteKey::new(harness);
    let Ok(_lock) = lock_route(state, &key) else {
        return named(
            harness,
            Admission::Deny {
                reason: "half-open, health record busy; trial not claimed".to_string(),
            },
        );
    };
    let current = load(state, &key, now);
    let (next, verdict, transition) = health::claim(&current, claimant, now, policy);
    if next != current {
        store(state, &key, &next, None, now);
    }
    if let Some(transition) = &transition {
        log_transition(state, &key, transition, now, claimant, "route-health");
    }
    named(harness, verdict)
}

/// The configured endpoint host two harnesses would SHARE, `None` when this
/// harness runs on its own native account. A native default account is not a
/// shared dependency: zirv knows nothing about which vendor estate is behind
/// it, and inventing one would let an Anthropic outage deny a codex route.
pub fn dependency_of(cfg: &CtxConfig, harness: &str) -> Option<String> {
    let target = match harness.to_lowercase().as_str() {
        "claude" => cfg.endpoint.claude.as_ref(),
        "codex" => cfg.endpoint.codex.as_ref(),
        _ => None,
    }?;
    health::dependency_from_base_url(&target.base_url)
}

/// Every named harness's admission at once, with shared-endpoint denials
/// folded in.
///
/// A route open on a TRANSPORT or SERVER failure says its endpoint host is
/// down, and an operator who pointed both harnesses at one gateway has two
/// routes behind that one host. Sending the work to the sibling then just
/// buys a second failure, so the sibling is denied too, naming the route that
/// actually failed. `Auth` never propagates (a rejected credential is one
/// account's, not the host's), and neither does `RateLimit`, `Unavailable` or
/// a merely degraded route.
///
/// Keyed by the LOWER-CASED harness name; [`admission_for`] is the matching
/// lookup.
pub fn admissions(
    state: &StateDir,
    cfg: &CtxConfig,
    names: &[String],
    now: u64,
    policy: &HealthPolicy,
) -> std::collections::BTreeMap<String, Admission> {
    let mut verdicts: std::collections::BTreeMap<String, Admission> =
        std::collections::BTreeMap::new();
    if !policy.enabled {
        return names
            .iter()
            .map(|name| (name.to_lowercase(), Admission::Allow))
            .collect();
    }
    // One record read per harness, reused for both the verdict and the
    // shared-endpoint check below.
    let loaded: Vec<(String, RouteHealth)> = names
        .iter()
        .map(|name| (name.clone(), load(state, &RouteKey::new(name), now)))
        .collect();
    for (name, record) in &loaded {
        verdicts.insert(
            name.to_lowercase(),
            named(name, health::admission(record, now, policy)),
        );
    }
    let failing: Vec<(&str, String, String)> = loaded
        .iter()
        .filter_map(|(name, record)| {
            let Phase::Open { reason, class, .. } = &record.phase else {
                return None;
            };
            if !matches!(
                class,
                super::event::ProviderErrorClass::Transport
                    | super::event::ProviderErrorClass::Server
            ) {
                return None;
            }
            health::admission(record, now, policy).denied()?;
            let dependency = dependency_of(cfg, name)?;
            Some((name.as_str(), dependency, reason.clone()))
        })
        .collect();
    for (name, _) in &loaded {
        let key = name.to_lowercase();
        if verdicts.get(&key).is_some_and(|a| a.denied().is_some()) {
            continue;
        }
        let Some(dependency) = dependency_of(cfg, name) else {
            continue;
        };
        let Some((failed, _, reason)) = failing
            .iter()
            .find(|(failed, dep, _)| !failed.eq_ignore_ascii_case(name) && *dep == dependency)
        else {
            continue;
        };
        verdicts.insert(
            key,
            Admission::Deny {
                reason: format!("{name}: shares endpoint {dependency} with {failed} ({reason})"),
            },
        );
    }
    verdicts
}

/// One harness's verdict out of an [`admissions`] map, matched the same
/// case-insensitive way every other harness lookup in this codebase is.
pub fn admission_for(
    verdicts: &std::collections::BTreeMap<String, Admission>,
    harness: &str,
) -> Admission {
    verdicts
        .get(&harness.to_lowercase())
        .cloned()
        .unwrap_or_default()
}

/// Whether `harness`'s OWN route is shut -- an open breaker inside its
/// cooldown, or an unavailable one. Deliberately narrower than
/// [`harness_admission`]: a shared-endpoint denial belongs to another route
/// and an in-flight half-open trial belongs to another caller, and neither is
/// evidence that THIS seat's session cannot continue where it is. Only this
/// answer may trigger a rollover or relax the successor's hysteresis floor.
pub fn harness_block(
    state: &StateDir,
    harness: &str,
    now: u64,
    policy: &HealthPolicy,
) -> Option<String> {
    if !policy.enabled {
        return None;
    }
    let key = RouteKey::new(harness);
    let record = load(state, &key, now);
    if !matches!(record.phase, Phase::Open { .. } | Phase::Unavailable { .. }) {
        return None;
    }
    named(harness, health::admission(&record, now, policy))
        .denied()
        .map(str::to_string)
}

/// Prefixes a verdict's reason with the harness it is about: these reasons
/// travel into standalone human lines (`rollover`'s park message, `zirv ctx
/// status`'s exclusions) where nothing else names the route.
fn named(harness: &str, verdict: Admission) -> Admission {
    match verdict {
        Admission::Deny { reason } => Admission::Deny {
            reason: format!("{harness}: {reason}"),
        },
        Admission::Degraded { reason } => Admission::Degraded {
            reason: format!("{harness}: {reason}"),
        },
        other => other,
    }
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
    named(
        harness,
        health::admission(&load(state, &key, now), now, policy),
    )
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
/// `successes` and `latencies` are this poll's dated turn outcomes, the
/// rolling error rate's denominator and the optional first-token latency
/// signal. They need no `incremental` gate of their own: both carry the
/// row's OWN time, so a re-parse from offset 0 hands over samples that prune
/// straight back out of the window.
///
/// Exactly one decision-log line is appended per phase change -- never one
/// per observation.
#[allow(clippy::too_many_arguments)]
pub fn observe_poll(
    state: &StateDir,
    harness: &str,
    model: Option<&str>,
    errors: &[Observed],
    successes: &[u64],
    latencies: &[LatencySample],
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
    // Last, and unconditionally: this is the single per-poll evaluation of
    // the degrade rule (finding 9), judged against every piece of evidence
    // the three steps above folded -- this poll's failures, its successes and
    // whatever `record_success` just healed.
    if let Some(transition) =
        record_samples_and_persist(state, &key, successes, latencies, model, now, policy)
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

    fn endpoint_cfg(claude: Option<&str>, codex: Option<&str>) -> CtxConfig {
        let target = |base_url: &str| super::super::config::EndpointTarget {
            vendor: "zai".to_string(),
            base_url: base_url.to_string(),
            credential_env: "TOKEN".to_string(),
            model: Some("glm".to_string()),
            wire_api: None,
        };
        let mut cfg = CtxConfig::default();
        cfg.endpoint.claude = claude.map(target);
        cfg.endpoint.codex = codex.map(target);
        cfg
    }

    fn open_on(state: &StateDir, harness: &str, class: ProviderErrorClass, now: u64) {
        let key = RouteKey::new(harness);
        let policy = HealthPolicy::default();
        for at in [now - 20, now - 10, now] {
            observe_and_persist(
                state,
                &key,
                &Observed::new(class, Some(at), Some(format!("{harness}-{at}"))),
                None,
                at,
                &policy,
            );
        }
    }

    /// Slice B: an open TRANSPORT breaker on one harness denies the sibling
    /// pointed at the same configured endpoint host -- sending the work there
    /// only buys a second failure.
    #[test]
    fn an_open_transport_route_denies_the_harness_sharing_its_endpoint_host() {
        let (_guard, state) = state();
        let now = 2_000;
        let policy = HealthPolicy::default();
        let cfg = endpoint_cfg(
            Some("https://gateway.example.com/anthropic"),
            Some("https://user:secret@gateway.example.com/openai"),
        );
        open_on(&state, "claude", ProviderErrorClass::Transport, now);

        let names = vec!["claude".to_string(), "codex".to_string()];
        let verdicts = admissions(&state, &cfg, &names, now, &policy);
        let codex = admission_for(&verdicts, "codex");
        let reason = codex.denied().expect("the alias is denied too");
        assert!(
            reason.starts_with("codex: shares endpoint gateway.example.com with claude ("),
            "{reason}"
        );
        assert!(
            !reason.contains("secret"),
            "a dependency string never carries a credential: {reason}"
        );
    }

    #[test]
    fn independent_endpoints_and_native_accounts_never_share_a_denial() {
        let (_guard, state) = state();
        let now = 2_000;
        let policy = HealthPolicy::default();
        open_on(&state, "claude", ProviderErrorClass::Transport, now);
        let names = vec!["claude".to_string(), "codex".to_string()];

        let native = admissions(&state, &CtxConfig::default(), &names, now, &policy);
        assert_eq!(admission_for(&native, "codex"), Admission::Allow);

        let split = endpoint_cfg(
            Some("https://one.example.com/v1"),
            Some("https://two.example.com/v1"),
        );
        let verdicts = admissions(&state, &split, &names, now, &policy);
        assert_eq!(admission_for(&verdicts, "codex"), Admission::Allow);
    }

    #[test]
    fn an_auth_failure_never_propagates_to_a_shared_endpoint() {
        let (_guard, state) = state();
        let now = 2_000;
        let policy = HealthPolicy::default();
        let cfg = endpoint_cfg(
            Some("https://gateway.example.com/anthropic"),
            Some("https://gateway.example.com/openai"),
        );
        open_on(&state, "claude", ProviderErrorClass::Auth, now);

        let names = vec!["claude".to_string(), "codex".to_string()];
        let verdicts = admissions(&state, &cfg, &names, now, &policy);
        assert!(
            admission_for(&verdicts, "claude").denied().is_some(),
            "the rejected credential still denies its own route"
        );
        assert_eq!(
            admission_for(&verdicts, "codex"),
            Admission::Allow,
            "a rejected credential belongs to one account, not to the host"
        );
    }

    /// Slice B, finding 4: an alias denial is never a rollover trigger, and
    /// neither is a trial someone else holds -- `harness_block` is the seat's
    /// own route and nothing else. `rollover::evaluate` derives both
    /// `confirmed_block`'s health half and `source_unreachable` from exactly
    /// this call.
    #[test]
    fn harness_block_sees_only_the_routes_own_phase() {
        let (_guard, state) = state();
        let now = 2_000;
        let policy = HealthPolicy::default();
        let cfg = endpoint_cfg(
            Some("https://gateway.example.com/anthropic"),
            Some("https://gateway.example.com/openai"),
        );
        open_on(&state, "claude", ProviderErrorClass::Transport, now);

        assert!(harness_block(&state, "claude", now, &policy).is_some());
        assert_eq!(
            harness_block(&state, "codex", now, &policy),
            None,
            "the seat on the alias is still working; only its placements are denied"
        );
        let names = vec!["claude".to_string(), "codex".to_string()];
        assert!(
            admission_for(&admissions(&state, &cfg, &names, now, &policy), "codex")
                .denied()
                .is_some(),
            "which is exactly what makes the two answers different"
        );
    }

    #[test]
    fn harness_block_ignores_a_degraded_route_and_an_in_flight_trial() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();
        let key = RouteKey::new("claude");

        let degraded = RouteHealth {
            phase: Phase::Degraded {
                since: 1_000,
                reason: "error rate 30% over 10 turns".to_string(),
            },
            // Evidence inside the window: a degraded record with none left
            // admits normally again (`health::admission`).
            failures: vec![1_000],
            successes: vec![1_001],
            ..RouteHealth::default()
        };
        store(&state, &key, &degraded, None, 1_000);
        assert_eq!(harness_block(&state, "claude", 1_010, &policy), None);
        assert!(
            harness_admission(&state, "claude", 1_010, &policy)
                .degraded()
                .is_some()
        );

        let half_open = RouteHealth {
            phase: Phase::HalfOpen {
                since: 1_000,
                trial: Some(super::super::health::Trial {
                    claim: "sess-a".to_string(),
                    at: 1_000,
                }),
            },
            ..RouteHealth::default()
        };
        store(&state, &key, &half_open, None, 1_000);
        assert_eq!(harness_block(&state, "claude", 1_010, &policy), None);
        assert!(
            harness_admission(&state, "claude", 1_010, &policy)
                .denied()
                .is_some()
        );
    }

    /// Slice C: the first claimant gets the trial, the second is told who has
    /// it, and the slot frees itself once the cooldown elapses.
    #[test]
    fn claim_trial_admits_one_caller_and_denies_the_next_until_it_expires() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();
        let key = RouteKey::new("claude");
        store(
            &state,
            &key,
            &RouteHealth {
                phase: Phase::HalfOpen {
                    since: 1_000,
                    trial: None,
                },
                ..RouteHealth::default()
            },
            None,
            1_000,
        );

        assert_eq!(
            claim_trial(&state, "claude", "sess-a", 1_010, &policy),
            Admission::Trial
        );
        assert_eq!(
            claim_trial(&state, "claude", "sess-b", 1_020, &policy).denied(),
            Some(
                "claude: half-open: recovery trial already in flight (sess-a); next attempt in \
                 ~4m (estimate)"
            )
        );
        let expired = 1_010 + policy.cooldown_secs;
        assert_eq!(
            claim_trial(&state, "claude", "sess-b", expired, &policy),
            Admission::Trial
        );

        let log = std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE))
            .expect("decision log");
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\"health-trial\""))
                .count(),
            2,
            "one line per successful claim, never one per lost race: {log}"
        );
    }

    #[test]
    fn claim_trial_is_a_no_op_for_a_healthy_route() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();
        assert_eq!(
            claim_trial(&state, "claude", "sess-a", 1_000, &policy),
            Admission::Allow
        );
        assert!(!record_path(&state, &RouteKey::new("claude")).exists());
    }

    /// A record written before slices A-C existed still loads: no `class`, no
    /// `trial`, none of the three sample rings.
    #[test]
    fn a_legacy_record_without_the_new_fields_still_loads() {
        let (_guard, state) = state();
        let key = RouteKey::new("claude");
        super::super::state::create_private_dir_all(&dir(&state)).expect("mkdir");
        std::fs::write(
            record_path(&state, &key),
            r#"{
              "key": {"harness": "claude"},
              "model": "opus",
              "health": {
                "phase": {"phase": "open", "opened_at": 1000, "until": 1300,
                          "reason": "3 transport error(s) in 10m"},
                "observations": [],
                "seen_ids": []
              },
              "updated_at": 1000
            }"#,
        )
        .expect("write");

        let stored = load(&state, &key, 1_100);
        assert!(
            matches!(
                stored.phase,
                Phase::Open {
                    class: ProviderErrorClass::Other,
                    ..
                }
            ),
            "an unattributed legacy open never propagates to an alias: {stored:?}"
        );
        assert!(stored.successes.is_empty());
        assert!(stored.failures.is_empty());
        assert!(stored.latency.is_empty());

        let half_open = std::fs::read_to_string(record_path(&state, &key))
            .expect("read")
            .replace(
                r#""phase": "open", "opened_at": 1000, "until": 1300"#,
                r#""phase": "half-open", "since": 1000"#,
            );
        std::fs::write(record_path(&state, &key), half_open).expect("write");
        assert!(
            matches!(
                load(&state, &key, 1_100).phase,
                Phase::HalfOpen { trial: None, .. }
            ),
            "a legacy half-open record has no trial in flight"
        );
    }

    /// Slice A end to end: one poll's turn outcomes reach the record and
    /// degrade a route whose rolling error rate crossed the threshold.
    #[test]
    fn observe_poll_folds_turn_samples_and_degrades_a_flaky_route() {
        let (_guard, state) = state();
        let policy = HealthPolicy::default();
        let now = 2_000;
        let successes: Vec<u64> = (0..6).map(|i| 1_800 + i * 10).collect();

        observe_poll(
            &state,
            "claude",
            Some("opus"),
            &[transport(1_900), transport(1_910)],
            &successes,
            &[],
            false,
            true,
            now,
            &policy,
            "sess",
            "wrap",
        );

        let stored = load(&state, &RouteKey::new("claude"), now);
        assert!(matches!(stored.phase, Phase::Degraded { .. }), "{stored:?}");
        assert_eq!(stored.successes.len(), 6);
        assert_eq!(stored.failures.len(), 2);

        let log = std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE))
            .expect("decision log");
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\"health-degraded\""))
                .count(),
            1,
            "{log}"
        );
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
            &[],
            &[],
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
            &state,
            "claude",
            None,
            &undated,
            &[],
            &[],
            false,
            false,
            5_000,
            &policy,
            "sess",
            "exec",
        );
        assert_eq!(
            load(&state, &RouteKey::new("claude"), 5_000),
            RouteHealth::default(),
            "a full re-parse must record nothing it cannot date"
        );

        observe_poll(
            &state,
            "claude",
            None,
            &undated,
            &[],
            &[],
            false,
            true,
            5_000,
            &policy,
            "sess",
            "exec",
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
            &[],
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
