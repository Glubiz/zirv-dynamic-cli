use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::state::StateDir;

/// One subscription window as last reported by the collector. `resets_at` is a
/// unix epoch second; `0` means the field was absent and callers must fall back.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub used_percentage: f64,
    pub resets_at: u64,
    pub observed_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageWindows {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// A window needs a percentage to be useful. `resets_at` may be absent, and `0`
/// is the documented "unknown" marker callers fall back on.
fn window_at(node: Option<&Value>, observed_at: u64) -> Option<Window> {
    let node = node?;
    let used_percentage = node.get("used_percentage").and_then(Value::as_f64)?;
    Some(Window {
        used_percentage,
        resets_at: node.get("resets_at").and_then(Value::as_u64).unwrap_or(0),
        observed_at,
    })
}

/// Reads the documented statusline `rate_limits` block. `None` means there was
/// nothing to persist, which is the normal case for non-subscribers and for the
/// first statusline of a session, so it is never an error.
pub fn parse_statusline(json: &str, observed_at: u64) -> Option<UsageWindows> {
    let value: Value = serde_json::from_str(json).ok()?;
    let limits = value.get("rate_limits")?;
    if !limits.is_object() {
        return None;
    }

    let windows = UsageWindows {
        five_hour: window_at(limits.get("five_hour"), observed_at),
        seven_day: window_at(limits.get("seven_day"), observed_at),
    };
    if windows.five_hour.is_none() && windows.seven_day.is_none() {
        return None;
    }
    Some(windows)
}

/// `None` means there is no source at all for these windows -- no file, or one
/// that says nothing readable. Distinct from `Some(UsageWindows::default())`,
/// which is a real file that happens to report neither window: "unknown" and
/// "nothing used" are opposite things to say to an operator.
fn read_at(path: &Path) -> Option<UsageWindows> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Never fails: an absent or corrupt file reads as "nothing known", because a
/// statusline hook must not break on a half-written state file.
pub fn load(state: &StateDir) -> UsageWindows {
    read_at(&state.usage()).unwrap_or_default()
}

/// Atomic: every live session's statusline writes this file, so a reader must
/// never observe a truncated one.
fn store_at(path: &Path, windows: &UsageWindows) -> CtxResult<()> {
    if let Some(parent) = path.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    super::state::write_private(&temp, &serde_json::to_string(windows)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

pub fn store(state: &StateDir, windows: &UsageWindows) -> CtxResult<()> {
    store_at(&state.usage(), windows)
}

/// The account the legacy global `usage.json` holds readings for. Its only
/// writer is `usage::run_tee`, which is Claude Code's own statusline hook, so
/// whatever is in that file is Anthropic subscription data stored before
/// there was anywhere provider-specific to put it. Stated here as a fact
/// about a file already on disk rather than read off the adapter registry --
/// the file outlives any particular registry -- and pinned against
/// `ClaudeAdapter::provider` by a test so the two cannot drift.
pub const LEGACY_USAGE_PROVIDER: &str = "anthropic";

/// Per-provider counterpart of [`store`], written to
/// `StateDir::usage_for(provider)` with the same temp-plus-rename atomicity.
pub fn store_for(state: &StateDir, provider: &str, windows: &UsageWindows) -> CtxResult<()> {
    store_at(&state.usage_for(provider), windows)
}

/// This provider's usage windows, or `None` when nothing has ever recorded
/// any for it -- which is the honest answer for a provider with no collector
/// (codex/openai today), and must render as "no source", never as 0%.
///
/// [`LEGACY_USAGE_PROVIDER`] falls back to the legacy global file when it has
/// no provider file of its own yet, so an operator upgrading into this layout
/// keeps the reading their statusline has been collecting all along. The
/// legacy file is only read here, never moved or deleted: `load`, `zirv ctx
/// usage` and `wrap`'s status bar still read it directly.
pub fn load_for(state: &StateDir, provider: &str) -> Option<UsageWindows> {
    if let Some(windows) = read_at(&state.usage_for(provider)) {
        return Some(windows);
    }
    if super::state::provider_slug(provider) == LEGACY_USAGE_PROVIDER {
        return read_at(&state.usage());
    }
    None
}

/// True when nothing has ever been recorded for this provider. Since the
/// codex collector and the poller exist, no provider is structurally exempt
/// any more — callers refresh sources first, then ask.
pub fn has_no_usage_source(state: &StateDir, provider: &str) -> bool {
    load_for(state, provider).is_none()
}

fn newer(existing: Option<Window>, fresh: Option<Window>) -> Option<Window> {
    match (existing, fresh) {
        (Some(existing), Some(fresh)) if fresh.observed_at >= existing.observed_at => Some(fresh),
        (Some(existing), Some(_)) => Some(existing),
        (None, fresh) => fresh,
        (existing, None) => existing,
    }
}

/// Per-window merge. Each window may be independently absent from any given
/// statusline payload, so an absent window never erases a known one.
pub fn merge(existing: UsageWindows, fresh: UsageWindows) -> UsageWindows {
    UsageWindows {
        five_hour: newer(existing.five_hour, fresh.five_hour),
        seven_day: newer(existing.seven_day, fresh.seven_day),
    }
}

pub fn age_secs(window: &Window, now: u64) -> u64 {
    now.saturating_sub(window.observed_at)
}

/// Keeps each window slot only if it is still *available*, dropping the rest.
/// A window whose `resets_at` has certainly passed says nothing about current
/// usage -- the vendor has already rolled it over -- so it is dropped. A
/// window still inside its own live span (`resets_at` unknown, i.e. `0`, but
/// observed no longer ago than the slot's own length) is kept: within a live
/// window a vendor-reported percent is still an honest lower bound on current
/// usage, even without a reset timestamp to confirm it. Each slot is judged
/// independently, so a stale five_hour reading never drops a still-live
/// seven_day one, or vice versa. Pure: `now` is always the caller's own unix
/// second, never read internally.
pub fn available(windows: &UsageWindows, now: u64) -> UsageWindows {
    fn keep(window: Option<Window>, span: u64, now: u64) -> Option<Window> {
        let w = window?;
        let is_available = if w.resets_at != 0 {
            w.resets_at >= now
        } else {
            age_secs(&w, now) <= span
        };
        is_available.then_some(w)
    }
    UsageWindows {
        five_hour: keep(windows.five_hour, FIVE_HOUR_SECS, now),
        seven_day: keep(windows.seven_day, SEVEN_DAY_SECS, now),
    }
}

pub const FIVE_HOUR_SECS: u64 = 5 * 3600;
pub const SEVEN_DAY_SECS: u64 = 7 * 24 * 3600;

/// How far into the future an event timestamp may sit before it is treated as
/// bogus rather than merely clock-skewed. Within this tolerance a future
/// timestamp still clamps to age-zero via `saturating_sub`, same as before;
/// beyond it, the event is skipped rather than inflating the freshest usage
/// bucket until wall-clock time catches up.
const FUTURE_SKEW_TOLERANCE_SECS: u64 = 5 * 60;

/// Days from the unix epoch for a civil date, valid for any year in range.
/// Howard Hinnant's `days_from_civil`, which is why no date crate is needed.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parses the exact shape claude writes: `2026-07-31T14:15:15.968Z`. Fractional
/// seconds and the offset suffix are ignored; anything else returns `None` so a
/// malformed line is skipped rather than counted at the wrong time.
pub fn parse_iso8601_utc(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }

    let field = |from: usize, to: usize| ts.get(from..to)?.parse::<i64>().ok();
    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let minute = field(14, 16)?;
    let second = field(17, 19)?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(total).ok()
}

/// Cache reads are excluded by default: they are the dominant class in a cached
/// session and are discounted by the API, and the notes file records that the
/// limiter's real weighting is undocumented.
pub fn usage_tokens_of(usage: &Value, count_cache_reads: bool) -> u64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut total =
        field("input_tokens") + field("cache_creation_input_tokens") + field("output_tokens");
    if count_cache_reads {
        total += field("cache_read_input_tokens");
    }
    total
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenSums {
    pub five_hour: u64,
    pub seven_day: u64,
    /// Unix second of the oldest event counted in each window, or `0` when the
    /// window counted nothing. Used to estimate when the window frees up.
    pub oldest_in_five_hour: u64,
    pub oldest_in_seven_day: u64,
    pub files_scanned: usize,
    pub events_counted: usize,
}

fn note_oldest(slot: &mut u64, at: u64) {
    if *slot == 0 || at < *slot {
        *slot = at;
    }
}

/// Accumulates one transcript's assistant usage into the trailing windows.
/// Events without a parseable timestamp cannot be placed in a window and are
/// skipped rather than counted at the wrong time.
pub fn sum_file(jsonl: &str, now: u64, count_cache_reads: bool, into: &mut TokenSums) {
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(at) = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc)
        else {
            continue;
        };
        if at > now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS) {
            continue;
        }
        let age = now.saturating_sub(at);
        if age > SEVEN_DAY_SECS {
            continue;
        }

        let Some(usage) = row.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let tokens = usage_tokens_of(usage, count_cache_reads);

        into.events_counted += 1;
        into.seven_day += tokens;
        note_oldest(&mut into.oldest_in_seven_day, at);
        if age <= FIVE_HOUR_SECS {
            into.five_hour += tokens;
            note_oldest(&mut into.oldest_in_five_hour, at);
        }
    }
}

