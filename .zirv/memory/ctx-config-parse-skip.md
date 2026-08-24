## Memory
- Key: ctx-config-parse-skip
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: config, trust, gotcha
- Paths: src/commands/ctx/config.rs, src/commands/workflow/verification.rs

A ctx.toml layer that merely fails to parse as TOML is SKIPPED (config::UnparsableLayer, announced once) rather than failing the whole load -- but a REPO_FORBIDDEN key rejection still hard-errors. Exception: a broken HOME layer still fails a verb that launches a harness (CtxConfig::load vs load_for_launch). Consequence: workflow::repo_gates closes only on a real Err, so a plain repo-layer syntax error no longer disables repo_checks_enabled/repo_skills_enabled.
