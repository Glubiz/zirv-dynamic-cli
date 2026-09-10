//! Issue #457: "this session" spend, for real -- `zirv ctx status` and the
//! dashboard footer used to count only `zirv agent` delegations
//! (`delegations.jsonl`), so an orchestrator seat that ran for hours and
//! spawned native Claude subagents (the Task tool, not `zirv agent`) showed
//! `--`/`$0.00` no matter how much it actually cost. [`fold_session_spend`]
//! is the ONE function both surfaces now call, folding three disjoint
//! sources into one total:
//!
//! 1. The seat's OWN transcript (every non-sidechain assistant row).
//! 2. Every native subagent transcript under that same session's own
//!    `<session-id>/subagents/*.jsonl` (the Task tool; see
//!    `adapters::claude`'s own module doc comment on why these live in
//!    sibling files rather than `isSidechain` rows in the main transcript).
//! 3. Every `zirv agent` delegation row this session spawned
//!    (`delegations.jsonl`, filtered by `parent_session`).
//!
//! No double-counting is possible by construction: a `zirv agent` worker is
//! a wholly separate process with its OWN session id, and its transcript (if
//! any) lives under THAT worker's own `~/.claude/projects/.../<worker-
//! session-id>.jsonl` -- a path [`session_transcript_usage`] never reads,
//! since it only ever walks the CALLING session's own transcript and that
//! same session's own `subagents/` directory. The delegation ledger and a
//! session's own transcript sources are therefore always disjoint file sets;
//! `zirv agent`'s own cost is counted exactly once, via `delegations.jsonl`.
//!
//! Cost discipline: reading megabytes of subagent transcripts on every
//! dashboard tick would be unacceptable, so [`fingerprint`] answers "has
//! anything actually changed" from `stat` metadata alone (`len`+`mtime`,
//! never file contents) -- the same "unchanged fingerprint, skip the
//! re-fold" discipline `dash::FactsCache` already applied to the delegation
//! ledger alone, now covering the transcript sources too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::adapters::{self, AgentAdapter};
use super::event::{SessionId, SessionRef, TranscriptUsage};
use super::log::DelegationRow;
use super::price::{self, PriceTable};
use super::sessions;
use super::state::StateDir;
use super::window;

/// One model's summed usage across however many deduplicated assistant
/// responses [`session_transcript_usage`] folded into it -- `model` is
/// `None` for a row that carried no `message.model` field at all, and
/// `messages` is the deduplicated response count (never a raw row count: a
/// modern Claude Code response can split across several transcript rows all
/// repeating the same `usage` object, and counting each row would inflate
/// both the token sums and the "skipped" count below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    pub model: Option<String>,
    pub usage: TranscriptUsage,
    pub messages: u64,
}

/// The result of reading one session's own transcript sources -- see this
/// module's own doc comment for exactly which files that is (and, just as
/// importantly, which it structurally cannot be: a delegated `zirv agent`
/// worker's own transcript).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptFold {
    pub buckets: Vec<ModelUsage>,
    /// Whether at least one transcript FILE was actually read, even if it
    /// contributed zero in-scope assistant rows. Issue #457 item 3: a
    /// session with a real transcript but nothing priced (yet) must still
    /// render a dollar figure, never fall back to the "no source at all"
    /// placeholder -- that distinction lives here, not in `buckets` being
    /// non-empty.
    pub source_present: bool,
}

/// This session's own priced total -- see [`fold_session_spend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionSpend {
    /// `None` only when NEITHER a transcript source (main or subagent) NOR a
    /// matching delegation row exists at all -- the one case the footer/
    /// status line may legitimately render as `--`. Once any source exists
    /// this is `Some`, even `Some(0)` when every contributing message
    /// happened to price as unknown (`<synthetic>`, or a model this table
    /// has never priced) -- `skipped_messages` is how that is told apart
    /// from "genuinely nothing to report", never by hiding the number again.
    pub cost_micros: Option<u64>,
    /// How many deduplicated assistant messages / delegation rows contributed
    /// nothing to `cost_micros` because their model priced as unknown
    /// (`price::price`'s own "None, never 0" contract) -- exposed so a
    /// caller can say "$0.00 (12 messages skipped, unpriced model)" rather
    /// than silently looking like nothing happened.
    pub skipped_messages: u64,
    /// How many of the counted delegation rows did not end `"ok"` -- unrelated
    /// to pricing, kept only because the dashboard's aggregate row renders it
    /// alongside the cost cell.
    pub delegation_failed: u64,
}

