## Memory
- Key: codex-hook-trust-gate-may-fail-registered-hooks
- Written-by: claude
- Written: 1788980045
- Verified: 1788980045
- Source: explicit

codex exec can print "hook: <name> Failed" for zirv's registered hooks even though they run clean by hand and via a live codex exec round-trip. Unconfirmed theory: codex hashes each hook definition ([hooks.state].trusted_hash in config.toml) and refuses one whose hooks.json no longer matches until re-trusted interactively or with --dangerously-bypass-hook-trust; unattended codex exec has no TTY for that. The theory is only testable from hooks.json and [hooks.state] captured at failure time.
