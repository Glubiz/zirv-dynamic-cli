# Fix Round 2 — Batch A Report

Branch: `feat/usage-credits-throttle`
Files touched: `src/commands/ctx/pace.rs`, `src/commands/ctx/announce.rs`,
`src/commands/ctx/window.rs`, `src/commands/ctx/wrap.rs`

All five checklist items applied, TDD per item where behavior changed. Gates
(`cargo test --quiet -- --test-threads=1 pace announce`, `cargo fmt --
--check`, `cargo clippy --all-targets -- -D warnings`) all pass.

## Item 1 — Estimator-only pacing was silently disabled

**Root cause confirmed**: `wait_for_window`'s no-usage-source early return
(`usage_window::has_no_usage_source(state, provider)`) fired purely off the
collector, with no regard for whether `cfg.estimator` + a configured budget
could still produce a decision via `current_windows`. A claude machine with
no statusline tee and no working poll, but `pace.estimator = true` and a
nonzero `five_hour_budget_tokens`/`seven_day_budget_tokens`, took the skip
path every cycle.

**Fix**: extracted `fn estimator_configured(cfg: &PaceConfig) -> bool` (used
by both `current_windows`, which already had the equivalent inline check,
and the no-source guard in `wait_for_window`). The guard is now:

```rust
if usage_window::has_no_usage_source(state, provider) && !estimator_configured(cfg) {
```

**Test**: `estimator_only_pacing_engages_when_the_collector_has_nothing`
(pace.rs). Builds a real transcript under a `HomeGuard`-isolated
`.claude/projects/`, no collector data stored anywhere, `five_hour_budget_
tokens: 1000` with usage that estimates to 100%. Asserts `flags.no_source_
announced` stays false, no "no usage source" line is printed, and `outcome.
waited_secs > 0` (proving it entered the real wait loop rather than
zero-wait skipping).

**RED confirmed**: reverting the `&& !estimator_configured(cfg)` guard back
to the bare `has_no_usage_source` check fails the test with `estimator
pacing configured must not take the no-source skip path`. Restored and
GREEN.

## Item 2 — A Slow wait truncated when the reading went stale mid-wait

**Root cause confirmed**: once a Slow deadline latched (`slow_deadline:
Option<u64>`), a later recheck whose reading crossed `collector_max_age_
secs` made `decide` return `Unknown`. The old match's `_` arm treated any
non-`Slow` decision (including this `Unknown`) as "no active throttle":
it reset `slow_deadline = None` and computed `wait_deadline(&Unknown, ...)
== None`, which hit the `let Some(deadline) = deadline else { return ... }`
early exit -- ending the wait hours before the announced deadline. The
existing test `a_slow_wait_does_not_extend_itself_across_rechecks` passed
via this exact escape (staleness fired at ~890s, before the 1000s deadline
was ever reached), so the monotonic-min machinery was never exercised
end-to-end.

**Fix**: `slow_deadline: Option<u64>` became `slow: Option<(u64,
&'static str)>` (deadline + the window that produced it, needed for the
cap fix below). Three call sites changed:
- **deadline match**: added `PaceDecision::Unknown if slow.is_some() =>
  slow.map(|(deadline, _)| deadline)` -- a latched-but-currently-stale
  recheck keeps the latched deadline instead of falling into the `_` arm.
  `WaitUntil` still falls into `_` (clears `slow` and uses its own absolute
  deadline, superseding as specified), and a fresh `Slow` recheck still
  applies the `.min()` rule as before.
- **cap match**: added the same `Unknown if slow.is_some()` arm so the cap
  check right after (`now.saturating_sub(started) >= cap`) uses the latched
  window's `wait_cap`, not the bare `0` a plain `Unknown` gets (which would
  otherwise immediately trip "usage still high... proceeding anyway" and
  defeat the deadline fix).
