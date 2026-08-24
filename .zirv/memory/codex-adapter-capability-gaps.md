## Memory
- Key: codex-adapter-capability-gaps
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Importance: high
- Tags: codex, adapters, capabilities
- Paths: src/commands/ctx/adapters/codex.rs, src/commands/ctx/window.rs

Never assume claude/codex adapter parity. codex capabilities(): events true and system_prompt true (turn/token events derived from rollout JSON, issue #86; developer_instructions on a DIRECT launch), but marker_signal, token_usage and turn_signal stay false. On a shell-shim (Windows .cmd) launch there is no injection at all -- context falls back to task text, and zirv ctx status reports it. Tool calls, tool results and compaction have no verified rollout shape (issue #11).
