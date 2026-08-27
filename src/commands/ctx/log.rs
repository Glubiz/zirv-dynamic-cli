use std::io::Write;

use serde::Serialize;

use super::CtxResult;
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

/// Issue #155: no CLI surface reads `delegations.jsonl` back yet -- that is
/// Phase 5's reporting work, once `work_group_id` gives it something to
/// group by. This reader lands now so `append_delegation`'s own round-trip
/// is testable today, the same "accessor lands ahead of its production
/// caller" pattern `LaunchMode::label`/`is_interactive` used.
#[allow(dead_code)]
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
