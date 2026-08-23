## Memory
- Key: handover-keeps-short-id
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: handover, sessions, wrap
- Paths: src/commands/ctx/handover.rs, src/commands/ctx/wrap.rs, src/commands/ctx/sessions.rs

zirv ctx handover swaps the orchestrator seat's model or harness IN PLACE using the same SessionGuard::adopt_child_pid calls a same-harness restart makes (parked on zirv's own pid, then the fresh child's) and never calls refresh_session -- so the registry short id is identical before and after, and mail plus `zirv ctx nudge <id>` keep working across the swap. Cross-harness argv/turn-env assembly lives in handover::resolve_swap_launch/build_turn_env, shared by both live-swap sites.