- **fingerprint match**: same arm, reusing the `slow:<window>` fingerprint
  from the `Slow` phase -- since it doesn't change, the `announced !=
  fingerprint` gate silently absorbs the stale recheck rather than
  re-printing or (worse) printing "usage state unknown, proceeding" while
  actually still waiting.

**Masked test fixed**: `a_slow_wait_does_not_extend_itself_across_rechecks`
now sets `collector_max_age_secs: 3000` so the stored reading (fixed at
`observed_at = NOW - 10`) stays fresh for the whole ~1000s simulated wait,
and gained a new lower-bound assertion (`outcome.waited_secs >= 1000`) so a
future regression that exits early again cannot silently pass.

**New test**: `a_latched_slow_deadline_survives_the_reading_going_stale_
mid_wait` -- same 90%/1900s-reset shape (delay_secs = 1000) but
`collector_max_age_secs: 200`, so staleness genuinely fires ~190s into the
wait, well before the 1000s deadline. Asserts `outcome.waited_secs >= 1000`
(not truncated to ~200s).

**RED confirmed**: reverting just the deadline-match `Unknown if slow.is_
some()` arm fails the new test (`waited only 210s`) while the fixed masked
test still passes (by design -- it never goes stale, so it doesn't exercise
this particular arm; it proves the monotonic-min rule on its own). Restored
and GREEN.

## Item 3 — Phantom throttle right after a genuine window reset

**Root cause confirmed**: inside `decide`'s soft-band branch, a fresh
reading (age < `collector_max_age_secs`) whose `resets_at` was nonzero but
already `<= now` fell into the `else` arm of `if window.resets_at > now`,
i.e. `t_rem = cfg.fallback_delay_secs` (~900s), producing a `Slow` decision
even though the window had genuinely rolled over and real usage should read
near 0%.

**Fix**: added `let reset_passed = window.resets_at != 0 && window.resets_at
<= now;` and gated the whole soft-band `Slow` computation on `&& !reset_
passed`. A `resets_at == 0` ("never reported") is unaffected and still hits
the fallback branch exactly as before -- only a *known, passed* reset now
skips straight to `Proceed`. Scoped deliberately to the soft-band (`Slow`)
branch only; the `>= max_percent` (`WaitUntil`) branch is untouched, so the
pre-existing test `a_reset_already_in_the_past_uses_the_fallback_too`
(99.5%, past reset -> `WaitUntil` via the fallback) still passes unchanged.
`decide` remains pure (no clock/fs/env reads added).

**Test**: `a_reading_after_a_genuine_reset_does_not_phantom_throttle` --
95% (soft-band, given soft 80/max 99), `resets_at = NOW - 120`, fresh
`observed_at`. Asserts `Proceed { source: Collector, worst_percent: 95.0 }`.

**RED confirmed**: reverting the `&& !reset_passed` guard fails with `left:
Slow { delay_secs: 710, window: "five_hour", percent: 95.0, ... }`. Restored
and GREEN.

## Item 4 — Slow was invisible on the announce channel

**Root cause confirmed**: the announce block matched only `(Some(announcer),
PaceDecision::WaitUntil { .. })`, so a `Slow` throttle -- potentially hours
of soft-band delay -- never reached `announcer.emit` at all, only the plain
`w` writer.

**Fix**: added `Event::PacingThrottled { window, delay_secs, percent }` to
`announce.rs`, following the existing `PacingWait` pattern exactly (same
doc-comment style, `text()` arm, and an entry in `announcements_never_
touch_the_reserved_bar_row`'s sample list). In `pace.rs`, extracted the
decision-to-event mapping into a small pure helper:

```rust
fn pacing_event(decision: &PaceDecision) -> Option<super::announce::Event>
```

covering `WaitUntil -> PacingWait` and `Slow -> PacingThrottled`, `None`
otherwise. `wait_for_window`'s announce block now just does `if let
(Some(announcer), Some(event)) = (announcer, pacing_event(&decision)) {
announcer.emit(&event); }`. This extraction was needed for genuine
testability: `Announcer::emit` always writes to real stderr (confirmed by
grep -- every existing caller in `wrap.rs`/`exec.rs`/`run_loop.rs` does the
same, and every test that touches an announcer-consuming function uses
`Announcer::silent()` and never asserts on stderr content), so without
pulling the mapping out into a pure function there was no way to write a
mutation-sensitive test for "Slow now maps to an event" at all.
Emitted once per latched episode via the same `announced != fingerprint`
gate as before (see item 2's fingerprint note).

**Tests**:
- `pacing_event_maps_wait_until_and_slow_and_nothing_else` (pace.rs) --
  direct, pure test of the mapping for `WaitUntil`, `Slow`, `Unknown`,
  `Proceed`. This is the real proof of item 4.
- `a_pacing_throttled_announcement_names_the_window_delay_and_percent`
  (announce.rs) -- `Event::PacingThrottled::line()` renders the window,
  delay, and percent.
- `a_slow_pass_announces_once_not_per_recheck` (pace.rs) -- integration-level
  check that the throttle-episode text appears exactly once across a
  multi-recheck wait, not once per 30s chunk (uses `Announcer::silent()` to
  exercise the code path without writing to real stderr, matching this
  crate's established convention).

**RED confirmed**: reverting just the `PaceDecision::Slow { .. } => Some(...
PacingThrottled ...)` arm out of `pacing_event` fails `pacing_event_maps_
wait_until_and_slow_and_nothing_else` with `left: None, right: Some(Pacing
Throttled { ... })`. Restored and GREEN.

## Item 5 — Full rollout-tree walks every 30s during parks

**Root cause confirmed**: `pace::refresh_sources` called `window::refresh_
codex_usage` unconditionally on every wait-loop iteration when `provider ==
"openai"`. `refresh_codex_usage`'s own internal gate only floors on how
*stale a stored reading* has to be -- it does nothing for a provider with no
stored reading, or once that reading is genuinely stale (which a parked
session with no new rollouts guarantees). Result: a parked codex session
re-walked the whole `~/.codex/sessions` tree on every 30s recheck for as
long as the park lasted.

**Fix**:
- `window.rs` gained `pub(crate) const CODEX_SCAN_FLOOR_SECS: u64 = 60;`
  (placed next to `CODEX_USAGE_PROVIDER`).
- `wrap.rs`'s private `const CODEX_BAR_SCAN_SECS: u64 = 60;` was replaced
  with `use super::window::CODEX_SCAN_FLOOR_SECS as CODEX_BAR_SCAN_SECS;`
  -- every existing reference site (`BarRuntime` doc comments, `redraw_bar_
  if_due`) is untouched, since the alias keeps the same local name in scope.
- `pace::PaceGateFlags` gained `pub last_codex_scan: u64` (unix-seconds of
  the last scan *attempt*, `0` = never; `Default` derive covers every
  existing `PaceGateFlags::default()` construction site, so no other file
  needed changes).
- `refresh_sources` gained a `flags: &mut PaceGateFlags` parameter and now
  only attempts the codex scan when `now.saturating_sub(flags.last_codex_
  scan) >= usage_window::CODEX_SCAN_FLOOR_SECS`, updating the flag whenever
  an attempt is made (regardless of whether the scan finds anything new).
  Both call sites inside `wait_for_window` (pre-loop and in-loop) were
  updated to pass `flags` through (already threaded through the whole
  function).

**Test**: `refresh_sources_floors_codex_scan_attempts_to_the_shared_
constant` (pace.rs). Three direct calls to the (module-private)
`refresh_sources` under a `HomeGuard`-isolated `.codex/sessions/`:
call 1 (floor open, nothing scanned yet) picks up a 12% rollout and stores
it; call 2, 30s later (`< 60s`, floor closed), leaves a newly-written 99%
rollout file untouched and `flags.last_codex_scan` unmoved -- proving the
attempt was skipped, not just naturally deduped; call 3, 90s after call 1
(floor open again), picks up the 99% file. `cfg.collector_max_age_secs = 1`
isolates the *new* floor as the only thing that can explain call 2's skip
(the existing internal staleness gate would otherwise also explain it).

**RED confirmed**: reverting the floor condition (scan unconditionally)
fails at call 2 with `left: 99.0, right: 12.0` (the second file was picked
up 30s later, when it should have been floored). Restored and GREEN.

## Verification

- `cargo test --quiet -- --test-threads=1 pace announce`: **86 passed, 0
  failed**.
- `cargo test --quiet -- --test-threads=1 wrap:: window::`: 106 passed, 3
  failed -- confirmed identical failures (same test names, same panic
  messages, `Os { code: 193, ... }` spawn-family) on the base branch via
  `git stash`; not caused by this change.
- `cargo test --quiet -- --test-threads=1 exec:: run_loop:: config::`: 109
  passed, 21 failed -- confirmed a representative sample (2 os-193 spawn
  failures + 1 machine-dependent restart-budget assertion) identical on the
  base branch via `git stash`; not caused by this change. Full 21-name list
  matches the "known pre-existing" pattern described in the task (os-193
  spawn family).
- `cargo fmt -- --check`: clean.
- `cargo clippy --all-targets -- -D warnings`: clean.
- Every item's fix was verified RED (test fails with the fix reverted) then
  GREEN (test passes with the fix restored) by hand, not just written and
  assumed correct.

## Self-review / concerns

- **Item 2's cap-arm and fingerprint-arm changes are not independently
  RED-tested.** I verified RED/GREEN for the deadline-match arm (the core
  of item 2) by reverting it alone; the cap-arm fix (`Unknown if slow.is_
  some() => wait_cap(...)`) is necessary for the deadline fix to actually
  work end-to-end (without it, the very next line's cap check would
  immediately trip "proceeding anyway" using cap `0`), and the dedicated
  new test (`a_latched_slow_deadline_survives...`) does exercise the cap
  arm as a side effect of the loop actually running to ~1000s -- but I did
  not separately mutate just the cap arm to confirm it fails in isolation.
  I'm confident in it by inspection (cap `0` with elapsed `>= 0` always
  trips immediately), but flagging that the RED/GREEN discipline here
  covered the composite behavior, not each of the three match sites in
  total isolation.
- **Item 4's "once per episode" integration test is a proxy, not a direct
  observation of `.emit()`.** `a_slow_pass_announces_once_not_per_recheck`
  counts lines in the plain writer `w`, which is gated by the identical
  `announced != fingerprint` check as the `announcer.emit` call, so it's an
  honest proxy for "the announce arm fired once" -- but it cannot detect a
  regression where the emit call itself was deleted while the writeln!
  stayed (that specific regression is what `pacing_event_maps_wait_until_
  and_slow_and_nothing_else` catches instead, which is why I added both).
- **Item 5's test uses real filesystem I/O and wall-clock-shaped unix
  timestamps** (via `window::parse_iso8601_utc` on literal date strings)
  rather than the file's usual `NOW` constant, since the estimator/rollout
  paths both need real dates for their timestamp parsers. This is
  consistent with the existing pattern in `window.rs`'s own
  `refresh_skips_when_stored_reading_is_fresh_and_stores_when_stale` test.
- Item 1's test also needed a `HomeGuard`-isolated real transcript on disk
  (the estimator path has no injectable seam) -- same real-filesystem
  caveat as item 5, consistent with existing precedent in `score.rs`/
  `optimize.rs` tests.
- No changes were made to `exec.rs`, `run_loop.rs`, or any other caller of
  `pace::wait_for_window`/`PaceGateFlags` -- both changed signatures
  (`refresh_sources` gaining a parameter, `PaceGateFlags` gaining a field)
  are either module-private or covered by `#[derive(Default)]`, so no other
  call site needed updating. Confirmed by full `exec::`/`run_loop::`
  compile + test run above.
