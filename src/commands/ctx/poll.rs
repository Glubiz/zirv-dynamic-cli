//! Active usage-poll fallback: consulted only when the passive collector
//! reading is stale at a decision point. Every failure degrades to whatever
//! passive data exists — this module must never make a session worse.

use serde::{Deserialize, Serialize};

use super::config::PaceConfig;
use super::state::StateDir;
use super::window::UsageWindows;

#[allow(dead_code)]
const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
#[allow(dead_code)]
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
/// UNVERIFIED (2026-08-16: no readable token on the reference machine to
/// exercise it). Ships best-effort; see Known Issues.
#[allow(dead_code)]
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
#[allow(dead_code)]
const HTTP_TIMEOUT_SECS: u64 = 10;
/// Distinct from the overall/body timeout above: an unreachable endpoint
/// must not hold a supervisor's cycle-launch gate for up to `HTTP_TIMEOUT_
/// SECS` per attempt just to fail to connect.
#[allow(dead_code)]
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 3;

// `maybe_poll`/`UsagePoller` are wired into the pacing gate
// (`pace::refresh_sources`, all four `wait_for_window` call sites) and into
// `zirv ctx usage`'s no-subcommand readout. The `#[allow(dead_code)]`
// markers below predate that wiring and are kept only until the next
// cleanup pass confirms which items every build target actually reaches.

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct PollReading {
    pub windows: UsageWindows,
    /// Vendor-side credits state, advisory only (anthropic: extra_usage.is_enabled).
    pub vendor_credits_enabled: Option<bool>,
}

#[allow(dead_code)]
pub trait UsagePoller {
    fn poll(&self, provider: &str) -> Option<PollReading>;
}

/// The real poller: makes a blocking HTTP request against the vendor's usage
/// endpoint using the operator's own OAuth token. Constructed by the pacing
/// gate call sites and `zirv ctx usage`; tests never construct it -- they
/// stub `UsagePoller` instead, and every test that can reach a construction
/// site redirects home via `HomeGuard` so no token file is ever readable
/// (this module's tests never touch the network; see the module-level
/// security constraint).
#[allow(dead_code)]
pub struct HttpPoller;

/// Built once and reused for every call: constructing a `ureq::Agent`
/// spins up a fresh rustls config and root store, which is wasted work on
/// every `HttpPoller::poll` call otherwise. The connect timeout is kept
/// short (`HTTP_CONNECT_TIMEOUT_SECS`) and distinct from the overall/body
/// timeout (`HTTP_TIMEOUT_SECS`) so an unreachable endpoint fails fast
/// instead of blocking a supervisor's cycle-launch gate for up to 10s per
/// attempt.
#[allow(dead_code)]
static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

#[allow(dead_code)]
fn shared_agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .timeout_connect(Some(std::time::Duration::from_secs(
                HTTP_CONNECT_TIMEOUT_SECS,
            )))
            .build()
            .into()
    })
}

#[allow(dead_code)]
fn parse_anthropic_usage(body: &str, now: u64) -> Option<PollReading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let read_window = |key: &str| -> Option<super::window::Window> {
        let w = v.get(key).filter(|w| w.is_object())?;
        Some(super::window::Window {
            used_percentage: w.get("utilization")?.as_f64()?,
            resets_at: w
                .get("resets_at")
                .and_then(|r| r.as_str())
                .and_then(super::window::parse_rfc3339_utc)
                .unwrap_or(0),
            observed_at: now,
        })
    };
    let windows = UsageWindows {
        five_hour: read_window("five_hour"),
        seven_day: read_window("seven_day"),
    };
    (windows.five_hour.is_some() || windows.seven_day.is_some()).then(|| PollReading {
        windows,
        vendor_credits_enabled: v
            .pointer("/extra_usage/is_enabled")
            .and_then(|b| b.as_bool()),
    })
}

#[allow(dead_code)]
fn parse_codex_usage(body: &str, now: u64) -> Option<PollReading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let limits = v.get("rate_limits").unwrap_or(&v);
    let windows = super::window::windows_from_rate_limits(limits, now)?;
    let vendor_credits_enabled = limits
        .pointer("/credits/has_credits")
        .and_then(|b| b.as_bool());
    Some(PollReading {
        windows,
        vendor_credits_enabled,
    })
}