pub fn projects_root() -> CtxResult<PathBuf> {
    Ok(crate::utils::home_dir()?.join(".claude").join("projects"))
}

/// Walks every transcript under the projects root, including the `subagents/`
/// subdirectories, because subagent turns live in their own files and still
/// spend the account's budget.
pub fn sum_transcripts(projects_root: &Path, now: u64, count_cache_reads: bool) -> TokenSums {
    let mut sums = TokenSums::default();
    let mut stack = vec![projects_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            sums.files_scanned += 1;
            sum_file(&text, now, count_cache_reads, &mut sums);
        }
    }
    sums
}

fn estimated_window(used: u64, budget: u64, oldest: u64, span: u64, now: u64) -> Option<Window> {
    if budget == 0 {
        return None;
    }
    let percent = ((used as f64 / budget as f64) * 100.0).clamp(0.0, 100.0);
    let resets_at = if oldest == 0 { now } else { oldest + span };
    Some(Window {
        used_percentage: percent,
        resets_at,
        observed_at: now,
    })
}

/// Percentages only exist once the operator configures a budget: the notes file
/// records that a plan's real token allowance is undocumented, so a default
/// would be a guess presented as data.
pub fn estimate_windows(
    sums: &TokenSums,
    now: u64,
    five_hour_budget: u64,
    seven_day_budget: u64,
) -> UsageWindows {
    UsageWindows {
        five_hour: estimated_window(
            sums.five_hour,
            five_hour_budget,
            sums.oldest_in_five_hour,
            FIVE_HOUR_SECS,
            now,
        ),
        seven_day: estimated_window(
            sums.seven_day,
            seven_day_budget,
            sums.oldest_in_seven_day,
            SEVEN_DAY_SECS,
            now,
        ),
    }
}

/// Parses an RFC 3339 timestamp ("2026-08-16T20:49:59.785342+00:00", trailing
/// "Z" or "+/-HH:MM", fraction ignored) to unix seconds. None on anything
/// malformed or pre-epoch. Used by the codex collector and the rollout parser.
#[allow(dead_code)]
pub fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let (date, rest) = s.split_once('T')?;
    let mut dp = date.split('-');
    // Bounded the same way `parse_iso8601_utc` is bounded: exactly 4 digits,
    // so `days_from_civil(y, ..) * 86_400` can never see a year absurd enough
    // to overflow `i64` and panic in debug builds. Reachable from wrap's
    // status-bar redraw via the codex rollout scan, so this must degrade to
    // `None`, never panic.
    let y_field = dp.next()?;
    if y_field.len() != 4 || !y_field.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = y_field.parse().ok()?;
    if !(1970..=9999).contains(&y) {
        return None;
    }
    let mo: u64 = dp.next()?.parse().ok()?;
    let d: u64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Split the time from the offset: "Z", or the last '+'/'-' in the string.
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else {
        let idx = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(idx);
        let sign = if off.starts_with('-') { -1i64 } else { 1i64 };
        let (oh, om) = off[1..].split_once(':')?;
        let oh: i64 = oh.parse().ok()?;
        let om: i64 = om.parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    };
    let time = time.split_once('.').map_or(time, |(t, _frac)| t);
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec: i64 = tp.next()?.parse().ok()?;
    if tp.next().is_some()
        || !(0..24).contains(&h)
        || !(0..60).contains(&mi)
        || !(0..61).contains(&sec)
    {
        return None;
    }
    let total =
        days_from_civil(y, mo as i64, d as i64) * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    u64::try_from(total).ok()
}

