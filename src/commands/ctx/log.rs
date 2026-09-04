use std::io::Write;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::permit::WorkerMode;
use super::state::StateDir;

pub const LOG_FILE: &str = "decisions.jsonl";
pub const SAFETY_LOG_DIR: &str = "safety-decisions";
pub const DELEGATION_FILE: &str = "delegations.jsonl";
pub const PERMISSION_PROMPTS_FILE: &str = "permission-prompts.jsonl";
pub const ORCHESTRATOR_BLOCKS_FILE: &str = "orchestrator-blocks.jsonl";

/// `Decision::action` for the one-line marker written into the MAIN decision
/// log alongside every delegation record.
pub const DELEGATION_ACTION: &str = "delegation-complete";

/// Issue #264: what KIND of work one delegation was, for later cost
/// analysis/routing by kind rather than only by harness or model. Populated
/// from `--task-class` on `zirv ctx agent`, or derived from the workflow step
/// that auto-spawned the worker (`workflow::engine::auto_spawn_decision`,
/// alongside the same call's own `WorkerMode` choice). `#[serde(default)]`
/// on every reader makes `None` -- "unclassified" -- what a row written
/// before this field existed, or one whose caller never named a class,
/// deserializes as; never a guessed default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TaskClass {
    Review,
    Test,
    Implement,
    Research,
    Other,
}

impl std::fmt::Display for TaskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Review => "review",
            Self::Test => "test",
            Self::Implement => "implement",
            Self::Research => "research",
            Self::Other => "other",
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Decision<'a> {
    pub ts: u64,
    pub session: &'a str,
    pub verb: &'a str,
    pub verdict: &'a str,
    pub score: u32,
    pub action: &'a str,
    pub detail: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<u64>,
}

/// Privacy-preserving evidence for one command-policy decision. Commands are
/// represented only by their SHA-256 identity: an operator can correlate a
/// known command during an incident without turning the state directory into
/// a second transcript full of source, paths, tokens, or shell secrets.
#[derive(Debug, Serialize)]
pub struct SafetyDecision<'a> {
    pub ts: u64,
    pub session: &'a str,
    pub mode: &'a str,
    pub verdict: &'a str,
    pub command_sha256: &'a str,
    pub policy_sha256: &'a str,
    pub launch_policy_sha256: Option<&'a str>,
    pub attestation: &'a str,
    pub matched_pattern: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub platform: &'a str,
}

/// One completed `zirv ctx agent` delegation, with what it actually cost.
///
/// Its own file rather than a `Decision` variant (issue #155): the decision
/// log is a rotation log keyed by verdict/score, and a cost record has
/// neither. `Delegation` is what answers "was delegating this cheaper than
/// doing it on the orchestrator seat", which is the question every later
/// phase's design rests on.
///
/// Token classes are the four RAW ones (`event::TranscriptUsage`), never a
/// pre-summed total: a delegated worker's cache-hit ratio is precisely how
/// you tell a well-shaped worker prompt from a badly-shaped one.
#[derive(Debug, Serialize)]
pub struct Delegation<'a> {
    pub ts: u64,
    pub session: &'a str,
    pub parent_session: &'a str,
    pub work_group_id: Option<&'a str>,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
    pub exit_code: i32,
    pub outcome: &'a str,
    /// Issue #267: whether this worker ran `read-only` or `writing`. A row
    /// written before this field existed deserializes (via [`DelegationRow`]
    /// `#[serde(default)]`) as `None` -- readers must treat that as
    /// "unknown", never as either mode outright.
    pub mode: Option<WorkerMode>,
    /// Issue #264: what kind of work this delegation was, when known -- see
    /// [`TaskClass`]'s own doc comment. Same "row predates the field, or its
    /// caller never named one" contract as `mode` above.
    pub task_class: Option<TaskClass>,
}

