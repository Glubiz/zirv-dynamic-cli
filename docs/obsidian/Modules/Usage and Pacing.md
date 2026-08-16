---
last-verified: 2026-08-16
---

# Usage and Pacing

## Quick Reference

- **Files:** `src/commands/ctx/window.rs`, `src/commands/ctx/usage.rs`, `src/commands/ctx/pace.rs`
- **Used by:** [[Ctx Supervisors]] (`run_loop`, `exec`, `wrap` call `pace::wait_for_window` before/around a supervised run); the `zirv ctx usage` verb and `usage tee` statusline wrapper are used directly by an operator's Claude Code `statusLine` config
- **Depends on:** [[Ctx Subsystem]] for config layering and state-dir resolution (`config.rs`, `state.rs`), [[Rot Engine]]'s sibling `log.rs` decision log (pace-wait entries), [[Ctx Adapters]] only indirectly (transcripts under `~/.claude/projects` come from the claude adapter's sessions)
- **Tests:** inline `#[cfg(test)] mod tests` in `window.rs` (parsing, merge, atomic store, transcript summation, estimator math), in `pace.rs` (gating decisions, jitter, wait-cap scaling, `wait_for_window` with a fake clock), and in `usage.rs` (tee persistence/fallback behavior, the `report` output)
- **If changed:** [[Ctx Subsystem]] (config schema for `[pace]`), [[Ctx Supervisors]] (the only callers of the gate), [[Context Management]] (concept-level description of pacing), [[Decision Log]] (`pace-wait` and `limit-wording-drift` entries)
- **Gotchas:**
  - `usage tee` must **never** fail to print a line. If parsing the payload or the chained command fails, it still writes a fallback statusline and always exits `0` — see the `run_tee` and `dispatch`'s `ParseFailure::Statusline` notes below.
  - `five_hour_budget_tokens` and `seven_day_budget_tokens` default to `0`, which means "no budget configured," not "zero tokens." The estimator produces no percentage for a `0`-budget window (`estimate_windows` returns `None` for it) rather than inventing one from an undocumented plan allowance.
  - `usage.json` under the state dir is a single, machine-wide file — not per-session, not per-repo. Every live Claude Code session's statusline tee writes to the same file, merged by newest-observation-per-window. A parallel per-provider file (`usage-<provider>.json`, see below) now exists alongside it. `pace::wait_for_window`/`current_windows`, `zirv ctx usage` (no subcommand), and `wrap`'s reserved status bar are all per-provider since 2026-08-15 (`window::load_for`/`has_no_usage_source`, keyed off the resolved adapter's own `provider()`) — none of the three read the unscoped `window::load` directly anymore.
  - `AgentAdapter::provider()` is the account (`"anthropic"`, `"openai"`), not the binary or the adapter's own `name()` — two harnesses can share one billed account. It has no default, so a new adapter must declare it explicitly rather than silently inheriting someone else's usage windows.

## Purpose

This module answers two related questions for a machine running unattended agent sessions: *how close is this account to its Anthropic rate-limit windows*, and *should a supervised run proceed right now or wait*. `window.rs` owns the data (what a window is, how it is persisted and merged); `pace.rs` owns the decision (the gate consulted by [[Ctx Supervisors]]); `usage.rs` is the human/operator-facing surface (`zirv ctx usage` reporting, and the `usage tee` statusline wrapper that feeds `window.rs` its data).

## How It Works

### `window.rs` — the rolling usage window

A `Window` is one subscription rate-limit reading: `used_percentage`, `resets_at` (unix epoch seconds, `0` meaning "unknown"), and `observed_at`. `UsageWindows` holds two optional windows, `five_hour` and `seven_day` — matching Anthropic's session and weekly rate-limit windows.

There are two independent sources of a `Window`:

- **Collector**: `parse_statusline` reads the `rate_limits.five_hour` / `rate_limits.seven_day` block that Claude Code's statusline JSON payload carries for Pro/Max sessions (server-authoritative). A payload with no `rate_limits` object, or with neither sub-window present, parses to `None` — this is normal (non-subscriber, or before the session's first response), not an error.
- **Estimator**: `sum_transcripts` walks every `*.jsonl` transcript under `~/.claude/projects` (including `subagents/` subdirectories, since subagent turns spend the same budget), sums `input + cache_creation_input + output` tokens per assistant event (`cache_read_input` is excluded by default via `count_cache_reads`, since cached reads are the dominant class in a cached session and are discounted by the API), buckets them into trailing 5-hour and 7-day windows by parsing each event's ISO-8601 timestamp, and tracks the oldest counted event per window (used to predict when the window frees up). `estimate_windows` then turns those token sums into percentages — but only against a caller-supplied budget; see the zero-budget gotcha above.