/// Which UsageWindows slot a window of this length belongs to: the nearest of
/// 5h/7d, accepted only within a factor of two — anything else is a window
/// shape we do not understand and must drop, never guess.
fn window_slot(window_secs: u64) -> Option<bool /* true = five_hour */> {
    if (FIVE_HOUR_SECS / 2..=FIVE_HOUR_SECS * 2).contains(&window_secs) {
        Some(true)
    } else if (SEVEN_DAY_SECS / 2..=SEVEN_DAY_SECS * 2).contains(&window_secs) {
        Some(false)
    } else {
        None
    }
}

/// Maps a codex `rate_limits` object (primary/secondary with used_percent,
/// window_minutes, resets_at in unix seconds) onto UsageWindows. Shared by the
/// rollout collector and the codex poller.
#[allow(dead_code)]
pub fn windows_from_rate_limits(
    limits: &serde_json::Value,
    observed_at: u64,
) -> Option<UsageWindows> {
    let mut out = UsageWindows::default();
    for key in ["primary", "secondary"] {
        let Some(w) = limits.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(used) = w.get("used_percent").and_then(|p| p.as_f64()) else {
            continue;
        };
        let minutes = w
            .get("window_minutes")
            .and_then(|m| m.as_u64())
            .unwrap_or(0);
        let resets_at = w.get("resets_at").and_then(|r| r.as_u64()).unwrap_or(0);
        // Saturating: `window_minutes` comes straight from untrusted JSON, and
        // a value near u64::MAX would otherwise panic in debug builds instead
        // of falling through to the slot rejection below.
        let Some(five_hour) = window_slot(minutes.saturating_mul(60)) else {
            continue;
        };
        let win = Window {
            used_percentage: used,
            resets_at,
            observed_at,
        };
        if five_hour {
            out.five_hour = Some(win);
        } else {
            out.seven_day = Some(win);
        }
    }
    (out.five_hour.is_some() || out.seven_day.is_some()).then_some(out)
}

/// One codex session-rollout JSONL line -> usage windows, if it is a
/// token_count event carrying rate limits.
#[allow(dead_code)]
pub fn parse_rollout_line(line: &str) -> Option<UsageWindows> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = v.get("payload")?;
    if payload.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let observed_at = parse_rfc3339_utc(v.get("timestamp")?.as_str()?)?;
    windows_from_rate_limits(payload.get("rate_limits")?, observed_at)
}

/// The account the codex provider's usage is attributed to.
#[allow(dead_code)]
pub const CODEX_USAGE_PROVIDER: &str = "openai";

/// Floor between codex rollout-tree scan *attempts*, shared by
/// `pace::refresh_sources` (item 5: a parked codex session's wait loop must
/// not re-walk `~/.codex/sessions` on every 30s recheck) and `wrap.rs`'s
/// status-bar refresh (`redraw_bar_if_due`, formerly its own private
/// `CODEX_BAR_SCAN_SECS`) -- one constant so the two floors cannot drift.
pub(crate) const CODEX_SCAN_FLOOR_SECS: u64 = 60;

/// Rollout files grow large; only the tail can hold the newest snapshot.
#[allow(dead_code)]
const ROLLOUT_TAIL_BYTES: u64 = 64 * 1024;
#[allow(dead_code)]
const ROLLOUT_SCAN_FILES: usize = 3;

#[allow(dead_code)]
fn collect_jsonl(
    dir: &Path,
    depth: u8,
    out: &mut Vec<(std::time::SystemTime, std::path::PathBuf)>,
) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
        {
            out.push((modified, path));
        }
    }
}

#[allow(dead_code)]
fn last_snapshot_in(path: &Path, now: u64) -> Option<UsageWindows> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(ROLLOUT_TAIL_BYTES)))
        .ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    // The tail may start mid-line/mid-char; lossy decode + rev line scan copes.
    let text = String::from_utf8_lossy(&buf);
    // Collect all valid snapshots (skew-valid), then return the one with max timestamp
    text.lines()
        .filter_map(parse_rollout_line)
        .filter(|w| newest_observation(w) <= now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS))
        .max_by_key(newest_observation)
}

/// Newest rate-limit snapshot across the most recently modified rollout files.
#[allow(dead_code)]
pub fn scan_codex_rollouts(
    sessions_dir: &Path,
    max_files: usize,
    now: u64,
) -> Option<UsageWindows> {
    let mut files = Vec::new();
    collect_jsonl(sessions_dir, 0, &mut files);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
        .into_iter()
        .take(max_files)
        .filter_map(|(_, p)| last_snapshot_in(&p, now))
        .max_by_key(newest_observation)
}

#[allow(dead_code)]
pub(crate) fn newest_observation(windows: &UsageWindows) -> u64 {
    windows
        .five_hour
        .iter()
        .chain(windows.seven_day.iter())
        .map(|w| w.observed_at)
        .max()
        .unwrap_or(0)
}

