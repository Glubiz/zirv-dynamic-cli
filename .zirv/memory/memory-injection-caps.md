## Memory
- Key: memory-injection-caps
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: memory, prompt, config
- Paths: src/commands/ctx/config.rs, src/commands/ctx/prompt.rs, src/commands/ctx/memory.rs

MemoryConfig defaults: max_entries 50 (oldest by Written pruned), max_entry_bytes 512 (remember truncates rather than fails), core_max_bytes 2048 for the always-injected core layer, plus retrieval_max_bytes/retrieval_max_entries for the query-ranked layer. Private entries structurally outrank shared ones by partition and suppression, never by score, and the core layer ranks on VERIFICATION recency, not the importance field. See prompt::select_memory_within_cap.
