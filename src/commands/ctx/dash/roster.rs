//! The dashboard's quit/restore roster: a snapshot of every pane this
//! dashboard owned, written right before shutdown (`dash::mod::on_quit`) and
//! offered back, once, the next time a dashboard opens for the same repo
//! (`dash::mod::run_dashboard`'s own startup restore dialog).
//!
//! A roster is consumed at most once (`take_roster` renames the file away
//! the moment it is read, whether or not it turns out to be stale) so a
//! restore is never offered twice, and a stale roster (older than
//! `cfg.dash.roster_max_age_secs`) is treated the same as an absent one --
//! session state that old is more likely to confuse a restore than help one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::adapters::AgentAdapter;
use super::super::state::StateDir;

/// The role label `dash::mod::on_quit` stamps on the orchestrator pane
/// (`Verb::Chat`). A roster entry carrying this role is never offered for
/// restore -- see this module's own `restore` doc section and `dash::mod`'s
/// startup filter: the `first` `PaneSpec` a fresh dashboard launch already
/// builds *is* the orchestrator, so spawning a second one from the roster
/// would duplicate it. Kept as a plain string (not an enum) because the
/// roster file is read by a struct with no other typed vocabulary to lean
/// on, matching `sessions::Record`'s own plain-string `agent` field.
pub const ROLE_ORCHESTRATOR: &str = "orchestrator";
/// The role label for every other pane (`Verb::Dash`).
pub const ROLE_WORKER: &str = "worker";

/// One pane's own snapshot at quit time: enough to relaunch it (`agent`,
/// `session_id` -- fed to `resume_args`) and enough to label it in the
/// restore dialog (`role`, `short`, `title`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterPane {
    pub agent: String,
    pub session_id: String,
    pub role: String,
    pub short: String,
    pub title: String,
}

/// A full dashboard's worth of panes, stamped with the time it was written
/// (`state::now_secs`) so `take_roster` can tell a fresh roster from a stale
/// one without touching the clock itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub written: u64,
    pub panes: Vec<RosterPane>,
}

/// `<state>/dash/roster-<slug>.json`. One roster per repo -- a second
/// dashboard quitting against the same repo overwrites the first's, which is
/// the right behaviour: only the most recent quit is worth offering back.
pub fn roster_path(state: &StateDir, repo_slug: &str) -> PathBuf {
    state.dash().join(format!("roster-{repo_slug}.json"))
}

/// The path `take_roster` renames a freshly read roster to, so a second call
/// never finds it again under its offering name. Kept alongside (not
/// deleted): a `.consumed.json` trail is cheap and mirrors `mail::consume`'s
/// own move-not-delete discipline for read-once state.
fn consumed_path(state: &StateDir, repo_slug: &str) -> PathBuf {
    state
        .dash()
        .join(format!("roster-{repo_slug}.consumed.json"))
}

/// Overwrites this repo's roster file with `roster` -- the only writer is
/// `dash::mod::on_quit`, called with every pane this dashboard still owns
/// (orchestrator included; the startup restore path is what excludes the
/// orchestrator, not this write).
pub fn write_roster(state: &StateDir, repo_slug: &str, roster: &Roster) -> CtxResult<()> {
    let dir = state.dash();
    super::super::state::create_private_dir_all(&dir)?;
    let body = serde_json::to_string(roster)?;
    super::super::state::write_private(&roster_path(state, repo_slug), &body)?;
    Ok(())
}

/// Reads and consumes this repo's roster, if one is there. The rename to the
/// `.consumed` path is the **claim**, and it happens first, before the read
/// and before the age check: exactly the idiom `sessions::claim_nudge_marker`
/// and `mail::consume` already use, where the single atomic filesystem
/// operation is what decides who got it. A rename that fails means somebody
/// else claimed it (or it was never there), and the answer is `None`.
///
/// N3: reading first and renaming afterwards -- with the rename's own error
/// discarded -- made consume-at-most-once best-effort in both directions: two
/// dashboards launching together both read the same roster and both restored
/// it, and a rename that failed left the roster to be offered again on every
/// later launch.
///
/// `None` covers every reason there is nothing to restore: absent, already
/// claimed, unreadable, malformed, or older than `max_age` as of `now`. A
/// stale roster is still consumed -- it must not linger to be picked up by
/// some later, larger `max_age`.
pub fn take_roster(state: &StateDir, repo_slug: &str, now: u64, max_age: u64) -> Option<Roster> {
    let path = roster_path(state, repo_slug);
    let consumed = consumed_path(state, repo_slug);
    std::fs::rename(&path, &consumed).ok()?;
    let contents = std::fs::read_to_string(&consumed).ok()?;
    let roster: Roster = serde_json::from_str(&contents).ok()?;
    if now.saturating_sub(roster.written) > max_age {
        return None;
    }
    Some(roster)
}

