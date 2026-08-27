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
    #[serde(default)]
    pub deadline_secs: Option<u64>,
    pub completion_contract: String,
    pub created_at: u64,
    #[serde(default)]
    pub closed_at: Option<u64>,
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

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// What this group of delegated work is for.
    pub scope: String,
    #[arg(long, default_value_t = 3)]
    pub child_limit: u32,
    #[arg(long)]
    pub token_budget: Option<u64>,
    #[arg(long)]
    pub deadline_secs: Option<u64>,
    #[arg(
        long,
        default_value = "report a compact structured result by mail to the requesting session"
    )]
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
        deadline_secs: args.deadline_secs,
        completion_contract: args.completion_contract.clone(),
        created_at: now,
        closed_at: None,
    };
    create(state, &group)?;
    writeln!(w, "work group {id} created (scope: {})", group.scope)?;
    Ok(id)
}

fn print_group<W: Write>(w: &mut W, group: &WorkGroup) -> CtxResult<()> {
    let status = if group.closed_at.is_some() {
        "closed"
    } else {
        "open"
    };
    writeln!(
        w,
        "{} [{status}] scope=\"{}\" child_limit={} parent={}",
        group.work_group_id, group.scope, group.child_limit, group.parent_session_id
    )?;
    Ok(())
}

pub fn run_status<W: Write>(state: &StateDir, w: &mut W, args: &StatusArgs) -> CtxResult<i32> {
    match &args.work_group_id {
        Some(id) => match load(state, id)? {
            Some(group) => {
                print_group(w, &group)?;
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
                print_group(w, group)?;
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
        GroupVerb::Status(a) => run_status(&state, w, a),
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
            deadline_secs: Some(3_600),
            completion_contract: "every child reports a compact result by mail".to_string(),
            created_at: 1_700_000_000,
            closed_at: None,
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
            deadline_secs: Some(3_600),
            completion_contract: "every child reports a compact result by mail".to_string(),
            created_at: 1_700_000_000,
            closed_at: None,
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
}