Persistence: `load`/`store` read and write `StateDir::usage()`, i.e. `usage.json` in the platform state directory — one file, shared by every session on the machine. `store` writes through a `tmp<pid>` file and renames it into place, so a reader never observes a half-written file even with several concurrent sessions' statuslines writing at once. A missing or corrupt file reads back as `UsageWindows::default()` rather than an error (a statusline hook must not break on a half-written state file). `merge` combines an existing and a freshly-parsed `UsageWindows` per sub-window, keeping whichever observation has the newer `observed_at`, and treating an absent window in the fresh reading as "no new information" rather than erasing what was already known.

`window.rs` also contains `parse_iso8601_utc`, a small hand-rolled parser for the exact `2026-07-31T14:15:15.968Z` shape Claude writes into transcripts (using Howard Hinnant's `days_from_civil` so it needs no date crate), and a future-skew guard: an event timestamped more than 5 minutes in the future is skipped entirely rather than being clamped to age-zero and inflating the freshest bucket.

**Per-provider storage (2026-08-15).** The single machine-wide `usage.json` above cannot say *which account* a reading belongs to, and the old `load`'s default-on-absent made "no source" and "sitting at 0%" indistinguishable. `StateDir::usage_for(provider)` names a second file per account, `usage-<provider>.json`, where `provider` is sanitized by `state::provider_slug` (lowercase `[a-z0-9-]`, non-empty, `"unknown"` if that would otherwise be empty — cannot escape the state directory). `window::store_for`/`load_for` are the per-provider counterparts of `store`/`load`; `load_for` returns `None` — never a zero-valued `UsageWindows` — when nothing has ever been recorded for that provider, and for `anthropic` specifically it falls back to reading the legacy global `usage.json` (never moving or deleting it), so an operator upgrading into this layout keeps the reading their statusline has already been collecting. `usage::run_tee` writes both the legacy file and the resolved adapter's own `usage_for(adapter.provider())` file on every statusline tick. This storage was originally paired with dashboard-header rendering (`window::provider_summary`, `window::load_all`, `adapters::providers()`), all since **deleted** (2026-08-15, `a3b81c3`) once the dashboard dropped usage from its header. Usage is **back in the header as of Task 7 (2026-08-16)**, via a different, narrower reader (`window::load_for`/`max_used_percentage` per enabled harness, not the deleted `load_all`/`provider_summary`) — see [[Ctx Supervisors]] and [[Decision Log]] for why the "always reads no usage source" objection no longer holds.

**The pacing gate, `zirv ctx usage`, and `wrap`'s status bar all became per-provider readers the same day (2026-08-15, rounds 2 and 3).** Until this fix, `pace::current_windows`/`wait_for_window`, `usage::run_with`'s no-subcommand report, and `wrap`'s `redraw_bar_if_due` all called the unscoped `window::load`, so a codex session was paced against, and `zirv ctx usage`/the wrap status bar displayed, whatever Anthropic data claude's own statusline tee happened to have written -- not "no data for this account". `window::has_no_usage_source(state, provider)` is the fix's own key predicate: `true` for any provider but claude when `load_for` finds nothing (claude is exempt even before its first tee, since "not yet" is still the true, actionable answer there, unlike codex's structural "never"). `wait_for_window` now takes the resolved adapter's `provider()` and, when `has_no_usage_source` is true, skips the gate outright with one `Event::PacingSkipped` announcement (`"pacing off: <provider> has no usage source"`) rather than entering the loop and reading the absence as a fresh 0%-used collector reading for that provider. `zirv ctx usage` (no subcommand) prints the same `"<provider>: no usage source"` line instead of `report`'s "not reported ... wire your statusline" text, which is only ever true advice for claude. `wrap`'s bar needed no renderer change at all: `BarRuntime` now carries the resolved adapter's own `provider` (set at both construction sites, `wrap.rs`), `redraw_bar_if_due` reads `window::load_for(state_dir, &bar.provider).unwrap_or_default()` instead of the unscoped `window::load`, and `chrome::status_bar` already rendered a `None` `usage_percent` as the placeholder dash rather than a fabricated `0%` (its `BarState`/`PLACEHOLDER` contract predates this fix) -- only the *source* of the value changed.

`score::cached_score(state, repo, session_id) -> Option<u32>` (in `score.rs`, covered in depth on [[Rot Engine]]) is unrelated to usage windows but was added the same round for the same "cheap per-pane facts on a ~1s render throttle" reason: a dashboard pane's rot score, backed by the same incremental fold the Stop hook uses, cached behind a single `metadata()` call in steady state. `None` means unknown, never a healthy `0`.

