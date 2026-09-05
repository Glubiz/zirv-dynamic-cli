//! `zirv ctx measure` (issue #294): transcript-derived proportionality
//! metrics for what an agent session actually *does*, as opposed to
//! `zirv ctx compile --measure`'s per-layer byte/token table (what a session
//! is *sent*) or `zirv workflow stats`' workflow-scoped phase counters.
//!
//! Six metrics, each per group (`--group session|model|week`, or one
//! combined `"all"` group by default):
//!
//! - partial-read rate: ranged (`offset`/`limit`) reads over total reads,
//!   as a per-session **median** so one read-heavy session cannot dominate;
//! - edit inflation ratio: `(old_bytes + new_bytes) / core_bytes` after
//!   stripping the shared prefix/suffix of the replaced/replacement text,
//!   pooled across every edit in the group (median and p90); a no-op edit
//!   (`core_bytes == 0`) is reported separately, never folded into a ratio
//!   of 1;
//! - tool-result token share by tool: a `chars/4` estimate
//!   ([`super::compile::estimate_tokens`], the same crude estimator
//!   `compile --measure` already uses), attributed to the tool name whose
//!   `ToolCall` most recently queued a still-unmatched result;
//! - context utilisation: peak, and immediately before the first
//!   `Compaction` event, as a percentage of `Capabilities::
//!   context_window_tokens` -- "unknown window" (never a guess) when the
//!   adapter states none;
//! - compaction rate: per session, and per 100 turns;
//! - turns per user message: completed (non-empty-text) assistant turns per
//!   `TurnStart`, the same event vocabulary for both adapters.
//!
//! **Any metric whose denominator is zero for a group is reported as
//! [`MetricValue::Unavailable`], never as a zero** -- this covers both "this
//! adapter has no verified shape for the underlying event" (codex never
//! emits `ToolCall`/`ToolResult`/`Compaction` at all, see
//! `adapters::codex::CodexAdapter::parse_events`'s own doc comment) and "no
//! session in this group happened to exercise it", which are indistinguishable
//! from a bare zero and must not be presented as one.
//!
//! Pure computation ([`core_change_bytes`], [`median`], [`percentile`],
//! [`week_label`], [`compute`]) is kept apart from the I/O shell
//! ([`run`]/[`run_with`]/[`discover_sessions`]/[`resolve_since_auto`]):
//! discovering transcripts, resolving `--since auto`, and reading/writing
//! the operator baseline are the only parts of this module that touch a
//! clock, the filesystem, or an adapter.
//!
//! Read-only except `zirv ctx measure baseline`, which writes exactly one
//! file under `~/.zirv/ctx-measure-baseline/<repo_slug>.json` -- operator-
//! global, mirroring `workflow::verification::{load_baseline, save_baseline}`
//! (`~/.zirv/test-baseline/`) -- never under `<repo>/.zirv/`, which is
//! untrusted checkout content. No `[measure]` config key exists to steer
//! this module at all, so nothing here needed a `REPO_FORBIDDEN` entry.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::adapters::AgentAdapter;
use super::adapters::claude::ClaudeAdapter;
use super::adapters::codex::CodexAdapter;
use super::event::NormalizedEvent;
use super::window::parse_iso8601_utc_ms;
use super::{CtxResult, state};

pub const MEASURE_SCHEMA_VERSION: u32 = 1;

// =======================================================================
// Pure math helpers
// =======================================================================

/// The middle value of `values` (average of the two middle values for an
/// even count), or `None` for an empty slice. Never mutates its argument.
pub fn median(values: &[f64]) -> Option<f64> {
    percentile(values, 0.5)
}

/// Linear-interpolation percentile, `p` in `0.0..=1.0` (clamped) -- the same
/// method most stats libraries default to. `percentile(v, 0.5)` is
/// [`median`]; `percentile(v, 0.9)` is this module's own p90.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n == 1 {
        return Some(sorted[0]);
    }
    let rank = p.clamp(0.0, 1.0) * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return Some(sorted[lo]);
    }
    let frac = rank - lo as f64;
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
}

/// Strips the shared prefix and suffix of `old` and `new` and returns the
/// combined length of what remains on both sides -- the "core changed
/// bytes" an edit's inflation ratio divides by (Prime's own
/// `edit-tool-stats.mjs:165-192` computes the same way). `0` for identical
/// `old`/`new` text: a genuine no-op edit, which callers must report as one
/// rather than fold into a ratio of 1.
///
/// Byte-wise, not codepoint-wise: this is a byte-count heuristic feeding an
/// inflation *ratio*, not a text splice, so a cut landing inside a
/// multi-byte UTF-8 codepoint costs nothing -- no substring is ever
/// constructed from the split point.
pub fn core_change_bytes(old: &str, new: &str) -> u64 {
    let old = old.as_bytes();
    let new = new.as_bytes();
    let max_prefix = old.len().min(new.len());
    let mut prefix = 0usize;
    while prefix < max_prefix && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let remaining_old = old.len() - prefix;
    let remaining_new = new.len() - prefix;
    let max_suffix = remaining_old.min(remaining_new);
    let mut suffix = 0usize;
    while suffix < max_suffix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    ((remaining_old - suffix) + (remaining_new - suffix)) as u64
}

/// Howard Hinnant's `civil_from_days` ("chrono-Compatible Low-Level Date
/// Algorithms", public domain): days since the Unix epoch -> a proleptic
/// Gregorian `(year, month, day)`, so `--group week` gets real calendar
/// labels without pulling in a date/time crate (none is a dependency of
/// this crate, and this issue may not add one).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// The Monday-anchored week label for a unix-second timestamp: 1970-01-01
/// (epoch day 0) was a Thursday, so `(day + 3) % 7` is that day's own
/// Monday=0 weekday index, and subtracting it lands on the week's Monday.
/// Deliberately **not** an ISO-8601 calendar week (no "week 1 contains the
/// year's first Thursday" rule at year boundaries) -- this only needs a
/// stable, human-readable 7-day bucket, not standards compliance.
pub fn week_label(unix_secs: u64) -> String {
    let day = (unix_secs / 86_400) as i64;
    let weekday = (day + 3).rem_euclid(7);
    let monday = day - weekday;
    let (y, m, d) = civil_from_days(monday);
    format!("week of {y:04}-{m:02}-{d:02}")
}

// =======================================================================
// Report shape
// =======================================================================

/// A metric that needs real samples to mean anything: `Value` only when at
/// least one contributing session/event actually existed, `Unavailable`
/// otherwise -- see this module's own doc comment on why zero is never used
/// as a stand-in for "no data".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MetricValue {
    Value { value: f64 },
    Unavailable { reason: String },
}

