# Fix round 2, batch B — usage/poll/window review fixes

Branch: `feat/usage-credits-throttle`
Base commit before this batch: `4a44eb2` (pace batch `82fa28b` had already landed).

## Item 1 — `parse_rfc3339_utc` unbounded-year overflow panic

**File:** `src/commands/ctx/window.rs`, `parse_rfc3339_utc` (originally ~L373-410).

The year field came straight from `date.split('-').next()` with no length or
range bound, unlike the pre-existing `parse_iso8601_utc` in the same file,
which is implicitly bounded to exactly 4 characters because it slices
`ts.get(0..4)`. An absurd year (e.g. `"999999999999-01-01T00:00:00Z"`) reached
`days_from_civil(y, ..) * 86_400`, which overflows `i64` and panics in debug
builds. This is reachable from `wrap`'s status-bar redraw via the codex
rollout scan (`scan_codex_rollouts` → `last_snapshot_in` → `parse_rollout_line`
→ `parse_rfc3339_utc`), so a malicious or corrupted rollout timestamp could
crash a wrap session — a direct violation of "wrap must never make a session
worse."

**Fix:** reject the year unless the split token is exactly 4 ASCII digits and
falls in `1970..=9999`, mirroring `parse_iso8601_utc`'s bound.

```rust
let y_field = dp.next()?;
if y_field.len() != 4 || !y_field.bytes().all(|b| b.is_ascii_digit()) {
    return None;
}
let y: i64 = y_field.parse().ok()?;
if !(1970..=9999).contains(&y) {
    return None;
}
```

**Test:** `an_absurd_year_is_rejected_not_overflowed` — asserts the absurd-year
timestamp, a shorter-but-still-oversized year, and a pre-epoch year all return
`None` (never panic), that `1970-01-01T...` still parses, that `9999-...`
still parses, and that a full rollout line carrying the absurd year returns
`None` via `parse_rollout_line`.

**RED/GREEN evidence:** Temporarily reverted the guard and ran the new test —
it genuinely panicked:
```
thread '...an_absurd_year_is_rejected_not_overflowed' panicked at
src\commands\ctx\window.rs:413:9:
attempt to multiply with overflow
```
Restored the fix; the same test passes.

## Item 2 — `refresh_codex_usage` rewrites identical state on every refresh

**File:** `src/commands/ctx/window.rs`, `refresh_codex_usage` (originally ~L574-595).

After a stale-reading scan, `merge` + `store_for` ran unconditionally, even
when the scan found nothing newer than what was already stored — rewriting an
identical file (temp-write + rename) on every passive refresh.

**Fix:** compare the merged result against the existing stored value
(`UsageWindows` already derives `PartialEq`) and skip `store_for` when they
match.

```rust
let merged = merge(existing.clone().unwrap_or_default(), fresh);
if Some(&merged) == existing.as_ref() {
    return;
}
let _ = store_for(state, CODEX_USAGE_PROVIDER, &merged);
```

**Test:** `refresh_skips_the_store_when_the_merge_produces_no_change` — seeds
the provider file with exactly what a scan of a fixed rollout file produces,
back-dates the file's mtime via `set_modified` (same idiom the existing
`scan_finds_newest_by_timestamp_not_mtime` test already uses), calls
`refresh_codex_usage` with `now` far enough past the observation to force it
past the freshness early-return and into the scan/merge path, then asserts
the mtime is unchanged — proof `store_for`'s temp-write+rename never ran.

**RED/GREEN evidence:** Temporarily removed the equality-guard and ran the
test — it failed on a real mtime mismatch:
```
assertion `left == right` failed: store_for must be skipped when the merge is unchanged
  left: SystemTime { intervals: 134314200905553952 }
 right: SystemTime { intervals: 134314236905576995 }
```
Restored the fix; the same test passes.

## Item 3 — `HttpPoller::poll` rebuilds a fresh `ureq::Agent` per call

**File:** `src/commands/ctx/poll.rs`.