/// The owned, deserializable counterpart of [`Delegation`] (which borrows
/// and is serialize-only) -- what [`read_delegations`] parses one logged
/// line back into for `status::group_tree_lines` to render. Field names and
/// shape mirror `Delegation` exactly, the same borrowed-for-writing/owned-
/// for-reading split `SafetyDecision`/`SafetyDecisionRecord` already uses.
#[derive(Debug, Clone, Deserialize)]
pub struct DelegationRow {
    /// Kept for parity with every field `Delegation` writes -- not read by
    /// `status::group_tree_lines` today (which orders by `group::list`'s own
    /// creation order, not by delegation time), the same
    /// kept-for-parity-not-yet-read pattern `SafetyDecisionRecord::mode`
    /// already uses.
    #[allow(dead_code)]
    pub ts: u64,
    pub session: String,
    /// Kept for parity with `Delegation`; no caller has needed a delegation's
    /// parent yet -- `status::group_tree_lines` already knows which group
    /// (and thus which parent) a row belongs to via `work_group_id`.
    #[allow(dead_code)]
    pub parent_session: String,
    #[serde(default)]
    pub work_group_id: Option<String>,
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
    /// Kept for parity with `Delegation`; `status::group_tree_lines` renders
    /// `outcome` (already a human string derived from this code), not the
    /// raw code itself.
    #[allow(dead_code)]
    pub exit_code: i32,
    pub outcome: String,
    /// Issue #267: mirrors `Delegation::mode`. `#[serde(default)]` so a row
    /// written before this field existed deserializes as `None` rather than
    /// failing to parse -- the only reading an old row can honestly carry.
    #[serde(default)]
    #[allow(dead_code)]
    pub mode: Option<WorkerMode>,
    /// Issue #264: mirrors `Delegation::task_class`. `#[serde(default)]` so a
    /// row written before this field existed deserialises as `None` rather
    /// than failing to parse. Read by `spend::aggregate`'s `--by task-class`
    /// grouping.
    #[serde(default)]
    pub task_class: Option<TaskClass>,
}

/// The owned, deserializable counterpart of `hook::PermissionPromptRow`
/// (which borrows and is serialize-only) -- what `permissions.rs`'s own
/// audit (issue #307/#320/#321) parses one logged `permission-prompts.jsonl`
/// line back into. Field names/shape mirror `PermissionPromptRow` exactly:
/// for Read/Edit/Write/MultiEdit/NotebookEdit, `family` is the file's parent
/// directory; for Bash/PowerShell, it is `program subcommand` with the full
/// command kept only as `command_sha256`, never in clear. Nothing here
/// screens for secrets beyond that, the same trust boundary `SafetyDecision`'s
/// own doc comment already draws for the command-policy log next to it.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionPromptRecord {
    /// Kept for parity with every field `PermissionPromptRow` writes; no
    /// reader has needed a permission prompt's own timestamp yet, the same
    /// kept-for-parity-not-yet-read pattern `SafetyDecisionRecord::mode`
    /// already uses.
    #[allow(dead_code)]
    pub ts: u64,
    pub session: String,
    /// `"PermissionRequest"` or `"PermissionDenied"` (the hook's own
    /// `hook_event_name` field, carried through verbatim).
    pub event: String,
    pub tool: String,
    pub family: String,
    /// Kept for parity; `permissions.rs`'s own audit groups Bash/PowerShell
    /// prompts by event only (never by directory), so it does not read the
    /// command hash today.
    #[allow(dead_code)]
    #[serde(default)]
    pub command_sha256: Option<String>,
    #[allow(dead_code)]
    pub cwd: String,
    #[allow(dead_code)]
    pub permission_mode: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub reason: Option<String>,
}

/// Reads every parseable line in `permission-prompts.jsonl`, oldest first --
/// a missing file is an empty list, not an error, and a corrupt line is
/// skipped rather than fatal, the same best-effort contract `read_
/// delegations` gives its own file.
pub fn read_permission_prompts(state: &StateDir) -> Vec<PermissionPromptRecord> {
    let path = state.logs().join(PERMISSION_PROMPTS_FILE);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// One tool call an orchestrator seat's own guard refused because it would
/// have edited a repository file directly (issues #328/#334): an
/// orchestrator seat must be technically unable to edit repository files,
/// and delegation inside the same harness must go through the harness's own
/// native subagent tool instead. Privacy-preserving like `SafetyDecision`:
/// `target` never carries full command text, only a path or a program
/// family.
#[derive(Debug, Serialize)]
pub struct OrchestratorBlock<'a> {
    pub ts: u64,
    /// zirv session short id (`mail::session_identity`), else the harness's
    /// own session id.
    pub session: &'a str,
    /// Tool name: Edit, Write, MultiEdit, NotebookEdit, Bash, PowerShell.
    pub tool: &'a str,
    /// What was blocked: a file path for file tools; for Bash the program
    /// family (e.g. "sed -i"), NEVER the full command text.
    pub target: &'a str,
    pub reason: &'a str,
}

