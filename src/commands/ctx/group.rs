//! Work-group state (issue #155, Phase 5(b)): a work group is the unit an
//! orchestrator actually reasons about -- this batch, this budget, this
//! contract -- replacing today's unit, which is "one process that happens
//! to be alive". Persisted so a child spawned minutes later, in another
//! process, can still find the terms it was launched under.
//!
//! Mirrors `sessions.rs`'s storage idioms: one JSON file per record under
//! the state dir, written via `create_private_dir_all` + `write_private`,
//! and a tolerant listing that skips a file that fails to parse rather than
//! failing the whole read.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::state::{StateDir, create_private_dir_all, now_secs, write_private};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkGroup {
    pub work_group_id: String,
    pub parent_session_id: String,
    pub scope: String,
    pub child_limit: u32,
    #[serde(default)]
    pub token_budget: Option<u64>,
    /// Completed child spend rolled up across this group's delegated work.
    /// Older records predate the meter and therefore start at zero.
    #[serde(default)]
    pub spent_tokens: u64,
    #[serde(default)]
    pub deadline_secs: Option<u64>,
    /// Display-only (issue #155 review finding D2): the terms every child
    /// must satisfy before the group can close, shown by `zirv ctx group
    /// status` and nothing else. Nothing parses or enforces it -- there is
    /// no verified, adapter-agnostic way to check a delegated worker's own
    /// transcript against free text, so this stays exactly what an operator
    /// or reviewing orchestrator reads, never a machine-checked gate.
    pub completion_contract: String,
    pub created_at: u64,
    #[serde(default)]
    pub closed_at: Option<u64>,
    /// How many children this group has admitted so far (issue #155 review
    /// finding D2). Monotonic -- a work group is a batch of at most
    /// `child_limit` total delegated tasks, not a live concurrency count, so
    /// this never decrements when a child finishes. Incremented by
    /// [`admit_child`], the sole place any admission is granted.
    #[serde(default)]
    pub admitted_children: u32,
    /// Issue #170: the session id of the SubOrchestrator this group is bound
    /// to -- first-claim-wins (`claim_sub_orchestrator`), so a group either
    /// belongs to no coordinator yet or to exactly one for its whole life.
    /// Drives two things: the group closes automatically once that session's
    /// own supervised run ends (`agent::run_with`'s completion path), and
    /// `is_abandoned` can tell "still open because the work continues" apart
    /// from "still open because its coordinator died before it could close
    /// this itself." `None` for a group with no coordinator claim yet -- a
    /// plain one-off batch of workers reporting straight to an Orchestrator,
    /// or a group written by an older build.
    #[serde(default)]
    pub sub_orchestrator_session: Option<String>,
}

fn record_path(state: &StateDir, id: &str) -> PathBuf {
    state.groups().join(format!("{id}.json"))
}

/// Writes a group's record. Matches `sessions::write_record`'s own
/// private-dir-then-atomic-write shape.
pub fn create(state: &StateDir, group: &WorkGroup) -> CtxResult<()> {
    create_private_dir_all(&state.groups())?;
    let json = serde_json::to_string_pretty(group)?;
    write_private(&record_path(state, &group.work_group_id), &json)?;
    Ok(())
}

/// `Ok(None)` both for a missing file and for one that fails to parse --
/// a caller cannot tell "never existed" apart from "malformed" anyway. A
/// file that fails to parse is left on disk: reading must never destroy an
/// operator's state to make itself succeed.
pub fn load(state: &StateDir, id: &str) -> CtxResult<Option<WorkGroup>> {
    let Ok(contents) = std::fs::read_to_string(record_path(state, id)) else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&contents).ok())
}

/// Every group currently on disk, newest first. A file that fails to parse
/// is skipped outright -- matching `sessions::list`'s own tolerance, one
/// malformed record must never fail the whole listing (a group written by a
/// future zirv with extra fields, or an older one with fewer, both still
/// round-trip via `#[serde(default)]`; only genuine corruption is skipped).
pub fn list(state: &StateDir) -> Vec<WorkGroup> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(state.groups()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(group) = serde_json::from_str::<WorkGroup>(&contents) else {
                continue;
            };
            found.push(group);
        }
    }
    found.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.work_group_id.cmp(&b.work_group_id))
    });
    found
}