Verified the resolved crate: `ureq = "3.4.0"` (Cargo.lock), confirmed against
the local registry source (`ureq-3.4.0/src/config.rs`) that
`Agent::config_builder()`, `ConfigBuilder::timeout_global`, and
`ConfigBuilder::timeout_connect` all exist with the signatures already used
in this codebase (`ureq::Agent::config_builder()...build().into()`).

**Fix:** build the agent once in a `std::sync::OnceLock<ureq::Agent>` and
reuse it across calls; added a distinct 3s connect timeout
(`HTTP_CONNECT_TIMEOUT_SECS`) alongside the existing 10s
global/body timeout (`HTTP_TIMEOUT_SECS`), so an unreachable endpoint fails
to connect in ~3s instead of blocking a cycle-launch gate for up to 10s.

```rust
static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

fn shared_agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .timeout_connect(Some(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS)))
            .build()
            .into()
    })
}
```

`HttpPoller::poll` now calls `shared_agent()` instead of constructing a new
`Agent` inline.

**Test:** `the_shared_agent_is_constructed_once_and_reused` — as scoped in the
task, no live HTTP: calls `shared_agent()` twice and asserts both calls
return the same pointer, proving the `OnceLock` is doing its job rather than
rebuilding. This is a compile-plus-construction check; the timeout values
themselves aren't independently assertable without a real request (ureq's
`Config` has no public getters exercised here), so I did not add a second
test for that — noting it per your instruction to say so explicitly.

## Item 4 — Fresh claude machine lost the tee guidance

**Files:** `src/commands/ctx/usage.rs` (no-subcommand branch, ~L349-360).
`status.rs` checked and left untouched — see below.

The no-subcommand branch printed `"{provider}: no usage source"` for *every*
provider once `window::has_no_usage_source` was true, including anthropic on
a fresh claude machine with no legacy file and no active poll (anthropic has
no poll fallback path distinct from the tee). That made `report()`'s "not
reported ... wire your statusline through `zirv ctx usage tee`" line
unreachable exactly where it used to help.

**Fix:** special-case `window::LEGACY_USAGE_PROVIDER` ("anthropic") to fall
through to the existing `report()` call (which already handles an empty
`UsageWindows::default()` collector gracefully — `pace::current_windows`
`unwrap_or_default()`s on no data) instead of returning the generic line.
Every other provider (codex/openai today) keeps the generic line, since there
genuinely is no richer guidance to give a provider with no collector at all.

```rust
if window::has_no_usage_source(&state, provider) && provider != window::LEGACY_USAGE_PROVIDER {
    writeln!(w, "{provider}: no usage source")?;
    return Ok(0);
}
```

**`status.rs`:** read its no-source arm (`run_with`, ~L226-246). It only ever
printed `"usage windows: {provider}: no usage source"` — a one-line summary,
never `report()`'s richer tee-wiring hint. Per your instruction to mirror
"only if it genuinely had richer guidance before," it did not, so `status.rs`
is untouched. All its existing tests
(`status_shows_no_usage_source_for_a_codex_configured_repo_...`,
`status_names_no_usage_source_for_a_disabled_codex_...`) still pass unchanged.

**Tests:**
- `a_fresh_claude_machine_still_gets_the_tee_guidance` (new) — explicit
  `ZIRV_CTX_AGENT=claude`, fresh `HomeGuard` home and fresh state dir;
  asserts the output does *not* start with `"anthropic: no usage source"`,
  contains `"not reported"`, and contains `"zirv ctx usage tee"`. Explicit
  agent selection so it does not depend on which adapter `resolve_default`
  picks on the test machine.
- `the_verb_reports_without_a_subcommand` (existing, machine-dependent per
  your note) — left untouched; ran it standalone and it now passes on this
  machine (`resolve_default` picks claude here, which previously hit the
  regressed generic-line path and would have failed the `"not reported"`
  assertion pre-fix — did not chase this further since it's explicitly out of
  scope).
