use std::io::Write;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::permit::WorkerMode;
use super::state::StateDir;

pub const LOG_FILE: &str = "decisions.jsonl";
pub const SAFETY_LOG_DIR: &str = "safety-decisions";
pub const DELEGATION_FILE: &str = "delegations.jsonl";

/// `Decision::action` for the one-line marker written into the MAIN decision
/// log alongside every delegation record.
pub const DELEGATION_ACTION: &str = "delegation-complete";

#[derive(Debug, Serialize)]
pub struct Decision<'a> {
    pub ts: u64,
    pub session: &'a str,
    pub verb: &'a str,
    pub verdict: &'a str,
    pub score: u32,
    pub action: &'a str,
    pub detail: &'a str,
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
        assert_eq!(rows[1].session, "sess-child-2");
        assert_eq!(rows[1].work_group_id, None);
        assert_eq!(rows[1].outcome, "failed");
        assert_eq!(
            rows[1].mode, None,
            "issue #267: an omitted mode reads back as unknown, never a guessed default"
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
}
