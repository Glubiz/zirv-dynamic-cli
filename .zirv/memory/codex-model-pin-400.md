## Memory
- Key: codex-model-pin-400
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: codex, gotcha, debugging
- Paths: src/commands/ctx/adapters/codex.rs

If a codex delegation (zirv ctx agent codex, a dashboard codex pane, or codex review) fails outright with an HTTP 400 and no obviously bad zirv config, check ~/.codex/config.toml for a `model` line before looking anywhere in this codebase. zirv passes no --model to codex by default (CodexAdapter::default_worker_model() is None), so codex's own resolution applies; an operator pin the account's login does not support fails at the vendor, indistinguishable at zirv's level.