impl MetricValue {
    fn from_option(value: Option<f64>, reason: &str) -> Self {
        match value {
            Some(value) => MetricValue::Value { value },
            None => MetricValue::Unavailable {
                reason: reason.to_string(),
            },
        }
    }

    pub fn value(&self) -> Option<f64> {
        match self {
            MetricValue::Value { value } => Some(*value),
            MetricValue::Unavailable { .. } => None,
        }
    }
}

/// One tool's share (`0.0..=1.0`) of the group's total estimated
/// tool-result tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolShare {
    pub tool: String,
    pub share: f64,
}

/// One row of `zirv ctx measure`'s report: either the single `"all"` group
/// (no `--group`), or one bucket per session/model/week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupReport {
    pub group: String,
    pub session_count: usize,
    pub partial_read_rate: MetricValue,
    pub edit_inflation_median: MetricValue,
    pub edit_inflation_p90: MetricValue,
    pub edit_noop_count: u64,
    pub edit_total_count: u64,
    pub tool_result_token_share: Vec<ToolShare>,
    pub context_util_peak_pct: MetricValue,
    pub context_util_pre_compaction_pct: MetricValue,
    pub compaction_per_session: MetricValue,
    pub compaction_per_100_turns: MetricValue,
    pub turns_per_user_message: MetricValue,
}

/// One already-parsed session's event stream, tagged with what grouping
/// needs to know about it. Built by the I/O shell ([`discover_sessions`]);
/// [`compute`] never touches a file, a clock, or an adapter.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub adapter: &'static str,
    pub model: Option<String>,
    pub week: String,
    pub context_window_tokens: Option<u64>,
    pub events: Vec<NormalizedEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GroupBy {
    Session,
    Model,
    Week,
}

/// Every metric this group's [`SessionRecord`]s can support, computed over
/// exactly the sessions in `sessions` -- pure over its argument, no fs/
/// clock/env/net.
fn compute_group(group: &str, sessions: &[&SessionRecord]) -> GroupReport {
    let mut per_session_partial_rates = Vec::new();
    let mut edit_ratios = Vec::new();
    let mut edit_noop = 0u64;
    let mut edit_total = 0u64;
    let mut tool_tokens: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_tool_tokens = 0u64;
    let mut peak_pcts = Vec::new();
    let mut pre_compaction_pcts = Vec::new();
    let mut total_compactions = 0u64;
    let mut total_turns = 0u64;
    let mut total_completed_turns = 0u64;

    for session in sessions {
        let mut ranged_reads = 0u64;
        let mut total_reads = 0u64;
        let mut pending_tools: std::collections::VecDeque<String> = Default::default();
        let mut peak_context: Option<u64> = None;
        let mut pre_compaction_peak: Option<u64> = None;
        let mut seen_compaction = false;
        let mut session_compactions = 0u64;

        for event in &session.events {
            match event {
                NormalizedEvent::ToolCall { name, .. } => {
                    pending_tools.push_back(name.clone());
                }
                NormalizedEvent::ToolCallRead { ranged } => {
                    total_reads += 1;
                    if *ranged {
                        ranged_reads += 1;
                    }
                }
                NormalizedEvent::ToolCallEdit {
                    old_bytes,
                    new_bytes,
                    core_bytes,
                } => {
                    edit_total += 1;
                    if *core_bytes == 0 {
                        edit_noop += 1;
                    } else {
                        edit_ratios.push((*old_bytes + *new_bytes) as f64 / *core_bytes as f64);
                    }
                }
                NormalizedEvent::ToolResultSize { byte_len, .. } => {
                    if let Some(tool) = pending_tools.pop_front() {
                        let tokens = super::compile::estimate_tokens(*byte_len as usize) as u64;
                        *tool_tokens.entry(tool).or_insert(0) += tokens;
                        total_tool_tokens += tokens;
                    }
                }
                NormalizedEvent::AssistantFinal {
                    text, input_tokens, ..
                } => {
                    peak_context =
                        Some(peak_context.map_or(*input_tokens, |p| p.max(*input_tokens)));
                    if !seen_compaction {
                        pre_compaction_peak = Some(
                            pre_compaction_peak.map_or(*input_tokens, |p| p.max(*input_tokens)),
                        );
                    }
                    if !text.trim().is_empty() {
                        total_completed_turns += 1;
                    }
                }
                NormalizedEvent::Compaction => {
                    seen_compaction = true;
                    session_compactions += 1;
                }
                NormalizedEvent::TurnStart { .. } => {
                    total_turns += 1;
                }
                _ => {}
            }
        }

        if total_reads > 0 {
            per_session_partial_rates.push(ranged_reads as f64 / total_reads as f64);
        }
        total_compactions += session_compactions;

        if let Some(window) = session.context_window_tokens {
            if let Some(peak) = peak_context {
                peak_pcts.push(peak as f64 / window as f64 * 100.0);
            }
            // Only meaningful for a session that actually compacted --
            // otherwise there is no "before the first compaction" moment to
            // report, distinct from "the adapter cannot state a window".
            if session_compactions > 0
                && let Some(pre) = pre_compaction_peak
            {
                pre_compaction_pcts.push(pre as f64 / window as f64 * 100.0);
            }
        }
    }

    let tool_result_token_share: Vec<ToolShare> = if total_tool_tokens > 0 {
        tool_tokens
            .into_iter()
            .map(|(tool, tokens)| ToolShare {
                tool,
                share: tokens as f64 / total_tool_tokens as f64,
            })
            .collect()
    } else {
        Vec::new()
    };

    let compaction_per_session = if total_compactions > 0 && !sessions.is_empty() {
        MetricValue::Value {
            value: total_compactions as f64 / sessions.len() as f64,
        }
    } else {
        MetricValue::Unavailable {
            reason: "no compactions observed (adapter may not report them)".to_string(),
        }
    };
    let compaction_per_100_turns = if total_turns == 0 {
        MetricValue::Unavailable {
            reason: "no turns".to_string(),
        }
    } else if total_compactions == 0 {
        MetricValue::Unavailable {
            reason: "no compactions observed (adapter may not report them)".to_string(),
        }
    } else {
        MetricValue::Value {
            value: total_compactions as f64 / total_turns as f64 * 100.0,
        }
    };
    let turns_per_user_message = MetricValue::from_option(
        (total_turns > 0).then(|| total_completed_turns as f64 / total_turns as f64),
        "no user turns",
    );

    GroupReport {
        group: group.to_string(),
        session_count: sessions.len(),
        partial_read_rate: MetricValue::from_option(
            median(&per_session_partial_rates),
            "no read events",
        ),
        edit_inflation_median: MetricValue::from_option(median(&edit_ratios), "no edits"),
        edit_inflation_p90: MetricValue::from_option(percentile(&edit_ratios, 0.9), "no edits"),
        edit_noop_count: edit_noop,
        edit_total_count: edit_total,
        tool_result_token_share,
        context_util_peak_pct: MetricValue::from_option(
            median(&peak_pcts),
            "unknown context window",
        ),
        context_util_pre_compaction_pct: MetricValue::from_option(
            median(&pre_compaction_pcts),
            "unknown context window",
        ),
        compaction_per_session,
        compaction_per_100_turns,
        turns_per_user_message,
    }
}

