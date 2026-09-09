## Memory
- Key: claude-code-loads-nested-context-claude-md
- Written-by: claude
- Written: 1788979271
- Verified: 1788979271
- Source: explicit

On a case-insensitive filesystem (Windows, macOS) Claude Code auto-loads any file literally named claude.md as a nested CLAUDE.md the first time a file in that directory is read. zirv's context dedupe (compile.rs) only tracks the repo-root CLAUDE.md/AGENTS.md, so reading .zirv/context/common.md mid-session silently double-loads .zirv/context/claude.md, uncounted. Avoid reading files under .zirv/context/ during a session; zirv cannot suppress it.
