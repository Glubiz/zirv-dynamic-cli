---
last-verified: 2026-08-16
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

_None right now — see Recently Completed below for the latest landed work._

## Recently Completed

<!-- Cap at ~10 entries; drop the oldest when adding a new one. -->

### Derived harness roster + zirv-owned review policy; dashboard sidebar ownership scoping (`feat/harness-roster-prompt`, 2026-08-16)
**Status (2026-08-16):** Two commits (`a5bb389`, `183d45f`) plus an uncommitted review round on top. `a5bb389`: a new derived prompt layer (`PromptSource::Harnesses`, `adapters::harness_prompt_lines`) tells an Orchestrator session which harnesses it can `zirv agent` right now, gated by `prompt.harnesses`; `HARNESS_PROMPT` (v3→v4) gained self-initiative guidance and a single zirv-owned, bounded cross-harness review policy that claude's `ORCHESTRATOR_PROMPT`/`.zirv/claude.yaml` stay self-contained per role and layer on top of. `183d45f`: the dashboard sidebar's view-only rows are scoped to sessions this dashboard owns (`sessions::Record.owner_pid`), reversing `ac40418`. The review round (uncommitted) hardened the roster (a real presence check past `ready()`'s fail-open, self-exclusion, `agent_bin` scoped to the adapter it names) and moved `owner_pid` stamping into `SessionGuard::register` itself so every registration path is attributed uniformly. See [[Work Journal]] (2026-08-16) and [[Decision Log]] for both decisions.
**Next:** commit the review round, then open a PR against main. Two residuals recorded in [[Known Issues]], not fixed: the roster-restore liveness gap, and raw-pid ownership missing a pane child's own in-process headless fallback.

### zirv meta-harness (chat entry, coordination, dashboard) — merged (PR #21, 2026-08-14)
**Status (2026-08-16):** Merged into main. Delivered `zirv chat`'s dashboard multiplexer, cross-harness coordination (`zirv ctx agent`/`send`/`inbox`/`nudge`), codex shipping supported-but-degraded, and the pane-scrolling/overlay/header polish recorded in [[Work Journal]]'s 2026-08-15 entries. Remaining roadmap: codex event-parsing (issue #11), cross-harness handoff provenance, per-agent model routing.
**Next:** none — superseded by the entry above for anything dashboard-adjacent.

### Agent enable/disable settings and Obsidian vault setup — both merged (2026-08-12)
**Status (2026-08-16):** Both landed as part of the meta-harness work above (`.zirv/.settings.toml` gate; the 23-note vault plus CLAUDE.md contract, vault-keeper agent, and doc-coverage hooks) — confirmed ancestors of the current main HEAD. This entry replaces the two stale "in review" entries this page previously carried for PRs #17/#18.
**Next:** none.
