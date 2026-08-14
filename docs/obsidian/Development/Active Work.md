---
last-verified: 2026-08-14
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

_None right now — the zirv meta-harness stack (chat entry, coordination, dashboard) is consolidated into the single open PR #21; see Recently Completed below._

## Recently Completed

<!-- Cap at ~10 entries; drop the oldest when adding a new one. -->

### zirv meta-harness (chat entry, coordination, dashboard) — round-9 review fixes landed, PR #21 open (`feat/dashboard`, 2026-08-14)
**Status (2026-08-14):** PRs #17–#20 (obsidian vault, agent-settings gate, zirv-chat entry point, agent coordination) are all closed, folded into **PR #21**, which is the single open PR into main. This round (commits `45ba361`, `ab86b0b`, `98bfe52`) closed a live Windows RCE in the `--help` capability probe and a case-folded reserved-name bypass, fixed the shim-detection false negative that left the forced system-prompt-file defense inert on `zirv chat`/bare `zirv`/the dash orchestrator, made Windows child termination kill the whole process tree instead of just the launcher, made state-dir writes atomic, and fixed the dashboard's cursor rendering, key encoding, and per-tick event/mail draining. Full suite green at the documented Windows baseline (44 environmental failures), zero regressions. See [[Work Journal]] (2026-08-14 entry) and [[Decision Log]] for specifics.
**Next:** merge PR #21. A rot score for a pane's own header is still a known gap (`score: None` always) — see [[Ctx Supervisors]]. Remaining roadmap unchanged: codex adapter completion (externally blocked on codex hooks contract), cross-harness handoff provenance, per-agent model routing.

### Agent enable/disable settings — in review (`feat/agent-settings`, 2026-08-12)
**Status (2026-08-12):** `.zirv/.settings.toml` gate implemented, two review rounds passed; PR #18, stacked on PR #17.
**Next:** merge after PR #17.

### Obsidian vault setup — in review (`feat/obsidian-vault`, 2026-08-12)
**Status (2026-08-12):** Vault complete (23 notes, all wikilinks resolving) with Claude Code wiring: CLAUDE.md contract, vault-keeper agent, doc-coverage push hook (warn-once deny), staleness checker. PR #17.
**Next:** merge, then confirm the hook behaves in a real session.
