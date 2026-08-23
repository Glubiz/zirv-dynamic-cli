## Memory
- Key: workflow-repo-gates
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: workflow, trust, config
- Paths: src/commands/workflow/verification.rs, src/commands/ctx/config.rs

[workflow] is REPO_FORBIDDEN in full. repo_checks_enabled gates whether .zirv/verify.toml and package.json script commands execute at all (off = listed with a skip line, never run, never passing evidence); repo_skills_enabled gates the repo skill layer; the three telemetry_* keys replaced ZIRV_WORKFLOW_TELEMETRY* env reads any repo script could set for itself. Repo-supplied check timeouts clamp to 900s and repo checks to 32, gate or no gate.
