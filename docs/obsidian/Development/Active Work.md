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

### Dashboard multiplexer for `zirv chat` — docs done, review pending (`feat/dashboard`, 2026-08-13)
**Status (2026-08-13):** Code-complete: eight feature commits (`6f09c69`..`2abd7e3`) on top of `feat/agent-coordination`'s tip. `zirv chat`/bare `zirv` now opens a ratatui/crossterm/vt100 dashboard on a terminal at least 80x20 with both stdin and stdout a real terminal (`--simple` or a smaller terminal still falls back to plain `wrap` chrome). New `dash/{mod,pane,ui,spawnreq,roster}.rs`: one ConPTY child per pane behind an embedded `vt100::Screen`, `Ctrl+A`-prefixed commands (switch/spawn/nudge/mail/memory/zoom/quit), idle-gated visible nudge/mail injection into attached panes (orchestrator pane excluded from the automated mail sweep, matching `wrap`'s own advisory-only rule), a spawn-request IPC so `zirv ctx agent` joins a running dashboard as a pane, and a quit-time roster that offers restoring the previous panes (claude resumes via `--resume`, codex falls back to a note). `[dash]` config is fully `REPO_FORBIDDEN`; `[chat] model` is deliberately not. Obsidian sweep landed for this branch (see the Work Journal and Decision Log entries dated 2026-08-13); a rot score for a pane's own header is a known gap (`score: None` always) — see [[Ctx Supervisors]].
**Next:** review + merge order is `feat/zirv-chat` -> `feat/agent-coordination` -> `feat/dashboard` (unchanged from the two entries below, this branch just stacks one further); manually verify pane resize/zoom in a real Windows Terminal (ConPTY corner, not automatable here). Wiring an actual rot score into a pane's header is the next roadmap item this branch surfaced.

### zirv chat, mail, and TUI chrome — in review (`feat/zirv-chat`, 2026-08-13)
**Status (2026-08-13):** Full sweep landed and pushed as PR #19 (stacked on #18 → #17): bare-`zirv`/`zirv chat` supervised entry, registry-driven default-harness resolution, `zirv agent` delegation, `ctx send`/`inbox` mail with trust-split delivery, launch banner + reserved-row status bar + `zirv ▸` event channel. Two adversarial review rounds passed; exit-path audit done (every session end now announces why on stderr).
**Next:** merge #17 → #18 → #19; manually verify the status bar under resize in a real Windows Terminal (ConPTY corner is not automatable here). Remaining roadmap: codex adapter completion (externally blocked on codex hooks contract), cross-harness handoff provenance, per-agent model routing.

### Agent coordination: session registry, nudge, memory bank — coordination increment in review (`feat/agent-coordination`, 2026-08-13)
**Status (2026-08-13):** Built on top of the zirv-chat sweep above. Landed in waves: session registry + `zirv ctx nudge` (`sessions.rs`), per-session mail addressing (`send --to-session`), the memory bank (`remember`/`recall`/`forget`, injected as its own prompt layer), and this increment's own wave 3 — opt-in handoff-to-memory harvesting (`[memory] harvest`, default off), `status`'s registry-backed `sessions:` block and `memory:` line, `optimize`'s memory-bank size summary (never quotes bank content), and the T12b bar's split broadcast/direct mail count. Full test suite green against the documented Windows os-193 baseline; fmt/clippy clean.
**Next:** PR #20 is up (stacked on #19). Merge order #17 -> #18 -> #19 -> #20. Remaining roadmap items (codex adapter, cross-harness handoff provenance, per-agent model routing) are unchanged from the zirv-chat entry above.

## Recently Completed

<!-- Cap at ~10 entries; drop the oldest when adding a new one. -->

### Agent enable/disable settings — in review (`feat/agent-settings`, 2026-08-12)
**Status (2026-08-12):** `.zirv/.settings.toml` gate implemented, two review rounds passed; PR #18, stacked on PR #17.
**Next:** merge after PR #17.

### Obsidian vault setup — in review (`feat/obsidian-vault`, 2026-08-12)
**Status (2026-08-12):** Vault complete (23 notes, all wikilinks resolving) with Claude Code wiring: CLAUDE.md contract, vault-keeper agent, doc-coverage push hook (warn-once deny), staleness checker. PR #17.
**Next:** merge, then confirm the hook behaves in a real session.