/// Load-modify-write, idempotent. Leaves an already-set `closed_at` alone:
/// a closed group is evidence of what a batch was launched under, not a
/// tombstone, and the first close time is the one that stands.
pub fn close(state: &StateDir, id: &str, now: u64) -> CtxResult<()> {
    let Some(mut group) = load(state, id)? else {
        return Err(format!("no work group '{id}'").into());
    };
    if group.closed_at.is_none() {
        group.closed_at = Some(now);
        create(state, &group)?;
    }
    Ok(())
}

#[derive(Debug)]
struct AdmissionExhausted(String);

impl std::fmt::Display for AdmissionExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AdmissionExhausted {}

/// Whether an admission failed because the group's spend or deadline is
/// exhausted. Callers map both to the existing budget-exhausted exit.
pub fn is_admission_exhausted(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<AdmissionExhausted>()
}

/// The sole place a child is admitted into a group. Refuses exhausted token
/// budgets, elapsed deadlines, and groups already at their child limit before
/// incrementing the persisted count. Called at both real admission choke
/// points: `agent::resolve_worker_budget` for a headless delegation, and
/// `dash::fulfill_spawn_request` for a dashboard pane -- never both for the
/// same request (a headless run that first tries to join a live dashboard
/// admits nowhere in `agent.rs` itself; whichever side actually ends up
/// spawning is the one that calls this).
///
/// Load-modify-write, like every other mutation in this file
/// (`close` above) -- not a distributed lock. zirv's own callers are
/// mostly-sequential orchestrator activity, not a tight concurrent loop, so
/// a lost update in a genuine race would at worst admit one child past the
/// limit, never wrongly refuse legitimate work.
pub fn admit_child(state: &StateDir, id: &str, now: u64) -> CtxResult<()> {
    let Some(mut group) = load(state, id)? else {
        return Err(format!("no work group '{id}'").into());
    };
    if group
        .token_budget
        .is_some_and(|budget| group.spent_tokens >= budget)
    {
        return Err(Box::new(AdmissionExhausted(format!(
            "work group '{id}' token budget is exhausted ({} tokens spent)",
            group.spent_tokens
        ))));
    }
    if is_overdue(&group, now) {
        return Err(Box::new(AdmissionExhausted(format!(
            "work group '{id}' deadline has elapsed"
        ))));
    }
    if group.admitted_children >= group.child_limit {
        return Err(format!(
            "work group '{id}' already has its full {} children admitted",
            group.child_limit
        )
        .into());
    }
    group.admitted_children += 1;
    create(state, &group)?;
    Ok(())
}

/// Adds one completed child's token spend to its group. Best-effort callers
/// may ignore an I/O failure, but arithmetic never wraps and the same
/// load-modify-write race trade-off as admission applies.
pub fn add_spent_tokens(state: &StateDir, id: &str, spent: u64) -> CtxResult<()> {
    let Some(mut group) = load(state, id)? else {
        return Err(format!("no work group '{id}'").into());
    };
    group.spent_tokens = group.spent_tokens.saturating_add(spent);
    create(state, &group)
}

/// Re-review (2026-08-27) finding 1: rolls back one admission granted by
/// [`admit_child`] when the spawn/delegation it was granted for turns out to
/// fail before a child is ever actually launched -- both real choke points
/// (`agent::run_with`'s headless path, `dash::fulfill_spawn_request`'s pane
/// path) call this on every failure between a successful `admit_child` and
/// the point the child is definitely running. Saturating-decrements
/// `admitted_children`, never below zero.
///
/// Best-effort, unlike `admit_child`: logs to stderr and returns rather than
/// propagating a second error over the original spawn failure that triggered
/// the rollback -- an operator who already sees a spawn error must not also
/// see "the state file wouldn't decrement" stacked on top of it. Same
/// load-modify-write, non-locking pattern as `admit_child`/`close`: losing a
/// rollback to a race just leaves the count one high until the group is
/// inspected, never wrongly grants an extra admission.
pub fn rollback_admission(state: &StateDir, id: &str) {
    match load(state, id) {
        Ok(Some(mut group)) => {
            group.admitted_children = group.admitted_children.saturating_sub(1);
            if let Err(e) = create(state, &group) {
                eprintln!("zirv ctx: failed to roll back admission for work group '{id}': {e}");
            }
        }
        Ok(None) => {
            eprintln!("zirv ctx: could not roll back admission -- no work group '{id}'");
        }
        Err(e) => {
            eprintln!("zirv ctx: failed to roll back admission for work group '{id}': {e}");
        }
    }
}

