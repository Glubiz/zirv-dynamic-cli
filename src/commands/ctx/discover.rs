//! `zirv ctx discover` (issue #423): lists the largest `Bash` tool results in
//! recent sessions that reached the model UNCOMPACTED, bucketed by why.
//!
//! The mechanism: scan claude transcripts for `Bash` `tool_use`/`tool_result`
//! pairs above `[output] compact_min_bytes`, then join each one against the
//! compaction ledger (`ledger.rs`, issue #422) by `tool_use_id`. A match is
//! MEASURED -- the hook actually looked at this exact result, and the
//! ledger's own `outcome` column says what it did. No match is ESTIMATED --
//! nothing was ever recorded for this `tool_use_id`, so [`estimate_reason`]
//! re-runs today's `output::classify_compaction` against the command to say
//! why it would (or would not) be compacted under the CURRENT config. Never
//! guesses which one a row is: the ledger join is the only thing that ever
//! decides MEASURED vs ESTIMATED (see [`classify_rows`]'s own doc comment --
//! this is deliberately the one place review attention belongs).
//!
//! Scope decision: only claude transcripts are scanned (`search::
//! claude_candidates`, the same discovery `zirv ctx search`/`measure.rs`
//! already share) -- `hook::run_posttool`/`run_pretool` are claude-only, so a
//! codex (or other harness) transcript can never have a ledger row to begin
//! with, and pairing codex's differently-shaped rollout JSONL would need its
//! own, unverified extraction. `--all` widens `claude_candidates`' own scope
//! from the current repository's project directory to every claude project
//! (`~/.claude/projects/*`), rather than to other harnesses.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::config::{CtxConfig, env_from_process};
use super::output::{CompactionScope, bare_program, classify_compaction};
use super::state::{self, StateDir};
use super::{CtxResult, ledger, search};

#[derive(Debug, Clone, clap::Args)]
pub struct DiscoverArgs {
    /// Restrict to transcripts modified within this window, e.g. `24h`,
    /// `7d`, `30d`, or a bare number of seconds.
    #[arg(long, default_value = "7d")]
    pub since: String,
    /// Widen scope to every claude project, not just the current
    /// repository's own (mirrors `zirv ctx search --all-repos`).
    #[arg(long, default_value_t = false)]
    pub all: bool,
}

/// One large `Bash` tool result found in a transcript, before it is joined
/// against the ledger.
struct RawResult {
    tool_use_id: String,
    program: String,
    command: String,
    bytes: u64,
}

/// Why a row is bucketed the way it is -- see this module's own doc comment
/// for why the ledger join, and nothing else, decides which variant a row
/// gets.
enum RowProvenance {
    /// The ledger has a row for this exact `tool_use_id`: `outcome` is that
    /// row's own `ledger::Outcome::as_str()` value.
    Measured { outcome: String },
    /// No ledger row exists for this `tool_use_id` -- `reason` is
    /// [`estimate_reason`]'s best account of why, from today's config.
    Estimated { reason: String },
}

struct DiscoverRow {
    program: String,
    bytes: u64,
    classification: RowProvenance,
}

/// The raw text of a `tool_result` block's `content`, falling back to a
/// JSON-stringified form for a non-string (array/object) content shape.
/// Deliberately its own small copy rather than a shared helper (no drive-by
/// refactor of `adapters::claude`'s private, identically-shaped
/// `tool_result_text`) -- the same accepted duplication this codebase
/// already gives its own small, single-purpose text extractors.
fn tool_result_text(block: &Value) -> String {
    block
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            block
                .get("content")
                .map(Value::to_string)
                .unwrap_or_default()
        })
}

