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

/// Never fails: an absent or corrupt file reads as "nothing known", because a
/// statusline hook must not break on a half-written state file.
pub fn load(state: &StateDir) -> UsageWindows {
    std::fs::read_to_string(state.usage())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic: every live session's statusline writes this file, so a reader must
/// never observe a truncated one.
pub fn store(state: &StateDir, windows: &UsageWindows) -> CtxResult<()> {
    let target = state.usage();
    if let Some(parent) = target.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    let temp = target.with_extension(format!("tmp{}", std::process::id()));
    super::state::write_private(&temp, &serde_json::to_string(windows)?)?;
    std::fs::rename(&temp, &target)?;
    Ok(())
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
}
