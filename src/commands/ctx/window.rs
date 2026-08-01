// Consumed by the usage verb and the pacing gate in later tasks of this plan.
#![allow(dead_code)]

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
        std::fs::create_dir_all(parent)?;
    }
    let temp = target.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_string(windows)?)?;
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
}
