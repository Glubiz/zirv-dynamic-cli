## Memory
- Key: distiller-read-only-pins
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Importance: high
- Tags: security, adapters, optimize, workflow
- Paths: src/commands/ctx/adapters/claude.rs, src/commands/ctx/adapters/codex.rs

The distiller/judgment model child and the workflow reviewer are restricted structurally, not by model judgment, because their prompts embed untrusted repo text (a CLAUDE.md, a repo diff). ClaudeAdapter::distiller_cmd pins --disallowedTools=Write,Edit,Bash,NotebookEdit as one =-bound argv token; CodexAdapter::distiller_cmd pins --sandbox read-only. AgentAdapter::read_only_args applies the same flags to the reviewer. Never relax these to "the model will behave".