/// Security review round 2 (Finding 4): removes a group record that was
/// minted for a delegation which then never started. `agent::resolve_group_
/// binding` mints a `--scope` group before the refusal gates downstream of it
/// have all been passed (a dashboard that refuses the spawn, a budget that
/// cannot be resolved, a launch that fails outright), and each such refusal
/// used to leave an open, unclaimed, childless group on disk forever.
///
/// Only a PRISTINE group is ever removed -- no admitted child, no coordinator
/// claim, not already closed -- so anything that did in fact start under it
/// keeps its record: a dashboard that spawned the pane admits and claims
/// before this side's ack can time out, which is exactly the ambiguous case
/// this guard exists for. Returns whether the record was removed, and is
/// best-effort otherwise: the delegation being unwound is the caller's real
/// news, never this.
pub fn discard_if_unused(state: &StateDir, id: &str) -> bool {
    let Ok(Some(group)) = load(state, id) else {
        return false;
    };
    if group.admitted_children > 0
        || group.sub_orchestrator_session.is_some()
        || group.closed_at.is_some()
    {
        return false;
    }
    std::fs::remove_file(record_path(state, id)).is_ok()
}

/// `now` is a parameter, not `state::now_secs()`, so both the status marker
/// and admission gate stay deterministic in tests. A closed group is never
/// overdue: it is evidence of what a batch finished under, not a still-running
/// one a deadline can still be missed by.
pub fn is_overdue(group: &WorkGroup, now: u64) -> bool {
    group.closed_at.is_none()
        && group
            .deadline_secs
            .is_some_and(|deadline| now > group.created_at.saturating_add(deadline))
}

/// Issue #170: binds `group` to `session` as its SubOrchestrator, first-
/// claim-wins. A no-op (`Ok`, unchanged) when the group is already claimed --
/// by `session` itself (idempotent: a coordinator that resolves its own
/// already-bound group again must not error) or by a different one (the
/// group already belongs to whichever session claimed it first; a second
/// claimant simply never becomes the one `agent::run_with` auto-closes it
/// for). Load-modify-write, like every other mutation in this file.
pub fn claim_sub_orchestrator(state: &StateDir, id: &str, session: &str) -> CtxResult<()> {
    let Some(mut group) = load(state, id)? else {
        return Err(format!("no work group '{id}'").into());
    };
    if group.sub_orchestrator_session.is_none() {
        group.sub_orchestrator_session = Some(session.to_string());
        create(state, &group)?;
    }
    Ok(())
}

/// Issue #170: an open group whose claimed SubOrchestrator (`sub_
/// orchestrator_session`) is no longer alive -- its own session ended
/// (crashed, was killed, or the process otherwise vanished) without ever
/// reaching `agent::run_with`'s own completion path, which is what closes a
/// group it claimed under ordinary circumstances. `alive` is supplied by the
/// caller (`sessions::list`'s own liveness, the same the registry already
/// computes) rather than resolved here, mirroring `is_overdue`'s own
/// "caller supplies `now`" testability shape -- this module has no reason to
/// depend on `sessions.rs` for a process-liveness check. A group with no
/// claim yet is never abandoned: nothing has failed to close it, because
/// nothing has claimed responsibility for closing it.
pub fn is_abandoned(group: &WorkGroup, claimant_alive: bool) -> bool {
    group.closed_at.is_none() && group.sub_orchestrator_session.is_some() && !claimant_alive
}

