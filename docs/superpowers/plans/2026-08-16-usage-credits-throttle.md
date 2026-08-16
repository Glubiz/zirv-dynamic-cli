# Usage Monitoring, Use-Credits Gating, and Pace-to-Reset Throttle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give zirv a usage source for both vendors (passive codex collector + active API poll fallback), a per-harness `use_credits` bypass (default off), a pace-to-reset soft throttle before the existing hard pause, and per-harness usage in the status bar and dashboard header.

**Architecture:** All usage data continues to flow through the per-provider `usage-<provider>.json` files; the new codex collector and the new poller are just additional writers feeding the existing pure `pace::decide` core, which gains a `Slow` verdict. Gating stays at the three existing `wait_for_window` call sites.

**Tech Stack:** Rust edition 2024, serde/serde_json, new dependency `ureq = "3"` (blocking HTTP, rustls). No tokio changes.

**Spec:** `docs/superpowers/specs/2026-08-16-usage-credits-throttle-design.md` — read it first; it carries the approved semantics and the verified data shapes.

## Global Constraints

- Branch: `feat/usage-credits-throttle` (exists). Never commit to `main`.
- Gates before claiming done: `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --verbose -- --test-threads=1`. ~47 tests fail on main on this machine (os error 193 / temp-path length): compare failing-test NAMES against main, never chase the count.
- `pace::decide` and all new parsing stays pure: no clock, no filesystem, no env reads — callers pass `now`.
- The wrap hot path must never get worse: no `unwrap`/`expect` there, no network calls from `wrap.rs`, all new failure paths degrade to today's behavior.
- Fixtures under `tests/fixtures/` are data files only; tests stay inline in `#[cfg(test)] mod tests`.
- Tests build configs as `PaceConfig { field: x, ..PaceConfig::default() }`, use a `const NOW: u64`, and small local `window(...)`/`collector(...)` helpers (see `pace.rs:533-548`).
- Commit after every task; message style `feat(ctx): ...` / `test(ctx): ...` matching recent history. No Co-Authored-By lines.
- Every PR must bump `Cargo.toml` `version` above its base branch (CI-enforced). Done once in Task 8: `2.8.0` → `2.9.0`.
- OAuth tokens: read into locals, never logged, never printed, never persisted anywhere new.

## Verified data shapes (from 2026-08-16 probes — treat as ground truth)

**Codex rollout line** (codex-cli 0.105.0, `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`):

```json
{"timestamp":"2026-02-26T18:52:21.222Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":10.0,"window_minutes":300,"resets_at":1772135737},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1772722537},"credits":{"has_credits":false,"unlimited":false,"balance":null},"plan_type":null}}}
```

`window_minutes: 300` ⇒ five-hour window; `10080` ⇒ seven-day. `resets_at` is unix seconds. A second shape has `info` populated instead of null; `rate_limits` is identical — parse only `rate_limits`.

**Anthropic OAuth usage endpoint** — `GET https://api.anthropic.com/api/oauth/usage`, headers `Authorization: Bearer <claudeAiOauth.accessToken from ~/.claude/.credentials.json>`, `anthropic-beta: oauth-2025-04-20` ⇒ HTTP 200:

```json
{"five_hour":{"utilization":7.0,"resets_at":"2026-08-16T20:49:59.785342+00:00"},
 "seven_day":{"utilization":23.0,"resets_at":"2026-08-22T20:59:59.785372+00:00"},
 "extra_usage":{"is_enabled":false,"used_credits":0.0,"credits_ever_enabled":true}}
```

(Additional fields exist; the full captured body is the fixture in Task 5.) `resets_at` is RFC 3339 with fractional seconds and a `+00:00` offset — hence the hand-rolled parser in Task 2 (no chrono dependency).

**Codex/ChatGPT endpoint: UNVERIFIED.** `~/.codex/auth.json` had no readable token on the reference machine. Ship the codex poller best-effort per Task 5; it must degrade to `None` on every failure and has no live test.

---

### Task 1: Config — `soft_percent`, poll keys, `use_credits`

**Files:**
- Modify: `src/commands/ctx/config.rs` (`PaceConfig` at :139-182, `ENV_MAP` at :435+, `REPO_FORBIDDEN` at :686+)
- Test: inline `#[cfg(test)]` in `config.rs`

**Interfaces:**
- Consumes: existing `PaceConfig`, `ENV_MAP`, `REPO_FORBIDDEN` machinery.
- Produces: `PaceConfig { soft_percent: f64, poll_enabled: bool, poll_min_interval_secs: u64, use_credits: UseCreditsConfig, .. }`; `UseCreditsConfig { claude: bool, codex: bool }` with `pub fn for_provider(&self, provider: &str) -> bool`. Later tasks call `cfg.pace.use_credits.for_provider(adapter.provider())`.

- [ ] **Step 1: Write failing tests** in `config.rs`'s existing test module (follow its existing layering/env test style — read neighboring tests first):

```rust
#[test]
fn pace_gains_soft_and_poll_and_use_credits_defaults() {
    let cfg = PaceConfig::default();
    assert_eq!(cfg.soft_percent, 80.0);
    assert!(cfg.poll_enabled);
    assert_eq!(cfg.poll_min_interval_secs, 60);
    assert!(!cfg.use_credits.claude);
    assert!(!cfg.use_credits.codex);
}

#[test]
fn use_credits_maps_providers_to_agent_flags() {
    let uc = UseCreditsConfig { claude: true, codex: false };
    assert!(uc.for_provider("anthropic"));
    assert!(!uc.for_provider("openai"));
    assert!(!uc.for_provider("something-else")); // unknown provider: gate stays on
}

#[test]
fn a_repo_layer_may_not_touch_use_credits_or_poll_keys() {
    // Follow the exact pattern of the existing REPO_FORBIDDEN tests in this file:
    // a repo ctx.toml containing `[pace.use_credits]\nclaude = true` must be
    // rejected/stripped, and likewise `poll_enabled = false` / `poll_min_interval_secs = 1`
    // under `[pace]`. Assert the loaded config keeps the defaults.
}

#[test]
fn env_overrides_use_credits_and_poll() {
    // Follow the existing ENV_MAP test pattern: ZIRV_CTX_PACE_USE_CREDITS_CLAUDE=true,
    // ZIRV_CTX_PACE_POLL=false, ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS=120,
    // ZIRV_CTX_PACE_SOFT_PERCENT=70 -> assert each lands.
}
```