/// Every `Bash` tool result in `jsonl` at or above `min_bytes`, paired to its
/// own command by `tool_use_id` -- the identical pending-map pairing
/// `adapters::claude::structural_context` already uses for its own handoff
/// verification section (matched by id, however many other tool calls fall
/// between the two rows), just kept as its own small copy here rather than
/// exposing that private pairing for a second, unrelated caller.
fn extract_bash_results(jsonl: &str, min_bytes: u64) -> Vec<RawResult> {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = row.get("message").cloned().unwrap_or(Value::Null);
        match row.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let tool_name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    if !tool_name.eq_ignore_ascii_case("Bash") {
                        continue;
                    }
                    if let (Some(id), Some(command)) = (
                        block.get("id").and_then(Value::as_str),
                        block
                            .get("input")
                            .and_then(|input| input.get("command"))
                            .and_then(Value::as_str),
                    ) {
                        pending.insert(id.to_string(), command.to_string());
                    }
                }
            }
            Some("user") => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
                    let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(command) = pending.remove(tool_use_id) else {
                        continue;
                    };
                    let bytes = tool_result_text(block).len() as u64;
                    if bytes < min_bytes {
                        continue;
                    }
                    let program = command
                        .split_whitespace()
                        .next()
                        .map(bare_program)
                        .unwrap_or_default();
                    out.push(RawResult {
                        tool_use_id: tool_use_id.to_string(),
                        program,
                        command,
                        bytes,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Why an ESTIMATED row (no ledger match) most likely reached the model
/// uncompacted, re-derived from TODAY's config -- never a guess about what
/// happened historically, only an account of what would happen now. Mirrors
/// `hook::run_posttool`'s own threshold-per-scope logic exactly, so a row
/// this function calls "would compact today" is one `run_posttool` really
/// would replace under the current config.
fn estimate_reason(command: &str, bytes: u64, cfg: &CtxConfig) -> String {
    let scope = classify_compaction(command, &cfg.output.verbatim);
    match scope {
        CompactionScope::Verbatim => "verbatim reader".to_string(),
        CompactionScope::Known if bytes < cfg.output.compact_min_bytes as u64 => {
            "below known threshold".to_string()
        }
        CompactionScope::Generic if bytes < cfg.output.compact_generic_min_bytes as u64 => {
            "below generic threshold".to_string()
        }
        CompactionScope::Diff if bytes < cfg.output.diff_max_bytes as u64 => {
            "below diff threshold".to_string()
        }
        // Today's config would compact this, yet nothing was ever recorded
        // for it -- the likely explanations are exactly these two.
        _ => "hook not installed / no decision recorded".to_string(),
    }
}

/// Joins `raw` against `ledger_outcomes` (keyed by `tool_use_id`) to decide
/// MEASURED vs ESTIMATED for each row -- see this module's own doc comment
/// for why the ledger join is the ONLY thing that ever decides this: a row
/// with a ledger entry has direct evidence of what the hook actually did to
/// it; a row without one has none, no matter how confidently
/// `estimate_reason` can explain what today's config WOULD do.
fn classify_rows(
    raw: Vec<RawResult>,
    ledger_outcomes: &HashMap<String, String>,
    cfg: &CtxConfig,
) -> Vec<DiscoverRow> {
    raw.into_iter()
        .map(|r| {
            let classification = match ledger_outcomes.get(&r.tool_use_id) {
                Some(outcome) => RowProvenance::Measured {
                    outcome: outcome.clone(),
                },
                None => RowProvenance::Estimated {
                    reason: estimate_reason(&r.command, r.bytes, cfg),
                },
            };
            DiscoverRow {
                program: r.program,
                bytes: r.bytes,
                classification,
            }
        })
        .collect()
}

/// Renders `n` bytes as a short human-readable size -- the identical
/// rounding/unit choice `ledger.rs`'s own `human_bytes` uses, kept as its own
/// small copy for the same reason that module's doc comment already gives:
/// this exact formatter only ever needs to match its own callers.
fn human_bytes(n: u64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("GiB", 1024.0 * 1024.0 * 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("KiB", 1024.0),
        ("B", 1.0),
    ];
    let bytes = n as f64;
    for (unit, size) in UNITS {
        if bytes >= size {
            if unit == "B" {
                return format!("{n} B");
            }
            return format!("{:.1} {unit}", bytes / size);
        }
    }
    format!("{n} B")
}

fn reason_label(c: &RowProvenance) -> String {
    match c {
        RowProvenance::Measured { outcome } => format!("measured: {outcome}"),
        RowProvenance::Estimated { reason } => format!("estimated: {reason}"),
    }
}

/// One suggested next step per reason bucket -- `None` when there is nothing
/// actionable to say (a healthy outcome, or a reason this build does not yet
/// have a specific hint for). Never proposes a fix `discover` could apply
/// itself: this command only ever reports.
fn hint_for_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "estimated: verbatim reader" | "measured: verbatim" => {
            Some("readers are never compacted regardless of size -- expected, no action needed.")
        }
        "estimated: below known threshold" => Some(
            "lower `output.compact_min_bytes` if these known-shape results are worth compacting sooner.",
        ),
        "estimated: below generic threshold" => Some(
            "raise or lower `output.compact_generic_min_bytes` depending on whether these are \
             worth keeping verbatim.",
        ),
        "estimated: below diff threshold" => {
            Some("lower `output.diff_max_bytes` if these diffs are worth summarizing sooner.")
        }
        "estimated: hook not installed / no decision recorded" => Some(
            "today's config would compact these but nothing was ever recorded -- confirm the \
             PostToolUse hook is installed (`zirv setup status`).",
        ),
        "measured: below_threshold" => Some(
            "raise `output.compact_min_bytes`/`compact_generic_min_bytes` if these are worth \
             compacting.",
        ),
        "measured: disabled" => {
            Some("`output.compact` is off -- enable it to start compacting large Bash results.")
        }
        "measured: offloaded" => {
            Some("claude's own harness already spilled this output to a file -- nothing to change.")
        }
        "measured: persist_failed" => Some(
            "the hook could not persist a summary for this result -- check the state dir's own \
             disk space/permissions.",
        ),
        "measured: not_smaller" => {
            Some("a summary was produced but was not smaller than the raw output.")
        }
        _ => None,
    }
}

