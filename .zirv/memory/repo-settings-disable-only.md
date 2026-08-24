## Memory
- Key: repo-settings-disable-only
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: settings, trust, agents
- Paths: src/settings.rs

A repo .zirv/.settings.toml may only DISABLE an agent, never enable one -- the same trust asymmetry as ctx.toml's repo-forbidden keys, but folded per agent (an AND-fold) rather than deep-merged. A repo disabling the configured default must refuse rather than silently switch vendor. .settings.toml is a distinct file from ctx.toml and is reserved from script lookup (utils::RESERVED_ZIRV_FILES, compared case-insensitively).
