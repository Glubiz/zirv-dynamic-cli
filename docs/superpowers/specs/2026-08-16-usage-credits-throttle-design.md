# Vendor usage monitoring, use-credits gating, and pace-to-reset throttle

- **Date:** 2026-08-16
- **Status:** Approved design, pre-implementation
- **Owner modules:** `src/commands/ctx/{pace,window,usage,config,chrome,wrap,run_loop,exec}.rs`, `src/commands/ctx/dash/ui.rs`, new `src/commands/ctx/poll.rs`

## Problem

zirv already gates work on Claude's rate-limit windows (`pace::decide` + `pace::wait_for_window`), but:

1. **Codex has no usage source at all.** `window::has_no_usage_source` structurally exempts `openai`, so a codex limit-park relaunches unthrottled — a recorded Known Issue.
2. **Claude's only source is passive** (the statusline tee): data refreshes only while Claude Code renders a statusline, so gating decisions can run on stale data.
3. **There is no soft throttle.** Sessions burn the window at full speed, then stall for hours at the hard pause.
4. **There is no way to express "I pay for overage."** An operator whose vendor plan has extra-usage/credits enabled wants zirv to stop gating entirely for that harness.
5. The dashboard header's usage row was removed (Decision Log 2026-08-15) because it always read "no usage source"; with real sources for both vendors it can return honestly.

## Goals

- A per-harness `use_credits` setting, **default false**. `false` ⇒ throttle-then-pause; `true` ⇒ no throttle, no pause for that harness.
- A passive codex usage collector (session rollout files), closing the unthrottled-limit-park Known Issue.
- An active API-poll fallback for both vendors, used only when passive data is stale at a decision point.
- A pace-to-reset soft throttle between a new `soft_percent` and the existing `max_percent`.
- Usage shown in the wrap status bar (existing slot, fresher data) and re-added to the dashboard header per-harness.

## Non-goals

- No change to the estimator (`pace.estimator`, transcript summing) beyond coexistence.
- No per-script or per-command token accounting; throttling operates at the cycle/pre-flight granularity the gate already has.
- No new verb. `zirv ctx usage` keeps its reporting role and picks up the new data for free.
- No attempt to *discover* whether the vendor plan actually has credits enabled; `use_credits` is an operator declaration.

## 1. Configuration (`config.rs`)

New keys, all under `[pace]`:

```toml
[pace]
soft_percent = 80.0            # start of the throttle band; must be < max_percent
poll_enabled = true            # active API-poll fallback on/off
poll_min_interval_secs = 60    # per-provider floor between poll attempts

[pace.use_credits]
claude = false                 # default; true = never throttle or pause claude
codex = false                  # default; true = never throttle or pause codex
```

- `use_credits` is keyed by **agent name** (what the operator thinks in), resolved to a provider (`anthropic`/`openai`) at gate time via the adapter in play.
- **Trust:** `pace.use_credits.*`, `pace.poll_enabled`, and `pace.poll_min_interval_secs` join `REPO_FORBIDDEN`. `use_credits` is a spend decision (same class as `agent`/`agent_bin`); the poll keys control credential reads and network egress, which a repo checkout must not toggle. `soft_percent` is *not* repo-forbidden (a repo slowing itself down is harmless, same reasoning as existing pace tuning keys).
- **Env overrides** via `ENV_MAP`: `ZIRV_CTX_PACE_SOFT_PERCENT`, `ZIRV_CTX_PACE_POLL_ENABLED`, `ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS`, `ZIRV_CTX_PACE_USE_CREDITS_CLAUDE`, `ZIRV_CTX_PACE_USE_CREDITS_CODEX`.
- Validation: `soft_percent >= max_percent` is treated as "no throttle band" (hard pause only), not an error — config must never brick a session.

## 2. Codex passive collector (`window.rs` + adapter)

Codex-cli writes `rate_limits` snapshots (used percent, window length in minutes, seconds until reset, for a primary and secondary window) into its session rollout JSONL under `~/.codex/sessions/`. New collector:

- Scans the most recently modified rollout file(s), reads the **latest** `rate_limits` snapshot, and maps primary/secondary onto the existing `UsageWindows { five_hour, seven_day }` shape by window length (nearest of 5h/7d; a window that matches neither is dropped, never guessed).
- Persists via the existing atomic write to `usage-openai.json` with `observed_at` = the snapshot's own timestamp (not scan time), so `collector_max_age_secs` staleness stays honest.
- Runs **opportunistically**: invoked from the same points that read usage (gate checks, status-bar redraw, `zirv ctx usage`) whenever the persisted codex file is stale. Scanning is bounded (newest N files by mtime, small read budget) so a hot path never walks the whole sessions tree.
- **Verification item (pre-implementation):** confirm the exact rollout JSON shape against the real files on this machine and record a fixture under `tests/fixtures/`. If the installed codex-cli version writes no `rate_limits` snapshots, the collector ships dormant (codex falls through to the poller) and Known Issues records which codex-cli version is required.

`window::has_no_usage_source` loses its structural `openai` exemption: it becomes a plain "no data ever recorded for this provider" check, because openai now has two possible sources.

## 3. Active poll fallback (new `poll.rs`)

- Trait for testability:

  ```rust
  pub trait UsagePoller {
      fn poll(&self, provider: &str) -> Option<UsageWindows>;
  }
  ```

  The real implementation uses **`ureq`** (new dependency: small, blocking, rustls; no tokio feature changes). Tests use a stub; no test ever touches the network.
- **Claude:** OAuth access token from `~/.claude/.credentials.json`, `GET` Anthropic's OAuth usage endpoint (community-known: `https://api.anthropic.com/api/oauth/usage` with the OAuth beta header). Response utilization/reset fields map to `UsageWindows`.
- **Codex:** token from `~/.codex/auth.json` against the ChatGPT-backend usage endpoint. **This endpoint is unverified.** If it cannot be confirmed working during implementation, codex ships passive-only and Known Issues records the gap. The design must not block on it.
- **When it fires:** only when (a) `poll_enabled`, (b) a gating or display decision needs usage, (c) the passive per-provider file is stale beyond `collector_max_age_secs`, and (d) at least `poll_min_interval_secs` have passed since the last attempt for that provider. The last-attempt timestamp is persisted per provider in the state dir (`poll-<provider>.json`) so concurrent zirv processes share the floor.
- **Failure handling:** any failure (missing credentials, HTTP error, unparseable body) degrades silently to whatever passive data exists, with a one-time `zirv ▸` announcement per process (same pattern as `pace_no_source_announced`). Poll results merge through `window::merge` into the same per-provider files, so `pace::decide` stays pure and single-sourced — the poller is just another writer.
- Credentials are read-only, never logged, never persisted anywhere new.

## 4. Pace-to-reset throttle (`pace.rs`)

`PaceDecision` grows a variant:

```rust
Slow { delay_secs: u64, window: WindowKind, percent: f64, source: Source }
```

Chosen when the worst binding window `p` satisfies `soft_percent <= p < max_percent` (the existing worst-of and collector-beats-estimator rules unchanged). The delay is a linear interpolation between "no delay" at the soft threshold and "wait until reset" at the max:

```
D = t_rem * (p - soft) / (max - soft)      // t_rem = resets_at - now
```