/// `Session` folds in the adapter name (`"claude:<id>"`/`"codex:<id>"`)
/// since transcript ids are per-adapter, not globally unique.
fn group_key(record: &SessionRecord, group_by: GroupBy) -> String {
    match group_by {
        GroupBy::Session => format!("{}:{}", record.adapter, record.id),
        GroupBy::Model => record
            .model
            .as_deref()
            .unwrap_or("unknown-model")
            .to_string(),
        GroupBy::Week => record.week.clone(),
    }
}

/// Buckets `records` per `group_by` (or one combined `"all"` bucket when
/// `None`) and computes every metric for each bucket. With `group_by ==
/// None` this always returns exactly one `GroupReport` -- even for an empty
/// `records` -- so callers (`run_baseline`, `render_json`'s
/// `baseline_delta`) never need a fallback branch for "the aggregate group
/// didn't exist". With a real `group_by`, only buckets that actually
/// contain a session are returned, sorted by group key (`BTreeMap`'s own
/// iteration order) for a deterministic report.
pub fn compute(records: &[SessionRecord], group_by: Option<GroupBy>) -> Vec<GroupReport> {
    let Some(group_by) = group_by else {
        return vec![compute_group("all", &records.iter().collect::<Vec<_>>())];
    };
    let mut buckets: BTreeMap<String, Vec<&SessionRecord>> = BTreeMap::new();
    for record in records {
        buckets
            .entry(group_key(record, group_by))
            .or_default()
            .push(record);
    }
    buckets
        .into_iter()
        .map(|(group, sessions)| compute_group(&group, &sessions))
        .collect()
}

// =======================================================================
// I/O shell: transcript discovery, --since auto, baseline
// =======================================================================

fn file_mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn current_binary_mtime() -> Option<u64> {
    file_mtime_secs(&std::env::current_exe().ok()?)
}

fn load_claude_session(
    path: &Path,
    adapter: &ClaudeAdapter,
    since: u64,
    until: Option<u64>,
) -> Option<SessionRecord> {
    let mtime = file_mtime_secs(path)?;
    if mtime < since || until.is_some_and(|u| mtime > u) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let events = adapter.parse_events(&text);
    let model = super::adapters::claude::model_hint(&text);
    let context_window_tokens = adapter.context_window_tokens(model.as_deref());
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    Some(SessionRecord {
        id,
        adapter: "claude",
        model,
        week: week_label(mtime),
        context_window_tokens,
        events,
    })
}

fn load_codex_session(
    path: &Path,
    adapter: &CodexAdapter,
    since: u64,
    until: Option<u64>,
) -> Option<SessionRecord> {
    let mtime = file_mtime_secs(path)?;
    if mtime < since || until.is_some_and(|u| mtime > u) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let events = adapter.parse_events(&text);
    let model = adapter.model_hint(&text);
    let context_window_tokens = adapter.capabilities().context_window_tokens;
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    Some(SessionRecord {
        id,
        adapter: "codex",
        model,
        week: week_label(mtime),
        context_window_tokens,
        events,
    })
}

