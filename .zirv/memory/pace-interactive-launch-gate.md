## Memory
- Key: pace-interactive-launch-gate
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: pacing, wrap, dashboard
- Paths: src/commands/ctx/pace.rs, src/commands/ctx/wrap.rs, src/commands/ctx/dash/mod.rs

T10: wrap's pre-spawn launch path consults the pacing gate (pace::resolve_interactive_gate, InteractiveGate). Soft band = show usage plus a skippable pause (any key or --force-pace); hard ceiling = refuse unless 'y' or --force-pace; blind data reuses usage_source_hint's reason. Gated on stdin AND stdout being real terminals. A dashboard worker pane spawned while the event loop is live (fulfill_spawn_request) cannot read keys, so it gates non-interactively: soft spawns with a notice, hard refuses.