fn render<W: Write>(rows: &[DiscoverRow], args: &DiscoverArgs, w: &mut W) -> CtxResult<i32> {
    if rows.is_empty() {
        writeln!(
            w,
            "no large Bash results found in the scanned transcripts, --since {}",
            args.since
        )?;
        return Ok(0);
    }

    // "Uncompacted" excludes only a MEASURED row whose outcome was actually
    // `compacted` -- that one reached the model as a summary, not raw.
    // Everything else (measured with any other outcome, or estimated at
    // all) genuinely reached the model uncompacted -- see this module's own
    // doc comment on why an estimated row is always uncompacted by
    // construction.
    let uncompacted: Vec<&DiscoverRow> = rows
        .iter()
        .filter(|r| !matches!(&r.classification, RowProvenance::Measured { outcome } if outcome == "compacted"))
        .collect();

    writeln!(
        w,
        "uncompacted Bash results, --since {} ({} of {} large results)",
        args.since,
        uncompacted.len(),
        rows.len()
    )?;

    let mut by_program: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for r in &uncompacted {
        let entry = by_program.entry(r.program.clone()).or_insert((0, 0));
        entry.0 += r.bytes;
        entry.1 += 1;
    }
    let mut programs: Vec<(String, (u64, u64))> = by_program.into_iter().collect();
    programs.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    writeln!(w, "\ntop programs by uncompacted bytes:")?;
    if programs.is_empty() {
        writeln!(w, "  none")?;
    } else {
        for (program, (bytes, count)) in programs.iter().take(10) {
            writeln!(
                w,
                "  {:<20} {:>10}  ({count} rows)",
                program,
                human_bytes(*bytes)
            )?;
        }
    }

    let mut by_reason: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for r in &uncompacted {
        let entry = by_reason
            .entry(reason_label(&r.classification))
            .or_insert((0, 0));
        entry.0 += r.bytes;
        entry.1 += 1;
    }
    writeln!(w, "\nby reason:")?;
    for (reason, (bytes, count)) in &by_reason {
        writeln!(
            w,
            "  {:<55} {count:>4} rows  {}",
            reason,
            human_bytes(*bytes)
        )?;
    }

    let hints: Vec<(&String, &'static str)> = by_reason
        .keys()
        .filter_map(|reason| hint_for_reason(reason).map(|hint| (reason, hint)))
        .collect();
    if !hints.is_empty() {
        writeln!(w, "\nhints:")?;
        for (reason, hint) in hints {
            writeln!(w, "  - {reason}: {hint}")?;
        }
    }
    Ok(0)
}

pub fn run<W: Write>(args: &DiscoverArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let state = StateDir::resolve(&env)?;
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, &env)?;
    run_with(&state, &cfg, args, &repo, w, state::now_secs())
}

