## Memory
- Key: pace-repo-may-only-narrow
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: pacing, config, trust
- Paths: src/commands/ctx/config.rs, src/commands/ctx/pace.rs

pace.enabled/max_percent/soft_percent are a spend gate, not a tuning knob (T9, 2026-08-22). A repo layer may NARROW pacing (lower either percent, force enabled=true even against an operator's false) but never widen it (raise a percent, turn pacing off). config.rs's narrow_pace_bool/narrow_pace_percent lift the three keys out of both layers before the ordinary deep merge -- the same seam [policy] and sandbox.extra_deny use -- and fold them before ZIRV_CTX_PACE* env, which still wins outright.