/// Reads the OAuth access token straight from disk on every call: it must
/// never be cached, logged, or persisted anywhere but the request this call
/// makes -- see the module's security constraint.
///
/// Resolved via `crate::utils::home_dir()` (`HOME`/`USERPROFILE`), not
/// `dirs::home_dir()`: on Windows the latter calls `SHGetKnownFolderPath`
/// directly and ignores both env vars, so a test's `HomeGuard` -- the
/// mechanism every other home-directory override in this crate relies on --
/// could never point this at a fixture instead of the operator's real
/// credentials. This is also what keeps `cargo test` from ever making a real
/// network call with the operator's own token, which is the whole point of
/// this module's "tests never touch the network" constraint once a real
/// caller (`wait_for_window`, `zirv ctx usage`) actually constructs an
/// `HttpPoller`.
#[allow(dead_code)]
fn anthropic_token() -> Option<String> {
    let path = crate::utils::home_dir()
        .ok()?
        .join(".claude")
        .join(".credentials.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(
        v.pointer("/claudeAiOauth/accessToken")?
            .as_str()?
            .to_owned(),
    )
}

/// Same contract as [`anthropic_token`]: read fresh on every call, never
/// cached or logged, and resolved the same env-aware way for the same
/// reason.
#[allow(dead_code)]
fn codex_token() -> Option<String> {
    let path = crate::utils::home_dir()
        .ok()?
        .join(".codex")
        .join("auth.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.pointer("/tokens/access_token")
        .or_else(|| v.get("access_token"))
        .and_then(|t| t.as_str())
        .map(str::to_owned)
}

impl UsagePoller for HttpPoller {
    fn poll(&self, provider: &str) -> Option<PollReading> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let (url, token, extra_header) = match provider {
            "anthropic" => (
                ANTHROPIC_USAGE_URL,
                anthropic_token()?,
                Some(("anthropic-beta", ANTHROPIC_OAUTH_BETA)),
            ),
            super::window::CODEX_USAGE_PROVIDER => (CODEX_USAGE_URL, codex_token()?, None),
            _ => return None,
        };
        let agent = shared_agent();
        let mut req = agent
            .get(url)
            .header("Authorization", &format!("Bearer {token}"));
        if let Some((k, v)) = extra_header {
            req = req.header(k, v);
        }
        let body = req.call().ok()?.body_mut().read_to_string().ok()?;
        match provider {
            "anthropic" => parse_anthropic_usage(&body, now),
            _ => parse_codex_usage(&body, now),
        }
    }
}

/// `{"last_attempt": u64}` marker written every time a real poll is
/// *attempted* (whether it succeeds or not), so a failed attempt still
/// counts against `poll_min_interval_secs` and a provider with a broken
/// token does not retry on every single call.
#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct PollMarker {
    last_attempt: u64,
}

#[allow(dead_code)]
fn last_attempt(state: &StateDir, provider: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(state.poll_marker_for(provider)).ok()?;
    serde_json::from_str::<PollMarker>(&contents)
        .ok()
        .map(|m| m.last_attempt)
}

/// Mirrors `window.rs`'s `store_at`: a temp sibling written in full, then
/// renamed over the target, so a concurrent reader never observes a
/// truncated marker. Best-effort: marker I/O failures degrade to "not
/// floored" rather than surfacing as an error, same as every other failure
/// path in this module.
#[allow(dead_code)]
fn record_attempt(state: &StateDir, provider: &str, now: u64) {
    let path = state.poll_marker_for(provider);
    let Some(parent) = path.parent() else {
        return;
    };
    if super::state::create_private_dir_all(parent).is_err() {
        return;
    }
    let Ok(contents) = serde_json::to_string(&PollMarker { last_attempt: now }) else {
        return;
    };
    let _ = super::state::write_private(&path, &contents);
}