/// Opportunistic passive refresh for codex: scan its session rollouts only
/// when the stored reading is stale. Best-effort by design — every failure
/// leaves the stored state exactly as it was.
#[allow(dead_code)]
pub fn refresh_codex_usage(
    state: &StateDir,
    sessions_dir: Option<&Path>,
    now: u64,
    max_age_secs: u64,
) {
    let existing = load_for(state, CODEX_USAGE_PROVIDER);
    if let Some(w) = &existing
        && now.saturating_sub(newest_observation(w)) <= max_age_secs
    {
        return;
    }
    let default_dir = dirs::home_dir().map(|h| h.join(".codex").join("sessions"));
    let Some(dir) = sessions_dir.or(default_dir.as_deref()) else {
        return;
    };
    let Some(fresh) = scan_codex_rollouts(dir, ROLLOUT_SCAN_FILES, now) else {
        return;
    };
    let merged = merge(existing.clone().unwrap_or_default(), fresh);
    if Some(&merged) == existing.as_ref() {
        // The scan produced nothing newer than what is already stored:
        // rewriting an identical file on every refresh is pure churn.
        return;
    }
    let _ = store_for(state, CODEX_USAGE_PROVIDER, &merged);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn documented_rate_limit_fields_are_parsed() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let windows = parse_statusline(&json, 1_784_999_000).expect("rate_limits present");

        let five = windows.five_hour.expect("five_hour");
        assert_eq!(five.used_percentage, 87.5);
        assert_eq!(five.resets_at, 1_785_000_000);
        assert_eq!(five.observed_at, 1_784_999_000);

        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(
            seven.used_percentage, 31.0,
            "integer percentages parse as floats"
        );
        assert_eq!(seven.resets_at, 1_785_400_000);
    }

    #[test]
    fn a_statusline_without_rate_limits_yields_nothing_to_persist() {
        let json = std::fs::read_to_string(fixture("statusline-no-limits.json")).expect("fixture");
        assert_eq!(
            parse_statusline(&json, 1_784_999_000),
            None,
            "non-subscriber and pre-first-response sessions are normal, not errors"
        );
    }

    #[test]
    fn each_window_may_be_independently_absent() {
        let only_five =
            "{\"rate_limits\":{\"five_hour\":{\"used_percentage\":10,\"resets_at\":5}}}";
        let windows = parse_statusline(only_five, 1).expect("five_hour present");
        assert!(windows.five_hour.is_some());
        assert!(windows.seven_day.is_none());
    }

    #[test]
    fn a_window_missing_resets_at_is_still_usable_for_its_percentage() {
        let json = "{\"rate_limits\":{\"five_hour\":{\"used_percentage\":99.9}}}";
        let five = parse_statusline(json, 7)
            .expect("parsed")
            .five_hour
            .expect("five");
        assert_eq!(five.used_percentage, 99.9);
        assert_eq!(
            five.resets_at, 0,
            "zero means unknown, callers use the fallback delay"
        );
    }

    #[test]
    fn available_drops_a_window_whose_resets_at_has_certainly_passed() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 1000,
                observed_at: 500,
            }),
            seven_day: None,
        };
        let out = available(&windows, 1001);
        assert_eq!(
            out.five_hour, None,
            "resets_at in the past means the reading says nothing about now"
        );
    }

    #[test]
    fn available_keeps_a_window_whose_resets_at_is_still_in_the_future() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 1000,
                observed_at: 500,
            }),
            seven_day: None,
        };
        let out = available(&windows, 999);
        assert_eq!(out.five_hour, windows.five_hour);
    }

    #[test]
    fn available_keeps_a_zero_resets_at_window_that_is_still_fresh() {
        let windows = UsageWindows {
            five_hour: None,
            seven_day: Some(Window {
                used_percentage: 20.0,
                resets_at: 0,
                observed_at: 1000,
            }),
        };
        // Just inside the seven_day span from observation.
        let out = available(&windows, 1000 + SEVEN_DAY_SECS);
        assert_eq!(out.seven_day, windows.seven_day);
    }

    #[test]
    fn available_drops_a_zero_resets_at_window_older_than_its_span() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 20.0,
                resets_at: 0,
                observed_at: 1000,
            }),
            seven_day: None,
        };
        // One second past the five_hour span from observation.
        let out = available(&windows, 1000 + FIVE_HOUR_SECS + 1);
        assert_eq!(
            out.five_hour, None,
            "no resets_at and older than the window's own span is stale, not honest"
        );
    }

    #[test]
    fn available_judges_each_slot_independently() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 100,
                observed_at: 0,
            }),
            seven_day: Some(Window {
                used_percentage: 20.0,
                resets_at: 100_000,
                observed_at: 0,
            }),
        };
        let out = available(&windows, 5000);
        assert_eq!(
            out.five_hour, None,
            "the expired five_hour reading must not survive"
        );
        assert_eq!(
            out.seven_day, windows.seven_day,
            "a stale five_hour slot must not drop a still-live seven_day slot"
        );
    }

    #[test]
    fn garbage_input_parses_to_nothing_rather_than_erroring() {
        assert_eq!(parse_statusline("not json at all", 1), None);
        assert_eq!(parse_statusline("", 1), None);
        assert_eq!(parse_statusline("{\"rate_limits\":\"nope\"}", 1), None);
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        assert_eq!(
            load(&state),
            UsageWindows::default(),
            "absent file is empty state"
        );

        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 50.0,
                resets_at: 100,
                observed_at: 10,
            }),
            seven_day: None,
        };
        store(&state, &windows).expect("store");
        assert_eq!(load(&state), windows);
    }

    #[test]
    fn a_corrupt_state_file_reads_as_empty_instead_of_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        std::fs::create_dir_all(state.root()).expect("mkdir");
        std::fs::write(state.usage(), "{ this is not json").expect("write");
        assert_eq!(load(&state), UsageWindows::default());
    }

    #[test]
    fn store_leaves_no_partial_file_behind() {
        // Concurrent live sessions all write this file, so the write is atomic:
        // a temp file plus rename, never a truncate-then-write.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store(&state, &UsageWindows::default()).expect("store");
        let strays: Vec<_> = std::fs::read_dir(state.root())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "usage.json")
            .collect();
        assert!(
            strays.is_empty(),
            "temp file was not cleaned up: {strays:?}"
        );
    }

    fn windows_at(pct: f64, observed_at: u64) -> UsageWindows {
        UsageWindows {
            five_hour: Some(Window {
                used_percentage: pct,
                resets_at: 1000,
                observed_at,
            }),
            seven_day: None,
        }
    }

    /// The slug the legacy file's data is attributed to has to be the same
    /// slug the claude adapter reports, or an upgrading user's readings would
    /// be filed under a provider nothing ever asks about.
    #[test]
    fn the_legacy_file_is_attributed_to_the_claude_adapters_own_provider() {
        use crate::commands::ctx::adapters::AgentAdapter;
        assert_eq!(
            crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None).provider(),
            LEGACY_USAGE_PROVIDER
        );
    }

    #[test]
    fn per_provider_state_round_trips_through_its_own_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        assert_eq!(
            load_for(&state, "openai"),
            None,
            "a provider with no collector has no source, which is not zero"
        );

        let windows = windows_at(50.0, 10);
        store_for(&state, "openai", &windows).expect("store");
        assert_eq!(load_for(&state, "openai"), Some(windows.clone()));
        assert_eq!(
            load(&state),
            UsageWindows::default(),
            "a provider write must not touch the legacy global file"
        );
    }

    /// E: codex/openai has no possible source (no tee writes for it, ever,
    /// today), so a fresh state dir with nothing written must read that way
    /// -- even after storing data for a *different* provider, which must
    /// never leak into this one's answer.
    #[test]
    fn has_no_usage_source_is_true_for_a_provider_with_no_collector() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(has_no_usage_source(&state, "openai"));

        store_for(&state, "anthropic", &windows_at(50.0, 10)).expect("store");
        assert!(
            has_no_usage_source(&state, "openai"),
            "another provider's own data must not count as this one's source"
        );
    }

    /// E: the codex collector and poller now exist, so no provider is
    /// structurally exempt any more. Callers refresh sources first, then ask
    /// whether one is available.
    #[test]
    fn has_no_usage_source_is_true_for_any_provider_when_nothing_recorded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(
            has_no_usage_source(&state, "anthropic"),
            "no provider is exempt: all are 'nothing recorded' when no file exists"
        );
    }

    /// The upgrade case, and the one that matters most: a user who has been
    /// collecting into the legacy global file since before per-provider files
    /// existed must not see their readout go blank. The legacy file is read,
    /// never moved or deleted.
    #[test]
    fn an_upgrading_users_legacy_reading_still_shows_under_anthropic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let legacy = windows_at(87.5, 10);
        store(&state, &legacy).expect("store the legacy file, as an older zirv did");
        assert!(
            !state.usage_for("anthropic").exists(),
            "no provider file exists yet: this is exactly the upgrade moment"
        );

        assert_eq!(
            load_for(&state, "anthropic"),
            Some(legacy),
            "the legacy file backs anthropic until a provider file exists"
        );
        assert!(
            state.usage().exists(),
            "the legacy file is read, never moved: other readers still use it"
        );
    }

    /// Once a provider file exists it is the answer; the legacy file is only
    /// ever the fallback, so a fresh reading is never shadowed by an old one.
    #[test]
    fn a_provider_file_wins_over_the_legacy_fallback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store(&state, &windows_at(10.0, 10)).expect("legacy");
        store_for(&state, "anthropic", &windows_at(90.0, 99)).expect("provider");

        let five = load_for(&state, "anthropic")
            .expect("present")
            .five_hour
            .expect("five");
        assert_eq!(five.used_percentage, 90.0);
    }

    #[test]
    fn a_provider_store_leaves_no_partial_file_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store_for(&state, "anthropic", &UsageWindows::default()).expect("store");
        let strays: Vec<_> = std::fs::read_dir(state.root())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "usage-anthropic.json")
            .collect();
        assert!(
            strays.is_empty(),
            "temp file was not cleaned up: {strays:?}"
        );
    }

    #[test]
    fn merging_keeps_the_newest_observation_per_window() {
        let old = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 10.0,
                resets_at: 100,
                observed_at: 10,
            }),
            seven_day: Some(Window {
                used_percentage: 20.0,
                resets_at: 200,
                observed_at: 10,
            }),
        };
        let fresh = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: 300,
                observed_at: 50,
            }),
            seven_day: None,
        };

        let merged = merge(old, fresh);
        assert_eq!(merged.five_hour.expect("five").used_percentage, 90.0);
        assert_eq!(
            merged.seven_day.expect("seven").used_percentage,
            20.0,
            "an absent window in a fresh reading must not erase what is known"
        );
    }

    #[test]
    fn merging_never_moves_a_window_backwards_in_time() {
        let newer = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: 300,
                observed_at: 50,
            }),
            seven_day: None,
        };
        let stale = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 5.0,
                resets_at: 100,
                observed_at: 10,
            }),
            seven_day: None,
        };
        let merged = merge(newer, stale);
        assert_eq!(
            merged.five_hour.expect("five").used_percentage,
            90.0,
            "a late-arriving stale sample must not win"
        );
    }

    #[test]
    fn age_is_measured_from_the_observation() {
        let window = Window {
            used_percentage: 1.0,
            resets_at: 0,
            observed_at: 100,
        };
        assert_eq!(age_secs(&window, 160), 60);
        assert_eq!(
            age_secs(&window, 90),
            0,
            "clock skew reads as fresh, not negative"
        );
    }

    #[test]
    fn real_transcript_timestamps_parse_to_unix_seconds() {
        // Exact format observed in ~/.claude/projects/**/*.jsonl.
        assert_eq!(
            parse_iso8601_utc("2026-07-31T14:15:15.968Z"),
            Some(1_785_507_315),
        );
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso8601_utc("1970-01-02T00:00:01.000Z"), Some(86_401));
        // Leap-year handling, since the window arithmetic depends on it.
        assert_eq!(
            parse_iso8601_utc("2024-02-29T00:00:00.000Z"),
            Some(1_709_164_800)
        );
    }

    #[test]
    fn malformed_timestamps_are_skipped_not_guessed() {
        assert_eq!(parse_iso8601_utc(""), None);
        assert_eq!(parse_iso8601_utc("yesterday"), None);
        assert_eq!(parse_iso8601_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601_utc("2026-07-31"), None);
    }

    #[test]
    fn cache_reads_are_excluded_by_default_and_optional() {
        // The usage block of a real cached assistant event.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(
            usage_tokens_of(&usage, false),
            1036,
            "input + cache_creation + output, cache reads excluded"
        );
        assert_eq!(usage_tokens_of(&usage, true), 109_463);
        assert_eq!(usage_tokens_of(&serde_json::json!({}), false), 0);
    }

    /// Builds a transcript whose assistant events sit at given ages in seconds.
    fn transcript_with_ages(now: u64, ages: &[u64], tokens: u64) -> String {
        let mut text = String::new();
        for age in ages {
            let at = now - age;
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":{tokens},\"cache_read_input_tokens\":999999}}}}}}\n",
                iso_of(at)
            ));
        }
        text
    }

    /// Inverse of `parse_iso8601_utc`, for building fixtures only.
    fn iso_of(unix: u64) -> String {
        let days = (unix / 86_400) as i64;
        let secs = unix % 86_400;
        let (year, month, day) = civil_from_days_for_tests(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }

    fn civil_from_days_for_tests(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn the_fixture_timestamp_helper_round_trips() {
        for unix in [0_u64, 1_785_507_315, 1_709_164_800] {
            assert_eq!(
                parse_iso8601_utc(&iso_of(unix)),
                Some(unix),
                "round trip {unix}"
            );
        }
    }

    #[test]
    fn a_future_dated_timestamp_is_excluded_from_every_bucket() {
        // Clock skew or corrupt data can date an event a day in the future.
        // `now.saturating_sub` would otherwise read that as age-zero and
        // inflate the freshest usage bucket until wall-clock time catches up.
        let now = 1_785_507_315;
        let far_future_at = now + 86_400;
        let jsonl = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":100}}}}}}\n",
            iso_of(far_future_at)
        );

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(sums.five_hour, 0);
        assert_eq!(sums.seven_day, 0);
        assert_eq!(
            sums.events_counted, 0,
            "a far-future-dated event must not be counted at all"
        );
    }

    #[test]
    fn a_timestamp_within_the_skew_tolerance_still_clamps_to_age_zero() {
        // A few seconds of clock skew, well inside the tolerance, keeps today's
        // behavior: it counts, clamped to age zero.
        let now = 1_785_507_315;
        let slightly_future_at = now + 30;
        let jsonl = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":100}}}}}}\n",
            iso_of(slightly_future_at)
        );

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(
            sums.five_hour, 100,
            "small skew still clamps to age zero, as before"
        );
        assert_eq!(sums.events_counted, 1);
    }

    #[test]
    fn only_events_inside_each_window_are_summed() {
        let now = 1_785_507_315;
        // 1h ago (both windows), 6h ago (7d only), 8d ago (neither).
        let jsonl = transcript_with_ages(now, &[3600, 21_600, 691_200], 100);

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(sums.five_hour, 100, "one event within 5h");
        assert_eq!(sums.seven_day, 200, "two events within 7d");
        assert_eq!(sums.events_counted, 2);
    }

    #[test]
    fn the_oldest_counted_event_is_tracked_for_reset_estimation() {
        let now = 1_785_507_315;
        let jsonl = transcript_with_ages(now, &[3600, 7200], 10);
        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(sums.oldest_in_five_hour, now - 7200);
        assert_eq!(sums.oldest_in_seven_day, now - 7200);
    }

    #[test]
    fn non_assistant_and_malformed_lines_are_ignored() {
        let now = 1_785_507_315;
        let mut jsonl = String::new();
        jsonl.push_str("{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n");
        jsonl.push_str("not json\n\n");
        jsonl.push_str("{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n");
        jsonl.push_str(&transcript_with_ages(now, &[60], 7));

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(
            sums.five_hour, 7,
            "the event with no timestamp cannot be placed"
        );
        assert_eq!(sums.events_counted, 1);
    }

    #[test]
    fn the_walk_includes_subagent_files() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let session_dir = projects.join("-home-testuser-repo");
        std::fs::create_dir_all(session_dir.join("subagents")).expect("mkdir");

        std::fs::write(
            session_dir.join("main.jsonl"),
            transcript_with_ages(now, &[600], 100),
        )
        .expect("write main");
        std::fs::write(
            session_dir.join("subagents").join("sub.jsonl"),
            transcript_with_ages(now, &[600], 25),
        )
        .expect("write subagent");
        // A non-transcript file must not be parsed.
        std::fs::write(session_dir.join("notes.txt"), "ignore me").expect("write txt");

        let sums = sum_transcripts(&projects, now, false);
        assert_eq!(sums.files_scanned, 2, "main plus subagent, not the txt");
        assert_eq!(
            sums.five_hour, 125,
            "subagent turns live in their own files and must be counted"
        );
    }

    #[test]
    fn an_absent_projects_root_sums_to_zero() {
        let sums = sum_transcripts(std::path::Path::new("/nonexistent/projects"), 100, false);
        assert_eq!(sums, TokenSums::default());
    }

    #[test]
    fn percentages_need_a_configured_budget() {
        let now = 1_785_507_315;
        let sums = TokenSums {
            five_hour: 500,
            seven_day: 2000,
            oldest_in_five_hour: now - 3600,
            oldest_in_seven_day: now - 86_400,
            files_scanned: 1,
            events_counted: 4,
        };

        assert_eq!(
            estimate_windows(&sums, now, 0, 0),
            UsageWindows::default(),
            "no budget means no honest percentage"
        );

        let windows = estimate_windows(&sums, now, 1000, 8000);
        let five = windows.five_hour.expect("five_hour");
        assert_eq!(five.used_percentage, 50.0);
        assert_eq!(five.observed_at, now);
        assert_eq!(
            five.resets_at,
            now - 3600 + FIVE_HOUR_SECS,
            "a rolling window frees up when its oldest counted event ages out"
        );

        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(seven.used_percentage, 25.0);
        assert_eq!(seven.resets_at, now - 86_400 + SEVEN_DAY_SECS);
    }

    #[test]
    fn percentages_are_capped_at_one_hundred() {
        let now = 1_000_000;
        let sums = TokenSums {
            five_hour: 5000,
            seven_day: 0,
            oldest_in_five_hour: now - 60,
            oldest_in_seven_day: 0,
            files_scanned: 1,
            events_counted: 1,
        };
        let five = estimate_windows(&sums, now, 1000, 0)
            .five_hour
            .expect("five");
        assert_eq!(five.used_percentage, 100.0);
    }

    #[test]
    fn a_window_with_no_events_reports_zero_and_resets_now() {
        let now = 1_000_000;
        let windows = estimate_windows(&TokenSums::default(), now, 1000, 1000);
        let five = windows.five_hour.expect("five");
        assert_eq!(five.used_percentage, 0.0);
        assert_eq!(five.resets_at, now, "nothing to wait for");
    }

    #[test]
    fn rfc3339_utc_parses_fraction_and_offset() {
        // 2026-02-26T18:52:21.222Z -> known epoch; verify against a precomputed value.
        let z = parse_rfc3339_utc("2026-02-26T18:52:21.222Z").unwrap();
        let plus = parse_rfc3339_utc("2026-02-26T18:52:21.222+00:00").unwrap();
        assert_eq!(z, plus);
        // +01:00 is one hour EARLIER in UTC
        let cet = parse_rfc3339_utc("2026-02-26T19:52:21+01:00").unwrap();
        assert_eq!(z, cet);
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_utc("not a time"), None);
        assert_eq!(parse_rfc3339_utc("2026-13-40T99:00:00Z"), None);
    }

    #[test]
    fn rollout_snapshot_maps_primary_and_secondary_by_window_length() {
        let lines: Vec<&str> =
            include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl")
                .lines()
                .collect();
        let w = parse_rollout_line(lines[0]).unwrap();
        let fh = w.five_hour.unwrap();
        assert_eq!(fh.used_percentage, 10.0);
        assert_eq!(fh.resets_at, 1772135737);
        let sd = w.seven_day.unwrap();
        assert_eq!(sd.used_percentage, 3.0);
        assert_eq!(sd.resets_at, 1772722537);
        // observed_at comes from the line's own timestamp, not scan time
        assert_eq!(
            fh.observed_at,
            parse_rfc3339_utc("2026-02-26T18:52:21.222Z").unwrap()
        );
        // populated-info shape parses identically
        assert!(parse_rollout_line(lines[1]).is_some());
        // non-token_count lines and garbage yield None
        assert!(parse_rollout_line(lines[2]).is_none());
        assert!(parse_rollout_line("{broken").is_none());
        // a 1-minute window maps to neither slot -> dropped -> no windows -> None
        assert!(parse_rollout_line(lines[3]).is_none());
        // an absurd window_minutes must reject, never overflow-panic (review
        // finding on 4a44eb2: `minutes * 60` panicked in debug builds)
        let huge = format!(
            "{{\"timestamp\":\"2026-02-26T18:52:21.222Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"rate_limits\":{{\"primary\":{{\"used_percent\":1.0,\"window_minutes\":{},\"resets_at\":1}}}}}}}}",
            u64::MAX
        );
        assert!(parse_rollout_line(&huge).is_none());
    }

    #[test]
    fn scan_finds_newest_by_timestamp_not_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        // lines[0]: 10% snapshot at 2026-02-26T18:52:21.222Z
        // lines[1]: 12% snapshot at 2026-02-26T18:52:27.310Z (newer timestamp)
        // rollout-a.jsonl: mtime older, holds 12% (newer timestamp) -> should win
        // rollout-b.jsonl: mtime newer, holds 10% (older timestamp) -> should lose
        std::fs::write(day.join("rollout-a.jsonl"), format!("{}\n", lines[1])).unwrap();
        std::fs::write(day.join("rollout-b.jsonl"), format!("{}\n", lines[0])).unwrap();
        // make b's mtime strictly newer (but it has the older timestamp)
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options()
            .append(true)
            .open(day.join("rollout-b.jsonl"))
            .unwrap();
        f.set_modified(newer).unwrap();
        let now = 1_784_999_000u64;
        let w = scan_codex_rollouts(dir.path(), 3, now).unwrap();
        assert_eq!(
            w.five_hour.unwrap().used_percentage,
            12.0,
            "should pick the snapshot with the newest embedded timestamp, not mtime"
        );
    }

    #[test]
    fn scan_of_missing_or_empty_dir_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_784_999_000u64;
        assert!(scan_codex_rollouts(&dir.path().join("nope"), 3, now).is_none());
        assert!(scan_codex_rollouts(dir.path(), 3, now).is_none());
    }

    #[test]
    fn scan_finds_newest_snapshot_among_out_of_order_lines() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        // Write lines in reverse order: 12% first, then 10%, so the first one is NOT the max
        // Verifies we pick the max timestamp, not just the first or last line
        std::fs::write(
            day.join("rollout.jsonl"),
            format!("{}\n{}\n", lines[1], lines[0]),
        )
        .unwrap();
        let now = 1_784_999_000u64;
        let w = scan_codex_rollouts(dir.path(), 3, now).unwrap();
        assert_eq!(
            w.five_hour.unwrap().used_percentage,
            12.0,
            "should pick snapshot with newest timestamp despite line order"
        );
    }

    #[test]
    fn scan_skips_far_future_dated_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let now = 1_784_999_000u64;
        // Create a line with far-future timestamp beyond the skew tolerance
        let far_future_json = r#"{"timestamp":"2099-12-31T23:59:59Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":50.0,"window_minutes":300,"resets_at":1772135737},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1772722537}}}}"#;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", far_future_json)).unwrap();
        // Should find no snapshot (far-future is skipped)
        assert!(
            scan_codex_rollouts(dir.path(), 3, now).is_none(),
            "far-future snapshot should be skipped"
        );
    }

    #[test]
    fn scan_uses_valid_snapshot_after_skipping_future_dated_line() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        let now = 1_784_999_000u64;
        // Create a line with far-future timestamp
        let far_future_json = r#"{"timestamp":"2099-12-31T23:59:59Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":99.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        // Write future line first, then valid 12% line
        std::fs::write(
            day.join("rollout.jsonl"),
            format!("{}\n{}\n", far_future_json, lines[1]),
        )
        .unwrap();
        let w = scan_codex_rollouts(dir.path(), 3, now).unwrap();
        assert_eq!(
            w.five_hour.unwrap().used_percentage,
            12.0,
            "should use the valid (non-future) snapshot when future-dated line is present"
        );
    }

    #[test]
    fn refresh_skips_when_stored_reading_is_fresh_and_stores_when_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        // Create a codex sessions dir with a test file
        let sessions_dir = tmp.path().join("codex_sessions");
        let day = sessions_dir.join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();

        // A rollout line just before `now`, so a scan of it is genuinely
        // newer than the "stale" stored reading below and the merge must
        // prefer it -- the review of c3c7fe9 caught this test asserting
        // nothing when the line's timestamp predated the stale reading.
        let test_json = r#"{"timestamp":"2026-02-26T18:52:21Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        let line_ts = parse_rfc3339_utc("2026-02-26T18:52:21Z").expect("test timestamp parses");
        let now = line_ts + 60;
        let max_age = 900u64;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", test_json)).unwrap();

        let scanned = scan_codex_rollouts(sessions_dir.as_path(), ROLLOUT_SCAN_FILES, now);
        assert!(
            scanned.is_some(),
            "scan should find snapshot with compatible timestamp"
        );
        let scanned_val = scanned.unwrap();
        assert_eq!(
            scanned_val.five_hour.unwrap().used_percentage,
            12.0,
            "scan should find 12%"
        );

        // Pre-store a fresh openai reading (observed_at close to now)
        let fresh = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 50.0,
                resets_at: 1000,
                observed_at: now - 100, // well within max_age
            }),
            seven_day: None,
        };
        store_for(&state, CODEX_USAGE_PROVIDER, &fresh).expect("store fresh");

        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, max_age);
        let after_fresh_refresh = load_for(&state, CODEX_USAGE_PROVIDER);
        assert_eq!(
            after_fresh_refresh,
            Some(fresh),
            "fresh reading should not be updated"
        );

        // Now pre-store a stale reading (observed_at = now - 10_000)
        let stale = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 20.0,
                resets_at: 500,
                observed_at: now - 10_000, // well beyond max_age
            }),
            seven_day: None,
        };
        store_for(&state, CODEX_USAGE_PROVIDER, &stale).expect("store stale");

        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, max_age);
        let after_stale_refresh = load_for(&state, CODEX_USAGE_PROVIDER);

        // The scanned 12% (observed_at = now - 60) is newer than the stale
        // 20% (observed_at = now - 10_000), so the merge must replace it.
        let merged = after_stale_refresh.expect("merged present");
        let five = merged.five_hour.expect("five_hour after refresh");
        assert_eq!(
            five.used_percentage, 12.0,
            "a stale stored reading is replaced by the fresher scan"
        );
        assert_eq!(five.resets_at, 1772135737);
    }

    #[test]
    fn no_usage_source_is_now_a_plain_no_data_check() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        // openai has nothing stored -> true
        assert!(has_no_usage_source(&state, CODEX_USAGE_PROVIDER));

        // Store something for openai -> false
        store_for(
            &state,
            CODEX_USAGE_PROVIDER,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: 50.0,
                    resets_at: 1000,
                    observed_at: 10,
                }),
                seven_day: None,
            },
        )
        .expect("store");
        assert!(!has_no_usage_source(&state, CODEX_USAGE_PROVIDER));

        // anthropic with nothing stored -> true (previously hardcoded false)
        let tmp2 = tempfile::tempdir().unwrap();
        let state2 = StateDir::from_root(tmp2.path().to_path_buf());
        assert!(has_no_usage_source(&state2, "anthropic"));

        // Store something for anthropic -> false
        store_for(
            &state2,
            "anthropic",
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: 75.0,
                    resets_at: 2000,
                    observed_at: 20,
                }),
                seven_day: None,
            },
        )
        .expect("store");
        assert!(!has_no_usage_source(&state2, "anthropic"));
    }

    /// Item 1 (review): an absurd year must never reach `days_from_civil`'s
    /// multiplication, which overflows `i64` and panics in debug builds.
    /// Reachable from wrap's status-bar redraw via the codex rollout scan.
    #[test]
    fn an_absurd_year_is_rejected_not_overflowed() {
        assert_eq!(
            parse_rfc3339_utc("999999999999-01-01T00:00:00Z"),
            None,
            "must reject, never panic"
        );
        assert_eq!(parse_rfc3339_utc("99999-01-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc("1969-12-31T23:59:59Z"), None);
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert!(parse_rfc3339_utc("9999-12-31T23:59:59Z").is_some());

        // Same absurd year, carried through a full rollout line: must yield
        // `None` end to end, never panic.
        let line = r#"{"timestamp":"999999999999-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1}}}}"#;
        assert_eq!(parse_rollout_line(line), None);
    }

    /// Item 2 (review): `refresh_codex_usage` must not rewrite the stored
    /// file when a scan produces exactly what is already on disk -- that is
    /// pure churn on every passive refresh. Proven via the file's mtime: a
    /// real `store_for` always renames a fresh temp file over the target,
    /// which would move the mtime forward.
    #[test]
    fn refresh_skips_the_store_when_the_merge_produces_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let sessions_dir = tmp.path().join("codex_sessions");
        let day = sessions_dir.join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let test_json = r#"{"timestamp":"2026-02-26T18:52:21Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", test_json)).unwrap();
        let line_ts = parse_rfc3339_utc("2026-02-26T18:52:21Z").expect("test timestamp parses");

        // Seed the store with exactly what a scan of this file produces, so
        // the merge that follows is a genuine no-op.
        let scanned = scan_codex_rollouts(&sessions_dir, ROLLOUT_SCAN_FILES, line_ts + 60)
            .expect("scan finds the seeded snapshot");
        store_for(&state, CODEX_USAGE_PROVIDER, &scanned).expect("seed store");

        let usage_path = state.usage_for(CODEX_USAGE_PROVIDER);
        // Back-date the file's mtime so a rewrite would be observable.
        let old_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::File::options()
            .append(true)
            .open(&usage_path)
            .unwrap();
        f.set_modified(old_mtime).unwrap();
        let before = std::fs::metadata(&usage_path).unwrap().modified().unwrap();

        // `now` is far enough past the seeded observation that the existing
        // reading counts as stale, forcing the function past the early
        // freshness return and into the scan-and-merge path.
        let now = line_ts + 60 + 10_000;
        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, 900);

        let after = std::fs::metadata(&usage_path).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "store_for must be skipped when the merge is unchanged"
        );
    }
}
