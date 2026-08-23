## Memory
- Key: shared-memory-scope-untrusted
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: memory, trust, security
- Paths: src/commands/ctx/memory.rs

The shared memory bank is <repo>/.zirv/memory/<key>.md (committed, UNTRUSTED); the private bank is <state>/memory/<repo_slug>/. Shared keys are validated: lowercase a-z0-9- only, non-empty, not all dashes, not a Windows device name, length-capped -- one charset check that also rules out traversal. Every header-rendered field is checked for embedded newlines, since one would inject a fake `- Key:` line that parse_markdown reads back as legitimate. safe_shared_dir refuses symlinked .zirv or .zirv/memory.