/// Some(reading) when a poll ran, produced data and it was stored; None covers
/// "not needed", "floored", "disabled" and "failed" alike — callers never
/// branch on why. The reading carries the vendor_credits_enabled advisory for
/// callers that surface it (`zirv ctx usage`).
#[allow(dead_code)]
pub fn maybe_poll(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
    poller: &dyn UsagePoller,
) -> Option<PollReading> {
    if !cfg.poll_enabled {
        return None;
    }
    let existing = super::window::load_for(state, provider);
    if let Some(w) = &existing
        && now.saturating_sub(super::window::freshest_available_observation(w, now))
            <= cfg.collector_max_age_secs
    {
        return None; // passive data is fresh enough; the poll exists only as fallback
    }
    if last_attempt(state, provider)
        .is_some_and(|t| now.saturating_sub(t) < cfg.poll_min_interval_secs)
    {
        return None;
    }
    record_attempt(state, provider, now); // failed attempts count against the floor too
    let reading = poller.poll(provider)?;
    let merged = super::window::merge(existing.unwrap_or_default(), reading.windows.clone());
    super::window::store_for(state, provider, &merged).ok()?;
    Some(reading)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::window::{self, Window};
    use std::cell::RefCell;

    #[test]
    fn anthropic_response_parses_windows_and_credits_flag() {
        let body = include_str!("../../../tests/fixtures/anthropic-oauth-usage.json");
        let r = parse_anthropic_usage(body, 1_000).unwrap();
        let fh = r.windows.five_hour.unwrap();
        assert_eq!(fh.used_percentage, 7.0);
        assert_eq!(
            fh.resets_at,
            super::super::window::parse_rfc3339_utc("2026-08-16T20:49:59.785342+00:00").unwrap()
        );
        assert_eq!(fh.observed_at, 1_000);
        assert_eq!(r.windows.seven_day.unwrap().used_percentage, 23.0);
        assert_eq!(r.vendor_credits_enabled, Some(false));
        assert!(parse_anthropic_usage("{}", 1_000).is_none());
        assert!(parse_anthropic_usage("nonsense", 1_000).is_none());
    }

    #[test]
    fn codex_response_parser_accepts_rate_limits_shapes_and_rejects_junk() {
        // Synthetic bodies (endpoint unverified): wrapped and bare rate_limits
        let wrapped = r#"{"rate_limits":{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":100},"secondary":null}}"#;
        let bare = r#"{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":100}}"#;
        assert!(parse_codex_usage(wrapped, 1_000).is_some());
        assert!(parse_codex_usage(bare, 1_000).is_some());
        assert!(parse_codex_usage(r#"{"unrelated":true}"#, 1_000).is_none());
    }

    struct CountingPoller {
        calls: RefCell<u32>,
        reading: Option<PollReading>,
    }

    impl UsagePoller for CountingPoller {
        fn poll(&self, _provider: &str) -> Option<PollReading> {
            *self.calls.borrow_mut() += 1;
            self.reading.clone()
        }
    }

    fn windows_at(pct: f64, resets_at: u64, observed_at: u64) -> UsageWindows {
        UsageWindows {
            five_hour: Some(Window {
                used_percentage: pct,
                resets_at,
                observed_at,
            }),
            seven_day: None,
        }
    }

    #[test]
    fn maybe_poll_respects_staleness_and_the_interval_floor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig::default();
        let provider = "anthropic";
        let now = 1_000_000u64;

        // (a) fresh stored reading -> no poll, None.
        let fresh = windows_at(10.0, now + 600, now - 10);
        window::store_for(&state, provider, &fresh).expect("store fresh");
        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(r.is_none(), "a fresh collector reading needs no poll");
        assert_eq!(*poller.calls.borrow(), 0, "poller must not run");

        // (b) stale reading -> polls, merges+stores, returns Some(reading), marker written.
        let stale = windows_at(10.0, now + 600, now - 10_000);
        window::store_for(&state, provider, &stale).expect("store stale");
        let fresh_from_poll = windows_at(50.0, now + 1_000, now);
        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: fresh_from_poll.clone(),
                vendor_credits_enabled: Some(true),
            }),
        };
        assert!(
            !state.poll_marker_for(provider).exists(),
            "no marker before the first real poll"
        );
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        let reading = r.expect("a stale reading triggers a poll");
        assert_eq!(reading.vendor_credits_enabled, Some(true));
        assert_eq!(*poller.calls.borrow(), 1);
        let stored = window::load_for(&state, provider).expect("stored after poll");
        assert_eq!(
            stored.five_hour.unwrap().used_percentage,
            50.0,
            "the merged reading is stored"
        );
        assert!(
            state.poll_marker_for(provider).exists(),
            "a marker is written after a real poll"
        );

        // (c) immediately again with a still-stale-looking store but a fresh marker
        //     -> floored, None, no second poll call.
        window::store_for(&state, provider, &stale).expect("re-stale the store");
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(r.is_none(), "floored by poll_min_interval_secs");
        assert_eq!(
            *poller.calls.borrow(),
            1,
            "no second poll call while floored"
        );

        // (d) poller returning None -> None, stored state untouched, marker still
        //     written (a failed attempt also counts against the floor).
        let now2 = now + cfg.poll_min_interval_secs + 1;
        window::store_for(&state, provider, &stale).expect("re-stale the store");
        let before = window::load_for(&state, provider).expect("before");
        let failing = CountingPoller {
            calls: RefCell::new(0),
            reading: None,
        };
        let r = maybe_poll(&state, &cfg, now2, provider, &failing);
        assert!(r.is_none(), "a failed poll surfaces as None");
        assert_eq!(*failing.calls.borrow(), 1);
        let after = window::load_for(&state, provider).expect("after");
        assert_eq!(before, after, "stored state untouched on a failed poll");

        // The failed attempt still counts against the floor: an immediate
        // recheck must not poll again.
        let r = maybe_poll(&state, &cfg, now2 + 1, provider, &failing);
        assert!(r.is_none());
        assert_eq!(
            *failing.calls.borrow(),
            1,
            "a failed attempt still floors the next poll"
        );
    }

    /// Fix 3: a stored reading whose window has already rolled over
    /// (`resets_at` in the past) must not count as "fresh" just because it
    /// was `observed_at` recently -- `available` would blank it from the
    /// display, so the gate must judge freshness the same way the display
    /// does and let the poll proceed, instead of refusing for up to
    /// `collector_max_age_secs` while the operator sees nothing.
    #[test]
    fn maybe_poll_proceeds_when_the_recent_reading_has_already_rolled_over() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig::default();
        let provider = "anthropic";
        let now = 1_000_000u64;

        // observed_at is 10 seconds old (well within collector_max_age_secs),
        // but resets_at is already in the past: `available` drops this
        // window, so the gate must treat it as stale too.
        let rolled_over = windows_at(10.0, now - 1, now - 10);
        window::store_for(&state, provider, &rolled_over).expect("store rolled-over reading");

        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(
            r.is_some(),
            "a rolled-over-but-recently-observed reading must trigger a poll, not be treated as fresh"
        );
        assert_eq!(
            *poller.calls.borrow(),
            1,
            "the poller must actually run rather than being gated out"
        );
    }

    /// Both windows are always written from one snapshot, so in practice they
    /// share an `observed_at` (parse_statusline, parse_anthropic_usage, and
    /// windows_from_rate_limits all do this). When five_hour rolls over but
    /// seven_day is still live, `available` drops five_hour while seven_day
    /// survives with the shared recent `observed_at` -- so a gate that judged
    /// freshness off the raw newest observation, instead of the freshest
    /// *available* one, would wrongly call this fresh and leave five_hour
    /// blank for up to `collector_max_age_secs` after every five-hour
    /// boundary. The single-window fixture above (`seven_day: None`) cannot
    /// catch this: this case pins the two-window shape.
    #[test]
    fn maybe_poll_proceeds_when_one_of_two_shared_observation_windows_has_rolled_over() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig::default();
        let provider = "anthropic";
        let now = 1_000_000u64;

        let mixed = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: now - 1,
                observed_at: now - 10,
            }),
            seven_day: Some(Window {
                used_percentage: 30.0,
                resets_at: now + 7 * 24 * 3600,
                observed_at: now - 10,
            }),
        };
        window::store_for(&state, provider, &mixed).expect("store mixed reading");

        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(
            r.is_some(),
            "a dropped five_hour slot must trigger a poll even though seven_day is still live"
        );
        assert_eq!(
            *poller.calls.borrow(),
            1,
            "the poller must actually run rather than being gated out"
        );
    }

    /// Item 3 (review): the shared agent is built once and reused. No live
    /// HTTP is exercised here -- constructing the `ureq::Agent` config and
    /// confirming the `OnceLock` hands back the same instance on repeat
    /// calls is the cheapest honest probe available without a real request.
    #[test]
    fn the_shared_agent_is_constructed_once_and_reused() {
        let first = shared_agent() as *const ureq::Agent;
        let second = shared_agent() as *const ureq::Agent;
        assert_eq!(
            first, second,
            "repeat calls must reuse the same agent instance, not rebuild one"
        );
    }

    #[test]
    fn poll_disabled_never_polls() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig {
            poll_enabled: false,
            ..PaceConfig::default()
        };
        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, 1_000, "anthropic", &poller);
        assert!(r.is_none());
        assert_eq!(
            *poller.calls.borrow(),
            0,
            "disabled polling never calls out"
        );
    }
}