/// The seat transcript backing `session_short`'s own "this session" spend,
/// resolved the same way an operator would resolve it by hand: `sessions/
/// <short>.json` names the session id Claude Code itself minted plus the
/// repo it was launched in ([`sessions::load_record`]), and those two are
/// exactly [`SessionRef`]'s own fields -- so `ClaudeAdapter::transcript_path`
/// (the one function in this codebase that already knows how to turn a
/// `SessionRef` into a real path on disk, scan-fallback included) resolves
/// it without this module reimplementing any of that.
///
/// `None` when there is no registered record for `session_short` at all, or
/// its `agent` is not `claude`: native Task subagents are a Claude Code
/// feature, so every other harness's delegated work is already fully
/// accounted for through `delegations.jsonl` alone, unchanged.
pub fn resolve_transcript(state: &StateDir, session_short: &str) -> Option<PathBuf> {
    let record = sessions::load_record(state, session_short)?;
    let claude = adapters::claude::ClaudeAdapter::new(None);
    if record.agent != claude.name() {
        return None;
    }
    let session_ref = SessionRef {
        id: SessionId::parse(&record.session),
        cwd: record.repo,
    };
    Some(claude.transcript_path(&session_ref))
}

/// Reads `transcript`'s own assistant rows plus every native subagent
/// transcript under its `subagents/` directory, bucketed by model. `since`,
/// when set, additionally requires a row's own `timestamp` to be no older
/// than it (a row with no parseable timestamp is skipped in that case,
/// rather than guessed into or out of the window); `None` counts every row
/// regardless of age, for the unbounded "this session" query.
///
/// Bounded the same way `adapters::claude::subagent_transcript_usage`
/// already bounds this same directory (issue #457's own cost concern: a
/// long-lived session can accumulate a subagent directory in the tens of
/// megabytes, and re-reading all of it on every dashboard tick would not be
/// acceptable) -- newest files first, capped at
/// `adapters::claude::MAX_SUBAGENT_TRANSCRIPTS` files and
/// `adapters::claude::MAX_SUBAGENT_BYTES` total, so a directory that has
/// outgrown the caps still keeps whatever a recent phase actually wrote to,
/// rather than reading in filesystem order and stalling on whichever cap
/// hits first. Paired with [`fingerprint`]'s own metadata-only cache key,
/// this function itself is only ever called again once something has
/// actually changed.
pub fn session_transcript_usage(transcript: Option<&Path>, since: Option<u64>) -> TranscriptFold {
    let mut buckets: BTreeMap<Option<String>, (TranscriptUsage, u64)> = BTreeMap::new();
    let mut source_present = false;

    let Some(transcript) = transcript else {
        return TranscriptFold::default();
    };

    if let Ok(text) = std::fs::read_to_string(transcript) {
        source_present = true;
        fold_model_usage(&mut buckets, &text, since, |row| {
            row.get("isSidechain").and_then(Value::as_bool) != Some(true)
        });
    }

    if let Some(dir) = adapters::claude::subagents_dir(transcript) {
        let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .filter_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                Some((
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    meta.len(),
                    path,
                ))
            })
            .collect();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));

        let mut files = 0usize;
        let mut bytes = 0u64;
        for (_, len, path) in entries {
            if files >= adapters::claude::MAX_SUBAGENT_TRANSCRIPTS
                || bytes >= adapters::claude::MAX_SUBAGENT_BYTES
            {
                break;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            files += 1;
            bytes = bytes.saturating_add(len);
            source_present = true;
            fold_model_usage(&mut buckets, &text, since, |_| true);
        }
    }

    TranscriptFold {
        buckets: buckets
            .into_iter()
            .map(|(model, (usage, messages))| ModelUsage {
                model,
                usage,
                messages,
            })
            .collect(),
        source_present,
    }
}

/// The shared fold behind both branches of [`session_transcript_usage`]:
/// every assistant row `keep` accepts, deduplicated by
/// `adapters::claude::response_identity` exactly like
/// `adapters::claude::fold_usage_rows` already dedups for the untyped
/// (model-blind) transcript readers, so this can never disagree with them
/// about what counts as one response's usage -- just bucketed by model
/// rather than summed into one total.
fn fold_model_usage(
    buckets: &mut BTreeMap<Option<String>, (TranscriptUsage, u64)>,
    jsonl: &str,
    since: Option<u64>,
    keep: impl Fn(&Value) -> bool,
) {
    let mut last_id: Option<String> = None;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") || !keep(&row) {
            continue;
        }
        if let Some(since) = since {
            let at = row
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(window::parse_iso8601_utc);
            if !at.is_some_and(|at| at >= since) {
                continue;
            }
        }
        let Some(message) = row.get("message") else {
            continue;
        };
        let Some(usage_val) = message.get("usage") else {
            continue;
        };
        let id = adapters::claude::response_identity(&row).map(str::to_string);
        if id.is_some() && id == last_id {
            continue;
        }
        last_id = id;
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let usage = adapters::claude::usage_categories(usage_val);
        let entry = buckets.entry(model).or_default();
        entry.0.input_tokens = entry.0.input_tokens.saturating_add(usage.input_tokens);
        entry.0.cache_creation_input_tokens = entry
            .0
            .cache_creation_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        entry.0.cache_read_input_tokens = entry
            .0
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        entry.0.output_tokens = entry.0.output_tokens.saturating_add(usage.output_tokens);
        entry.1 += 1;
    }
}