- [ ] **Step 2:** `cargo test --verbose -- --test-threads=1 pace_gains_soft` etc. — expect FAIL (fields missing).
- [ ] **Step 3: Implement.** Add to `PaceConfig` (keep `#[serde(default, deny_unknown_fields)]` intact):

```rust
    /// Start of the soft-throttle band. At or above this (and below
    /// `max_percent`) cycles are delayed so the remaining budget spreads
    /// linearly over the time left in the window. `>= max_percent` means no
    /// throttle band — hard pause only.
    pub soft_percent: f64,
    /// Active API-poll fallback: only consulted when the passive collector
    /// reading is stale at a gating point.
    pub poll_enabled: bool,
    /// Per-provider floor between poll attempts, shared across processes.
    pub poll_min_interval_secs: u64,
    /// Operator declaration that a harness's vendor plan covers overage from
    /// credits: gating (throttle and pause) is skipped for that harness.
    pub use_credits: UseCreditsConfig,
```

Defaults: `soft_percent: 80.0, poll_enabled: true, poll_min_interval_secs: 60, use_credits: UseCreditsConfig::default()`. New struct next to `PaceConfig`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UseCreditsConfig {
    pub claude: bool,
    pub codex: bool,
}

impl UseCreditsConfig {
    /// Keyed by agent in config (what the operator thinks in), resolved by
    /// provider at the gate (what pacing knows). Unknown providers gate.
    pub fn for_provider(&self, provider: &str) -> bool {
        match provider {
            "anthropic" => self.claude,
            "openai" => self.codex,
            _ => false,
        }
    }
}
```

`ENV_MAP` additions (match existing entry formatting):

```rust
    ("ZIRV_CTX_PACE_SOFT_PERCENT", &["pace", "soft_percent"], EnvKind::Float),
    ("ZIRV_CTX_PACE_POLL", &["pace", "poll_enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS",
        &["pace", "poll_min_interval_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_USE_CREDITS_CLAUDE",
        &["pace", "use_credits", "claude"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_PACE_USE_CREDITS_CODEX",
        &["pace", "use_credits", "codex"],
        EnvKind::Bool,
    ),
```

(Verify `ENV_MAP` application handles 3-segment paths — inspect the apply fn; if it only handles 2 segments, extend it in this task.) `REPO_FORBIDDEN` additions — forbid the whole `use_credits` table plus the poll keys (a repo checkout must not flip a spend decision, re-enable polling, or change credential-read/network cadence; comment accordingly in the style of the neighboring entries):

```rust
    (&["pace", "use_credits"], "ZIRV_CTX_PACE_USE_CREDITS_CLAUDE"),
    (&["pace", "poll_enabled"], "ZIRV_CTX_PACE_POLL"),
    (
        &["pace", "poll_min_interval_secs"],
        "ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS",
    ),
```

(Verify the forbidden-key checker matches a table node for a 2-segment path — if it only matches leaves, also add the two leaf paths `["pace","use_credits","claude"]` / `...codex`.)

- [ ] **Step 4:** Run the four tests — expect PASS. Run full `cargo test --verbose -- --test-threads=1` for config regressions.
- [ ] **Step 5:** Commit: `feat(ctx): pace config gains soft_percent, poll keys and use_credits`

---

### Task 2: Pure parsers — RFC 3339 timestamps and codex `rate_limits`

**Files:**
- Modify: `src/commands/ctx/window.rs`
- Create: `tests/fixtures/codex-rollout-rate-limits.jsonl`
- Test: inline in `window.rs`

**Interfaces:**
- Consumes: `Window`/`UsageWindows` (window.rs:11-23), `FIVE_HOUR_SECS`/`SEVEN_DAY_SECS` (window.rs:177-178).
- Produces (all in `window.rs`, all pure):
  - `pub fn parse_rfc3339_utc(s: &str) -> Option<u64>`
  - `pub fn windows_from_rate_limits(limits: &serde_json::Value, observed_at: u64) -> Option<UsageWindows>` (shared later by the codex poller)
  - `pub fn parse_rollout_line(line: &str) -> Option<UsageWindows>`

- [ ] **Step 1: Create the fixture** `tests/fixtures/codex-rollout-rate-limits.jsonl` — exactly these four lines (real captured shapes, one per line; line 3 is a non-snapshot line, line 4 has an unmappable window):

```jsonl
{"timestamp":"2026-02-26T18:52:21.222Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":10.0,"window_minutes":300,"resets_at":1772135737},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1772722537},"credits":{"has_credits":false,"unlimited":false,"balance":null},"plan_type":null}}}
{"timestamp":"2026-02-26T18:52:27.310Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15770,"cached_input_tokens":6528,"output_tokens":268,"reasoning_output_tokens":137,"total_tokens":16038},"last_token_usage":{"input_tokens":15770,"cached_input_tokens":6528,"output_tokens":268,"reasoning_output_tokens":137,"total_tokens":16038},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1772135737},"secondary":{"used_percent":4.0,"window_minutes":10080,"resets_at":1772722537},"credits":{"has_credits":false,"unlimited":false,"balance":null},"plan_type":null}}}
{"timestamp":"2026-02-26T18:52:30.000Z","type":"event_msg","payload":{"type":"agent_message","message":"hi"}}
{"timestamp":"2026-02-26T18:52:33.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":50.0,"window_minutes":1,"resets_at":1772135737},"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":null},"plan_type":null}}}
```

- [ ] **Step 2: Write failing tests** in `window.rs`'s test module:

```rust
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
    let lines: Vec<&str> = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl")
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
    assert_eq!(fh.observed_at, parse_rfc3339_utc("2026-02-26T18:52:21.222Z").unwrap());
    // populated-info shape parses identically
    assert!(parse_rollout_line(lines[1]).is_some());
    // non-token_count lines and garbage yield None
    assert!(parse_rollout_line(lines[2]).is_none());
    assert!(parse_rollout_line("{broken").is_none());
    // a 1-minute window maps to neither slot -> dropped -> no windows -> None
    assert!(parse_rollout_line(lines[3]).is_none());
}
```

(Adjust the `include_str!` relative path to however existing tests in this repo reference `tests/fixtures/` — grep for `include_str!` or fixture reads first and copy that idiom.)

- [ ] **Step 3:** Run them — expect FAIL (functions missing).
- [ ] **Step 4: Implement** in `window.rs`:

```rust
/// Days from civil date (Howard Hinnant's algorithm), for RFC 3339 -> epoch
/// without a chrono dependency.
fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719_468
}