**A wrapped codex session's own passive rollout scan (Task 7, 2026-08-16).** `wrap`'s status bar reads `window::load_for(state_dir, &bar.provider)` on every throttled redraw, same as before, but a wrapped codex session had no statusline tee of its own to keep that file fresh — its usage segment would otherwise stay a permanent placeholder for the session's whole life, unlike the pacing gate's own call sites (Tasks 4-6), which already scan/poll before every cycle. `redraw_bar_if_due` now runs `window::refresh_codex_usage` itself, floored to once per `CODEX_BAR_SCAN_SECS` (60s, independent of the 1s `BAR_THROTTLE` redraw cadence — a rollout scan is a real filesystem walk) and gated on `bar.provider == window::CODEX_USAGE_PROVIDER`. File-scanning only, exactly like `refresh_codex_usage` everywhere else it is called: `HttpPoller`/`maybe_poll` never appear on this path, matching `wrap`'s "must never make a session worse" invariant. The sessions directory is resolved via `crate::utils::home_dir()` (`HOME`/`USERPROFILE`), the same as `pace::refresh_sources` and `usage.rs`'s own refresh, not `refresh_codex_usage`'s internal `dirs::home_dir()` fallback, which ignores those variables on Windows and so cannot be pointed at a test fixture there.

### `pace.rs` — the pacing gate

`decide(collector, estimator, now, cfg) -> PaceDecision` is the pure core. `PaceDecision` is one of:

- `Proceed { source, worst_percent }`
- `WaitUntil { reset_at, window, percent, source }`
- `Unknown` — no usable data at all (proceeds, but is reported honestly as unknown rather than as a healthy 0%)

The logic, in order:

1. If `cfg.enabled` is `false`, always `Proceed` with `Source::None` — pacing is off entirely.
2. A collector window is *binding* (`binding()`) if it is fresh (age ≤ `collector_max_age_secs`, default 900s), **or** if it is stale but was last seen at or above `max_percent` and its `resets_at` hasn't passed yet — a window cannot free up before its own reset, so staleness alone must never clear a park. A stale reading below the ceiling is simply treated as unknown.
3. Of the two sub-windows, `worst()` picks the one with the higher `used_percentage`.
4. If a binding collector reading exists, it wins outright — `Source::Collector`, even when an estimator reading disagrees (server-authoritative data always beats the approximation). Only when no collector reading binds, and `cfg.estimator` is enabled, does an estimator reading (only produced when a budget is configured) get considered, as `Source::Estimator`.
5. If the winning window's percentage is below `max_percent` (default 99.0%, inclusive ceiling), `Proceed`. Otherwise `WaitUntil`, carrying the window's own `resets_at` (or `None` if it's `0`/unknown).

`wait_deadline(decision, now, cfg, seed)` turns a `WaitUntil` into a concrete wake time: the window's `resets_at` if known and still in the future, else `now + fallback_delay_secs` (default 900s). That target is jittered by `apply_jitter` (a deterministic, non-cryptographic PCG-style mix keyed by `seed`, bounded to `[0, jitter_secs)`, default up to 30s — so several supervisors on one machine don't all wake in the same second) and then capped by `wait_cap(window, cfg)`: either the operator's absolute `max_wait_secs` override, or (the default) the tripped window's own length (`FIVE_HOUR_SECS` / `SEVEN_DAY_SECS`) plus `wait_slack_secs` (default 3600s) head-room. The cap is scaled per-window deliberately: a seven-day trip legitimately needs to wait days, while a bogus far-future `resets_at` on the five-hour window must not park a supervisor for a year.

`wait_for_window<W: Write>(w, state, cfg, verb, session, now_fn, sleep_fn, announcer: Option<&announce::Announcer>)` is the actual blocking call [[Ctx Supervisors]] use. It loops: read `current_windows` (collector always; estimator only when `cfg.estimator` is on **and** at least one budget is nonzero, since walking every transcript is not free) → `decide` → if `Proceed`/`Unknown`, return immediately with `PaceOutcome { waited_secs, source }` → if `WaitUntil`, check the total elapsed time against the window-scaled safety cap (if exceeded, print `"usage still high after waiting Ns (cap Ns), proceeding anyway"` and return rather than blocking forever — pacing failing closed would be worse than not pacing) → otherwise sleep in ≤30s chunks (`SLEEP_CHUNK_SECS`) so it rechecks state periodically (a live session may refresh the collector file mid-wait), printing/logging the decision once per distinct fingerprint (window name + reset time) rather than once per sleep chunk, to avoid scrolling the operator's terminal for days on a long park. It never exits the process and never returns an error. `announcer`, when `Some`, also emits a `PacingWait` event on the `zirv ▸` channel (see [[Ctx Supervisors]]) alongside the existing `w`/decision-log reporting, so a wait is visible on the announcement channel and not just in the transcript. `exec.rs` passes its own `Announcer` at both of its call sites; `run_loop.rs` passes `None` at both of its.