pub fn run_with<W: Write>(
    state: &StateDir,
    cfg: &CtxConfig,
    args: &DiscoverArgs,
    repo: &Path,
    w: &mut W,
    now: u64,
) -> CtxResult<i32> {
    let since_secs = super::spend::parse_since(&args.since).ok_or_else(|| {
        format!(
            "--since '{}': expected a duration like 30m, 24h, or 7d (or a bare number of \
             seconds)",
            args.since
        )
    })?;
    let since_ts = now.saturating_sub(since_secs);
    let min_bytes = cfg.output.compact_min_bytes as u64;

    let mut raw = Vec::new();
    for (path, _source) in search::claude_candidates(repo, args.all) {
        let modified_secs = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        if modified_secs.is_none_or(|secs| secs < since_ts) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        raw.extend(extract_bash_results(&text, min_bytes));
    }

    let ledger_outcomes = ledger::outcomes_by_tool_use_id(state, since_ts);
    let rows = classify_rows(raw, &ledger_outcomes, cfg);
    render(&rows, args, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal claude-shaped transcript with three large `Bash`
    /// results: one `cargo test` command (paired with a program name so the
    /// top-programs table has something to show), one verbatim `cat`, and
    /// one `curl` result at/above `compact_min_bytes` but below
    /// `compact_generic_min_bytes` -- `curl` is neither a known progress-log
    /// program nor a verbatim reader, so it lands in `CompactionScope::
    /// Generic`.
    fn fixture_transcript(big: &str, below: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_use\",\"id\":\"tu_compacted\",\"name\":\"Bash\",\"input\":{{\"command\":\"cargo test\"}}}},\
             {{\"type\":\"tool_use\",\"id\":\"tu_verbatim\",\"name\":\"Bash\",\"input\":{{\"command\":\"cat big.log\"}}}},\
             {{\"type\":\"tool_use\",\"id\":\"tu_below\",\"name\":\"Bash\",\"input\":{{\"command\":\"curl https://example.com\"}}}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu_compacted\",\"content\":\"{big}\"}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu_verbatim\",\"content\":\"{big}\"}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu_below\",\"content\":\"{below}\"}}\
             ]}}}}\n"
        )
    }

    /// The known trap this issue names explicitly: a fixture with three
    /// large results -- one carrying a ledger row (measured), one verbatim
    /// reader and one below the generic threshold (both estimated) -- must
    /// classify as exactly one measured row and two estimated rows, with
    /// the right reasons on each estimated one.
    #[test]
    fn measured_vs_estimated_classification_matches_the_ledger_join_exactly() {
        let cfg = CtxConfig::default();
        let big = "x".repeat(cfg.output.compact_generic_min_bytes + 1_000);
        // Below `compact_generic_min_bytes` but still at/above
        // `compact_min_bytes`, so `extract_bash_results` still picks it up.
        let below = "y".repeat(cfg.output.compact_min_bytes + 10);

        let jsonl = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_use\",\"id\":\"tu_compacted\",\"name\":\"Bash\",\"input\":{{\"command\":\"cargo test\"}}}},\
             {{\"type\":\"tool_use\",\"id\":\"tu_verbatim\",\"name\":\"Bash\",\"input\":{{\"command\":\"cat big.log\"}}}},\
             {{\"type\":\"tool_use\",\"id\":\"tu_below\",\"name\":\"Bash\",\"input\":{{\"command\":\"curl https://example.com\"}}}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu_compacted\",\"content\":\"{big}\"}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu_verbatim\",\"content\":\"{big}\"}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu_below\",\"content\":\"{below}\"}}\
             ]}}}}\n"
        );

        let min_bytes = cfg.output.compact_min_bytes as u64;
        let raw = extract_bash_results(&jsonl, min_bytes);
        assert_eq!(
            raw.len(),
            3,
            "all three results are at/above compact_min_bytes: {}",
            raw.len()
        );

        let mut ledger_outcomes = HashMap::new();
        ledger_outcomes.insert("tu_compacted".to_string(), "compacted".to_string());

        let rows = classify_rows(raw, &ledger_outcomes, &cfg);
        assert_eq!(rows.len(), 3);

        let measured: Vec<&DiscoverRow> = rows
            .iter()
            .filter(|r| matches!(r.classification, RowProvenance::Measured { .. }))
            .collect();
        let estimated: Vec<&DiscoverRow> = rows
            .iter()
            .filter(|r| matches!(r.classification, RowProvenance::Estimated { .. }))
            .collect();
        assert_eq!(measured.len(), 1, "exactly one row has a ledger match");
        assert_eq!(estimated.len(), 2, "the other two have none");

        match &measured[0].classification {
            RowProvenance::Measured { outcome } => assert_eq!(outcome, "compacted"),
            _ => unreachable!(),
        }

        let reasons: Vec<String> = estimated
            .iter()
            .map(|r| match &r.classification {
                RowProvenance::Estimated { reason } => reason.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert!(
            reasons.contains(&"verbatim reader".to_string()),
            "got {reasons:?}"
        );
        assert!(
            reasons.contains(&"below generic threshold".to_string()),
            "got {reasons:?}"
        );
    }

    /// `run_with` end to end over a fixture transcript file: one measured
    /// (already compacted, excluded from the uncompacted totals) and two
    /// estimated rows, with a hint printed for each estimated reason.
    #[test]
    fn run_with_reports_uncompacted_results_bucketed_by_reason() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let big = "x".repeat(cfg.output.compact_generic_min_bytes + 1_000);
        let below = "y".repeat(cfg.output.compact_min_bytes + 10);
        let jsonl = fixture_transcript(&big, &below);

        let home = tmp.path().join("claude-home");
        let projects_root = home.join(".claude").join("projects");
        let project_dir = projects_root.join(super::super::permissions::claude_project_dir_name(
            tmp.path(),
        ));
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        std::fs::write(project_dir.join("sess-1.jsonl"), &jsonl).expect("write transcript");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        ledger::record(
            &state,
            &ledger::CompactionRow {
                ts: state::now_secs(),
                tool_use_id: "tu_compacted",
                session: "sess-1",
                repo: "repo-a",
                program: "cargo",
                bytes_in: big.len() as u64,
                bytes_out: 500,
                outcome: ledger::Outcome::Compacted,
                retrieval_id: Some("r1"),
            },
        );

        let args = DiscoverArgs {
            since: "7d".to_string(),
            all: false,
        };
        let mut out = Vec::new();
        let code =
            run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("(2 of 3 large results)"),
            "the compacted row is excluded from the uncompacted count: {text}"
        );
        assert!(text.contains("estimated: verbatim reader"), "{text}");
        assert!(
            text.contains("estimated: below generic threshold"),
            "a curl result below compact_generic_min_bytes with no ledger row: {text}"
        );
        assert!(text.contains("hints:"), "{text}");
    }

    /// An empty scan (no transcripts at all in the window) prints a clear
    /// "nothing found" line and exits 0, never an error.
    #[test]
    fn run_with_on_no_transcripts_prints_a_no_results_line_and_exits_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&tmp.path().join("claude-home"));
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let args = DiscoverArgs {
            since: "7d".to_string(),
            all: false,
        };
        let mut out = Vec::new();
        let code =
            run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("no large Bash results found"), "{text}");
    }
}