- At `p = soft_percent`, `D = 0`; as `p → max_percent`, `D → t_rem` — exactly continuous with the hard `WaitUntil` pause, which still takes over at `max_percent` unchanged. Near the reset, `t_rem` is small, so delays shrink naturally: 90% used with 2 h left throttles hard; 85% with 10 min left barely at all. This realizes "spread the remaining budget linearly over the time left" without needing a tokens-per-cycle estimate.
- `D` is capped by `t_rem` and gets the existing jitter treatment; the function stays pure and clock-free (caller passes `now`), matching the rot-engine convention.
- **Gate semantics per call site** (the three existing ones): `run_loop.rs` per-cycle and `exec.rs` pre-flight sleep `D` before proceeding (through `wait_for_window`'s chunked, re-checking sleep so a reset mid-delay releases early); `exec.rs` post-limit-park is unaffected (a hit limit is already past `max_percent`).
- **`use_credits` short-circuit:** resolved before `decide` is consulted. If the harness's flag is true, the gate logs one `Event::PacingSkipped`-style decision-log entry per run and proceeds — no throttle, no pause, no poll (polling exists to serve gating; display still uses passive data). This skips only zirv's *proactive* gating: if the vendor itself reports a limit hit (`scan_for_limit`), the existing post-limit park still applies — the vendor saying "you are limited" means credits are exhausted or not actually enabled vendor-side, and relaunching immediately would just re-hit it.

## 5. Display

- **Wrap status bar** (`chrome.rs` / `wrap.rs`): no format change; `BarState.usage_percent` is now fed by fresher data (opportunistic codex collector + poll fallback run from `redraw_bar_if_due`'s read path, subject to the same staleness/interval rules). The en-dash placeholder stays for genuinely unknown.
- **Dashboard header** (`dash/ui.rs`): the usage row returns, per-harness: `claude 72% · codex 41%`, with an en-dash for a vendor with no data (`claude 72% · codex –`). This reverses the 2026-08-15 removal *because its cause is fixed* (there is now a codex source and a staleness-driven poll); the Decision Log gets a new entry saying so. A harness with `use_credits = true` renders with a `credits` marker (e.g. `claude credits`) instead of a misleading percent-toward-pause.
- Rendering stays in the pure renderers (`chrome.rs`/`ui.rs`); data fetching stays in the callers — no I/O added to render paths.

## 6. Error handling summary

| Failure | Behavior |
|---|---|
| No credentials file / unreadable | Poll skipped, one-time announce, passive data used |
| Poll HTTP error / bad body | Same as above |
| Codex rollout format unrecognized | Snapshot dropped; provider treated as "no data" |
| `soft_percent >= max_percent` | Throttle band empty; hard pause only |
| Both sources stale | Existing `binding`/staleness rules in `decide` apply unchanged |
| `use_credits` true | Gate bypassed, logged once per run |

Nothing in this feature may make a session worse: every new path degrades to today's behavior (the `wrap` invariant extends to the poller and collector).

## 7. Testing

- Pure unit tests for the `Slow` math: band edges, continuity at `max_percent`, shrinking `t_rem`, jitter cap, empty band.
- Recorded codex rollout fixture(s) under `tests/fixtures/` (data files only, tests inline per convention) for the collector's parse-and-map, including a window length matching neither 5h nor 7d.
- Poller stubbed via the trait: staleness triggering, min-interval floor, failure degradation, merge-through-`window::merge`.
- `use_credits` resolution: agent→provider mapping, repo-layer rejection (`REPO_FORBIDDEN`), env override.
- Config: defaults, layering, `soft_percent` validation.
- Gates: `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --verbose -- --test-threads=1` (diff test-failure *names* against main; ~47 pre-existing failures on this machine).

## 8. Documentation updates on landing

- `Modules/Usage and Pacing.md` — new sources, throttle, `use_credits` (canonical owner).
- `Modules/Ctx Adapters.md` — codex collector note.
- `Architecture/Technology Stack.md` — `ureq` dependency.
- `Development/Decision Log.md` — first HTTP dependency; header usage row re-added and why.
- `Development/Known Issues.md` — resolve/update the unthrottled codex limit-park entry; record any unverified-endpoint residuals.

## Open verification items (resolve during implementation, before the affected code)

1. Exact codex rollout `rate_limits` JSON shape on this machine's codex-cli version.
2. Anthropic OAuth usage endpoint request/response shape against a real token.
3. Whether a workable codex usage endpoint exists; if not, codex is passive-only (recorded, not blocking).
