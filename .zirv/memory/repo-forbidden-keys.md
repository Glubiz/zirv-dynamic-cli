## Memory
- Key: repo-forbidden-keys
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Importance: high
- Tags: config, security, trust, ctx-toml
- Paths: src/commands/ctx/config.rs

REPO_FORBIDDEN in config.rs hard-errors if a repo .zirv/ctx.toml sets: agent, agent_bin, supervise.on_failure, handoff.model, optimize.model, sandbox.enabled/extra_allow, every prompt.* key, context.max_common_bytes/max_harness_bytes/max_harness_roster_bytes, mail.enabled/max_delivered_bytes, chrome.events, memory.shared_enabled, all of [workflow] and [review], and pace.use_credits/poll_enabled/poll_min_interval_secs/blind_delay_secs. Only ~/.zirv/ctx.toml, ZIRV_CTX_* and flags may set them.
