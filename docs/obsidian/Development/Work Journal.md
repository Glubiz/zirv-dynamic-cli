---
last-verified: 2026-08-12
---

# Work Journal

## How to use

- **Reading:** check the last 2–3 entries at the start of a session for recent context.
- **Writing:** entry after any non-trivial change (feature, refactor, bug fix, infra). Skip if a commit message already captures it.
- **Cap:** keep new entries to ~10 lines. If you need more, it's a spec or a [[Decision Log]] entry, not a journal note. Link out; don't inline.
- **Rotation:** when the active journal grows past ~10 entries, move the oldest ones to a quarterly file under `journal-archive/` (frontmatter `archived: true`, header stating the covered date range).

## Format

### YYYY-MM-DD: short title
**What:** one or two sentences.
**Key changes:** files/services touched.
**Follow-up:** anything unfinished (optional).

## Entries

### 2026-08-12: Obsidian vault created
**What:** Set up the docs/obsidian vault (23 notes: Architecture, Modules, Concepts, Development) mirroring the zirv-fitness setup, plus Claude Code wiring: CLAUDE.md vault contract with doc-update trigger table, vault-keeper agent, doc-coverage push hook, staleness checker.
**Key changes:** docs/obsidian/**, CLAUDE.md, .claude/settings.json, .claude/agents/vault-keeper.md, scripts/check-doc-*.sh, .gitignore.
**Follow-up:** none.
