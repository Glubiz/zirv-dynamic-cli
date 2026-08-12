---
last-verified: 2026-08-12
---

# Active Work

> Claude: update at session start and end for seamless handoffs.

Entries use the format:

```
### <title> — <state> (`branch-name`, YYYY-MM-DD)
**Status (date):** what's true right now.
**Next:** the next concrete step.
```

## In Progress

### Agent enable/disable settings — in review (`feat/agent-settings`, 2026-08-12)
**Status (2026-08-12):** `.zirv/.settings.toml` gate implemented, two review rounds passed; PR #18, stacked on PR #17. A prioritized roadmap for broader harness/inter-agent improvements (session registry, status telemetry, registry hygiene, mailbox, codex completion, cross-harness handoff, model routing) was designed and awaits prioritization.
**Next:** merge PR #17, then PR #18; pick the first roadmap item.

## Recently Completed

<!-- Cap at ~10 entries; drop the oldest when adding a new one. -->

### Obsidian vault setup — in review (`feat/obsidian-vault`, 2026-08-12)
**Status (2026-08-12):** Vault complete (23 notes, all wikilinks resolving) with Claude Code wiring: CLAUDE.md contract, vault-keeper agent, doc-coverage push hook (warn-once deny), staleness checker. PR #17.
**Next:** merge, then confirm the hook behaves in a real session.
