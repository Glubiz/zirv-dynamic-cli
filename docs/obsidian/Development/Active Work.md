---
last-verified: 2026-08-15
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

### zirv meta-harness (chat entry, coordination, dashboard) — release-candidate, PR #21 open (`feat/dashboard`, 2026-08-15)
**Status (2026-08-15):** PR #21 is the single open PR into main; the branch is release-candidate. This round (`40d5c84`, `ce7d69f`, `92d0d8b`, `9b32f02`, `a3b81c3`) fixed pane scrolling (the wheel is forwarded to a mouse-owning child; vt100 scrollback is used only on a normal screen), made `Ctrl+A` arrows move focus onto pane rows rather than just the sidebar cursor, made the sidebar itself scroll, made overlays opaque so an open one can no longer swallow every keystroke invisibly, and replaced the header's usage row (which in practice always read "no usage source") with the per-pane/per-row rot score (`score::cached_score`). Per-provider usage storage (`AgentAdapter::provider()`) is kept for `usage`/`pace`/`wrap` even though the dashboard no longer renders it. Full suite green at the documented Windows baseline (1266 passed / 44 environmental failures), zero regressions. PR #21 review's two remaining open decisions are now resolved: codex ships supported-but-degraded (launch-level only; `ready()`/`base()`/`launches_through_cmd_shim`/`headless_cmd_stdin` mirror claude's, event parsing stays issue #11) and `examples/key_probe.rs` is dropped in favor of `alt_screen_probe.rs`. That codex round (`adapters/{codex,mod}.rs` plus test updates across `agent.rs`/`chat.rs`/`dash/roster.rs`/`hook.rs`/`mod.rs`/`status.rs`, README and vault pages) is landed in the working tree on `feat/dashboard`, not yet committed. See [[Work Journal]] (2026-08-15 entries) and [[Decision Log]] for specifics.
**Next:** commit and push the codex-support round, then merge PR #21. Remaining roadmap unchanged: codex event-parsing (issue #11), cross-harness handoff provenance, per-agent model routing.

### Agent enable/disable settings — in review (`feat/agent-settings`, 2026-08-12)
**Status (2026-08-12):** `.zirv/.settings.toml` gate implemented, two review rounds passed; PR #18, stacked on PR #17.
**Next:** merge after PR #17.

### Obsidian vault setup — in review (`feat/obsidian-vault`, 2026-08-12)
**Status (2026-08-12):** Vault complete (23 notes, all wikilinks resolving) with Claude Code wiring: CLAUDE.md contract, vault-keeper agent, doc-coverage push hook (warn-once deny), staleness checker. PR #17.
**Next:** merge, then confirm the hook behaves in a real session.