/// The one fold both `zirv ctx status` and the dashboard footer call: prices
/// `transcript`'s own buckets plus `delegation_rows` (already read off
/// `delegations.jsonl` -- unfiltered; this applies whichever filters below
/// itself), into one total.
///
/// `session_short`, when `Some`, restricts delegation rows to
/// `parent_session == session_short` -- "this session"'s own delegated work.
/// `None` counts every delegation row regardless of who spawned it, for the
/// machine-wide "this 5h window" query `status::spend_status_line` also
/// renders (its transcript half stays scoped to one session's own sources
/// even so -- see this module's own doc comment on why a machine-wide
/// transcript walk is not attempted). `since`, when `Some`, additionally
/// requires a delegation row's own `ts` to be no older than it; `transcript`
/// is assumed to already be filtered to the same window by whoever built it
/// (see [`session_transcript_usage`]'s own `since` parameter).
pub fn fold_session_spend(
    delegation_rows: &[DelegationRow],
    session_short: Option<&str>,
    since: Option<u64>,
    transcript: &TranscriptFold,
    table: &PriceTable,
) -> SessionSpend {
    let mut cost_micros: u64 = 0;
    let mut skipped_messages: u64 = 0;
    let mut any_source = transcript.source_present;

    for bucket in &transcript.buckets {
        match bucket
            .model
            .as_deref()
            .and_then(|model| price::price(model, &bucket.usage, table))
        {
            Some(cost) => cost_micros = cost_micros.saturating_add(cost),
            None => skipped_messages = skipped_messages.saturating_add(bucket.messages),
        }
    }

    let mut delegation_failed = 0u64;
    for row in delegation_rows {
        if let Some(short) = session_short
            && row.parent_session != short
        {
            continue;
        }
        if let Some(since) = since
            && row.ts < since
        {
            continue;
        }
        any_source = true;
        if row.outcome != "ok" {
            delegation_failed += 1;
        }
        let usage = TranscriptUsage {
            input_tokens: row.input_tokens,
            cache_creation_input_tokens: row.cache_creation_input_tokens,
            cache_read_input_tokens: row.cache_read_input_tokens,
            output_tokens: row.output_tokens,
        };
        match row
            .model
            .as_deref()
            .and_then(|model| price::price(model, &usage, table))
        {
            Some(cost) => cost_micros = cost_micros.saturating_add(cost),
            None => skipped_messages += 1,
        }
    }

    SessionSpend {
        cost_micros: any_source.then_some(cost_micros),
        skipped_messages,
        delegation_failed,
    }
}

/// A cheap-to-compute identity of every file [`fold_session_spend`]'s own
/// inputs would need to be re-read from for `session_short`: the seat's own
/// transcript, its `subagents/` directory, and the delegation ledger --
/// metadata only (`len`+`mtime`/count), never file contents. [`dash::
/// FactsCache`]'s own dash-tick cache key: a session whose transcript and
/// subagent directory have not produced a single new byte since the last
/// tick costs nothing beyond a handful of `stat` calls to re-confirm that,
/// no matter how large those files have grown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpendFingerprint {
    pub(crate) delegations: (u64, u64),
    pub(crate) transcript: (u64, u64),
    pub(crate) subagents: (usize, u64, u64),
}

pub fn fingerprint(state: &StateDir, transcript: Option<&Path>) -> SpendFingerprint {
    SpendFingerprint {
        delegations: file_stat(&state.logs().join(super::log::DELEGATION_FILE)),
        transcript: transcript.map(file_stat).unwrap_or_default(),
        subagents: transcript
            .and_then(adapters::claude::subagents_dir)
            .map(|dir| dir_stat(&dir))
            .unwrap_or_default(),
    }
}

fn file_stat(path: &Path) -> (u64, u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, 0);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (meta.len(), mtime)
}