/// Appends to `orchestrator-blocks.jsonl`, the same private dir/append-file
/// contract `append`/`append_delegation` give their own flat logs.
pub fn append_orchestrator_block(state: &StateDir, block: &OrchestratorBlock<'_>) -> CtxResult<()> {
    let dir = state.logs();
    super::state::create_private_dir_all(&dir)?;
    let mut file = super::state::open_private_append(&dir.join(ORCHESTRATOR_BLOCKS_FILE))?;
    writeln!(file, "{}", serde_json::to_string(block)?)?;
    Ok(())
}

/// The owned, deserializable counterpart of [`OrchestratorBlock`] (which
/// borrows and is serialize-only) -- what [`read_orchestrator_blocks`]
/// parses one logged line back into, the same borrowed-for-writing/owned-
/// for-reading split `SafetyDecision`/`SafetyDecisionRecord` already uses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OrchestratorBlockRecord {
    pub ts: u64,
    pub session: String,
    pub tool: String,
    pub target: String,
    pub reason: String,
}

/// Reads every parseable line in `orchestrator-blocks.jsonl`, oldest first --
/// a missing file is an empty list, not an error, and a corrupt line is
/// skipped rather than fatal, the same best-effort contract `read_
/// permission_prompts` gives its own file.
pub fn read_orchestrator_blocks(state: &StateDir) -> Vec<OrchestratorBlockRecord> {
    let path = state.logs().join(ORCHESTRATOR_BLOCKS_FILE);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn append(state: &StateDir, decision: &Decision<'_>) -> CtxResult<()> {
    let dir = state.logs();
    super::state::create_private_dir_all(&dir)?;
    let mut file = super::state::open_private_append(&dir.join(LOG_FILE))?;
    writeln!(file, "{}", serde_json::to_string(decision)?)?;
    Ok(())
}

pub fn append_delegation(state: &StateDir, record: &Delegation<'_>) -> CtxResult<()> {
    let dir = state.logs();
    super::state::create_private_dir_all(&dir)?;
    let mut file = super::state::open_private_append(&dir.join(DELEGATION_FILE))?;
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

/// Appends to one UTC-day bucket. Daily files put a hard time boundary around
/// retention/rotation without a cross-process truncate race between the many
/// short-lived hook processes that may be writing concurrently.
pub fn append_safety(state: &StateDir, decision: &SafetyDecision<'_>) -> CtxResult<()> {
    let dir = state.logs().join(SAFETY_LOG_DIR);
    super::state::create_private_dir_all(&dir)?;
    let day = decision.ts / 86_400;
    let path = dir.join(format!("{day:010}.jsonl"));
    let mut file = super::state::open_private_append(&path)?;
    writeln!(file, "{}", serde_json::to_string(decision)?)?;
    Ok(())
}

pub fn tail(state: &StateDir, count: usize) -> CtxResult<Vec<String>> {
    let path = state.logs().join(LOG_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let from = lines.len().saturating_sub(count);
    Ok(lines[from..].to_vec())
}

/// The owned, deserializable counterpart of [`SafetyDecision`] (which
/// borrows and is serialize-only) -- what a reader (issue #147's
/// `permissions.rs` audit correlation) parses one logged line back into.
/// Field names/shape mirror `SafetyDecision` exactly; kept as a separate
/// type rather than making the borrowed one `Deserialize` too, the same
/// split every other borrowed-for-writing/owned-for-reading pair in this
/// codebase uses.
#[derive(Debug, Clone, Deserialize)]
pub struct SafetyDecisionRecord {
    pub ts: u64,
    pub session: String,
    /// Kept for parity with every field `SafetyDecision` writes, and for a
    /// future correlator that wants it -- `permissions.rs`'s own audit
    /// correlation (issue #147) infers launch mode from context instead
    /// (which branch matched `verdict`/`matched_pattern`), so it does not
    /// read this field today.
    #[allow(dead_code)]
    pub mode: String,
    pub verdict: String,
    pub command_sha256: String,
    #[serde(default)]
    pub matched_pattern: Option<String>,
}

/// Reads every parseable line across every day-bucketed
/// `<state>/logs/safety-decisions/*.jsonl` file, oldest file first (file
/// names are zero-padded day numbers, so lexicographic order is
/// chronological). A line that fails to parse is skipped, not fatal --
/// matching every other best-effort state-dir reader in this codebase; an
/// absent directory returns an empty vec rather than an error, since "no
/// safety decisions logged yet" is the common case, not a failure.
pub fn read_safety_decisions(state: &StateDir) -> Vec<SafetyDecisionRecord> {
    let dir = state.logs().join(SAFETY_LOG_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(record) = serde_json::from_str::<SafetyDecisionRecord>(line) {
                out.push(record);
            }
        }
    }
    out
}

/// Issue #313 (consecutive-denial breaker): a BOUNDED counterpart to
/// [`read_safety_decisions`] for a hook process that runs on every single
/// PreToolUse invocation and cannot afford that function's unbounded,
/// every-file-ever-written scan. Opens only the day-bucketed files for
/// `now_day` (today, as `ts / 86_400`) and the day before it, in case the
/// session's own history spans a UTC midnight -- never merely "the two
/// newest files that exist" (codex review round 1): after days with no
/// safety decisions at all, those would be stale records that must not
/// count toward a CURRENT run of denials. Filters to `session` and returns
/// at most the last `limit` matching records, oldest-first -- exactly the
/// tail the breaker needs to count a trailing run of consecutive denials.
/// Same best-effort tolerance as `read_safety_decisions`: an absent file or
/// a line that fails to parse is skipped, never fatal.
pub fn read_recent_safety_decisions(
    state: &StateDir,
    session: &str,
    limit: usize,
    now_day: u64,
) -> Vec<SafetyDecisionRecord> {
    let dir = state.logs().join(SAFETY_LOG_DIR);
    let mut out = Vec::new();
    for day in [now_day.saturating_sub(1), now_day] {
        let path = dir.join(format!("{day:010}.jsonl"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(record) = serde_json::from_str::<SafetyDecisionRecord>(line)
                && record.session == session
            {
                out.push(record);
            }
        }
    }
    let from = out.len().saturating_sub(limit);
    out[from..].to_vec()
}

/// Tails `delegations.jsonl`'s newest `count` raw JSON lines, same contract
/// `tail` gives the main decision log. [`read_delegations`] is the reader
/// that parses these back into [`DelegationRow`]s for `status::
/// group_tree_lines` (issue #155, Phase 5(f)).
pub fn tail_delegations(state: &StateDir, count: usize) -> CtxResult<Vec<String>> {
    let path = state.logs().join(DELEGATION_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let from = lines.len().saturating_sub(count);
    Ok(lines[from..].to_vec())
}

/// Parses [`tail_delegations`]'s newest `count` lines back into
/// [`DelegationRow`]s for `status::group_tree_lines`. A line that fails to
/// parse (a truncated write, a stray corrupt line) is skipped, not fatal --
/// the same tolerance `group::list` and `read_safety_decisions` already give
/// their own on-disk, best-effort state. Degrades to an empty `Vec` on any
/// read failure at all (missing file, unreadable file), exactly like
/// `tail_delegations` itself already does for a missing file.
pub fn read_delegations(state: &StateDir, count: usize) -> Vec<DelegationRow> {
    tail_delegations(state, count)
        .unwrap_or_default()
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;

    #[test]
    fn decision_rows_deserialize_with_or_without_an_observed_at_timestamp() {
        let old_json = r#"{"ts":1,"session":"s","verb":"agent","verdict":"reroute","score":0,"action":"harness-reroute","detail":"old"}"#;
        let old: Decision<'_> = serde_json::from_str(old_json).expect("old row deserializes");
        assert_eq!(old.observed_at, None);

        let current = Decision {
            ts: 2,
            session: "s",
            verb: "agent",
            verdict: "reroute",
            score: 0,
            action: "harness-reroute",
            detail: "current",
            observed_at: Some(1_700_000_000),
        };
        let json = serde_json::to_string(&current).expect("serialize");
        let round_tripped: Decision<'_> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped.observed_at, Some(1_700_000_000));
    }

    #[test]
    fn decisions_append_as_jsonl_and_tail_returns_newest_last() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        for (i, action) in ["observe", "advise", "compact"].iter().enumerate() {
            append(
                &state,
                &Decision {
                    ts: 1_700_000_000 + i as u64,
                    session: "abc",
                    verb: "wrap",
                    verdict: "compact",
                    score: 64,
                    action,
                    detail: "",
                    observed_at: None,
                },
            )
            .expect("append");
        }

        let lines = tail(&state, 2).expect("tail");
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].contains("\"action\":\"compact\""),
            "got {:?}",
            lines[1]
        );
        assert!(
            lines[0].contains("\"action\":\"advise\""),
            "got {:?}",
            lines[0]
        );

        let all = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("read");
        assert_eq!(all.lines().count(), 3);
    }

    /// Issue #147: `read_safety_decisions` reads every day-bucketed
    /// safety-decisions file back, in file order, and skips a line that
    /// fails to parse rather than failing the whole read.
    #[test]
    fn read_safety_decisions_reads_every_bucket_and_skips_bad_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        append_safety(
            &state,
            &SafetyDecision {
                ts: 1_700_000_000,
                session: "s1",
                mode: "interactive",
                verdict: "ask",
                command_sha256: "aaa",
                policy_sha256: "p",
                launch_policy_sha256: None,
                attestation: "not-present",
                matched_pattern: Some("<sandbox: unsandboxed retry>"),
                origin: Some("built-in"),
                platform: "linux",
            },
        )
        .expect("append");
        append_safety(
            &state,
            &SafetyDecision {
                ts: 1_700_086_400, // next day bucket
                session: "s1",
                mode: "headless",
                verdict: "allow",
                command_sha256: "bbb",
                policy_sha256: "p",
                launch_policy_sha256: None,
                attestation: "not-present",
                matched_pattern: Some("<sandbox: escape_allow>"),
                origin: Some("built-in"),
                platform: "linux",
            },
        )
        .expect("append");

        // A stray unparseable line in one of the bucket files must not
        // break reading the rest.
        let dir = state.logs().join(SAFETY_LOG_DIR);
        let bad_file = dir.join("0000000001.jsonl");
        std::fs::write(&bad_file, "not json\n").expect("write");

        let records = read_safety_decisions(&state);
        assert_eq!(records.len(), 2, "got {records:?}");
        assert_eq!(records[0].command_sha256, "aaa");
        assert_eq!(records[1].command_sha256, "bbb");
        assert_eq!(
            records[1].matched_pattern.as_deref(),
            Some("<sandbox: escape_allow>")
        );
    }

    /// Issue #313: `read_recent_safety_decisions` opens only the newest TWO
    /// day-bucketed files (a record from a third, older bucket is excluded
    /// even though it matches `session`), filters to the requested session
    /// (a different session's records never appear), and returns at most
    /// `limit` records, oldest first.
    #[test]
    fn read_recent_safety_decisions_is_bounded_to_two_files_and_filtered_by_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let record =
            |ts: u64, session: &'static str, command_sha256: &'static str| SafetyDecision {
                ts,
                session,
                mode: "headless",
                verdict: "deny",
                command_sha256,
                policy_sha256: "p",
                launch_policy_sha256: None,
                attestation: "not-present",
                matched_pattern: None,
                origin: Some("built-in"),
                platform: "linux",
            };

        // Three day buckets: day 0 (too old to be scanned), day 1, day 2.
        append_safety(&state, &record(0, "s1", "too-old")).expect("append");
        append_safety(&state, &record(86_400, "s1", "day1-a")).expect("append");
        append_safety(&state, &record(86_401, "s2", "day1-other-session")).expect("append");
        append_safety(&state, &record(172_800, "s1", "day2-a")).expect("append");

        let records = read_recent_safety_decisions(&state, "s1", 50, 2);
        assert_eq!(
            records
                .iter()
                .map(|r| r.command_sha256.as_str())
                .collect::<Vec<_>>(),
            vec!["day1-a", "day2-a"],
            "too-old bucket excluded, other session filtered, oldest-first: {records:?}"
        );

        // `limit` keeps only the newest matching records, still oldest-first.
        let limited = read_recent_safety_decisions(&state, "s1", 1, 2);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].command_sha256, "day2-a");

        // Codex review round 1: the window is anchored on TODAY, not on
        // whichever files happen to be newest -- days later, with no
        // decisions since, the same files contribute nothing.
        assert!(
            read_recent_safety_decisions(&state, "s1", 50, 10).is_empty(),
            "stale day buckets must never feed a current denial run"
        );
    }

    /// No directory at all (no safety decision ever logged for this state
    /// dir) is an empty list, not an error.
    #[test]
    fn read_recent_safety_decisions_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read_recent_safety_decisions(&state, "s1", 50, 0).is_empty());
    }

    /// The log names sessions, repositories and transcript paths, so on a
    /// shared machine it is nobody else's reading.
    #[cfg(unix)]
    #[test]
    fn the_decision_log_is_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        append(
            &state,
            &Decision {
                ts: 1,
                session: "s",
                verb: "hook",
                verdict: "healthy",
                score: 0,
                action: "observe",
                detail: "/home/someone/.claude/projects/x/y.jsonl",
                observed_at: None,
            },
        )
        .expect("append");

        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&state.logs().join(LOG_FILE)), 0o600);
        assert_eq!(mode(&state.logs()), 0o700);
    }

    #[test]
    fn append_creates_the_log_dir_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("not-yet"));
        append(
            &state,
            &Decision {
                ts: 1,
                session: "s",
                verb: "hook",
                verdict: "healthy",
                score: 0,
                action: "observe",
                detail: "",
                observed_at: None,
            },
        )
        .expect("append must create its directory");
        assert!(state.logs().join("decisions.jsonl").is_file());
    }

    /// Issue #155, Phase 2: a delegation checkpoint. Its own file, like
    /// `SafetyDecision`'s own daily buckets -- the decision log is a rotation
    /// log and mixing a per-delegation cost record into it would make both
    /// harder to read. The main log still gets a one-line
    /// `delegation-complete` decision so a reader who only looks there sees
    /// that a delegation happened.
    #[test]
    fn a_delegation_record_appends_as_jsonl_with_all_four_token_classes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        append_delegation(
            &state,
            &Delegation {
                ts: 1_700_000_000,
                session: "sess-child",
                parent_session: "sess-parent",
                work_group_id: None,
                agent: "codex",
                model: Some("gpt-5-codex"),
                input_tokens: 1_000,
                cache_creation_input_tokens: 8_000,
                cache_read_input_tokens: 91_000,
                output_tokens: 500,
                wall_ms: 42_000,
                exit_code: 0,
                outcome: "ok",
                mode: Some(WorkerMode::Writing),
                task_class: None,
            },
        )
        .expect("append");

        let lines = tail_delegations(&state, 10).expect("tail");
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("json");
        assert_eq!(value["agent"], "codex");
        assert_eq!(value["model"], "gpt-5-codex");
        assert_eq!(value["cache_read_input_tokens"], 91_000);
        assert_eq!(value["wall_ms"], 42_000);
        assert_eq!(value["outcome"], "ok");
        assert_eq!(value["exit_code"], 0);
    }

    /// An empty file (or none at all) is an empty list, never an error --
    /// same contract `tail` already has for the decision log.
    #[test]
    fn tailing_delegations_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(tail_delegations(&state, 10).expect("tail").is_empty());
    }

    /// Issue #155, Phase 5(f): `read_delegations` is what `status::
    /// group_tree_lines` reads its input from -- round-trips a real
    /// `Delegation` back into an owned `DelegationRow`, and a corrupt line
    /// landing in the middle of a concurrent write is skipped, not fatal,
    /// the same tolerance `group::list` and `read_safety_decisions` already
    /// give their own on-disk, best-effort state.
    #[test]
    fn read_delegations_round_trips_and_skips_a_corrupt_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        append_delegation(
            &state,
            &Delegation {
                ts: 1_700_000_000,
                session: "sess-child",
                parent_session: "sess-parent",
                work_group_id: Some("wg-1"),
                agent: "codex",
                model: Some("gpt-5-codex"),
                input_tokens: 1_000,
                cache_creation_input_tokens: 8_000,
                cache_read_input_tokens: 91_000,
                output_tokens: 500,
                wall_ms: 42_000,
                exit_code: 0,
                outcome: "ok",
                mode: Some(WorkerMode::Writing),
                task_class: Some(TaskClass::Implement),
            },
        )
        .expect("append");

        {
            let mut file =
                super::super::state::open_private_append(&state.logs().join(DELEGATION_FILE))
                    .expect("open");
            writeln!(file, "not json").expect("write corrupt line");
        }

        append_delegation(
            &state,
            &Delegation {
                ts: 1_700_000_100,
                session: "sess-child-2",
                parent_session: "sess-parent",
                work_group_id: None,
                agent: "claude",
                model: None,
                input_tokens: 10,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 5,
                wall_ms: 1_000,
                exit_code: 1,
                outcome: "failed",
                mode: None,
                task_class: None,
            },
        )
        .expect("append");

        let rows = read_delegations(&state, 10);
        assert_eq!(
            rows.len(),
            2,
            "the corrupt line is skipped, not fatal: {rows:?}"
        );
        assert_eq!(rows[0].session, "sess-child");
        assert_eq!(rows[0].work_group_id.as_deref(), Some("wg-1"));
        assert_eq!(rows[0].model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(
            rows[0].mode,
            Some(WorkerMode::Writing),
            "issue #267: mode round-trips when the writer set it"
        );
        assert_eq!(
            rows[0].task_class,
            Some(TaskClass::Implement),
            "issue #264: task_class round-trips when the caller set it"
        );
        assert_eq!(rows[1].session, "sess-child-2");
        assert_eq!(rows[1].work_group_id, None);
        assert_eq!(rows[1].outcome, "failed");
        assert_eq!(
            rows[1].mode, None,
            "issue #267: an omitted mode reads back as unknown, never a guessed default"
        );
        assert_eq!(
            rows[1].task_class, None,
            "issue #264: an omitted task_class reads back as unclassified, never a guessed default"
        );
    }

    /// Issue #264: a `delegations.jsonl` row written before `task_class`
    /// existed (no such field at all) must still deserialize, and must read
    /// back as `None` -- the only honest reading for a row that predates the
    /// field -- mirroring `a_delegation_row_written_before_mode_existed_
    /// still_deserialises_as_unknown` for the identical `mode` shape.
    #[test]
    fn a_delegation_row_written_before_task_class_existed_still_deserialises_as_unclassified() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = state.logs();
        super::super::state::create_private_dir_all(&dir).expect("mkdir");
        let old_line = r#"{"ts":1700000000,"session":"sess-child","parent_session":"sess-parent","agent":"codex","input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":500,"wall_ms":42000,"exit_code":0,"outcome":"ok","mode":"writing"}"#;
        let mut file =
            super::super::state::open_private_append(&dir.join(DELEGATION_FILE)).expect("open");
        writeln!(file, "{old_line}").expect("write");
        drop(file);

        let rows = read_delegations(&state, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].task_class, None,
            "an older row has no honest task_class to report"
        );
    }

    /// Issue #267: a `delegations.jsonl` row written before `mode` existed
    /// (no such field at all) must still deserialize, and must read back as
    /// `None` -- not a guessed default -- since no old row can honestly say
    /// which mode it ran in.
    #[test]
    fn a_delegation_row_written_before_mode_existed_still_deserialises_as_unknown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = state.logs();
        super::super::state::create_private_dir_all(&dir).expect("mkdir");
        let old_line = r#"{"ts":1700000000,"session":"sess-child","parent_session":"sess-parent","agent":"codex","input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":500,"wall_ms":42000,"exit_code":0,"outcome":"ok"}"#;
        let mut file =
            super::super::state::open_private_append(&dir.join(DELEGATION_FILE)).expect("open");
        writeln!(file, "{old_line}").expect("write");
        drop(file);

        let rows = read_delegations(&state, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].mode, None,
            "an older row has no honest mode to report"
        );
    }

    /// No file at all is an empty list, not an error -- `zirv ctx agent`
    /// has never successfully delegated on this machine yet.
    #[test]
    fn read_delegations_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read_delegations(&state, 10).is_empty());
    }

    /// A present-but-empty file (the directory was created but nothing was
    /// ever appended) is also an empty list, not an error.
    #[test]
    fn read_delegations_from_an_empty_file_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        super::super::state::create_private_dir_all(&state.logs()).expect("mkdir");
        std::fs::write(state.logs().join(DELEGATION_FILE), "").expect("write empty file");
        assert!(read_delegations(&state, 10).is_empty());
    }

    #[test]
    fn safety_audit_records_structured_evidence_without_the_raw_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        append_safety(
            &state,
            &SafetyDecision {
                ts: 1_700_000_000,
                session: "abc",
                mode: "interactive",
                verdict: "ask",
                command_sha256: "0123456789abcdef",
                policy_sha256: "fedcba9876543210",
                launch_policy_sha256: Some("aabbccdd"),
                attestation: "valid",
                matched_pattern: Some("curl * --data *"),
                origin: Some("built-in"),
                platform: "windows",
            },
        )
        .expect("append");

        let dir = state.logs().join("safety-decisions");
        let file = std::fs::read_dir(&dir)
            .expect("audit dir")
            .next()
            .expect("one file")
            .expect("entry")
            .path();
        let text = std::fs::read_to_string(file).expect("audit");
        assert!(text.contains("\"command_sha256\":\"0123456789abcdef\""));
        assert!(text.contains("\"policy_sha256\":\"fedcba9876543210\""));
        assert!(!text.contains("secret-value-from-command"));
    }

    /// Writes one raw `permission-prompts.jsonl` line matching `hook::
    /// PermissionPromptRow`'s own field shape -- there is no writer in this
    /// module (the hook writes the file directly), so tests seed it by hand.
    fn write_permission_prompt_line(
        state: &StateDir,
        ts: u64,
        session: &str,
        event: &str,
        tool: &str,
        family: &str,
    ) {
        let dir = state.logs();
        super::super::state::create_private_dir_all(&dir).expect("mkdir logs");
        let mut file = super::super::state::open_private_append(&dir.join(PERMISSION_PROMPTS_FILE))
            .expect("open");
        let line = serde_json::json!({
            "ts": ts,
            "session": session,
            "event": event,
            "tool": tool,
            "family": family,
            "cwd": "/work/repo",
            "permission_mode": "default",
        });
        writeln!(file, "{line}").expect("write line");
    }

    /// Issue #321: every field of one logged `permission-prompts.jsonl` line
    /// round-trips back through `read_permission_prompts`.
    #[test]
    fn permission_prompts_append_as_jsonl_and_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        write_permission_prompt_line(
            &state,
            1_700_000_000,
            "sess-1",
            "PermissionRequest",
            "Read",
            "/work/repo/src",
        );
        write_permission_prompt_line(
            &state,
            1_700_000_100,
            "sess-1",
            "PermissionDenied",
            "Bash",
            "echo hi",
        );

        let records = read_permission_prompts(&state);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].session, "sess-1");
        assert_eq!(records[0].event, "PermissionRequest");
        assert_eq!(records[0].tool, "Read");
        assert_eq!(records[0].family, "/work/repo/src");
        assert_eq!(records[1].event, "PermissionDenied");
        assert_eq!(records[1].tool, "Bash");
    }

    /// No file at all is an empty list, not an error -- no `PermissionRequest`/
    /// `PermissionDenied` hook has fired yet on this machine.
    #[test]
    fn read_permission_prompts_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read_permission_prompts(&state).is_empty());
    }

    /// A corrupt line landing in the middle of a concurrent write is
    /// skipped, not fatal -- the same tolerance `read_delegations` and
    /// `read_safety_decisions` already give their own on-disk state.
    #[test]
    fn read_permission_prompts_skips_a_corrupt_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        write_permission_prompt_line(
            &state,
            1_700_000_000,
            "sess-1",
            "PermissionRequest",
            "Edit",
            "/work/repo",
        );
        {
            let mut file = super::super::state::open_private_append(
                &state.logs().join(PERMISSION_PROMPTS_FILE),
            )
            .expect("open");
            writeln!(file, "not json").expect("write corrupt line");
        }

        let records = read_permission_prompts(&state);
        assert_eq!(records.len(), 1, "the corrupt line is skipped: {records:?}");
        assert_eq!(records[0].tool, "Edit");
    }

    /// Issues #328/#334: every field of two logged `orchestrator-blocks.jsonl`
    /// rows round-trips back through `read_orchestrator_blocks`, oldest first.
    #[test]
    fn orchestrator_blocks_append_as_jsonl_and_round_trip_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        append_orchestrator_block(
            &state,
            &OrchestratorBlock {
                ts: 1_700_000_000,
                session: "sess-1",
                tool: "Edit",
                target: "/work/repo/src/main.rs",
                reason: "orchestrator seats may not edit repository files",
            },
        )
        .expect("append");
        append_orchestrator_block(
            &state,
            &OrchestratorBlock {
                ts: 1_700_000_100,
                session: "sess-1",
                tool: "Bash",
                target: "sed -i",
                reason: "orchestrator seats may not edit repository files",
            },
        )
        .expect("append");

        let records = read_orchestrator_blocks(&state);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].session, "sess-1");
        assert_eq!(records[0].tool, "Edit");
        assert_eq!(records[0].target, "/work/repo/src/main.rs");
        assert_eq!(records[1].tool, "Bash");
        assert_eq!(records[1].target, "sed -i");
    }

    /// No file at all is an empty list, not an error -- no tool call has
    /// ever been blocked on this machine.
    #[test]
    fn read_orchestrator_blocks_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read_orchestrator_blocks(&state).is_empty());
    }

    /// A corrupt line landing in the middle of a concurrent write is
    /// skipped, not fatal -- the same tolerance `read_permission_prompts`
    /// already gives its own file.
    #[test]
    fn read_orchestrator_blocks_skips_a_corrupt_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        append_orchestrator_block(
            &state,
            &OrchestratorBlock {
                ts: 1_700_000_000,
                session: "sess-1",
                tool: "Edit",
                target: "/work/repo",
                reason: "orchestrator seats may not edit repository files",
            },
        )
        .expect("append");
        {
            let mut file = super::super::state::open_private_append(
                &state.logs().join(ORCHESTRATOR_BLOCKS_FILE),
            )
            .expect("open");
            writeln!(file, "not json").expect("write corrupt line");
        }

        let records = read_orchestrator_blocks(&state);
        assert_eq!(records.len(), 1, "the corrupt line is skipped: {records:?}");
        assert_eq!(records[0].tool, "Edit");
    }
}