`pace.rs` also owns limit-message detection used by the supervisors: `is_limit_hit` matches exactly three documented phrases (`"hit your session limit"`, `"hit your weekly limit"`, `"hit your opus limit"`, case-insensitive substring match) — deliberately narrow, since a false positive parks a healthy run and an unverified guess is not a fact. `scan_for_limit` runs this over a batch of tapped output lines and also calls `note_limit_wording_drift` for any line that loosely resembles a limit message (contains "limit" plus a hit/reached/exceeded verb) without matching strictly — this only logs a decision-log breadcrumb and prints a stderr advisory, it never itself changes the proceed/wait outcome.

### `usage.rs` — the `usage` verb and statusline tee

`zirv ctx usage` (no subcommand) loads config, resolves state, computes `current_windows`, and prints a human-readable `report`: the collector reading for each sub-window (with freshness and reset time, or "not reported" if absent — never shown as 0%), the estimator reading if a budget is configured (explicitly labeled "approximation" since token-class weighting is undocumented) or an "estimator: off" line telling the operator how to enable it, and a pacing summary (ceiling, wait bounds for each window, and the current `decide()` verdict).

`zirv ctx usage tee -- <original statusline command>` is meant to be wired directly into Claude Code's `statusLine` setting, wrapping the operator's real statusline script. `run_tee`:

1. Reads the statusline JSON payload from stdin.
2. Best-effort persists it first: if it parses into a `UsageWindows` (via `window::parse_statusline`), merges it into `usage.json` and writes it back — before attempting the chained command, so a broken statusline script downstream cannot cost the reading.
3. Runs the original chained command (`run_chained`), piping the same stdin JSON to it and capturing its stdout.
4. Prints the chained command's output if it succeeded and produced non-blank output; otherwise falls back to `fallback_line`, a one-line summary built from the payload's `model.display_name` and `context_window.used_percentage` (or just the model name, or literally `"claude"` if the payload itself is unparseable).
5. **Always returns `0`.**

This resilience is deliberate and load-bearing: Claude Code renders whatever the statusline command prints on every render tick, so a `tee` that errors out or prints nothing makes the terminal look broken. The same fallback path is reused one level up: `ctx/mod.rs`'s `dispatch` classifies a clap parse failure on `ctx usage tee ...` argv as `ParseFailure::Statusline` (as opposed to the generic `ParseFailure::Reject`, which exits `2`) and calls `usage::run_tee` with an empty command and no state, so even a malformed invocation of the tee itself still emits a fallback line and exits `0`.

## Data Flow

```mermaid
flowchart TD
    SL["Claude Code statusline<br/>JSON on stdin"] --> TEE["usage::run_tee"]
    TEE -->|parse_statusline| MERGE["window::merge(load, fresh)"]
    MERGE -->|atomic write| USAGEJSON[("usage.json<br/>(machine-wide state dir)")]
    TEE -->|chain| ORIG["original statusline command"]
    ORIG -->|ok, non-blank| PRINT["print chained output"]
    ORIG -->|failed/missing/blank| FALLBACK["fallback_line()"]
    FALLBACK --> PRINT
    PRINT --> EXIT0["exit 0, always"]

    subgraph Supervisor run
        USAGEJSON --> LOAD["pace::current_windows"]
        TRANSCRIPTS[("~/.claude/projects/**/*.jsonl")] -->|if estimator on and budget set| SUMS["window::sum_transcripts<br/>+ estimate_windows"]
        SUMS --> LOAD
        LOAD --> DECIDE["pace::decide<br/>(collector wins if binding,<br/>else estimator, else Unknown)"]
        DECIDE -->|Proceed| GO["run proceeds"]
        DECIDE -->|Unknown| GO
        DECIDE -->|WaitUntil| DEADLINE["wait_deadline<br/>(reset or fallback, jittered, capped)"]
        DEADLINE --> SLEEP["sleep in <=30s chunks,<br/>recheck each time"]
        SLEEP -->|cap exceeded| GO
        SLEEP -->|window healthy now| GO
    end
```

## See Also

- [[Ctx Subsystem]] — the hub page for the whole `zirv ctx` command tree and its config/state layering
- [[Ctx Supervisors]] — `run_loop`, `exec`, and `wrap`, the actual callers of `pace::wait_for_window`
- [[Decision Log]] — where `pace-wait` and `limit-wording-drift` entries land
- [[Context Management]] — the higher-level concept this module supports
