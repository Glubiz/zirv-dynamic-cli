## Memory
- Key: context-vs-memory-split
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Importance: high
- Tags: context, memory, prompt, design
- Paths: src/commands/ctx/context.rs, src/commands/ctx/memory.rs

context.rs owns .zirv/context/{common,claude,codex}.md -- HOW an agent should work: conventions, process, style, authored by a person, read fresh every session, never accumulated. memory.rs answers WHAT a past session learned: durable keyed facts. Neither substitutes for the other. Precedence: canonical common, then canonical harness-specific, then native global < repo < nested CLAUDE.md/AGENTS.md. Each context file caps at 4096 bytes (context.max_common_bytes/max_harness_bytes, REPO_FORBIDDEN).