/// Parses an RFC 3339 timestamp ("2026-08-16T20:49:59.785342+00:00", trailing
/// "Z" or "+/-HH:MM", fraction ignored) to unix seconds. None on anything
/// malformed or pre-epoch.
pub fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let (date, rest) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: u64 = dp.next()?.parse().ok()?;
    let d: u64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Split the time from the offset: "Z", or the last '+'/'-' in the string.
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(idx) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(idx);
        let sign = if off.starts_with('-') { -1i64 } else { 1i64 };
        let (oh, om) = off[1..].split_once(':')?;
        let oh: i64 = oh.parse().ok()?;
        let om: i64 = om.parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    } else {
        return None;
    };
    let time = time.split_once('.').map_or(time, |(t, _frac)| t);
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec: i64 = tp.next()?.parse().ok()?;
    if tp.next().is_some() || !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..61).contains(&sec) {
        return None;
    }
    let total = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    u64::try_from(total).ok()
}

/// Which UsageWindows slot a window of this length belongs to: the nearest of
/// 5h/7d, accepted only within a factor of two — anything else is a window
/// shape we do not understand and must drop, never guess.
fn window_slot(window_secs: u64) -> Option<bool /* true = five_hour */> {
    if window_secs >= FIVE_HOUR_SECS / 2 && window_secs <= FIVE_HOUR_SECS * 2 {
        Some(true)
    } else if window_secs >= SEVEN_DAY_SECS / 2 && window_secs <= SEVEN_DAY_SECS * 2 {
        Some(false)
    } else {
        None
    }
}

/// Maps a codex `rate_limits` object (primary/secondary with used_percent,
/// window_minutes, resets_at in unix seconds) onto UsageWindows. Shared by the
/// rollout collector and the codex poller.
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
        let minutes = w.get("window_minutes").and_then(|m| m.as_u64()).unwrap_or(0);
        let resets_at = w.get("resets_at").and_then(|r| r.as_u64()).unwrap_or(0);
        let Some(five_hour) = window_slot(minutes * 60) else {
            continue;
        };
        let win = Window { used_percentage: used, resets_at, observed_at };
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
```

- [ ] **Step 5:** Run the tests — expect PASS. `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 6:** Commit: `feat(ctx): parse codex rollout rate-limit snapshots and RFC 3339 timestamps`

---

### Task 3: Codex rollout scanner and opportunistic refresh

**Files:**
- Modify: `src/commands/ctx/window.rs` (scanner, refresh, `has_no_usage_source`)
- Test: inline in `window.rs` (tempdir-based, like existing store/load tests — `tempfile` is already a dev-dependency)

**Interfaces:**
- Consumes: Task 2's `parse_rollout_line`; existing `load_for`/`store_for`/`merge` (window.rs:83-150), `StateDir`.
- Produces:
  - `pub const CODEX_USAGE_PROVIDER: &str = "openai";`
  - `pub fn scan_codex_rollouts(sessions_dir: &Path, max_files: usize) -> Option<UsageWindows>`
  - `pub fn refresh_codex_usage(state: &StateDir, sessions_dir: Option<&Path>, now: u64, max_age_secs: u64)` — `sessions_dir: None` means the real `~/.codex/sessions` (tests inject a tempdir).
  - `has_no_usage_source` body becomes a plain "nothing ever recorded" check for every provider.

- [ ] **Step 1: Write failing tests:**

```rust
#[test]
fn scan_finds_last_snapshot_in_newest_rollout_file() {
    let dir = tempfile::tempdir().unwrap();
    let day = dir.path().join("2026").join("02").join("26");
    std::fs::create_dir_all(&day).unwrap();
    let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
    let lines: Vec<&str> = fixture.lines().collect();
    // older file: 10% snapshot; newer file: 12% snapshot after a non-snapshot line
    std::fs::write(day.join("rollout-a.jsonl"), format!("{}\n", lines[0])).unwrap();
    std::fs::write(day.join("rollout-b.jsonl"), format!("{}\n{}\n", lines[2], lines[1])).unwrap();
    // make b's mtime strictly newer
    let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    let f = std::fs::File::options().append(true).open(day.join("rollout-b.jsonl")).unwrap();
    f.set_modified(newer).unwrap();
    let w = scan_codex_rollouts(dir.path(), 3).unwrap();
    assert_eq!(w.five_hour.unwrap().used_percentage, 12.0);
}

#[test]
fn scan_of_missing_or_empty_dir_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(scan_codex_rollouts(&dir.path().join("nope"), 3).is_none());
    assert!(scan_codex_rollouts(dir.path(), 3).is_none());
}

#[test]
fn refresh_skips_when_stored_reading_is_fresh_and_stores_when_stale() {
    // StateDir on a tempdir (copy the construction used by existing window.rs
    // store/load tests). Pre-store a fresh openai reading (observed_at close to
    // `now`), point sessions_dir at a tempdir with the 12% fixture file, call
    // refresh_codex_usage with max_age_secs = 900 -> stored value unchanged.
    // Then pre-store a stale reading (observed_at = now - 10_000) -> refresh
    // overwrites via merge with the scanned 12% snapshot.
}

#[test]
fn no_usage_source_is_now_a_plain_no_data_check() {
    // StateDir on a tempdir: has_no_usage_source(state, "openai") is true with
    // nothing stored, false after store_for(state, "openai", ...). Same for
    // "anthropic" (previously hardcoded false): true with nothing stored.
}
```

