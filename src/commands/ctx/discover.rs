//! `zirv ctx discover` (issue #423): lists the largest `Bash` tool results in
//! recent sessions that reached the model UNCOMPACTED, bucketed by why.
//!
//! The mechanism: scan claude transcripts for `Bash` `tool_use`/`tool_result`
//! pairs at or above the SMALLEST of `[output] compact_min_bytes`/
//! `compact_generic_min_bytes`/`diff_max_bytes` (see [`scan_min_bytes`] -- an
//! operator config where `compact_min_bytes` is not itself the smallest of
//! the three must never pre-filter out a result a wider scope would still
//! compact), then join each one against the
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

/// Applies one already-trimmed transcript line to `pending`/`out` -- the
/// single per-line rule [`extract_bash_results`] (a small in-memory string,
/// for the existing unit fixtures) and [`extract_bash_results_bounded`] (a
/// streamed, byte-budgeted `BufRead`, for `run_with`'s real transcript
/// files) both apply, kept as one copy so a change to the pairing rule can
/// never drift between the two callers.
fn apply_transcript_line(
    line: &str,
    pending: &mut HashMap<String, String>,
    min_bytes: u64,
    out: &mut Vec<RawResult>,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(row) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let message = row.get("message").cloned().unwrap_or(Value::Null);
    match row.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                return;
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
                return;
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

/// Every `Bash` tool result in `jsonl` at or above `min_bytes`, paired to its
/// own command by `tool_use_id` -- the identical pending-map pairing
/// `adapters::claude::structural_context` already uses for its own handoff
/// verification section (matched by id, however many other tool calls fall
/// between the two rows), just kept as its own small copy here rather than
/// exposing that private pairing for a second, unrelated caller. Whole-string
/// (never a byte budget of its own): only ever called by this module's small
/// in-memory unit fixtures -- `run_with`'s real transcript files go through
/// [`extract_bash_results_bounded`] instead.
#[cfg(test)]
fn extract_bash_results(jsonl: &str, min_bytes: u64) -> Vec<RawResult> {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for line in jsonl.lines() {
        apply_transcript_line(line, &mut pending, min_bytes, &mut out);
    }
    out
}

/// The same pairing as [`extract_bash_results`], but reading `reader` one
/// line at a time (never loading the whole transcript into memory, unlike
/// `std::fs::read_to_string`) and stopping once `byte_budget` worth of lines
/// has been consumed -- an unledgered multi-GB `--all` corpus must never OOM
/// or stall `discover` just to find its largest results. Returns the rows
/// found, how many bytes were actually consumed, and whether the budget cut
/// the file short (a corrupt/unreadable line stops the read the same way a
/// budget does -- both leave `out` with whatever was already found).
fn extract_bash_results_bounded<R: std::io::BufRead>(
    reader: R,
    min_bytes: u64,
    byte_budget: u64,
) -> (Vec<RawResult>, u64, bool) {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut consumed: u64 = 0;
    let mut truncated = false;
    for line in reader.lines() {
        let Ok(line) = line else {
            truncated = true;
            break;
        };
        // `BufRead::lines()` already stripped the newline this line ended
        // with; count it back in so `consumed` tracks bytes actually read
        // off disk, not bytes left after stripping.
        let line_bytes = line.len() as u64 + 1;
        if consumed.saturating_add(line_bytes) > byte_budget {
            truncated = true;
            break;
        }
        consumed += line_bytes;
        apply_transcript_line(&line, &mut pending, min_bytes, &mut out);
    }
    (out, consumed, truncated)
}

/// The floor `run_with_budget` pre-filters transcript rows against before
/// any of them are even paired with their command, let alone scope-
/// classified. Must be the SMALLEST of every per-scope threshold `estimate_
/// reason`/`hook::run_posttool` apply -- `compact_min_bytes` (Known),
/// `compact_generic_min_bytes` (Generic/Shape), and `diff_max_bytes` (Diff)
/// -- never just `compact_min_bytes` on its own: nothing in `CtxConfig`
/// enforces that `compact_min_bytes` IS the smallest, and an operator config
/// where it is not (e.g. `compact_min_bytes` raised past the default
/// `diff_max_bytes`) would otherwise pre-filter out a `git diff` result the
/// hook itself would still have compacted today, before `estimate_reason`
/// ever got a chance to classify it correctly. `Verbatim` has no byte
/// threshold at all (never compacted regardless of size) and is
/// deliberately left out of this minimum: a verbatim row this filter drops
/// was never going to be reported as anything but "estimated: verbatim
/// reader" either way.
fn scan_min_bytes(cfg: &CtxConfig) -> u64 {
    [
        cfg.output.compact_min_bytes as u64,
        cfg.output.compact_generic_min_bytes as u64,
        cfg.output.diff_max_bytes as u64,
    ]
    .into_iter()
    .min()
    .unwrap_or(0)
}

/// Why an ESTIMATED row (no ledger match) most likely reached the model
/// uncompacted, re-derived from TODAY's config -- never a guess about what
/// happened historically, only an account of what would happen now. Mirrors
/// `hook::run_posttool`'s own threshold-per-scope logic exactly, so a row
/// this function calls "would compact today" is one `run_posttool` really
/// would replace under the current config. `run_posttool` checks
/// `cfg.output.compact` BEFORE it ever classifies a scope (a command that
/// would otherwise be a verbatim reader, or otherwise clear every
/// threshold, still records `Disabled` when the gate is off) -- this
/// mirrors that exact order, so a disabled config never gets misreported as
/// "hook not installed" here.
fn estimate_reason(command: &str, bytes: u64, cfg: &CtxConfig) -> String {
    if !cfg.output.compact {
        return "compaction disabled (output.compact = false)".to_string();
    }
    let scope = classify_compaction(command, &cfg.output.verbatim, cfg.output.compact_search);
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
        CompactionScope::Shape if bytes < cfg.output.compact_generic_min_bytes as u64 => {
            "below generic threshold".to_string()
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
        "estimated: verbatim reader" | "measured: verbatim" => Some(
            "readers are never compacted regardless of size -- expected; for rg/grep/find/ls \
             results, `output.compact_search = true` opts them into a grouped shape.",
        ),
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
        "estimated: compaction disabled (output.compact = false)" => {
            Some("set output.compact = true to let large Bash results be compacted.")
        }
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

/// Total bytes [`run_with`] will read across every scanned transcript before
/// it stops -- `--all` widens scope to every claude project on the machine,
/// and a multi-GB corpus of uncompacted (by definition -- that is what this
/// command looks for) transcripts read whole would OOM or stall it. 256 MiB
/// comfortably covers a normal `--since` window while still bounding the
/// worst case.
const TRANSCRIPT_SCAN_BYTE_BUDGET: u64 = 256 * 1024 * 1024;

/// How many candidate transcript files [`run_with`] will open at most,
/// independent of [`TRANSCRIPT_SCAN_BYTE_BUDGET`] -- a `--all` scan of a
/// machine with thousands of small, stale transcripts must not pay a
/// filesystem `open` for every one of them just to find they are all empty.
const MAX_TRANSCRIPT_CANDIDATES: usize = 500;

/// What one bounded scan across candidate transcripts found -- [`render`]
/// only ever sees `raw`; `files_scanned`/`bytes_scanned`/`truncated` exist
/// purely so `run_with` can print one honest "stopped early" note when the
/// scan did not cover every candidate. `unreadable` counts candidates that
/// failed to open (e.g. a stale path, a permissions error) -- those are
/// never a truncation (the scan did not stop early because of them, they
/// simply contributed nothing) and get their own note instead.
struct ScanOutcome {
    raw: Vec<RawResult>,
    files_scanned: usize,
    bytes_scanned: u64,
    truncated: bool,
    unreadable: usize,
}

/// Scans `candidates` (path, mtime-seconds pairs) newest-first, stopping
/// once `max_files` have been opened or `byte_budget` bytes have been read --
/// whichever comes first. Newest-first so a truncated scan on a machine with
/// more history than the budget allows still favours the results an operator
/// most likely cares about right now. `truncated` is set only at a real
/// cutoff -- the file cap, the byte budget, or a single file alone
/// exceeding the remaining budget -- never merely because a candidate
/// failed to open; those are counted in `unreadable` instead so `run_with`
/// does not tell an operator to "narrow --since" when nothing was actually
/// cut off.
fn scan_candidates(
    mut candidates: Vec<(std::path::PathBuf, u64)>,
    min_bytes: u64,
    byte_budget: u64,
    max_files: usize,
) -> ScanOutcome {
    candidates.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));

    let mut raw = Vec::new();
    let mut files_scanned = 0usize;
    let mut bytes_scanned: u64 = 0;
    let mut truncated = false;
    let mut unreadable = 0usize;

    for (path, _mtime) in &candidates {
        if files_scanned >= max_files {
            truncated = true;
            break;
        }
        let remaining = byte_budget.saturating_sub(bytes_scanned);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let Ok(file) = std::fs::File::open(path) else {
            unreadable += 1;
            continue;
        };
        files_scanned += 1;
        let reader = std::io::BufReader::new(file);
        let (rows, consumed, file_truncated) =
            extract_bash_results_bounded(reader, min_bytes, remaining);
        bytes_scanned += consumed;
        raw.extend(rows);
        if file_truncated {
            truncated = true;
            break;
        }
    }

    ScanOutcome {
        raw,
        files_scanned,
        bytes_scanned,
        truncated,
        unreadable,
    }
}

pub fn run<W: Write>(args: &DiscoverArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let state = StateDir::resolve(&env)?;
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, &env)?;
    run_with(&state, &cfg, args, &repo, w, state::now_secs())
}

