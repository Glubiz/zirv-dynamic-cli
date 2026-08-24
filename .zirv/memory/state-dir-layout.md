## Memory
- Key: state-dir-layout
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: state, storage, paths
- Paths: src/commands/ctx/state.rs

StateDir::resolve roots at ZIRV_CTX_STATE_DIR, else the OS state dir, else the OS local-data dir, then zirv/ctx. Subpaths: handoffs/<repo_slug>/, s/ (turn-signal sockets, short for the unix path-length limit), logs/decisions.jsonl, usage.json (one machine-wide merged file), scoring/, sessions/<short8>.json, memory/<repo_slug>/. Unix dirs are 0700 and files 0600, a no-op on Windows. Writes go through temp-file-plus-rename, never truncate-in-place.