- [ ] **Step 2:** Run — expect FAIL.
- [ ] **Step 3: Implement:**

```rust
pub const CODEX_USAGE_PROVIDER: &str = "openai";
/// Rollout files grow large; only the tail can hold the newest snapshot.
const ROLLOUT_TAIL_BYTES: u64 = 64 * 1024;
const ROLLOUT_SCAN_FILES: usize = 3;

fn collect_jsonl(dir: &Path, depth: u8, out: &mut Vec<(std::time::SystemTime, std::path::PathBuf)>) {
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

fn last_snapshot_in(path: &Path) -> Option<UsageWindows> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(ROLLOUT_TAIL_BYTES))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    // The tail may start mid-line/mid-char; lossy decode + rev line scan copes.
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().find_map(parse_rollout_line)
}

/// Newest rate-limit snapshot across the most recently modified rollout files.
pub fn scan_codex_rollouts(sessions_dir: &Path, max_files: usize) -> Option<UsageWindows> {
    let mut files = Vec::new();
    collect_jsonl(sessions_dir, 0, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(max_files).find_map(|(_, p)| last_snapshot_in(&p))
}

fn newest_observation(windows: &UsageWindows) -> u64 {
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
pub fn refresh_codex_usage(state: &StateDir, sessions_dir: Option<&Path>, now: u64, max_age_secs: u64) {
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
    let Some(fresh) = scan_codex_rollouts(dir, ROLLOUT_SCAN_FILES) else {
        return;
    };
    let merged = merge(existing.unwrap_or_default(), fresh);
    let _ = store_for(state, CODEX_USAGE_PROVIDER, &merged);
}
```

`has_no_usage_source` (window.rs:134-137) becomes:

```rust
/// True when nothing has ever been recorded for this provider. Since the
/// codex collector and the poller exist, no provider is structurally exempt
/// any more — callers refresh sources first, then ask.
pub fn has_no_usage_source(state: &StateDir, provider: &str) -> bool {
    load_for(state, provider).is_none()
}
```

