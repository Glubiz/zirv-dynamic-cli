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

### `fix/process-lifecycle` — reap child process trees on every teardown path (stacked on `feat/harness-roster-prompt`, 2026-08-16)
**Status (2026-08-16):** Two commits (`c843891`, `222b24f`), stacked on `feat/harness-roster-prompt`, pending final re-review + PR. Three Windows lifecycle layers landed: (P1) `supervise::kill_tree` now runs at every pty teardown seam (`wrap::quit_child`, `dash::pane::Pane::finish_shutdown`, the distiller's timeout escalation), not just `exec`/`loop`. (P2) a cross-platform supervised-pid registry swept by the Windows console-close handler on terminal events only. (P3) kill-on-close Job Objects (`ChildGuard`/`JobGuard`, new `windows-sys` `Win32_System_JobObjects` feature) as the kernel backstop for a crash or `taskkill /F` against zirv itself. The review round (`222b24f`) added the roster-restore liveness fail-safe (`partition_live`/`short_is_live`, deferred rather than dropped) and parked `wrap`'s registry record on zirv's own pid during a restart's kill→respawn window. See [[Work Journal]] and [[Decision Log]] (supersedes the 2026-08-14 Job-Object rejection).
**Next:** final re-review, then open the PR (stacked on the still-open `feat/harness-roster-prompt` PR). Five residuals recorded in [[Known Issues]], not fixed: a dropped `ChildGuard` kills its child on Windows (a new invariant every future caller must hold); Job-Object assignment races a shim's own grandchild; the distiller's `kill_tree` escalation has no dedicated test; a live session's roster entry never ages out while it stays alive; a held-back roster candidate is still lost if the dashboard never reaches `on_quit`.

### Derived harness roster + zirv-owned review policy; dashboard sidebar ownership scoping (`feat/harness-roster-prompt`, 2026-08-16)
**Status (2026-08-16):** Three commits (`a5bb389`, `183d45f`, `a2536a5`), all committed; `fix/process-lifecycle` above now stacks on top. `a5bb389`: a new derived prompt layer (`PromptSource::Harnesses`, `adapters::harness_prompt_lines`) tells an Orchestrator session which harnesses it can `zirv agent` right now, gated by `prompt.harnesses`; `HARNESS_PROMPT` (v3→v4) gained self-initiative guidance and a single zirv-owned, bounded cross-harness review policy that claude's `ORCHESTRATOR_PROMPT`/`.zirv/claude.yaml` stay self-contained per role and layer on top of. `183d45f`: the dashboard sidebar's view-only rows are scoped to sessions this dashboard owns (`sessions::Record.owner_pid`), reversing `ac40418`. `a2536a5` (the review round): hardened the roster (a real presence check past `ready()`'s fail-open, self-exclusion, `agent_bin` scoped to the adapter it names) and moved `owner_pid` stamping into `SessionGuard::register` itself so every registration path is attributed uniformly. See [[Work Journal]] (2026-08-16) and [[Decision Log]] for both decisions.
**Next:** open a PR against main (still open as of 2026-08-16). Two residuals recorded in [[Known Issues]], not fixed: the roster-restore liveness gap (now itself superseded by `fix/process-lifecycle`'s own fail-safe and residuals above), and raw-pid ownership missing a pane child's own in-process headless fallback.

### zirv meta-harness (chat entry, coordination, dashboard) — merged (PR #21, 2026-08-14)
**Status (2026-08-16):** Merged into main. Delivered `zirv chat`'s dashboard multiplexer, cross-harness coordination (`zirv ctx agent`/`send`/`inbox`/`nudge`), codex shipping supported-but-degraded, and the pane-scrolling/overlay/header polish recorded in [[Work Journal]]'s 2026-08-15 entries. Remaining roadmap: codex event-parsing (issue #11), cross-harness handoff provenance, per-agent model routing.
**Next:** none — superseded by the entry above for anything dashboard-adjacent.

### Agent enable/disable settings and Obsidian vault setup — both merged (2026-08-12)
**Status (2026-08-16):** Both landed as part of the meta-harness work above (`.zirv/.settings.toml` gate; the 23-note vault plus CLAUDE.md contract, vault-keeper agent, and doc-coverage hooks) — confirmed ancestors of the current main HEAD. This entry replaces the two stale "in review" entries this page previously carried for PRs #17/#18.
**Next:** none.
