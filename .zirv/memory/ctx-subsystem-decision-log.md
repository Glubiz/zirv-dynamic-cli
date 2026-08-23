## Memory
- Key: ctx-subsystem-decision-log
- Written-by: zirv-setup
- Written: 1787497926
- Verified: 1787497926
- Source: setup
- Importance: normal
- Confidence: high
- Tags: documentation
- Paths: docs/obsidian/Modules/Ctx Subsystem.md

`log.rs` appends one JSON line per decision to `<state>/logs/decisions.jsonl` via `append()`, using the same private-file helpers as the rest of the state dir. Each `Decision` record carries a timestamp, session id, verb, rot verdict, numeric score, action taken, and free-text detail. `tail(state, count)` reads the whole file and returns the last `count` lines (oldest of the tail first) — used by the `status` verb. The log is append-only; nothing in this module rewrites or rotates it.