- All existing codex/openai no-source tests
  (`the_verb_names_a_provider_with_no_usage_source`,
  `a_disabled_codex_shows_no_usage_source_not_anthropic_numbers`,
  `an_unset_agent_with_an_operator_disabled_claude_reports_codexs_own_provider`)
  still assert the exact `"openai: no usage source\n"` line and pass
  unchanged, since `provider != LEGACY_USAGE_PROVIDER` for all of them.
- `the_verb_falls_back_to_the_legacy_reading_when_select_refuses` (existing)
  still passes: that scenario has real stored data, so
  `has_no_usage_source` is false and the branch is never entered.

**RED/GREEN evidence:** Temporarily reverted the `provider !=
LEGACY_USAGE_PROVIDER` guard and ran the new test — it failed:
```
panicked at src\commands\ctx\usage.rs:1020:9:
the generic no-source line must not shadow the tee guidance: anthropic: no usage source
```
Restored the fix; the same test passes.

## Full gate

```
cargo test --quiet -- --test-threads=1 window:: poll:: usage:: status::
  -> 91 passed; 0 failed
cargo test --quiet -- --test-threads=1 window poll usage status   (broader name match, pulls in exec.rs etc.)
  -> 139 passed; 1 failed (pre-existing os-193 spawn-family failure in
     exec::tests::a_headless_worker_stops_at_the_next_poll_and_relaunches_with_the_guidance,
     unrelated to this batch — matches the documented pre-existing Windows
     spawn-family failure class)
cargo fmt -- --check          -> clean (one block needed `cargo fmt` to
                                  reflow a long `if` condition in usage.rs;
                                  applied, re-checked clean)
cargo clippy --all-targets -- -D warnings  -> clean, no warnings
```

## Self-review

- Item 1: bound matches `parse_iso8601_utc`'s existing 4-digit-year
  discipline exactly, so the two parsers no longer disagree about what a
  valid year looks like. Chose an explicit length+digit check rather than
  relying on `i64::checked_mul`/`checked_add` deeper in `days_from_civil`,
  because the task explicitly asked for the same bounding strategy as the
  sibling parser, and it's simpler to reason about at the call site than
  threading `checked_*` through the Howard Hinnant algorithm.
- Item 2: used `Some(&merged) == existing.as_ref()` rather than
  `merged == existing.unwrap_or_default()` — the latter would also (harmlessly
  but confusingly) skip the *first* write when a fresh scan happens to
  produce `UsageWindows::default()`-equivalent data and nothing was stored
  yet; comparing `Option`s keeps "no prior data, first write" and "prior
  data, unchanged write" distinct, which matches the finding's framing
  ("rewrites identical state") rather than a broader "never write default."
- Item 3: did not attempt to assert the actual timeout durations took effect
  — ureq's `Config`/`Agent` don't expose public getters for this without a
  real connection attempt, and the task explicitly permitted a
  compile-plus-construction test in that case. Flagging this as the one
  item where test coverage is weaker than the others by design.
- Item 4: worth double-checking that `pace::current_windows` and `report()`
  truly tolerate the fully-empty case — confirmed via
  `an_absent_window_says_so_rather_than_showing_zero` (pre-existing) and the
  new `a_fresh_claude_machine_still_gets_the_tee_guidance` test, both green.
- Did not touch `status.rs` — verified by reading its no-source arm in full
  rather than assuming; it never had richer guidance to restore.
- No changes outside the four files/items listed. `.superpowers/` is
  untracked scratch content from a prior batch, left alone.

## Concerns

- None blocking. The one soft spot is item 3's test coverage (acknowledged
  above and in the task's own allowance for it).
- `the_verb_reports_without_a_subcommand`'s machine-dependence is unchanged
  in nature — it was already documented as machine-dependent before this
  batch, and this batch's fix happens to make it pass on *this* machine, but
  a machine where `resolve_default` picks codex first would still hit the
  codex "no usage source" generic line and fail the `"not reported"`
  assertion. That is pre-existing scope, explicitly called out as
  out-of-scope in the task brief, so left as-is.