/// [`scan_candidates`]'s stopping rule, bundled into one value so
/// `run_with_budget` (already at its `clippy::too_many_arguments` limit
/// without it) takes it as a single parameter -- a test injects a tiny one
/// to observe the truncation note without needing a multi-GB fixture to
/// exceed the real default.
#[derive(Clone, Copy)]
struct ScanBudget {
    bytes: u64,
    max_files: usize,
}

impl ScanBudget {
    const DEFAULT: Self = Self {
        bytes: TRANSCRIPT_SCAN_BYTE_BUDGET,
        max_files: MAX_TRANSCRIPT_CANDIDATES,
    };
}

pub fn run_with<W: Write>(
    state: &StateDir,
    cfg: &CtxConfig,
    args: &DiscoverArgs,
    repo: &Path,
    w: &mut W,
    now: u64,
) -> CtxResult<i32> {
    run_with_budget(state, cfg, args, repo, w, now, ScanBudget::DEFAULT)
}

/// [`run_with`]'s full implementation, with the scan's stopping rule taken
/// as a parameter rather than the module constants directly -- so a test
/// can inject a tiny [`ScanBudget`] and observe the truncation note without
/// needing a multi-GB fixture to exceed the real one.
fn run_with_budget<W: Write>(
    state: &StateDir,
    cfg: &CtxConfig,
    args: &DiscoverArgs,
    repo: &Path,
    w: &mut W,
    now: u64,
    budget: ScanBudget,
) -> CtxResult<i32> {
    let since_secs = super::spend::parse_since(&args.since).ok_or_else(|| {
        format!(
            "--since '{}': expected a duration like 30m, 24h, or 7d (or a bare number of \
             seconds)",
            args.since
        )
    })?;
    let since_ts = now.saturating_sub(since_secs);
    let min_bytes = scan_min_bytes(cfg);

    let mut candidates = Vec::new();
    for (path, _source) in search::claude_candidates(repo, args.all) {
        let modified_secs = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let Some(modified_secs) = modified_secs else {
            continue;
        };
        if modified_secs < since_ts {
            continue;
        }
        candidates.push((path, modified_secs));
    }

    let outcome = scan_candidates(candidates, min_bytes, budget.bytes, budget.max_files);
    if outcome.truncated {
        writeln!(
            w,
            "note: stopped after {} files / {}; narrow --since",
            outcome.files_scanned,
            human_bytes(outcome.bytes_scanned)
        )?;
    }
    if outcome.unreadable > 0 {
        writeln!(
            w,
            "note: {} transcripts could not be read",
            outcome.unreadable
        )?;
    }

    let ledger_outcomes = ledger::outcomes_by_tool_use_id(state, since_ts);
    let rows = classify_rows(outcome.raw, &ledger_outcomes, cfg);
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

    /// `estimate_reason` must check `output.compact` before it ever
    /// classifies a scope -- `hook::run_posttool` itself does (`if
    /// !cfg.output.compact { record(Disabled); return; }` runs before
    /// `classify_compaction` is even called), so a disabled config must
    /// never be misreported as "hook not installed / no decision recorded"
    /// here. Checked against a `cat` command (a verbatim reader, which
    /// would otherwise win first under the old ordering) to prove the
    /// disabled check really does run before scope classification, not
    /// merely before the size thresholds within a scope.
    #[test]
    fn estimate_reason_honours_output_compact_before_classifying_scope() {
        let mut cfg = CtxConfig::default();
        cfg.output.compact = false;
        let bytes = cfg.output.compact_generic_min_bytes as u64 + 1_000;

        assert_eq!(
            estimate_reason("cat big.log", bytes, &cfg),
            "compaction disabled (output.compact = false)",
            "a verbatim reader must not out-rank the disabled gate"
        );
        assert_eq!(
            estimate_reason("cargo test", bytes, &cfg),
            "compaction disabled (output.compact = false)"
        );

        let hint = hint_for_reason("estimated: compaction disabled (output.compact = false)")
            .expect("a disabled-gate reason gets its own hint");
        assert!(hint.contains("output.compact = true"), "{hint}");
    }

    /// `scan_min_bytes` (the pre-filter floor `run_with_budget` uses before
    /// any row is even scope-classified) must be the SMALLEST of every
    /// per-scope threshold, never `compact_min_bytes` alone -- nothing in
    /// `CtxConfig` enforces that `compact_min_bytes` IS the smallest, and an
    /// operator config where it is not (raised past the default
    /// `diff_max_bytes`) must never drop a `git diff` result the hook would
    /// still have compacted today. A 70,536-byte diff -- above the default
    /// `diff_max_bytes` (65,536), below the raised `compact_min_bytes`
    /// (100,000) -- must survive the pre-filter and classify as "the hook
    /// would compact this today".
    #[test]
    fn scan_min_bytes_never_drops_a_result_a_wider_scope_would_still_compact() {
        let mut cfg = CtxConfig::default();
        cfg.output.compact_min_bytes = 100_000;
        cfg.output.compact_generic_min_bytes = 100_000;
        // `diff_max_bytes` stays at its default (65,536) -- it is now the
        // one genuinely smallest threshold, so the pre-filter must key off
        // it, not either of the two thresholds just raised past it.

        assert_eq!(
            scan_min_bytes(&cfg),
            cfg.output.diff_max_bytes as u64,
            "diff_max_bytes (65,536) is the smallest of the three here -- compact_min_bytes and \
             compact_generic_min_bytes were both raised past it"
        );

        let diff_bytes = cfg.output.diff_max_bytes + 5_000;
        let big = "x".repeat(diff_bytes);
        let jsonl = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_use\",\"id\":\"tu\",\"name\":\"Bash\",\"input\":{{\"command\":\"git diff\"}}}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu\",\"content\":\"{big}\"}}\
             ]}}}}\n"
        );
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("sess.jsonl");
        std::fs::write(&path, &jsonl).expect("write");

        let min_bytes = scan_min_bytes(&cfg);
        let candidates = vec![(path, 1_000u64)];
        let outcome = scan_candidates(
            candidates,
            min_bytes,
            TRANSCRIPT_SCAN_BYTE_BUDGET,
            MAX_TRANSCRIPT_CANDIDATES,
        );
        assert_eq!(
            outcome.raw.len(),
            1,
            "a compact_min_bytes-only pre-filter would have dropped this diff result entirely"
        );

        let rows = classify_rows(outcome.raw, &HashMap::new(), &cfg);
        assert_eq!(rows.len(), 1);
        match &rows[0].classification {
            RowProvenance::Estimated { reason } => assert_eq!(
                reason, "hook not installed / no decision recorded",
                "70,536 bytes is above diff_max_bytes (65,536): the hook would compact this today"
            ),
            RowProvenance::Measured { .. } => panic!("no ledger row exists for this tool_use_id"),
        }
    }

    /// `scan_candidates` (the helper `run_with` uses to bound its transcript
    /// scan) must stop once `byte_budget` is exhausted, favouring the
    /// NEWEST candidate first -- an unledgered multi-GB `--all` corpus must
    /// never be read whole into memory. Two candidates, budget sized to
    /// cover only the newer one: the older is left unscanned and
    /// `truncated` is set.
    #[test]
    fn scan_candidates_stops_at_the_byte_budget_favouring_the_newest_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let min_bytes = cfg.output.compact_min_bytes as u64;
        let big = "x".repeat(cfg.output.compact_generic_min_bytes + 1_000);

        // Two fixture transcripts, each with one large `cat` (verbatim, so
        // it always classifies as ESTIMATED regardless of the ledger) Bash
        // result -- `old.jsonl` carries a command naming it "old", `new.
        // jsonl` a command naming it "new", so the test can tell which file
        // a row actually came from.
        let jsonl_for = |command: &str| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
                 {{\"type\":\"tool_use\",\"id\":\"tu\",\"name\":\"Bash\",\"input\":{{\"command\":\"{command}\"}}}}\
                 ]}}}}\n\
                 {{\"type\":\"user\",\"message\":{{\"content\":[\
                 {{\"type\":\"tool_result\",\"tool_use_id\":\"tu\",\"content\":\"{big}\"}}\
                 ]}}}}\n"
            )
        };
        let old_text = jsonl_for("cat old.log");
        let new_text = jsonl_for("cat new.log");

        let old_path = tmp.path().join("old.jsonl");
        let new_path = tmp.path().join("new.jsonl");
        std::fs::write(&old_path, &old_text).expect("write old");
        std::fs::write(&new_path, &new_text).expect("write new");

        // Exactly enough budget for the newest file's own lines (each
        // consumed line is counted back to its on-disk `len + 1` for the
        // newline `BufRead::lines()` strips, so this covers it precisely)
        // and not one byte more -- the next candidate must find `remaining`
        // already at zero rather than squeezing in a partial read of its
        // own.
        let byte_budget = new_text.len() as u64;
        let candidates = vec![
            (old_path, 1_000u64), // older mtime
            (new_path, 2_000u64), // newer mtime
        ];

        let outcome = scan_candidates(candidates, min_bytes, byte_budget, usize::MAX);

        assert!(
            outcome.truncated,
            "the older file must be left unscanned by the budget"
        );
        assert_eq!(
            outcome.files_scanned, 1,
            "only the newest candidate fits the budget"
        );
        assert_eq!(outcome.raw.len(), 1, "got {:?}", outcome.raw.len());
        assert!(
            outcome.raw[0].command.contains("new.log"),
            "the newest file must be the one scanned: {}",
            outcome.raw[0].command
        );
    }

    /// An unopenable candidate (a stale path that no longer exists on disk)
    /// must be counted in `unreadable`, not mistaken for a cutoff: the scan
    /// still covers every other candidate and nothing was left unscanned
    /// because of a cap or the byte budget, so `truncated` must stay false.
    #[test]
    fn scan_candidates_does_not_treat_an_unreadable_candidate_as_truncated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let min_bytes = cfg.output.compact_min_bytes as u64;
        let big = "x".repeat(cfg.output.compact_generic_min_bytes + 1_000);

        let jsonl = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_use\",\"id\":\"tu\",\"name\":\"Bash\",\"input\":{{\"command\":\"cat readable.log\"}}}}\
             ]}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"tu\",\"content\":\"{big}\"}}\
             ]}}}}\n"
        );
        let readable_path = tmp.path().join("readable.jsonl");
        std::fs::write(&readable_path, &jsonl).expect("write");

        // Never written -- `File::open` fails on it, but this is not a
        // truncation: the scan simply could not read this one candidate.
        let missing_path = tmp.path().join("missing.jsonl");

        let candidates = vec![(readable_path, 2_000u64), (missing_path, 1_000u64)];

        let outcome = scan_candidates(
            candidates,
            min_bytes,
            TRANSCRIPT_SCAN_BYTE_BUDGET,
            MAX_TRANSCRIPT_CANDIDATES,
        );

        assert!(
            !outcome.truncated,
            "no file cap or byte budget was hit -- an unreadable candidate must not set truncated"
        );
        assert_eq!(outcome.unreadable, 1);
        assert_eq!(outcome.files_scanned, 1);
        assert_eq!(outcome.raw.len(), 1);
    }

    /// End to end through `run_with_budget`: a tiny injected byte budget
    /// over two fixture transcripts prints exactly one `note: stopped
    /// after ... ; narrow --since` line and only reports the newer file's
    /// row.
    #[test]
    fn run_with_budget_prints_a_truncation_note_when_the_budget_is_exceeded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let big = "x".repeat(cfg.output.compact_generic_min_bytes + 1_000);

        let home = tmp.path().join("claude-home");
        let projects_root = home.join(".claude").join("projects");
        let project_dir = projects_root.join(super::super::permissions::claude_project_dir_name(
            tmp.path(),
        ));
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");

        let jsonl_for = |command: &str| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
                 {{\"type\":\"tool_use\",\"id\":\"tu\",\"name\":\"Bash\",\"input\":{{\"command\":\"{command}\"}}}}\
                 ]}}}}\n\
                 {{\"type\":\"user\",\"message\":{{\"content\":[\
                 {{\"type\":\"tool_result\",\"tool_use_id\":\"tu\",\"content\":\"{big}\"}}\
                 ]}}}}\n"
            )
        };
        let older_text = jsonl_for("cat older.log");
        let newer_text = jsonl_for("cat newer.log");
        let older_path = project_dir.join("sess-older.jsonl");
        let newer_path = project_dir.join("sess-newer.jsonl");
        std::fs::write(&older_path, &older_text).expect("write older");
        std::fs::write(&newer_path, &newer_text).expect("write newer");

        let now = state::now_secs();
        // Distinct mtimes, oldest first, both well inside the `--since`
        // window `scan_candidates` filters against. Opened with `write`
        // (not just `open`, which is read-only) since setting a file's
        // modified time needs write-attribute access on Windows.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&older_path)
            .expect("open older")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - 200))
            .expect("set older mtime");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&newer_path)
            .expect("open newer")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - 100))
            .expect("set newer mtime");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let args = DiscoverArgs {
            since: "7d".to_string(),
            all: false,
        };

        // Exactly enough budget for the newer file's own lines and not one
        // byte more -- see `scan_candidates_stops_at_the_byte_budget_
        // favouring_the_newest_file` for why this must be exact, not
        // merely "big enough", to force the older file to be left
        // unscanned rather than partially read.
        let budget = ScanBudget {
            bytes: newer_text.len() as u64,
            max_files: usize::MAX,
        };
        let mut out = Vec::new();
        let code =
            run_with_budget(&state, &cfg, &args, tmp.path(), &mut out, now, budget).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("note: stopped after 1 files") && text.contains("narrow --since"),
            "got {text}"
        );
        assert!(
            text.contains("(1 of 1 large results)"),
            "only the newer file's row was scanned: {text}"
        );
    }
}