/// A one-line note used as the resumed session's initial prompt when the
/// agent has no verified resume mechanism (`AgentAdapter::resume_args`
/// returns `None`). Deliberately not `resume::resume_prompt`'s own
/// handoff-carrying convention: that needs a stored `Handoff` loaded from
/// disk, and this function's signature (matching the plan's own interface)
/// takes only an adapter and a roster entry, with nothing to load one from.
/// A future caller with a handoff in hand is free to build a richer prompt
/// and hand it to `interactive_cmd` itself; this is the floor every agent
/// gets today.
const NO_RESUME_NOTE: &str =
    "resuming after a dashboard restart; check the repo state and continue";

/// The argv to relaunch `pane` with: `adapter.resume_args` when the agent has
/// a verified one (`interactive_cmd(None, resume_args)`, so the resume flags
/// land right after the launch prefix, the same place any other `extra` argv
/// would), else a plain prompt-carrying launch that just tells the agent it
/// is picking a session back up. Flattened to `program, arg, arg, ...` via
/// `dash::mod::flatten_command`, matching `PaneSpec::argv`'s own shape.
pub fn restore_argv(adapter: &dyn AgentAdapter, pane: &RosterPane) -> Vec<String> {
    let command = match adapter.resume_args(&pane.session_id) {
        Some(resume_args) => adapter.interactive_cmd(None, &resume_args),
        None => adapter.interactive_cmd(Some(NO_RESUME_NOTE), &[]),
    };
    super::flatten_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;

    fn sample_roster() -> Roster {
        Roster {
            written: 1_000,
            panes: vec![
                RosterPane {
                    agent: "claude".to_string(),
                    session_id: "11111111-2222-4333-8444-555555555555".to_string(),
                    role: ROLE_ORCHESTRATOR.to_string(),
                    short: "aaaa1111".to_string(),
                    title: "orch".to_string(),
                },
                RosterPane {
                    agent: "codex".to_string(),
                    session_id: "22222222-2222-4333-8444-555555555555".to_string(),
                    role: ROLE_WORKER.to_string(),
                    short: "bbbb2222".to_string(),
                    title: "wrk codex".to_string(),
                },
            ],
        }
    }

    #[test]
    fn roster_path_hangs_off_the_dash_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(
            roster_path(&state, "-repo"),
            tmp.path().join("dash").join("roster--repo.json")
        );
    }

    #[test]
    fn a_roster_round_trips_through_write_and_take() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let roster = sample_roster();

        write_roster(&state, "-repo", &roster).expect("write_roster");
        let got = take_roster(&state, "-repo", 1_500, 1_000).expect("roster present");
        assert_eq!(got, roster);
    }

    #[test]
    fn take_roster_consumes_the_file_so_a_second_call_finds_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        write_roster(&state, "-repo", &sample_roster()).expect("write_roster");

        assert!(take_roster(&state, "-repo", 1_500, 1_000).is_some());
        assert!(
            take_roster(&state, "-repo", 1_500, 1_000).is_none(),
            "a second call must not re-offer the same roster"
        );
        assert!(
            !roster_path(&state, "-repo").exists(),
            "the offering path must be gone after the first read"
        );
        assert!(
            consumed_path(&state, "-repo").exists(),
            "the roster is renamed aside, not deleted"
        );
    }

    #[test]
    fn take_roster_rejects_a_stale_roster_but_still_consumes_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        write_roster(&state, "-repo", &sample_roster()).expect("write_roster");

        // written: 1_000, max_age: 100 -> anything at or after now=1_101 is stale.
        let got = take_roster(&state, "-repo", 1_200, 100);
        assert!(got.is_none(), "a roster older than max_age is not offered");
        assert!(
            !roster_path(&state, "-repo").exists(),
            "a stale roster is still consumed, not left to linger"
        );
    }

    #[test]
    fn take_roster_on_an_absent_file_is_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(take_roster(&state, "-repo", 1_000, 1_000).is_none());
    }

    /// N3: the rename *is* the claim, so it happens before the read -- the
    /// roster that comes back was parsed out of the already-claimed path, and
    /// nothing is left at the offering path for a concurrent launch to read.
    #[test]
    fn take_roster_claims_the_file_before_reading_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        write_roster(&state, "-repo", &sample_roster()).expect("write_roster");

        let got = take_roster(&state, "-repo", 1_500, 1_000).expect("roster present");
        assert_eq!(got, sample_roster());
        assert!(
            !roster_path(&state, "-repo").exists(),
            "the offering path is gone the moment it is claimed"
        );
        let claimed: Roster =
            serde_json::from_str(&std::fs::read_to_string(consumed_path(&state, "-repo")).unwrap())
                .expect("the claimed copy still holds the roster that was read");
        assert_eq!(claimed, sample_roster());
    }

    /// N3: a claim that cannot be made -- nothing there to rename -- is `None`,
    /// never a read of a file this call does not own.
    #[test]
    fn an_unclaimable_roster_is_never_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        write_roster(&state, "-repo", &sample_roster()).expect("write_roster");

        assert!(take_roster(&state, "-repo", 1_500, 1_000).is_some());
        // The second caller loses the race: the rename fails, and it reads
        // nothing at all rather than the copy the winner is already restoring.
        assert!(take_roster(&state, "-repo", 1_500, 1_000).is_none());
    }

    #[test]
    fn restore_argv_uses_claudes_verified_resume_flag() {
        let adapter = ClaudeAdapter::new(None);
        let pane = RosterPane {
            agent: "claude".to_string(),
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            role: ROLE_WORKER.to_string(),
            short: "aaaa1111".to_string(),
            title: "wrk claude".to_string(),
        };
        let argv = restore_argv(&adapter, &pane);
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--resume".to_string(),
                "11111111-2222-4333-8444-555555555555".to_string(),
            ]
        );
    }

    /// R1: a *fresh* dashboard pane pins its conversation with
    /// `session_pin_args` so this restore can find it later -- but a restored
    /// pane must not carry both. `--resume` picks the conversation up;
    /// `--session-id` asks for a new one under that id, and the two together
    /// are a contradiction the harness would refuse.
    #[test]
    fn a_restored_pane_resumes_and_never_re_pins_the_session_id() {
        let adapter = ClaudeAdapter::new(None);
        let pane = RosterPane {
            agent: "claude".to_string(),
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            role: ROLE_WORKER.to_string(),
            short: "aaaa1111".to_string(),
            title: "wrk claude".to_string(),
        };
        let argv = restore_argv(&adapter, &pane);
        assert!(argv.iter().any(|a| a == "--resume"), "got {argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--session-id"),
            "a resumed conversation is never re-pinned: {argv:?}"
        );
    }

    #[test]
    fn restore_argv_falls_back_to_a_prompt_carrying_launch_when_unverified() {
        // An explicit path rather than the bare default: on a machine with a
        // real npm-installed `codex.cmd` on `PATH` (this one, among others),
        // `CodexAdapter::base` now legitimately resolves the bare "codex"
        // through `cmd.exe /c <shim>` (mirroring claude's own shim
        // handling), so `argv[0]` would be `cmd.exe`, not `codex`. This test
        // is about the resume-fallback text, not the launcher rewrite, so it
        // pins a program that never resolves to anything on `PATH`.
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        let pane = RosterPane {
            agent: "codex".to_string(),
            session_id: "22222222-2222-4333-8444-555555555555".to_string(),
            role: ROLE_WORKER.to_string(),
            short: "bbbb2222".to_string(),
            title: "wrk codex".to_string(),
        };
        let argv = restore_argv(&adapter, &pane);
        assert_eq!(argv[0], "/tmp/fake-codex");
        assert!(
            argv.iter()
                .any(|a| a.contains("resuming after a dashboard restart")),
            "got {argv:?}"
        );
    }
}
