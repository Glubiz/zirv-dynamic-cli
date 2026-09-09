## Memory
- Key: codex-hook-trust-gate-may-fail-registered-hooks
- Written-by: claude
- Written: 1788979272
- Verified: 1788979272
- Source: explicit

codex exec can print "hook: <name> Failed" for zirv's registered hooks even though they run clean by hand and via a live codex exec round-trip. Unconfirmed theory: codex hashes each hook definition ([hooks.state].trusted_hash in config.toml) and refuses one whose hooks.json no longer matches until re-trusted interactively or with --dangerously-bypass-hook-trust; unattended codex exec has no TTY for that. On recurrence, capture hooks.json and [hooks.state] at failure time before changing anything.