fn dir_stat(dir: &Path) -> (usize, u64, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0, 0);
    };
    let mut count = 0usize;
    let mut total_len = 0u64;
    let mut max_mtime = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        count += 1;
        total_len = total_len.saturating_add(meta.len());
        if let Some(mtime) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
        {
            max_mtime = max_mtime.max(mtime);
        }
    }
    (count, total_len, max_mtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_row(model: &str, id: &str, at: &str, input: u64, output: u64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"isSidechain\":false,\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}}}\n"
        )
    }

    fn sidechain_row(model: &str, id: &str, at: &str, input: u64, output: u64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"isSidechain\":true,\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}}}\n"
        )
    }

    /// The load-bearing fixture: a seat transcript with one sonnet response
    /// plus one native subagent transcript (its own file under
    /// `<session>/subagents/`) with one haiku response. Both must be summed
    /// into ONE session total -- issue #457's own complaint was that native
    /// subagent spend was invisible entirely.
    #[test]
    fn a_seat_transcript_and_one_subagent_transcript_sum_together() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path().join("proj").join("sess123");
        std::fs::create_dir_all(session_dir.join("subagents")).expect("mkdir");
        let main = session_dir.parent().unwrap().join("sess123.jsonl");
        std::fs::write(
            &main,
            assistant_row(
                "claude-sonnet-5",
                "m1",
                "2026-09-01T00:00:00Z",
                1_000_000,
                0,
            ),
        )
        .expect("write main");
        std::fs::write(
            session_dir.join("subagents").join("agent-1.jsonl"),
            sidechain_row("claude-haiku-5", "m2", "2026-09-01T00:00:01Z", 0, 1_000_000),
        )
        .expect("write subagent");

        let fold = session_transcript_usage(Some(&main), None);
        assert!(fold.source_present);
        assert_eq!(
            fold.buckets.len(),
            2,
            "sonnet and haiku are distinct buckets"
        );

        let table = price::built_in_table();
        let spend = fold_session_spend(&[], None, None, &fold, &table);
        // 1M sonnet input @ $3/M = $3.00; 1M haiku output @ $4/M = $4.00.
        assert_eq!(
            spend.cost_micros,
            Some(7_000_000),
            "seat sonnet input plus subagent haiku output, summed: {spend:?}"
        );
        assert_eq!(spend.skipped_messages, 0);
    }

    /// The ledger-only case (no transcript at all) must keep working exactly
    /// as `status::spend_status_line`/the dashboard footer already did before
    /// issue #457 -- a session with delegations but no resolvable transcript
    /// still reports a real cost, not `--`.
    #[test]
    fn a_ledger_only_session_with_no_transcript_still_prices_correctly() {
        let table = price::built_in_table();
        let rows = vec![DelegationRow {
            ts: 1,
            session: "child".to_string(),
            parent_session: "orch0001".to_string(),
            work_group_id: None,
            agent: "claude".to_string(),
            model: Some("sonnet".to_string()),
            input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 1_000_000,
            wall_ms: 1_000,
            exit_code: 0,
            outcome: "ok".to_string(),
            mode: None,
            task_class: None,
            principal: "root".to_string(),
            envelope_sha256: None,
        }];
        let spend = fold_session_spend(
            &rows,
            Some("orch0001"),
            None,
            &TranscriptFold::default(),
            &table,
        );
        assert_eq!(
            spend.cost_micros,
            Some(15_000_000),
            "1M sonnet output @ $15/M"
        );
        assert_eq!(spend.delegation_failed, 0);
    }

    /// Issue #457 item 3: a transcript that exists but has nothing priced
    /// yet (or only unpriced models) must still report `Some(0)` -- never
    /// fall back to `None`/`--` just because nothing has been priced.
    #[test]
    fn a_present_but_unpriced_transcript_reports_some_zero_not_none() {
        let table = price::built_in_table();
        let fold = TranscriptFold {
            buckets: vec![ModelUsage {
                model: Some("<synthetic>".to_string()),
                usage: TranscriptUsage {
                    input_tokens: 1_000,
                    ..TranscriptUsage::default()
                },
                messages: 3,
            }],
            source_present: true,
        };
        let spend = fold_session_spend(&[], None, None, &fold, &table);
        assert_eq!(
            spend.cost_micros,
            Some(0),
            "a real source with nothing priced is $0.00, never `--`"
        );
        assert_eq!(spend.skipped_messages, 3);
    }

    /// No source at all (empty ledger, no transcript) is the one case that
    /// legitimately renders `--`/`n/a` upstream.
    #[test]
    fn no_source_at_all_reports_none() {
        let table = price::built_in_table();
        let spend = fold_session_spend(
            &[],
            Some("orch0001"),
            None,
            &TranscriptFold::default(),
            &table,
        );
        assert_eq!(spend.cost_micros, None);
        assert_eq!(spend.skipped_messages, 0);
    }

    /// `session_short` scopes delegation rows to this session's own; a
    /// different session's row (and a different session's failure) must not
    /// bleed into this one's total or `delegation_failed` count.
    #[test]
    fn session_short_scopes_out_another_sessions_delegation_rows() {
        let table = price::built_in_table();
        let rows = vec![
            DelegationRow {
                ts: 1,
                session: "child-a".to_string(),
                parent_session: "orch0001".to_string(),
                work_group_id: None,
                agent: "claude".to_string(),
                model: Some("sonnet".to_string()),
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1_000_000,
                wall_ms: 1_000,
                exit_code: 0,
                outcome: "ok".to_string(),
                mode: None,
                task_class: None,
                principal: "root".to_string(),
                envelope_sha256: None,
            },
            DelegationRow {
                ts: 1,
                session: "child-b".to_string(),
                parent_session: "other999".to_string(),
                work_group_id: None,
                agent: "claude".to_string(),
                model: Some("sonnet".to_string()),
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1_000_000,
                wall_ms: 1_000,
                exit_code: 0,
                outcome: "failed".to_string(),
                mode: None,
                task_class: None,
                principal: "root".to_string(),
                envelope_sha256: None,
            },
        ];
        let spend = fold_session_spend(
            &rows,
            Some("orch0001"),
            None,
            &TranscriptFold::default(),
            &table,
        );
        assert_eq!(
            spend.cost_micros,
            Some(15_000_000),
            "only orch0001's own row"
        );
        assert_eq!(
            spend.delegation_failed, 0,
            "other999's failure is not this session's"
        );
    }

    /// `since` time-boxes both the transcript fold and the delegation fold
    /// -- the "this 5h window" query's own shape.
    #[test]
    fn since_filters_both_transcript_rows_and_delegation_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("sess123.jsonl");
        let jsonl = format!(
            "{}{}",
            assistant_row(
                "claude-sonnet-5",
                "old",
                "2020-01-01T00:00:00Z",
                1_000_000,
                0
            ),
            assistant_row(
                "claude-sonnet-5",
                "new",
                "2030-01-01T00:00:00Z",
                1_000_000,
                0
            ),
        );
        std::fs::write(&main, jsonl).expect("write");

        // A `since` between the two timestamps keeps only the newer row.
        let since = window::parse_iso8601_utc("2025-01-01T00:00:00Z").expect("parse");
        let fold = session_transcript_usage(Some(&main), Some(since));

        let old_delegation = DelegationRow {
            ts: since - 1,
            session: "child-old".to_string(),
            parent_session: "orch0001".to_string(),
            work_group_id: None,
            agent: "claude".to_string(),
            model: Some("sonnet".to_string()),
            input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 1_000_000,
            wall_ms: 1_000,
            exit_code: 0,
            outcome: "ok".to_string(),
            mode: None,
            task_class: None,
            principal: "root".to_string(),
            envelope_sha256: None,
        };
        let new_delegation = DelegationRow {
            ts: since + 1,
            session: "child-new".to_string(),
            ..old_delegation.clone()
        };
        let rows = vec![old_delegation, new_delegation];

        let table = price::built_in_table();
        let spend = fold_session_spend(&rows, None, Some(since), &fold, &table);
        // Transcript: only the newer 1M sonnet input row ($3.00). Delegations:
        // only the newer 1M sonnet output row ($15.00). The older row of
        // each source must be excluded by `since`.
        assert_eq!(
            spend.cost_micros,
            Some(18_000_000),
            "only the in-window row of each source: {spend:?}"
        );
    }

    #[test]
    fn resolve_transcript_is_none_with_no_registered_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(resolve_transcript(&state, "nosuch1"), None);
    }

    #[test]
    fn fingerprint_changes_when_the_delegation_ledger_grows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let before = fingerprint(&state, None);
        crate::commands::ctx::log::append_delegation(
            &state,
            &crate::commands::ctx::log::Delegation {
                ts: 1,
                session: "child",
                parent_session: "orch0001",
                work_group_id: None,
                agent: "claude",
                model: Some("sonnet"),
                input_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 0,
                wall_ms: 1,
                exit_code: 0,
                outcome: "ok",
                mode: None,
                task_class: None,
                principal: "root",
                envelope_sha256: None,
            },
        )
        .expect("append");
        let after = fingerprint(&state, None);
        assert_ne!(before, after, "an appended row must change the fingerprint");
    }
}
