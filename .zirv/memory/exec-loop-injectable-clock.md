## Memory
- Key: exec-loop-injectable-clock
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: testing, pacing, supervisors
- Paths: src/commands/ctx/exec.rs, src/commands/ctx/run_loop.rs

T11: exec::run_with and run_loop::run_with are thin wrappers over a pub(crate) run_with_clock taking injectable sleep_fn/now_fn -- the same FakeClock seam pace.rs's own unit tests use one layer down. Both suites' shared base_env helper zeroes ZIRV_CTX_PACE_BLIND_DELAY_SECS for speed, so one dedicated fast test per file overrides it back to a small nonzero value and records the call through an injected closure. That is the only proof the T8 fail-safe delay reaches a real sleep.