#[derive(Debug, clap::Args)]
pub struct GroupArgs {
    #[command(subcommand)]
    pub command: GroupVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum GroupVerb {
    /// Open a work group: a scope, a child limit, and the contract every
    /// child must satisfy before the group can close.
    Create(CreateArgs),
    /// Show one group (or every group) and its terms.
    Status(StatusArgs),
    /// Close a group. Idempotent.
    Close(CloseArgs),
}

/// `CreateArgs::child_limit`'s own default -- also what `agent::
/// resolve_group_binding` mints a scope-bound group with (issue #170), so
/// the two never drift on what "unstated" means.
pub const DEFAULT_CHILD_LIMIT: u32 = 3;
/// `CreateArgs::completion_contract`'s own default, shared with `agent::
/// resolve_group_binding` for the same reason as [`DEFAULT_CHILD_LIMIT`].
pub const DEFAULT_COMPLETION_CONTRACT: &str =
    "report a compact structured result by mail to the requesting session";

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// What this group of delegated work is for.
    pub scope: String,
    #[arg(long, default_value_t = DEFAULT_CHILD_LIMIT)]
    pub child_limit: u32,
    #[arg(long)]
    pub token_budget: Option<u64>,
    #[arg(long)]
    pub deadline_secs: Option<u64>,
    #[arg(long, default_value = DEFAULT_COMPLETION_CONTRACT)]
    pub completion_contract: String,
    #[arg(long)]
    pub parent_session: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Show only this group; omit to list every group.
    pub work_group_id: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct CloseArgs {
    pub work_group_id: String,
}

/// Mints the id every `zirv agent --group` invocation for this batch must
/// carry, writes the record, and prints the id -- `state`, the writer and
/// `now` all arrive as parameters rather than being resolved here, the same
/// testable seam `usage::run_with` already uses.
pub fn run_create<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &CreateArgs,
    now: u64,
) -> CtxResult<String> {
    let id = format!("wg-{}", uuid::Uuid::new_v4());
    let group = WorkGroup {
        work_group_id: id.clone(),
        parent_session_id: args.parent_session.clone().unwrap_or_default(),
        scope: args.scope.clone(),
        child_limit: args.child_limit,
        token_budget: args.token_budget,
        spent_tokens: 0,
        deadline_secs: args.deadline_secs,
        completion_contract: args.completion_contract.clone(),
        created_at: now,
        closed_at: None,
        admitted_children: 0,
        sub_orchestrator_session: None,
    };
    create(state, &group)?;
    writeln!(w, "work group {id} created (scope: {})", group.scope)?;
    Ok(id)
}

/// `now` is a parameter (not resolved here) so a caller can render a
/// deterministic overdue marker in a test -- the same seam `run_create`/
/// `run_close` already take one for.
/// Issue #170: whether the session named by `short` (`sessions::Record::
/// short`) is currently live, per the same registry `zirv ctx status`
/// already reads. A short id with no matching record at all (its own file
/// swept, or never written) reads as not-alive -- there is nothing left to
/// call live.
fn short_id_is_alive(state: &StateDir, short: &str) -> bool {
    super::sessions::list(state)
        .into_iter()
        .any(|(record, liveness)| {
            record.short == short && liveness == super::sessions::Liveness::Live
        })
}

fn print_group<W: Write>(
    w: &mut W,
    group: &WorkGroup,
    now: u64,
    state: &StateDir,
) -> CtxResult<()> {
    let status = if group.closed_at.is_some() {
        "closed"
    } else {
        "open"
    };
    write!(
        w,
        "{} [{status}] scope=\"{}\" child_limit={} admitted={} parent={}",
        group.work_group_id,
        group.scope,
        group.child_limit,
        group.admitted_children,
        group.parent_session_id
    )?;
    if let Some(sub) = &group.sub_orchestrator_session {
        write!(w, " sub-orchestrator={sub}")?;
    }
    // Status only marks the elapsed deadline. Admission enforcement lives in
    // `admit_child`; neither path kills or restarts running work.
    if is_overdue(group, now) {
        write!(w, " OVERDUE")?;
    }
    // Issue #170: same display-only spirit -- an abandoned group is not
    // acted on here, only named, so an operator scanning `zirv ctx status`
    // can tell "still open because the work continues" apart from "still
    // open because its coordinator died before it could close this itself".
    if let Some(sub) = &group.sub_orchestrator_session
        && is_abandoned(group, short_id_is_alive(state, sub))
    {
        write!(w, " ABANDONED")?;
    }
    writeln!(w)?;
    Ok(())
}

