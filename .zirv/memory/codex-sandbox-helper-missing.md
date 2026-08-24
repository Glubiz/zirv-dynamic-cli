## Memory
- Key: codex-sandbox-helper-missing
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: codex, sandbox, gotcha, windows
- Paths: src/commands/ctx/adapters/codex.rs

On a codex-cli install whose Windows sandbox helper codex-windows-sandbox-setup.exe is absent (standalone installer, [windows] sandbox = "elevated"), `codex exec --sandbox read-only` fails outright with orchestrator_helper_launch_failed, so every zirv ctx optimize/handoff and every codex workflow review fails there. --ignore-rules/--ignore-user-config exist on codex-cli 0.146+, not on npm's 0.105.0; CodexAdapter::read_only_args probes --help and fails closed, announcing the residual once.