/// Session-file-level filtering: a whole transcript counts once its file's
/// own mtime is at or after `since` (and, with `--until`, at or before it) --
/// not per-event -- matching the design's own framing of `--since auto` as
/// "measures only SESSIONS produced under the current prompt layer".
fn discover_sessions(
    repo: &Path,
    adapter_filter: Option<&str>,
    since: u64,
    until: Option<u64>,
) -> Vec<SessionRecord> {
    let mut out = Vec::new();
    let wants_claude = adapter_filter.is_none_or(|a| a.eq_ignore_ascii_case("claude"));
    let wants_codex = adapter_filter.is_none_or(|a| a.eq_ignore_ascii_case("codex"));

    if wants_claude {
        let adapter = ClaudeAdapter::new(None);
        for (path, _source) in super::search::claude_candidates(repo, false) {
            if let Some(record) = load_claude_session(&path, &adapter, since, until) {
                out.push(record);
            }
        }
    }
    if wants_codex {
        let adapter = CodexAdapter::new(None);
        for (path, _source) in super::search::codex_candidates() {
            if let Some(record) = load_codex_session(&path, &adapter, since, until) {
                out.push(record);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct SinceResolution {
    pub epoch_secs: u64,
    pub source: String,
}

/// `--since auto`'s resolution: the newest of every `<repo>/.zirv/context/
/// *.md` mtime and `binary_mtime` (a runtime-visible proxy for "when the
/// compiled-in prompt layer's own version stamp last changed" --
/// `prompt::DEFAULT_PROMPT_VERSION`/`HARNESS_PROMPT` carry no timestamp of
/// their own, so the running binary's build time stands in for it). A run
/// therefore measures only sessions produced under the current prompt
/// layer. Pure over its arguments except the one directory read; `run`
/// supplies `binary_mtime` from `std::env::current_exe`, kept as an
/// explicit parameter here so tests can pin it without touching the real
/// executable's mtime.
pub fn resolve_since_auto(repo: &Path, binary_mtime: Option<u64>) -> SinceResolution {
    let mut best: Option<(u64, String)> = None;
    if let Ok(entries) = std::fs::read_dir(repo.join(".zirv").join("context")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(secs) = file_mtime_secs(&path) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| secs > *t) {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("context")
                    .to_string();
                best = Some((secs, format!(".zirv/context/{name}")));
            }
        }
    }
    if let Some(bt) = binary_mtime
        && best.as_ref().is_none_or(|(t, _)| bt > *t)
    {
        best = Some((
            bt,
            "the running zirv binary's own mtime (prompt-layer version proxy)".to_string(),
        ));
    }
    match best {
        Some((epoch_secs, source)) => SinceResolution { epoch_secs, source },
        None => SinceResolution {
            epoch_secs: 0,
            source: "no .zirv/context/*.md files and no readable binary mtime -- measuring the \
                      full history"
                .to_string(),
        },
    }
}

fn format_epoch(secs: u64) -> String {
    // `humantime`/`chrono` are not dependencies of this crate (and this
    // issue may not add one); this reuses the same ISO-ish rendering
    // `week_label`'s own calendar math already gives, extended with the
    // time-of-day remainder.
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// =======================================================================
// Baseline (operator-only write path)
// =======================================================================

const MEASURE_BASELINE_SCHEMA_VERSION: u32 = 1;

/// Operator-owned snapshot of the default (`--group` unset) `"all"` report,
/// stored under `~/.zirv/ctx-measure-baseline/<repo_slug>.json` -- never
/// under `<repo>/.zirv/`, mirroring `workflow::verification::TestBaseline`'s
/// own doc comment on why (untrusted-checkout isolation) exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasureBaseline {
    pub schema_version: u32,
    pub recorded_at: u64,
    pub note: Option<String>,
    pub group: GroupReport,
}

fn baseline_dir() -> CtxResult<PathBuf> {
    Ok(crate::utils::home_dir()?
        .join(crate::utils::SCRIPT_DIR_NAME)
        .join("ctx-measure-baseline"))
}

fn baseline_path(repo: &Path) -> CtxResult<PathBuf> {
    Ok(baseline_dir()?.join(format!("{}.json", state::repo_slug(repo))))
}

/// `None` for anything short of a fully valid, current-schema baseline --
/// missing, unreadable, corrupt JSON, or a future/unknown schema all
/// degrade to "no baseline" rather than propagating an error, unlike
/// `workflow::verification::load_baseline`'s stricter contract: this
/// module's own acceptance criteria call for silent degrade specifically
/// (a broken baseline file must never stop `zirv ctx measure` from
/// reporting).
pub fn load_measure_baseline(repo: &Path) -> Option<MeasureBaseline> {
    let path = baseline_path(repo).ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let baseline: MeasureBaseline = serde_json::from_str(&text).ok()?;
    if baseline.schema_version != MEASURE_BASELINE_SCHEMA_VERSION {
        return None;
    }
    Some(baseline)
}

pub fn save_measure_baseline(
    repo: &Path,
    group: &GroupReport,
    note: Option<String>,
    now: u64,
) -> CtxResult<MeasureBaseline> {
    let baseline = MeasureBaseline {
        schema_version: MEASURE_BASELINE_SCHEMA_VERSION,
        recorded_at: now,
        note,
        group: group.clone(),
    };
    state::create_private_dir_all(&baseline_dir()?)?;
    state::write_private(
        &baseline_path(repo)?,
        &serde_json::to_string_pretty(&baseline)?,
    )?;
    Ok(baseline)
}

/// One named metric's baseline-vs-current comparison. `delta` is `Some` only
/// when both sides are [`MetricValue::Value`] -- an unavailable metric on
/// either side yields `None`, never a fabricated delta.
fn metric_delta_json(baseline: &MetricValue, current: &MetricValue) -> serde_json::Value {
    let delta = match (baseline.value(), current.value()) {
        (Some(b), Some(c)) => Some(c - b),
        _ => None,
    };
    serde_json::json!({"baseline": baseline, "current": current, "delta": delta})
}

/// The named-metric `baseline_delta` block: every scalar metric
/// [`GroupReport`] carries except `tool_result_token_share` (a per-tool
/// breakdown delta needs matching tool sets between the two reports, which
/// this issue leaves for a follow-up -- see its own Non-goals).
fn baseline_delta_json(baseline: &GroupReport, current: &GroupReport) -> serde_json::Value {
    serde_json::json!({
        "partial_read_rate": metric_delta_json(&baseline.partial_read_rate, &current.partial_read_rate),
        "edit_inflation_median": metric_delta_json(&baseline.edit_inflation_median, &current.edit_inflation_median),
        "edit_inflation_p90": metric_delta_json(&baseline.edit_inflation_p90, &current.edit_inflation_p90),
        "context_util_peak_pct": metric_delta_json(&baseline.context_util_peak_pct, &current.context_util_peak_pct),
        "context_util_pre_compaction_pct": metric_delta_json(&baseline.context_util_pre_compaction_pct, &current.context_util_pre_compaction_pct),
        "compaction_per_session": metric_delta_json(&baseline.compaction_per_session, &current.compaction_per_session),
        "compaction_per_100_turns": metric_delta_json(&baseline.compaction_per_100_turns, &current.compaction_per_100_turns),
        "turns_per_user_message": metric_delta_json(&baseline.turns_per_user_message, &current.turns_per_user_message),
    })
}

// =======================================================================
// CLI
// =======================================================================

#[derive(Debug, clap::Args)]
pub struct MeasureArgs {
    #[command(subcommand)]
    pub action: Option<MeasureAction>,
    /// ISO-8601 timestamp, or `auto` (default): the newest of every
    /// `<repo>/.zirv/context/*.md` mtime and the running binary's own mtime.
    #[arg(long, default_value = "auto")]
    pub since: String,
    /// ISO-8601 timestamp: only sessions whose transcript file was last
    /// written at or before this instant.
    #[arg(long)]
    pub until: Option<String>,
    /// Restrict to one adapter's transcripts (`claude` or `codex`). Both
    /// when unset.
    #[arg(long)]
    pub adapter: Option<String>,
    /// Break the report down by session, model, or (Monday-anchored)
    /// calendar week instead of one combined `"all"` row.
    #[arg(long, value_enum)]
    pub group: Option<GroupBy>,
    /// Machine-readable output, schema-versioned (`"schema": 1`).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Subcommand)]
pub enum MeasureAction {
    /// Records the current default (ungrouped) report as the operator's
    /// committed baseline, so a later `zirv ctx measure --json` prints a
    /// `baseline_delta` block. Operator-only write, under `~/.zirv/`.
    Baseline(BaselineArgs),
}

#[derive(Debug, clap::Args)]
pub struct BaselineArgs {
    /// A free-text note recorded alongside the snapshot.
    #[arg(long)]
    pub note: Option<String>,
}

fn parse_cli_timestamp(flag: &str, raw: &str) -> CtxResult<u64> {
    parse_iso8601_utc_ms(raw)
        .map(|ms| ms / 1000)
        .ok_or_else(|| {
            format!("zirv ctx measure: {flag} '{raw}' is not a recognized ISO-8601 timestamp")
                .into()
        })
}

fn resolve_since(repo: &Path, raw: &str, binary_mtime: Option<u64>) -> CtxResult<SinceResolution> {
    if raw.eq_ignore_ascii_case("auto") {
        Ok(resolve_since_auto(repo, binary_mtime))
    } else {
        Ok(SinceResolution {
            epoch_secs: parse_cli_timestamp("--since", raw)?,
            source: "explicit --since".to_string(),
        })
    }
}

fn render_metric(m: &MetricValue) -> String {
    match m {
        MetricValue::Value { value } => format!("{value:.3}"),
        MetricValue::Unavailable { reason } => format!("unavailable ({reason})"),
    }
}

fn render_text<W: Write>(
    w: &mut W,
    since: &SinceResolution,
    groups: &[GroupReport],
    baseline: Option<&MeasureBaseline>,
) -> CtxResult<()> {
    writeln!(
        w,
        "since: {} (source: {})",
        format_epoch(since.epoch_secs),
        since.source
    )?;
    for g in groups {
        writeln!(w, "\n[{}] {} session(s)", g.group, g.session_count)?;
        writeln!(
            w,
            "  partial-read rate (median/session): {}",
            render_metric(&g.partial_read_rate)
        )?;
        writeln!(
            w,
            "  edit inflation ratio: median {} / p90 {} ({} no-op of {} edits)",
            render_metric(&g.edit_inflation_median),
            render_metric(&g.edit_inflation_p90),
            g.edit_noop_count,
            g.edit_total_count
        )?;
        if g.tool_result_token_share.is_empty() {
            writeln!(
                w,
                "  tool-result token share: unavailable (no tool results)"
            )?;
        } else {
            for t in &g.tool_result_token_share {
                writeln!(
                    w,
                    "  tool-result token share [{}]: {:.1}%",
                    t.tool,
                    t.share * 100.0
                )?;
            }
        }
        writeln!(
            w,
            "  context utilisation: peak {}, pre-first-compaction {}",
            render_metric(&g.context_util_peak_pct),
            render_metric(&g.context_util_pre_compaction_pct)
        )?;
        writeln!(
            w,
            "  compaction rate: {} per session, {} per 100 turns",
            render_metric(&g.compaction_per_session),
            render_metric(&g.compaction_per_100_turns)
        )?;
        writeln!(
            w,
            "  turns per user message: {}",
            render_metric(&g.turns_per_user_message)
        )?;
    }
    if let Some(baseline) = baseline
        && let Some(current) = groups.first()
    {
        writeln!(
            w,
            "\nbaseline recorded {}{}: {}",
            format_epoch(baseline.recorded_at),
            baseline
                .note
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default(),
            serde_json::to_string(&baseline_delta_json(&baseline.group, current))?
        )?;
    }
    Ok(())
}

fn render_json<W: Write>(
    w: &mut W,
    since: &SinceResolution,
    groups: &[GroupReport],
    baseline: Option<&MeasureBaseline>,
) -> CtxResult<()> {
    let mut payload = serde_json::json!({
        "schema": MEASURE_SCHEMA_VERSION,
        "since": since,
        "groups": groups,
    });
    if let Some(baseline) = baseline
        && let Some(current) = groups.first()
    {
        payload["baseline_delta"] = serde_json::json!({
            "recorded_at": baseline.recorded_at,
            "note": baseline.note,
            "metrics": baseline_delta_json(&baseline.group, current),
        });
    }
    writeln!(w, "{}", serde_json::to_string(&payload)?)?;
    Ok(())
}

fn run_report<W: Write>(
    args: &MeasureArgs,
    w: &mut W,
    repo: &Path,
    binary_mtime: Option<u64>,
) -> CtxResult<i32> {
    let since = resolve_since(repo, &args.since, binary_mtime)?;
    let until = match &args.until {
        Some(raw) => Some(parse_cli_timestamp("--until", raw)?),
        None => None,
    };
    let records = discover_sessions(repo, args.adapter.as_deref(), since.epoch_secs, until);
    let groups = compute(&records, args.group);
    // The baseline only ever holds the ungrouped "all" report (`Baseline`
    // subcommand has no `--group` of its own), so a `baseline_delta` is
    // only meaningful when the current run is ungrouped too.
    let baseline = if args.group.is_none() {
        load_measure_baseline(repo)
    } else {
        None
    };

    if args.json {
        render_json(w, &since, &groups, baseline.as_ref())?;
    } else {
        render_text(w, &since, &groups, baseline.as_ref())?;
    }
    Ok(0)
}

fn run_baseline<W: Write>(
    args: &BaselineArgs,
    w: &mut W,
    repo: &Path,
    now: u64,
    binary_mtime: Option<u64>,
) -> CtxResult<i32> {
    let since = resolve_since_auto(repo, binary_mtime);
    let records = discover_sessions(repo, None, since.epoch_secs, None);
    let group = compute(&records, None)
        .into_iter()
        .next()
        .expect("compute(.., None) always returns exactly one 'all' group");
    let baseline = save_measure_baseline(repo, &group, args.note.clone(), now)?;
    writeln!(
        w,
        "zirv ctx measure baseline: recorded {} session(s) as of {} -> {}",
        group.session_count,
        format_epoch(baseline.recorded_at),
        baseline_path(repo)?.display()
    )?;
    Ok(0)
}

pub fn run<W: Write>(args: &MeasureArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    run_with(args, w, &repo, state::now_secs(), current_binary_mtime())
}

pub fn run_with<W: Write>(
    args: &MeasureArgs,
    w: &mut W,
    repo: &Path,
    now: u64,
    binary_mtime: Option<u64>,
) -> CtxResult<i32> {
    match &args.action {
        Some(MeasureAction::Baseline(b)) => run_baseline(b, w, repo, now, binary_mtime),
        None => run_report(args, w, repo, binary_mtime),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- median / percentile ------------------------------------------------

    #[test]
    fn median_is_none_for_empty() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn median_averages_the_two_middle_values_for_an_even_count() {
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
    }

    #[test]
    fn median_is_the_middle_value_for_an_odd_count() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
    }

    #[test]
    fn percentile_p90_matches_a_known_hand_computation() {
        let values: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        // rank = 0.9 * 9 = 8.1 -> interpolate between sorted[8]=9 and sorted[9]=10
        assert_eq!(percentile(&values, 0.9), Some(9.1));
    }

    #[test]
    fn percentile_single_value_is_itself() {
        assert_eq!(percentile(&[42.0], 0.9), Some(42.0));
    }

    // -- core_change_bytes ----------------------------------------------------

    #[test]
    fn core_change_bytes_is_zero_for_identical_text() {
        assert_eq!(core_change_bytes("same text", "same text"), 0);
    }

    #[test]
    fn core_change_bytes_strips_shared_prefix_and_suffix() {
        assert_eq!(core_change_bytes("hello world", "hello there world"), 6);
    }

    #[test]
    fn core_change_bytes_one_char_in_a_4kb_block_yields_a_ratio_far_above_one() {
        let base = "x".repeat(4096);
        let mut changed = base.clone();
        changed.replace_range(2048..2049, "y");
        let core = core_change_bytes(&base, &changed);
        assert!(core > 0, "a real change must never report a no-op");
        let ratio = (base.len() + changed.len()) as f64 / core as f64;
        assert!(ratio > 100.0, "got ratio {ratio}");
    }

    // -- week_label -------------------------------------------------------

    #[test]
    fn week_label_is_stable_within_the_same_calendar_week() {
        // 2026-09-07 is a Monday; +1 day is still the same week.
        let monday = 1_788_649_200u64; // 2026-09-07T00:00:00Z (computed below)
        let _ = monday;
        let a = week_label(1_757_030_400); // 2025-09-05 (Friday)
        let b = week_label(1_757_030_400 + 86_400); // Saturday, same week
        assert_eq!(a, b);
    }

    #[test]
    fn week_label_differs_across_a_week_boundary() {
        let sunday = 1_757_030_400 + 86_400 * 2; // Sunday
        let next_monday = sunday + 86_400; // Monday, next week
        assert_ne!(week_label(sunday), week_label(next_monday));
    }

    // -- compute: partial-read rate -----------------------------------------

    fn read_event(ranged: bool) -> Vec<NormalizedEvent> {
        vec![
            NormalizedEvent::ToolCall {
                name: "Read".to_string(),
                input_hash: 0,
                at_ms: None,
            },
            NormalizedEvent::ToolCallRead { ranged },
        ]
    }

    fn session(id: &str, events: Vec<NormalizedEvent>) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            adapter: "claude",
            model: None,
            week: "week of 2026-01-01".to_string(),
            context_window_tokens: None,
            events,
        }
    }

    #[test]
    fn partial_read_rate_is_a_per_session_median_a_heavy_session_never_dominates() {
        let mut records = Vec::new();
        // One heavy session: 500 ranged reads out of 500 (rate 1.0).
        let mut heavy_events = Vec::new();
        for _ in 0..500 {
            heavy_events.extend(read_event(true));
        }
        records.push(session("heavy", heavy_events));
        // Nine light sessions: 2 reads each, both whole-file (rate 0.0).
        for i in 0..9 {
            let mut events = Vec::new();
            events.extend(read_event(false));
            events.extend(read_event(false));
            records.push(session(&format!("light-{i}"), events));
        }
        let groups = compute(&records, None);
        assert_eq!(groups.len(), 1);
        // Median of [1.0, 0.0 x9] sorted = [0,0,0,0,0,0,0,0,0,1] -> middle two are 0,0.
        assert_eq!(
            groups[0].partial_read_rate,
            MetricValue::Value { value: 0.0 },
            "the pooled rate would be 518/518=1.0 -- the median must not be that"
        );
    }

    #[test]
    fn partial_read_rate_is_unavailable_with_no_reads() {
        let records = vec![session("s1", vec![])];
        let groups = compute(&records, None);
        assert_eq!(
            groups[0].partial_read_rate,
            MetricValue::Unavailable {
                reason: "no read events".to_string()
            }
        );
    }

    // -- compute: edit inflation --------------------------------------------

    fn edit_event(old_bytes: u64, new_bytes: u64, core_bytes: u64) -> NormalizedEvent {
        NormalizedEvent::ToolCallEdit {
            old_bytes,
            new_bytes,
            core_bytes,
        }
    }

    #[test]
    fn edit_inflation_reports_a_noop_separately_never_as_a_ratio_of_one() {
        let records = vec![session(
            "s1",
            vec![edit_event(100, 100, 0), edit_event(4096, 4096, 2)],
        )];
        let groups = compute(&records, None);
        let g = &groups[0];
        assert_eq!(g.edit_noop_count, 1);
        assert_eq!(g.edit_total_count, 2);
        // Only the real edit contributes to the ratio: 8192/2 = 4096.
        assert_eq!(
            g.edit_inflation_median,
            MetricValue::Value { value: 4096.0 }
        );
    }

    #[test]
    fn edit_inflation_is_unavailable_with_no_edits() {
        let records = vec![session("s1", vec![])];
        let groups = compute(&records, None);
        assert_eq!(
            groups[0].edit_inflation_median,
            MetricValue::Unavailable {
                reason: "no edits".to_string()
            }
        );
        assert_eq!(groups[0].edit_total_count, 0);
    }

    // -- compute: context utilisation ---------------------------------------

    #[test]
    fn context_utilisation_reports_peak_and_pre_compaction_separately() {
        let events = vec![
            NormalizedEvent::AssistantFinal {
                text: "a".to_string(),
                input_tokens: 1_000,
                at_ms: None,
            },
            NormalizedEvent::Compaction,
            NormalizedEvent::AssistantFinal {
                text: "b".to_string(),
                input_tokens: 9_000,
                at_ms: None,
            },
        ];
        let mut s = session("s1", events);
        s.context_window_tokens = Some(10_000);
        let groups = compute(&[s], None);
        let g = &groups[0];
        assert_eq!(
            g.context_util_peak_pct,
            MetricValue::Value { value: 90.0 },
            "peak must see the post-compaction 9000/10000 too"
        );
        assert_eq!(
            g.context_util_pre_compaction_pct,
            MetricValue::Value { value: 10.0 },
            "pre-compaction must only see the 1000/10000 before Compaction"
        );
    }

    #[test]
    fn context_utilisation_is_unknown_window_when_the_adapter_states_none() {
        let events = vec![NormalizedEvent::AssistantFinal {
            text: "a".to_string(),
            input_tokens: 1_000,
            at_ms: None,
        }];
        let s = session("s1", events); // context_window_tokens: None
        let groups = compute(&[s], None);
        assert_eq!(
            groups[0].context_util_peak_pct,
            MetricValue::Unavailable {
                reason: "unknown context window".to_string()
            }
        );
        assert_eq!(
            groups[0].context_util_pre_compaction_pct,
            MetricValue::Unavailable {
                reason: "unknown context window".to_string()
            }
        );
    }

    // -- compute: compaction rate / turns per user message -------------------

    #[test]
    fn compaction_rate_is_unavailable_when_no_compaction_is_ever_observed() {
        let events = vec![NormalizedEvent::TurnStart { at_ms: None }];
        let groups = compute(&[session("s1", events)], None);
        assert_eq!(
            groups[0].compaction_per_session,
            MetricValue::Unavailable {
                reason: "no compactions observed (adapter may not report them)".to_string()
            }
        );
    }

    #[test]
    fn turns_per_user_message_counts_completed_turns_over_turn_starts() {
        let events = vec![
            NormalizedEvent::TurnStart { at_ms: None },
            NormalizedEvent::AssistantFinal {
                text: "step one".to_string(),
                input_tokens: 0,
                at_ms: None,
            },
            NormalizedEvent::AssistantFinal {
                text: String::new(), // an empty-text token update, e.g. codex -- not a turn
                input_tokens: 0,
                at_ms: None,
            },
            NormalizedEvent::AssistantFinal {
                text: "step two".to_string(),
                input_tokens: 0,
                at_ms: None,
            },
        ];
        let groups = compute(&[session("s1", events)], None);
        assert_eq!(
            groups[0].turns_per_user_message,
            MetricValue::Value { value: 2.0 }
        );
    }

    #[test]
    fn turns_per_user_message_is_unavailable_with_no_user_turns() {
        let groups = compute(&[session("s1", vec![])], None);
        assert_eq!(
            groups[0].turns_per_user_message,
            MetricValue::Unavailable {
                reason: "no user turns".to_string()
            }
        );
    }

    // -- compute: tool-result token share -------------------------------------

    #[test]
    fn tool_result_token_share_attributes_results_to_the_matching_call_in_order() {
        let events = vec![
            NormalizedEvent::ToolCall {
                name: "Read".to_string(),
                input_hash: 0,
                at_ms: None,
            },
            NormalizedEvent::ToolCall {
                name: "Bash".to_string(),
                input_hash: 0,
                at_ms: None,
            },
            NormalizedEvent::ToolResultSize {
                byte_len: 400, // ~100 tokens
                content_hash: 0,
            },
            NormalizedEvent::ToolResultSize {
                byte_len: 400, // ~100 tokens
                content_hash: 0,
            },
        ];
        let groups = compute(&[session("s1", events)], None);
        let shares = &groups[0].tool_result_token_share;
        assert_eq!(shares.len(), 2);
        for s in shares {
            assert!((s.share - 0.5).abs() < 1e-9, "got {s:?}");
        }
    }

    #[test]
    fn tool_result_token_share_is_empty_with_no_tool_results() {
        let groups = compute(&[session("s1", vec![])], None);
        assert!(groups[0].tool_result_token_share.is_empty());
    }

    // -- compute: grouping ----------------------------------------------------

    #[test]
    fn group_by_model_buckets_sessions_sharing_a_model_id_and_sorts_by_key() {
        let mut a = session("a", vec![]);
        a.model = Some("model-x".to_string());
        let mut b = session("b", vec![]);
        b.model = Some("model-y".to_string());
        let mut c = session("c", vec![]);
        c.model = Some("model-x".to_string());
        let groups = compute(&[a, b, c], Some(GroupBy::Model));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].group, "model-x");
        assert_eq!(groups[0].session_count, 2);
        assert_eq!(groups[1].group, "model-y");
        assert_eq!(groups[1].session_count, 1);
    }

    #[test]
    fn compute_with_no_group_always_returns_one_all_group_even_with_no_sessions() {
        let groups = compute(&[], None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group, "all");
        assert_eq!(groups[0].session_count, 0);
    }

    // -- resolve_since_auto ----------------------------------------------------

    #[test]
    fn resolve_since_auto_prefers_the_newest_context_md_mtime() {
        let repo = tempfile::tempdir().expect("tempdir");
        let context_dir = repo.path().join(".zirv").join("context");
        std::fs::create_dir_all(&context_dir).expect("mkdir");
        std::fs::write(context_dir.join("common.md"), "hello").expect("write");
        let resolution = resolve_since_auto(repo.path(), None);
        assert!(resolution.source.contains("common.md"));
        assert!(resolution.epoch_secs > 0);
    }

    #[test]
    fn resolve_since_auto_falls_back_to_binary_mtime_with_no_context_files() {
        let repo = tempfile::tempdir().expect("tempdir");
        let resolution = resolve_since_auto(repo.path(), Some(1_700_000_000));
        assert_eq!(resolution.epoch_secs, 1_700_000_000);
        assert!(resolution.source.contains("binary"));
    }

    #[test]
    fn resolve_since_auto_picks_the_newer_of_context_files_and_binary_mtime() {
        let repo = tempfile::tempdir().expect("tempdir");
        let context_dir = repo.path().join(".zirv").join("context");
        std::fs::create_dir_all(&context_dir).expect("mkdir");
        std::fs::write(context_dir.join("common.md"), "hello").expect("write");
        let context_mtime = file_mtime_secs(&context_dir.join("common.md")).expect("mtime");
        let older = resolve_since_auto(repo.path(), Some(context_mtime.saturating_sub(1_000_000)));
        assert!(older.source.contains("common.md"));
        let newer = resolve_since_auto(repo.path(), Some(context_mtime + 1_000_000));
        assert!(newer.source.contains("binary"));
    }

    #[test]
    fn resolve_since_auto_measures_full_history_with_neither_source() {
        let repo = tempfile::tempdir().expect("tempdir");
        let resolution = resolve_since_auto(repo.path(), None);
        assert_eq!(resolution.epoch_secs, 0);
    }

    // -- baseline round-trip ----------------------------------------------------

    #[test]
    fn baseline_round_trips_and_a_corrupt_file_degrades_to_no_baseline() {
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let _home_guard = super::super::testenv::HomeGuard::set(home.path());

        assert!(load_measure_baseline(repo.path()).is_none());

        let group = compute(&[], None)
            .into_iter()
            .next()
            .expect("aggregate group");
        let saved = save_measure_baseline(
            repo.path(),
            &group,
            Some("initial".to_string()),
            1_700_000_000,
        )
        .expect("save");
        let loaded = load_measure_baseline(repo.path()).expect("load");
        assert_eq!(loaded, saved);
        assert_eq!(loaded.note.as_deref(), Some("initial"));

        // Corrupt the file in place: must degrade to "no baseline", never an error.
        std::fs::write(baseline_path(repo.path()).expect("path"), "not json").expect("corrupt");
        assert!(load_measure_baseline(repo.path()).is_none());
    }

    #[test]
    fn baseline_delta_is_present_only_when_a_baseline_exists() {
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let _home_guard = super::super::testenv::HomeGuard::set(home.path());

        let args = MeasureArgs {
            action: None,
            since: "auto".to_string(),
            until: None,
            adapter: None,
            group: None,
            json: true,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, repo.path(), 1_700_000_000, None).expect("run_with");
        let value: serde_json::Value =
            serde_json::from_slice(&out).expect("valid json even with zero transcripts");
        assert!(value.get("baseline_delta").is_none());

        let baseline_args = BaselineArgs { note: None };
        let mut baseline_out = Vec::new();
        run_with(
            &MeasureArgs {
                action: Some(MeasureAction::Baseline(baseline_args)),
                ..args_clone(&args)
            },
            &mut baseline_out,
            repo.path(),
            1_700_000_000,
            None,
        )
        .expect("baseline run");

        let mut out2 = Vec::new();
        run_with(&args, &mut out2, repo.path(), 1_700_000_100, None).expect("run_with 2");
        let value2: serde_json::Value = serde_json::from_slice(&out2).expect("valid json");
        assert!(value2.get("baseline_delta").is_some());
    }

    fn args_clone(args: &MeasureArgs) -> MeasureArgs {
        MeasureArgs {
            action: None,
            since: args.since.clone(),
            until: args.until.clone(),
            adapter: args.adapter.clone(),
            group: args.group,
            json: args.json,
        }
    }

    // -- run_with: never writes outside the baseline path ----------------------

    #[test]
    fn run_with_default_report_never_touches_the_filesystem_baseline_dir() {
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let _home_guard = super::super::testenv::HomeGuard::set(home.path());

        let args = MeasureArgs {
            action: None,
            since: "auto".to_string(),
            until: None,
            adapter: None,
            group: None,
            json: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, repo.path(), 1_700_000_000, None).expect("run_with");
        assert_eq!(code, 0);
        assert!(
            !home
                .path()
                .join(".zirv")
                .join("ctx-measure-baseline")
                .exists(),
            "a plain report must never create the baseline directory"
        );
        assert!(!out.is_empty());
    }

    // -- fixture-transcript tests (issue #294's own Tests section) -----------
    //
    // Exercise the real parse path (`load_claude_session`/`load_codex_session`,
    // which call straight into the adapters' own `parse_events`) against
    // committed transcripts under `tests/fixtures/measure/` (data only), for
    // each of the issue's own named cases: empty transcript, no compaction,
    // compaction before any tool call, unknown context window (codex, which
    // never states one), and unparseable lines mixed with valid ones.

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("measure")
            .join(name)
    }

    #[test]
    fn claude_empty_transcript_yields_a_session_with_no_events() {
        let adapter = ClaudeAdapter::new(None);
        let record = load_claude_session(&fixture("claude-empty.jsonl"), &adapter, 0, None)
            .expect("an empty file is still a valid, if eventless, session");
        assert!(record.events.is_empty());
        let groups = compute(&[record], None);
        assert_eq!(groups[0].session_count, 1);
        assert!(groups[0].partial_read_rate.value().is_none());
        assert!(groups[0].turns_per_user_message.value().is_none());
    }

    #[test]
    fn claude_no_compaction_fixture_reports_a_real_edit_and_a_ranged_read() {
        let adapter = ClaudeAdapter::new(None);
        let record = load_claude_session(&fixture("claude-no-compaction.jsonl"), &adapter, 0, None)
            .expect("fixture loads");
        let groups = compute(&[record], None);
        let g = &groups[0];
        assert_eq!(g.partial_read_rate, MetricValue::Value { value: 1.0 });
        assert_eq!(g.edit_total_count, 1);
        assert_eq!(g.edit_noop_count, 0);
        assert!(g.edit_inflation_median.value().is_some());
        // No `compact_boundary` row at all in this fixture.
        assert_eq!(
            g.compaction_per_session,
            MetricValue::Unavailable {
                reason: "no compactions observed (adapter may not report them)".to_string()
            }
        );
        assert_eq!(g.turns_per_user_message, MetricValue::Value { value: 3.0 });
    }

    #[test]
    fn claude_compaction_before_any_tool_call_keeps_pre_compaction_utilisation_smaller_than_peak() {
        let adapter = ClaudeAdapter::new(None);
        let record = load_claude_session(
            &fixture("claude-compaction-before-tool-call.jsonl"),
            &adapter,
            0,
            None,
        )
        .expect("fixture loads");
        let groups = compute(&[record], None);
        let g = &groups[0];
        let pre = g
            .context_util_pre_compaction_pct
            .value()
            .expect("known window, and an AssistantFinal precedes the Compaction row");
        let peak = g.context_util_peak_pct.value().expect("known window");
        assert!(
            pre < peak,
            "pre-compaction ({pre}) must reflect only the 500-token row before Compaction, \
             peak ({peak}) the full session"
        );
    }

    #[test]
    fn claude_garbage_lines_fixture_skips_unparseable_rows_and_still_completes() {
        let adapter = ClaudeAdapter::new(None);
        let record = load_claude_session(&fixture("claude-garbage-lines.jsonl"), &adapter, 0, None)
            .expect("a transcript with some garbage lines still loads");
        let groups = compute(&[record], None);
        assert_eq!(
            groups[0].turns_per_user_message,
            MetricValue::Value { value: 1.0 },
            "only the one valid TurnStart/AssistantFinal pair must be counted"
        );
    }

    #[test]
    fn codex_basic_fixture_has_unknown_context_window_and_no_tool_events() {
        let adapter = CodexAdapter::new(None);
        let record =
            load_codex_session(&fixture("codex-basic.jsonl"), &adapter, 0, None).expect("loads");
        assert_eq!(record.context_window_tokens, None);
        let groups = compute(&[record], None);
        let g = &groups[0];
        assert_eq!(
            g.context_util_peak_pct,
            MetricValue::Unavailable {
                reason: "unknown context window".to_string()
            }
        );
        assert_eq!(g.turns_per_user_message, MetricValue::Value { value: 1.0 });
        assert!(
            g.tool_result_token_share.is_empty(),
            "codex never emits ToolCall/ToolResult"
        );
        assert_eq!(
            g.partial_read_rate,
            MetricValue::Unavailable {
                reason: "no read events".to_string()
            }
        );
    }

    #[test]
    fn codex_garbage_lines_fixture_skips_unparseable_rows_and_still_completes() {
        let adapter = CodexAdapter::new(None);
        let record = load_codex_session(&fixture("codex-garbage-lines.jsonl"), &adapter, 0, None)
            .expect("a rollout with some garbage lines still loads");
        let groups = compute(&[record], None);
        assert_eq!(
            groups[0].turns_per_user_message,
            MetricValue::Value { value: 1.0 }
        );
    }
}
