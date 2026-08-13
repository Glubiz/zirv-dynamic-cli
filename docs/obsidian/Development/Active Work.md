---
last-verified: 2026-08-13
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

### zirv chat, mail, and TUI chrome — in review (`feat/zirv-chat`, 2026-08-13)
**Status (2026-08-13):** Full sweep landed and pushed as PR #19 (stacked on #18 → #17): bare-`zirv`/`zirv chat` supervised entry, registry-driven default-harness resolution, `zirv agent` delegation, `ctx send`/`inbox` mail with trust-split delivery, launch banner + reserved-row status bar + `zirv ▸` event channel. Two adversarial review rounds passed; exit-path audit done (every session end now announces why on stderr).
**Next:** merge #17 → #18 → #19; manually verify the status bar under resize in a real Windows Terminal (ConPTY corner is not automatable here). Remaining roadmap: codex adapter completion (externally blocked on codex hooks contract), cross-harness handoff provenance, per-agent model routing, session registry.

## Recently Completed

<!-- Cap at ~10 entries; drop the oldest when adding a new one. -->

### Agent enable/disable settings — in review (`feat/agent-settings`, 2026-08-12)
**Status (2026-08-12):** `.zirv/.settings.toml` gate implemented, two review rounds passed; PR #18, stacked on PR #17.
**Next:** merge after PR #17.

### Obsidian vault setup — in review (`feat/obsidian-vault`, 2026-08-12)
**Status (2026-08-12):** Vault complete (23 notes, all wikilinks resolving) with Claude Code wiring: CLAUDE.md contract, vault-keeper agent, doc-coverage push hook (warn-once deny), staleness checker. PR #17.
**Next:** merge, then confirm the hook behaves in a real session.