pub fn run_status<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &StatusArgs,
    now: u64,
) -> CtxResult<i32> {
    match &args.work_group_id {
        Some(id) => match load(state, id)? {
            Some(group) => {
                print_group(w, &group, now, state)?;
                Ok(0)
            }
            None => {
                writeln!(w, "no work group '{id}'")?;
                Ok(1)
            }
        },
        None => {
            let groups = list(state);
            if groups.is_empty() {
                writeln!(w, "no work groups")?;
                return Ok(0);
            }
            for group in &groups {
                print_group(w, group, now, state)?;
            }
            Ok(0)
        }
    }
}

pub fn run_close<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &CloseArgs,
    now: u64,
) -> CtxResult<i32> {
    close(state, &args.work_group_id, now)?;
    writeln!(w, "closed work group {}", args.work_group_id)?;
    Ok(0)
}

pub fn run<W: Write>(args: &GroupArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let state = StateDir::resolve(&env)?;
    let now = now_secs();
    match &args.command {
        GroupVerb::Create(a) => {
            run_create(&state, w, a, now)?;
            Ok(0)
        }
        GroupVerb::Status(a) => run_status(&state, w, a, now),
        GroupVerb::Close(a) => run_close(&state, w, a, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_group(id: &str) -> WorkGroup {
        WorkGroup {
            work_group_id: id.to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: "phase 5 implementation".to_string(),
            child_limit: 3,
            token_budget: Some(400_000),
            spent_tokens: 0,
            deadline_secs: Some(3_600),
            completion_contract: "every child reports a compact result by mail".to_string(),
            created_at: 1_700_000_000,
            closed_at: None,
            admitted_children: 0,
            sub_orchestrator_session: None,
        }
    }

    /// Issue #155, Phase 5(b): a work group is the unit an orchestrator
    /// actually reasons about -- this batch, this budget, this contract --
    /// replacing today's unit, which is "one process that happens to be
    /// alive". Persisted so a child spawned minutes later, in another
    /// process, can still find the terms it was launched under.
    #[test]
    fn a_work_group_round_trips_through_state_and_lists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let group = WorkGroup {
            work_group_id: "wg-1".to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: "phase 5 implementation".to_string(),
            child_limit: 3,
            token_budget: Some(400_000),
            spent_tokens: 0,
            deadline_secs: Some(3_600),
            completion_contract: "every child reports a compact result by mail".to_string(),
            created_at: 1_700_000_000,
            closed_at: None,
            admitted_children: 0,
            sub_orchestrator_session: None,
        };
        create(&state, &group).expect("create");

        assert_eq!(load(&state, "wg-1").expect("load"), Some(group.clone()));
        assert_eq!(list(&state).len(), 1);
        assert_eq!(
            load(&state, "nope").expect("load"),
            None,
            "unknown id is None, not an error"
        );
    }

    #[test]
    fn a_pre_spend_work_group_record_defaults_spent_tokens_to_zero() {
        let group = sample_group("wg-old");
        let mut old_shape = serde_json::to_value(group).expect("serialize group");
        old_shape
            .as_object_mut()
            .expect("object")
            .remove("spent_tokens");
        let restored: WorkGroup = serde_json::from_value(old_shape).expect("deserialize old shape");

        assert_eq!(restored.spent_tokens, 0);
    }

    #[test]
    fn spent_tokens_round_trips_with_the_work_group_record() {
        let mut group = sample_group("wg-spent");
        group.spent_tokens = 123_456;
        let json = serde_json::to_value(group).expect("serialize group");

        let restored: WorkGroup = serde_json::from_value(json).expect("deserialize group");
        let restored = serde_json::to_value(restored).expect("serialize restored group");

        assert_eq!(restored["spent_tokens"], 123_456);
    }

    /// Closing is idempotent and preserves the terms: a closed group is
    /// evidence of what a batch was launched under, not a tombstone.
    #[test]
    fn closing_a_group_stamps_it_and_stays_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create");

        close(&state, "wg-1", 1_700_000_500).expect("close");
        let closed = load(&state, "wg-1").expect("load").expect("still present");
        assert_eq!(closed.closed_at, Some(1_700_000_500));
        assert_eq!(closed.scope, "phase 5 implementation");

        close(&state, "wg-1", 1_700_000_900).expect("closing twice is not an error");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .closed_at,
            Some(1_700_000_500),
            "the first close time stands"
        );
    }

    /// A group written by a future zirv with extra fields, or by an older one
    /// with fewer, must not break `list` for every OTHER group.
    #[test]
    fn an_unparsable_group_file_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-good")).expect("create");
        std::fs::write(state.groups().join("wg-bad.json"), "{ not json").expect("write");
        assert_eq!(list(&state).len(), 1);
    }

    /// `zirv ctx group create` mints an id and prints it, because that id is
    /// what every `zirv agent --group` invocation must carry.
    #[test]
    fn group_create_prints_the_id_it_minted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut out = Vec::new();
        let id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                scope: "phase 5 implementation".to_string(),
                child_limit: 3,
                token_budget: Some(400_000),
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: Some("sess-parent".to_string()),
            },
            1_700_000_000,
        )
        .expect("create");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&id), "the minted id must be printed: {text}");
        assert!(load(&state, &id).expect("load").is_some());
    }

    /// Issue #155 review finding D2: an admission under the limit succeeds
    /// and advances the count by exactly one.
    #[test]
    fn admit_child_succeeds_and_advances_the_count_under_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create"); // child_limit: 3

        admit_child(&state, "wg-1", 1_700_000_100).expect("first child admitted");
        let group = load(&state, "wg-1").expect("load").expect("present");
        assert_eq!(group.admitted_children, 1);

        admit_child(&state, "wg-1", 1_700_000_100).expect("second child admitted");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            2
        );
    }

    /// Issue #155 review finding D2: the whole point -- a spawn/delegation
    /// naming a full group must be refused rather than silently exceeding
    /// the batch the operator sized. The error names both the group and its
    /// limit, and the count is left unchanged by the refused attempt.
    #[test]
    fn admit_child_refuses_once_the_child_limit_is_reached() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut group = sample_group("wg-1");
        group.child_limit = 1;
        create(&state, &group).expect("create");

        admit_child(&state, "wg-1", 1_700_000_100).expect("the one allowed child is admitted");
        let err = admit_child(&state, "wg-1", 1_700_000_100).expect_err("the limit is reached");
        assert!(err.to_string().contains("wg-1"), "got {err}");
        assert!(err.to_string().contains('1'), "names the limit: {err}");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            1,
            "a refused admission must not advance the count"
        );
    }

    #[test]
    fn admit_child_refuses_when_the_group_token_budget_is_spent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut group = sample_group("wg-spent");
        group.spent_tokens = 400_000;
        create(&state, &group).expect("create");

        let err =
            admit_child(&state, "wg-spent", 1_700_000_100).expect_err("the token budget is spent");
        assert!(err.to_string().contains("wg-spent"), "got {err}");
        assert_eq!(
            load(&state, "wg-spent")
                .expect("load")
                .expect("present")
                .admitted_children,
            0,
            "a refused admission must not advance the count"
        );
    }

    #[test]
    fn admit_child_refuses_when_the_group_deadline_has_elapsed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut group = sample_group("wg-overdue");
        group.created_at = 100;
        group.deadline_secs = Some(10);
        create(&state, &group).expect("create");

        let err = admit_child(&state, "wg-overdue", 111).expect_err("the deadline elapsed");
        assert!(err.to_string().contains("wg-overdue"), "got {err}");
        assert_eq!(
            load(&state, "wg-overdue")
                .expect("load")
                .expect("present")
                .admitted_children,
            0,
            "a refused admission must not advance the count"
        );
    }

    /// Re-review (2026-08-27) finding 1: a rollback after a real admission
    /// restores exactly the slot it undoes, leaving any other admissions on
    /// the group untouched.
    #[test]
    fn rollback_admission_undoes_exactly_one_admitted_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create");

        admit_child(&state, "wg-1", 1_700_000_100).expect("first child admitted");
        admit_child(&state, "wg-1", 1_700_000_100).expect("second child admitted");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            2
        );

        rollback_admission(&state, "wg-1");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            1,
            "rollback must undo exactly one admission"
        );
    }

    /// Never underflows: a rollback with nothing to undo (already at zero)
    /// must saturate rather than wrap `u32::MAX`, since it is called
    /// best-effort on failure paths that cannot prove an admission actually
    /// happened moments earlier.
    #[test]
    fn rollback_admission_saturates_at_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create"); // admitted_children: 0

        rollback_admission(&state, "wg-1");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            0,
            "must saturate at zero, not underflow"
        );
    }

    #[test]
    fn adding_group_spend_uses_saturating_arithmetic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut group = sample_group("wg-saturating");
        group.spent_tokens = u64::MAX - 5;
        create(&state, &group).expect("persist group");

        add_spent_tokens(&state, "wg-saturating", 10).expect("roll up spend");

        let persisted =
            std::fs::read_to_string(record_path(&state, "wg-saturating")).expect("read group");
        let persisted: serde_json::Value = serde_json::from_str(&persisted).expect("json");
        assert_eq!(persisted["spent_tokens"], u64::MAX);
    }

    /// Best-effort: a rollback naming an unknown group must not panic --
    /// `admit_child`'s own choke points call this from an error path that
    /// already has a real failure to report, and a second panic/error here
    /// would only make that worse.
    #[test]
    fn rollback_admission_on_an_unknown_group_does_not_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        rollback_admission(&state, "nope");
    }

    #[test]
    fn admit_child_errors_on_an_unknown_group() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let err = admit_child(&state, "nope", 0).expect_err("no such group");
        assert!(err.to_string().contains("nope"), "got {err}");
    }

    /// The same predicate drives both the status marker and admission gate:
    /// an open group past its deadline is overdue; the same group before the
    /// deadline, or with none set at all, is not; and a CLOSED group is never
    /// overdue no matter how long ago its deadline passed.
    #[test]
    fn is_overdue_marks_only_an_open_group_past_its_deadline() {
        let mut group = sample_group("wg-1"); // created_at 1_700_000_000, deadline_secs 3_600
        assert!(
            !is_overdue(&group, 1_700_000_000 + 3_600),
            "exactly at the deadline is not yet overdue"
        );
        assert!(
            is_overdue(&group, 1_700_000_000 + 3_601),
            "one second past the deadline is overdue"
        );

        group.closed_at = Some(1_700_000_000 + 3_700);
        assert!(
            !is_overdue(&group, 1_700_000_000 + 999_999),
            "a closed group is never overdue"
        );

        let mut no_deadline = sample_group("wg-2");
        no_deadline.deadline_secs = None;
        assert!(
            !is_overdue(&no_deadline, u64::MAX),
            "no deadline set means never overdue"
        );
    }

    /// End to end: `zirv ctx group status` prints the `OVERDUE` marker for a
    /// group past its deadline, and omits it otherwise.
    #[test]
    fn group_status_prints_the_overdue_marker_past_the_deadline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create"); // deadline_secs 3_600

        let mut before = Vec::new();
        run_status(
            &state,
            &mut before,
            &StatusArgs {
                work_group_id: Some("wg-1".to_string()),
            },
            1_700_000_000 + 100,
        )
        .expect("status runs");
        assert!(
            !String::from_utf8(before).expect("utf8").contains("OVERDUE"),
            "well before the deadline"
        );

        let mut after = Vec::new();
        run_status(
            &state,
            &mut after,
            &StatusArgs {
                work_group_id: Some("wg-1".to_string()),
            },
            1_700_000_000 + 999_999,
        )
        .expect("status runs");
        assert!(
            String::from_utf8(after).expect("utf8").contains("OVERDUE"),
            "well past the deadline"
        );
    }

    /// Issue #170: first-claim-wins. A second claim on an already-bound group
    /// is a no-op, not an error and not a hijack -- the group belongs to
    /// whichever SubOrchestrator claimed it first for its whole life.
    #[test]
    fn claim_sub_orchestrator_binds_the_first_claimant_and_ignores_a_second() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create");

        claim_sub_orchestrator(&state, "wg-1", "sess-a").expect("first claim");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .sub_orchestrator_session,
            Some("sess-a".to_string())
        );

        claim_sub_orchestrator(&state, "wg-1", "sess-b")
            .expect("a second claim on an already-bound group is not an error");
        assert_eq!(
            load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .sub_orchestrator_session,
            Some("sess-a".to_string()),
            "the first claimant still owns the group"
        );
    }

    /// Security review round 2 (Finding 4): a group minted for a delegation
    /// that never started is removed -- but only while it is genuinely
    /// pristine. Anything that did start under it (an admitted child, a
    /// coordinator's claim) or has already finished keeps its record, which
    /// is what makes the unwind safe on the one ambiguous path that reaches
    /// it (a dashboard that claimed the request and never confirmed).
    #[test]
    fn discard_if_unused_removes_only_a_pristine_group() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        create(&state, &sample_group("wg-admitted")).expect("create");
        admit_child(&state, "wg-admitted", 1_700_000_100).expect("admit");
        assert!(!discard_if_unused(&state, "wg-admitted"));
        assert!(load(&state, "wg-admitted").expect("load").is_some());

        let mut claimed = sample_group("wg-claimed");
        claimed.sub_orchestrator_session = Some("sess-a".to_string());
        create(&state, &claimed).expect("create");
        assert!(!discard_if_unused(&state, "wg-claimed"));
        assert!(load(&state, "wg-claimed").expect("load").is_some());

        let mut closed = sample_group("wg-closed");
        closed.closed_at = Some(1_700_000_500);
        create(&state, &closed).expect("create");
        assert!(!discard_if_unused(&state, "wg-closed"));
        assert!(load(&state, "wg-closed").expect("load").is_some());

        create(&state, &sample_group("wg-pristine")).expect("create");
        assert!(discard_if_unused(&state, "wg-pristine"));
        assert!(
            load(&state, "wg-pristine").expect("load").is_none(),
            "a group nothing ever ran under leaves no record behind"
        );

        assert!(
            !discard_if_unused(&state, "wg-never-existed"),
            "and an unknown group is not an error, just nothing to remove"
        );
    }

    #[test]
    fn claim_sub_orchestrator_errors_on_an_unknown_group() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let err = claim_sub_orchestrator(&state, "nope", "sess-a").expect_err("no such group");
        assert!(err.to_string().contains("nope"), "got {err}");
    }

    /// Issue #170: abandoned means exactly one thing -- claimed, still open,
    /// and the claimant is gone. An unclaimed group is never abandoned (no
    /// one has failed to close it), a claimed-and-alive one is not abandoned
    /// either, and a closed group is never abandoned no matter what.
    #[test]
    fn is_abandoned_is_true_only_for_an_open_group_with_a_dead_claimed_coordinator() {
        let mut group = sample_group("wg-1");
        assert!(
            !is_abandoned(&group, false),
            "no claim yet -- nothing to abandon"
        );

        group.sub_orchestrator_session = Some("sess-a".to_string());
        assert!(!is_abandoned(&group, true), "claimed and alive");
        assert!(is_abandoned(&group, false), "claimed and dead");

        group.closed_at = Some(1_700_000_500);
        assert!(
            !is_abandoned(&group, false),
            "a closed group is never abandoned"
        );
    }

    /// End to end: `zirv ctx group status` names an ABANDONED group only
    /// once it has been claimed by a coordinator whose own session has since
    /// died -- an unclaimed open group never gets the marker.
    #[test]
    fn group_status_prints_abandoned_for_an_open_group_whose_claimed_coordinator_died() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        create(&state, &sample_group("wg-1")).expect("create");

        let mut before = Vec::new();
        run_status(
            &state,
            &mut before,
            &StatusArgs {
                work_group_id: Some("wg-1".to_string()),
            },
            1_700_000_100,
        )
        .expect("status runs");
        assert!(
            !String::from_utf8(before)
                .expect("utf8")
                .contains("ABANDONED"),
            "unclaimed, never abandoned"
        );

        // A session whose pid is provably dead claims the group.
        let mut record = super::super::sessions::Record::new(
            "deadbeef-2222-4333-8444-555555555555",
            "claude",
            &repo,
            super::super::sessions::Verb::Exec,
        );
        record.pid = super::super::testenv::dead_pid();
        let _guard = super::super::sessions::SessionGuard::register(&state, record);
        claim_sub_orchestrator(&state, "wg-1", "deadbeef").expect("claim");

        let mut after = Vec::new();
        run_status(
            &state,
            &mut after,
            &StatusArgs {
                work_group_id: Some("wg-1".to_string()),
            },
            1_700_000_100,
        )
        .expect("status runs");
        let text = String::from_utf8(after).expect("utf8");
        assert!(text.contains("sub-orchestrator=deadbeef"), "got {text}");
        assert!(text.contains("ABANDONED"), "got {text}");
    }
}