Then run the full test suite and fix every test that asserted the old anthropic-is-never-sourceless behavior — update those assertions deliberately (the semantic change is intended and spec'd), do not paper over them.

- [ ] **Step 4:** Run tests — expect PASS (including the updated ones). Clippy clean.
- [ ] **Step 5:** Commit: `feat(ctx): codex passive usage collector from session rollouts`

---

### Task 4: `PaceDecision::Slow` — pace-to-reset throttle and `use_credits` gate

**Files:**
- Modify: `src/commands/ctx/pace.rs`
- Modify (signatures only): `src/commands/ctx/run_loop.rs:141-153`, `src/commands/ctx/exec.rs:741-753` and `:1061-1072`, plus `usage.rs`'s `report` match (add a `Slow` arm printing the throttle) — enough to keep the tree compiling; full wiring is Task 6.
- Test: inline in `pace.rs`

**Interfaces:**
- Consumes: Task 1's `soft_percent`/`use_credits`; existing `decide` internals (pace.rs:186-219), `wait_for_window` loop (pace.rs:438-520).
- Produces:
  - `PaceDecision::Slow { delay_secs: u64, window: &'static str, percent: f64, source: Source }`
  - `pub struct PaceGate<'a> { pub use_credits: bool, pub poller: Option<&'a dyn crate::commands::ctx::poll::UsagePoller> }` — **this task** stubs it WITHOUT the poller field (added in Task 6 when `poll.rs` exists): `pub struct PaceGate { pub use_credits: bool }`
  - `#[derive(Default)] pub struct PaceGateFlags { pub no_source_announced: bool, pub credits_announced: bool }` replacing the bare `announced_no_source: &mut bool` parameter.
  - `wait_for_window(..., gate: PaceGate, flags: &mut PaceGateFlags)` — the `provider` param stays.

- [ ] **Step 1: Write failing tests** (reuse the module's `NOW`, `window(...)`, `collector(...)` helpers):

```rust
#[test]
fn below_soft_percent_proceeds_unthrottled() {
    let d = decide(&collector(79.0), None, NOW, &PaceConfig::default());
    assert!(matches!(d, PaceDecision::Proceed { .. }));
}

#[test]
fn inside_the_band_slows_proportionally_to_time_left() {
    // soft 80, max 99: at 90% the band fraction is 10/19. Reset in 1900s
    // -> delay = 1900 * 10/19 = 1000.
    let w = collector_with_reset(90.0, NOW + 1900); // build via the window() helper
    let d = decide(&w, None, NOW, &PaceConfig::default());
    assert_eq!(
        d,
        PaceDecision::Slow { delay_secs: 1000, window: "five_hour", percent: 90.0, source: Source::Collector }
    );
}

#[test]
fn near_reset_the_slow_delay_shrinks_toward_zero() {
    let w = collector_with_reset(90.0, NOW + 1); // 1s left: delay rounds to 0 -> Proceed
    assert!(matches!(decide(&w, None, NOW, &PaceConfig::default()), PaceDecision::Proceed { .. }));
}

#[test]
fn at_max_percent_the_hard_pause_still_wins() {
    let d = decide(&collector(99.0), None, NOW, &PaceConfig::default());
    assert!(matches!(d, PaceDecision::WaitUntil { .. }));
}

#[test]
fn unknown_reset_time_slows_by_the_fallback_delay() {
    // resets_at == 0 inside the band: t_rem stands in as fallback_delay_secs (900)
    // at 90%: 900 * 10/19 = 473.
    let w = collector_with_reset(90.0, 0);
    let d = decide(&w, None, NOW, &PaceConfig::default());
    assert!(matches!(d, PaceDecision::Slow { delay_secs: 473, .. }));
}

#[test]
fn an_empty_band_disables_the_throttle() {
    let cfg = PaceConfig { soft_percent: 99.0, ..PaceConfig::default() }; // == max
    assert!(matches!(decide(&collector(98.0), None, NOW, &cfg), PaceDecision::Proceed { .. }));
}

#[test]
fn use_credits_skips_the_gate_entirely() {
    // wait_for_window with a stored 100%-used collector reading and
    // PaceGate { use_credits: true }: returns immediately, sleep_fn never called.
    // Model the harness on the existing wait_for_window tests in this module
    // (StateDir on a tempdir, counting sleep_fn closure).
}

#[test]
fn a_slow_wait_does_not_extend_itself_across_rechecks() {
    // wait_for_window with a Slow-producing reading and an injected now_fn that
    // advances with each sleep: total slept must equal the FIRST computed delay
    // (+/- chunking), not stretch until the window reset. Assert total sleep
    // <= first delay + SLEEP_CHUNK_SECS.
}
```

(`collector_with_reset` is a 4-line local test helper next to `collector()`: same shape, explicit `resets_at`.)

- [ ] **Step 2:** Run — expect FAIL (no `Slow` variant).
- [ ] **Step 3: Implement.**

In `decide` (pace.rs:186-219), replace the final `if window.used_percentage < cfg.max_percent { Proceed } / WaitUntil` block with:

```rust
    if window.used_percentage < cfg.max_percent {
        let band = cfg.max_percent - cfg.soft_percent;
        if band > 0.0 && window.used_percentage >= cfg.soft_percent {
            let t_rem = if window.resets_at > now {
                window.resets_at - now
            } else {
                // Reset unknown (0) or already past while the reading still
                // binds: pace against the configured fallback horizon.
                cfg.fallback_delay_secs
            };
            let frac = (window.used_percentage - cfg.soft_percent) / band;
            let delay_secs = (t_rem as f64 * frac) as u64;
            if delay_secs > 0 {
                return PaceDecision::Slow {
                    delay_secs,
                    window: name,
                    percent: window.used_percentage,
                    source,
                };
            }
        }
        return PaceDecision::Proceed {
            source,
            worst_percent: window.used_percentage,
        };
    }
```

In `wait_deadline`, add a `Slow` arm: `PaceDecision::Slow { delay_secs, .. } => Some(now.saturating_add(*delay_secs))` — no jitter (the delay is already reading-derived; the loop's min-tracking below keeps it stable).

In the `wait_for_window` loop (pace.rs:438-520): the deadline for `WaitUntil` is absolute (`reset_at`) so re-deriving it each chunk is stable, but a re-derived `Slow` deadline creeps forward every chunk (`now + (resets_at - now) * frac` grows as `now` does) and would stretch a 10%-band throttle into a full park. Track it monotonically:

```rust
    let mut slow_deadline: Option<u64> = None;
    loop {
        // ...existing decision computation...
        let deadline = match &decision {
            PaceDecision::Slow { .. } => {
                let cand = wait_deadline(&decision, now, cfg, std::process::id() as u64 ^ now);
                let d = match (slow_deadline, cand) {
                    (Some(prev), Some(c)) => prev.min(c), // a better reading may shorten it, never lengthen
                    (None, c) => c?,
                    (prev, None) => prev?,
                };
                slow_deadline = Some(d);
                Some(d)
            }
            _ => {
                slow_deadline = None;
                wait_deadline(&decision, now, cfg, std::process::id() as u64 ^ now)
            }
        };
        // ...existing None => return, remaining/cap/sleep logic unchanged...
    }
```

(Integrate with the existing `let Some(deadline) = ... else return` structure — keep the existing return-on-no-deadline and announce-once-per-distinct-decision behavior, extending the announce text with a Slow form like `pacing: throttling ~{delay}s ({percent:.0}% of {window})`.)

`use_credits` short-circuit at the top of `wait_for_window`, before any source refresh or decision:

```rust
    if gate.use_credits {
        if !flags.credits_announced {
            flags.credits_announced = true;
            // decision-log entry + optional announcer line, following the
            // existing PacingSkipped/no-source once-per-run pattern:
            // "pacing: use_credits enabled for this harness, gate skipped"
        }
        return PaceOutcome { waited_secs: 0, source: Source::None };
    }
```

Signature change: replace `announced_no_source: &mut bool` with `gate: PaceGate, flags: &mut PaceGateFlags`; move the existing no-source announce bookkeeping onto `flags.no_source_announced`. Update the three call sites minimally so the tree compiles (each declares `let mut pace_flags = PaceGateFlags::default();` where `pace_no_source_announced` was, and passes `PaceGate { use_credits: false }` for now — real values in Task 6). Add the `Slow` arm to `usage.rs`'s report match (print `throttle: would delay ~{delay_secs}s ({percent:.0}% of {window}, {source})`).

- [ ] **Step 4:** Run the new tests and the whole pace/usage suites — expect PASS; fix any existing `wait_for_window` tests for the new signature (mechanical: `PaceGate { use_credits: false }` + a flags local).
- [ ] **Step 5:** Clippy clean. Commit: `feat(ctx): pace-to-reset Slow verdict and use_credits gate skip`

---

### Task 5: `poll.rs` — active usage poll fallback

**Files:**
- Create: `src/commands/ctx/poll.rs`; register `pub mod poll;` in `src/commands/ctx/mod.rs` (alphabetical, after `pace`)
- Create: `tests/fixtures/anthropic-oauth-usage.json` — copy the captured real response: `cp "C:/Users/josj/AppData/Local/Temp/claude/D--GitHub-zirv-dynamic-cli/72a23f97-867d-4e78-a2c0-979fe75d690b/scratchpad/anthropic-usage.json" tests/fixtures/anthropic-oauth-usage.json` (verified to contain only utilization figures, no secrets — confirm by reading it after copying)
- Modify: `Cargo.toml` (`ureq = "3"` under `[dependencies]`), `src/commands/ctx/state.rs` (poll marker path)
- Test: inline in `poll.rs`

**Interfaces:**
- Consumes: Task 2's `parse_rfc3339_utc` + `windows_from_rate_limits`, `window::{load_for, store_for, merge}`, `StateDir`.
- Produces:

```rust
pub struct PollReading {
    pub windows: UsageWindows,
    /// Vendor-side credits state, advisory only (anthropic: extra_usage.is_enabled).
    pub vendor_credits_enabled: Option<bool>,
}
pub trait UsagePoller {
    fn poll(&self, provider: &str) -> Option<PollReading>;
}
pub struct HttpPoller; // the real one
/// Some(reading) when a poll ran, produced data and it was stored; None covers
/// "not needed", "floored", "disabled" and "failed" alike — callers never
/// branch on why. The reading carries the vendor_credits_enabled advisory for
/// callers that surface it (`zirv ctx usage`).
pub fn maybe_poll(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
    poller: &dyn UsagePoller,
) -> Option<PollReading>;
```

  plus `StateDir::poll_marker_for(&self, provider: &str) -> PathBuf` in `state.rs` (mirror `usage_for`'s implementation with the `poll-` prefix).

- [ ] **Step 1: Write failing tests** (all through stub pollers — no network in tests, ever):

```rust
#[test]
fn anthropic_response_parses_windows_and_credits_flag() {
    let body = include_str!("../../../tests/fixtures/anthropic-oauth-usage.json");
    let r = parse_anthropic_usage(body, 1_000).unwrap();
    let fh = r.windows.five_hour.unwrap();
    assert_eq!(fh.used_percentage, 7.0);
    assert_eq!(fh.resets_at, super::window::parse_rfc3339_utc("2026-08-16T20:49:59.785342+00:00").unwrap());
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

#[test]
fn maybe_poll_respects_staleness_and_the_interval_floor() {
    // StateDir on a tempdir. Stub poller counts calls and returns a fixed reading.
    // (a) fresh stored reading -> no poll, None.
    // (b) stale reading -> polls, merges+stores, returns Some(reading), marker written.
    // (c) immediately again with a still-stale-looking store but a fresh marker
    //     -> floored, None, no second poll call.
    // (d) poller returning None -> None, stored state untouched, marker still
    //     written (a failed attempt also counts against the floor).
}

#[test]
fn poll_disabled_never_polls() {
    // cfg.poll_enabled = false -> maybe_poll returns None, stub never called.
}
```

- [ ] **Step 2:** Run — expect FAIL.
- [ ] **Step 3: Implement** `poll.rs`:

```rust
//! Active usage-poll fallback: consulted only when the passive collector
//! reading is stale at a decision point. Every failure degrades to whatever
//! passive data exists — this module must never make a session worse.

const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
/// UNVERIFIED (2026-08-16: no readable token on the reference machine to
/// exercise it). Ships best-effort; see Known Issues.
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
const HTTP_TIMEOUT_SECS: u64 = 10;

fn parse_anthropic_usage(body: &str, now: u64) -> Option<PollReading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let read_window = |key: &str| -> Option<Window> {
        let w = v.get(key).filter(|w| w.is_object())?;
        Some(Window {
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

fn parse_codex_usage(body: &str, now: u64) -> Option<PollReading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let limits = v.get("rate_limits").unwrap_or(&v);
    let windows = super::window::windows_from_rate_limits(limits, now)?;
    let vendor_credits_enabled = limits
        .pointer("/credits/has_credits")
        .and_then(|b| b.as_bool());
    Some(PollReading { windows, vendor_credits_enabled })
}

fn anthropic_token() -> Option<String> {
    let path = dirs::home_dir()?.join(".claude").join(".credentials.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(v.pointer("/claudeAiOauth/accessToken")?.as_str()?.to_owned())
}

fn codex_token() -> Option<String> {
    let path = dirs::home_dir()?.join(".codex").join("auth.json");
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
            "anthropic" => (ANTHROPIC_USAGE_URL, anthropic_token()?, Some(("anthropic-beta", ANTHROPIC_OAUTH_BETA))),
            super::window::CODEX_USAGE_PROVIDER => (CODEX_USAGE_URL, codex_token()?, None),
            _ => return None,
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .build()
            .into();
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
```

(The ureq 3 builder/call idiom above is the intended shape — check the resolved ureq 3.x docs/rustdoc if a method name differs; keep timeouts, keep rustls defaults, add no features beyond defaults.)

`maybe_poll` (marker read/write mirrors `window::store_at`'s atomic tmp+rename; a marker is `{"last_attempt": u64}` via a tiny serde struct):

```rust
pub fn maybe_poll(state: &StateDir, cfg: &PaceConfig, now: u64, provider: &str, poller: &dyn UsagePoller) -> Option<PollReading> {
    if !cfg.poll_enabled {
        return None;
    }
    let existing = super::window::load_for(state, provider);
    if let Some(w) = &existing
        && now.saturating_sub(super::window::newest_observation(w)) <= cfg.collector_max_age_secs
    {
        return None; // passive data is fresh enough; the poll exists only as fallback
    }
    if last_attempt(state, provider).is_some_and(|t| now.saturating_sub(t) < cfg.poll_min_interval_secs) {
        return None;
    }
    record_attempt(state, provider, now); // failed attempts count against the floor too
    let reading = poller.poll(provider)?;
    let merged = super::window::merge(existing.unwrap_or_default(), reading.windows.clone());
    super::window::store_for(state, provider, &merged).ok()?;
    Some(reading)
}
```

(Export `newest_observation` from `window.rs` (Task 3 wrote it) as `pub(crate)` rather than duplicating it.)

- [ ] **Step 4:** `cargo build` (new dep resolves), run the poll tests — expect PASS. Clippy clean.
- [ ] **Step 5:** Commit: `feat(ctx): active usage-poll fallback behind the passive collector`

---

### Task 6: Wire sources and gate options into the supervisors and `zirv ctx usage`

**Files:**
- Modify: `src/commands/ctx/pace.rs` (`PaceGate` gains `poller`, `wait_for_window` refreshes sources), `src/commands/ctx/run_loop.rs:139-155`, `src/commands/ctx/exec.rs:737-753` + `:1056-1072`, `src/commands/ctx/usage.rs` (`run_with`)
- Test: inline in `pace.rs`

**Interfaces:**
- Consumes: Tasks 3-5 (`refresh_codex_usage`, `maybe_poll`, `HttpPoller`, `PaceGate`/`PaceGateFlags`).
- Produces: final gate API used everywhere:

```rust
pub struct PaceGate<'a> {
    pub use_credits: bool,
    pub poller: Option<&'a dyn super::poll::UsagePoller>,
}
```

- [ ] **Step 1: Write failing tests** in `pace.rs`:

```rust
#[test]
fn the_gate_polls_only_when_the_stored_reading_is_stale() {
    // wait_for_window with: stale stored reading, stub poller returning a fresh
    // below-soft reading -> poller called once, gate proceeds. With a fresh
    // stored reading -> poller never called.
}

#[test]
fn a_failing_poller_leaves_the_gate_on_passive_data() {
    // Stub poller returns None; stale stored reading over max_percent with a
    // near-future resets_at -> gate still parks on the stale-but-binding
    // reading exactly as today (existing `binding` rule).
}
```

- [ ] **Step 2:** Run — expect FAIL (no poller field).
- [ ] **Step 3: Implement.** In `wait_for_window`, after the `use_credits` short-circuit and before the loop (and once per loop iteration, both cheap no-ops when fresh):

```rust
        let now = now_fn();
        if provider == super::window::CODEX_USAGE_PROVIDER {
            super::window::refresh_codex_usage(state, None, now, cfg.collector_max_age_secs);
        }
        if let Some(poller) = gate.poller {
            super::poll::maybe_poll(state, cfg, now, provider, poller);
        }
```

The no-source short-circuit (`has_no_usage_source`) moves AFTER these refresh attempts, so a first-ever run can acquire data before deciding it has none. Call sites:

- `run_loop.rs` (:139-155): before the loop, `let http_poller = super::poll::HttpPoller;` and `let mut pace_flags = pace::PaceGateFlags::default();`; the call becomes

```rust
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "loop",
            "loop",
            &now_fn,
            &sleep_fn,
            None,
            adapter.provider(),
            pace::PaceGate {
                use_credits: cfg.pace.use_credits.for_provider(adapter.provider()),
                poller: cfg.pace.poll_enabled.then_some(&http_poller as &dyn super::poll::UsagePoller),
            },
            &mut pace_flags,
        );
```

- `exec.rs` pre-flight (:741): identical shape (it already has `adapter`, `announcer`, its own flags local replacing `pace_no_source_announced`).
- `exec.rs` post-limit park (:1061): same, **except** `use_credits: false` with this comment:

```rust
            // A vendor-reported limit hit parks even with use_credits enabled:
            // the vendor limiting us means credits are exhausted or not actually
            // enabled plan-side, and an immediate relaunch would just re-hit it.
```

- `usage.rs` `run_with` (:252-313): before building the report, refresh both sources for the resolved provider (same two calls as the gate, using `HttpPoller`, keeping the returned `Option<PollReading>`), so `zirv ctx usage` is the manual end-to-end check. Add to the report: a `use_credits` line when enabled for the resolved provider (`"use_credits: enabled for this harness — pacing gate skipped"`); when a poll just returned `Some(reading)` with `vendor_credits_enabled: Some(v)`, an advisory line `"vendor reports credits {enabled|disabled} on this plan"` (and nothing when no poll ran — never invent vendor state from stale data); and keep the Task 4 `Slow` arm output.

- [ ] **Step 4:** Full test suite — expect PASS (names diffed against main). Clippy clean.
- [ ] **Step 5: Manual smoke test:** `cargo run -- ctx usage` — expect real claude percentages (from statusline history or a live poll) and, if codex rollouts exist on this machine, codex data stored under `usage-openai.json`. Report actual output.
- [ ] **Step 6:** Commit: `feat(ctx): gate refreshes usage sources and honors use_credits`

---

### Task 7: Display — status-bar freshness and dashboard header usage row

**Files:**
- Modify: `src/commands/ctx/wrap.rs` (:1507-1547 area + the bar struct it reads), `src/commands/ctx/dash/ui.rs` (:314-328 `header_line` + `HeaderFacts`), plus the `HeaderFacts` construction site (grep `HeaderFacts {` under `src/commands/ctx/dash/`)
- Test: inline in `chrome.rs`/`ui.rs` test modules (pure renderers)

**Interfaces:**
- Consumes: `window::{load_for, max_used_percentage, refresh_codex_usage, CODEX_USAGE_PROVIDER}`, `cfg.pace.use_credits`.
- Produces: `HeaderFacts` gains `pub usage: Vec<(&'static str, Option<f64>, bool)>` — `(harness name, percent, credits-mode)` for each enabled harness.

- [ ] **Step 1: Write failing tests** for the pure renderer:

```rust
#[test]
fn header_renders_per_harness_usage_with_honest_placeholders() {
    // facts.usage = [("claude", Some(72.4), false), ("codex", None, false)]
    // -> header contains "claude 72%" and "codex –" (same placeholder glyph
    //    chrome::status_bar uses — reuse its constant, don't retype it)
}

#[test]
fn a_credits_harness_shows_credits_instead_of_a_percent() {
    // [("claude", Some(72.4), true)] -> "claude credits", no percent
}

#[test]
fn empty_usage_facts_render_no_usage_segment() {
    // usage: vec![] -> header identical to before this feature
}
```

- [ ] **Step 2:** Run — expect FAIL.
- [ ] **Step 3: Implement.**

`ui.rs` `header_line` — after the `rot` part:

```rust
    for (name, percent, credits) in &facts.usage {
        parts.push(if *credits {
            format!("{name} credits")
        } else {
            match percent {
                Some(p) => format!("{name} {p:.0}%"),
                None => format!("{name} {PLACEHOLDER}"),
            }
        });
    }
```

(Import/reference the placeholder constant from `chrome.rs`; make it `pub(crate)` there if it isn't.) At the `HeaderFacts` construction site: for each enabled harness in the roster (claude/codex, respecting `cfg.agents`), map to its provider (`adapters::provider_for_agent_name(Some(name))`), `window::load_for` → `max_used_percentage`, credits flag from `cfg.pace.use_credits`. File reads only — the dash event loop must not scan rollouts or poll (inner sessions' gates keep the files fresh).

`wrap.rs` `redraw_bar_if_due`: immediately before the existing `load_for` (:1530), add a throttled passive refresh for a wrapped codex session — file scanning only, never HTTP (network on the redraw path would violate the wrap invariant):

```rust
    // Passive refresh only: a wrapped codex session has no statusline tee, so
    // the bar would otherwise stay a permanent placeholder. Scans are floored
    // to once per CODEX_BAR_SCAN_SECS; HTTP stays off this path entirely.
    if bar.provider == super::window::CODEX_USAGE_PROVIDER
        && now.saturating_sub(bar.last_codex_scan) >= CODEX_BAR_SCAN_SECS
    {
        bar.last_codex_scan = now;
        super::window::refresh_codex_usage(state_dir, None, now, bar.collector_max_age_secs);
    }
```

with `const CODEX_BAR_SCAN_SECS: u64 = 60;`, a `last_codex_scan: u64` field (init 0) and a `collector_max_age_secs: u64` field (copied from `cfg.pace` where the bar struct is built) added to the bar struct `redraw_bar_if_due` receives. Obtain `now` the way the surrounding wrap code already does. `BarState`/`status_bar` need no change.

- [ ] **Step 4:** Full suite + clippy — expect PASS/clean.
- [ ] **Step 5: Manual check:** run `cargo run -- ctx chat` briefly in this repo (or `zirv` dashboard if configured) and confirm the header shows the usage segment; report what rendered.
- [ ] **Step 6:** Commit: `feat(ctx): per-harness usage in dashboard header, codex-aware status bar`

---

### Task 8: Docs, version bump, gates, PR

**Files:**
- Modify: `Cargo.toml` (version `2.8.0` → `2.9.0`), `CLAUDE.md` (conventions: one short bullet on the new pace keys and the poller's trust posture), obsidian vault pages per the repo's doc-update table.

- [ ] **Step 1: Vault updates** (dispatch the `vault-keeper` agent or do inline, per the CLAUDE.md table):
  - `Modules/Usage and Pacing.md`: codex collector, poller, `Slow` verdict, `use_credits`, new config keys (canonical owner).
  - `Modules/Ctx Adapters.md`: codex passive usage source note.
  - `Architecture/Technology Stack.md`: `ureq = "3"` and why (first HTTP dependency; blocking; poll fallback only) + version 2.9.0.
  - `Development/Decision Log.md` (≤15 lines each): (1) first HTTP dependency + why passive-primary/poll-fallback; (2) dashboard header usage row re-added — cause of the 2026-08-15 removal fixed; (3) `use_credits` skips proactive gating but never the vendor-reported limit park.
  - `Development/Known Issues.md`: resolve "limit-park is guaranteed unthrottled for a provider with no usage collector"; add residuals: codex ChatGPT-backend endpoint unverified (best-effort code, no live test), codex collector verified against codex-cli 0.105.0 rollout shape only, Anthropic OAuth endpoint is unofficial and may drift (parser degrades to None).
  - `Development/Active Work.md` + `Development/Work Journal.md` entry (≤10 lines).
  - Update `last-verified` frontmatter on every touched page.
- [ ] **Step 2: Version bump** `Cargo.toml` to `2.9.0` (CI rejects PRs that don't rise above base).
- [ ] **Step 3: Full gates**, in order, all must pass (test failure NAMES diffed against main):

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test --verbose -- --test-threads=1
```

- [ ] **Step 4:** Commit: `docs: usage monitoring and throttle docs; bump version to 2.9.0`
- [ ] **Step 5:** Push and open the PR:

```bash
git push -u origin feat/usage-credits-throttle
gh pr create --title "feat(ctx): vendor usage monitoring, use_credits gating, pace-to-reset throttle" --body "<summary per spec; no Generated-with footer>"
```

---

## Post-plan review round (orchestrator, after Task 8)

Per session conventions, not a plan task for implementers: run `/code-review` over the full branch diff, plus one `zirv agent codex` review worker with a self-contained brief; triage, fix confirmed findings, re-review touched areas; hard-stop after 2 fix rounds.
