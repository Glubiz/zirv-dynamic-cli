---
last-verified: 2026-08-15
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

### 2026-08-15: codex enabled out of the box; `key_probe` example retired
**What:** Flipped the codex adapter from inert to launch-supported: `ready()` now resolves the binary like claude's (instead of hard-erroring), `base()` routes through `resolve_program`, `launches_through_cmd_shim` is overridden, and a new `headless_cmd_stdin` delivers the headless prompt via stdin on shim launches. Capabilities stay honestly all-false (no event parsing, rot score, usage source, turn signal, injected system prompt) — full event support stays issue #11. Also dropped `examples/key_probe.rs` (its finding is already encoded in `dash::is_prefix_key`), keeping `alt_screen_probe.rs` as the one reusable harness-behaviour diagnostic.
**Key changes:** `adapters/{codex,mod}.rs`, `agent.rs`, `chat.rs`, `dash/roster.rs`, `hook.rs`, `mod.rs`, `status.rs`, README, vault pages (`Known Issues`, `Ctx Adapters`, `Ctx Subsystem`, `_system-context`); `examples/key_probe.rs` deleted.
**Follow-up:** see [[Decision Log]] for the scope decision. PR #21 review decisions resolved; this round is uncommitted on `feat/dashboard`.

### 2026-08-15: dashboard scrolling, opaque overlays, and a leaner header
**What:** Five commits closing the dashboard's biggest usability gaps. A pty probe against the real harness explained why panes never scrolled (alternate screen plus two independent vt100 scrollback traps); the dashboard now forwards the wheel to a child that owns the mouse (SGR/X10, pane-local coordinates) and falls back to vt100 scrollback only on a normal screen. `Ctrl+A` arrows now move focus onto pane rows, not just the sidebar cursor, and the sidebar itself scrolls. Overlays (`render_dialog`/`render_overlay`) are opaque and fall back to the full frame instead of silently swallowing every keystroke behind invisible pane bleed-through. The header shrank back to one row: usage is gone (every figure read "no usage source" in practice) and the rot score (`score::cached_score`, per focused pane and per sidebar row) is the header's point now.
**Key changes:** `dash/{mod,pane,ui}.rs`, `term.rs` (mouse-mode bytes), `score.rs` (`cached_score`), `adapters/{mod,claude,codex}.rs` + `state.rs`/`window.rs` (per-provider usage storage, kept even though the header stopped reading it), `config.rs` (`dash.mouse`).
**Follow-up:** see [[Known Issues]] for the vt100/overlay/mouse gotchas and [[Decision Log]] for the four decisions; PR #21 is the single open PR into main and the branch is release-candidate.

### 2026-08-14: round-9 review — help-probe RCE, tree-kill, atomic state writes, dashboard cursor/keys
**What:** Three commits closing findings from a further adversarial pass on `feat/dashboard`. Security: the `--help` capability probe itself was an unguarded `cmd.exe` spawn (live RCE via a distilled `--resume` summary), the forced system-prompt-file form was inert on `zirv chat`/bare `zirv`/the dash orchestrator (shim-detection false negative), and reserved-name interception was case-sensitive while script dispatch's own guard wasn't. Supervisors: Windows `terminate` now kills the whole process tree (`taskkill /T /F`), not just the launcher; state writes are atomic (temp+rename); `nested_session_evidence` now requires a live `owner.pid`. Dashboard: the cursor is finally drawn, input/mail/drain are all bounded per tick, special-key encoding carries the xterm modifier parameter, and the dead `WaitingInput`/`⏸` state is removed.
**Key changes:** `adapters/{claude,codex,mod}.rs`, `config.rs`, `prompt.rs`, `main.rs` (45ba361); `memory.rs`, `sessions.rs`, `state.rs`, `supervise.rs` (ab86b0b); `dash/{mod,pane,ui,spawnreq}.rs` (98bfe52). Full suite green at the documented Windows baseline (44 environmental failures), zero regressions.
**Follow-up:** see [[Known Issues]] and [[Decision Log]] for the individual gotchas and the ESC-CR/taskkill/atomic-write/owner.pid decisions; PR #21 is the single open PR into main.

### 2026-08-14: dashboard adversarial-review fixes
**What:** Closed thirteen findings from the review of the dashboard branch. The load-bearing ones: an `Ord::clamp` panic that took the whole dashboard down whenever any overlay was drawn into a zero-width rect; a spawn-request prompt beginning with `-` reaching the harness child as a flag (now refused at the authority side, and never even written by the requester); the dashboard's orchestrator pane getting **no** composed zirv prompt at all while every other launch path did; and terminal restore that disabled nothing, showed no cursor, and wrote to the wrong stream. Also: zoom now changes what is drawn (not just the pty size), sidebar *focus* is separate from *selection* so a view-only row no longer swallows all typing, the mail sweep delivers one message per pane per tick, the `Ctrl+A s` dialog is wired to the same spawn path a request takes, and a claimed-but-unanswered request no longer double-runs headless.
**Key changes:** `dash/{mod,ui,spawnreq}.rs`, `chat.rs` (`dash_orchestrator_pane`), `agent.rs` (`try_join_dashboard`: option/argv/claim checks), `term.rs` (`dash_reset_bytes`, `set_dash_active`, `stash_current_console`), `config.rs` (`dash.max_panes`, default 9, `REPO_FORBIDDEN` + `ZIRV_CTX_DASH_MAX_PANES`).
**Follow-up:** none; the pre-existing Windows os-193 test baseline (44 failures, fake-agent.sh + temp path length) is unchanged.

### 2026-08-13: `zirv chat` dashboard multiplexer
**What:** `zirv chat`/bare `zirv` opens a ratatui/crossterm/vt100 session multiplexer on a capable terminal (>=80x20, both streams a tty, VT on, `cfg.dash.enabled`) instead of the plain `wrap` chrome — N panes, each a supervised ConPTY child behind its own embedded `vt100::Screen`. `Ctrl+A`-prefixed commands (digits/Tab/arrows switch panes, s/n/m/M open spawn/nudge/mail/memory overlays, z zooms, q quits with a confirm if any pane is `Working`). Idle-gated visible nudge/mail injection into attached panes; `zirv ctx agent` joins a running dashboard as a fresh pane via a capability-token spawn-request directory; quit writes a per-repo restore roster, offered once on the next launch.
**Key changes:** `src/commands/ctx/dash/{mod,pane,ui,spawnreq,roster}.rs` (new), `adapters/{mod,claude,codex}.rs` (`model_args`/`resume_args`), `chat.rs`/`chrome.rs` (`dash_eligible`, `chat.model` splice), `config.rs` (`[dash]`, `[chat]`), `agent.rs` (`try_join_dashboard`), `sessions.rs` (`Verb::Dash`), `mail.rs`/`window.rs` (shared `unread_counts`/`max_used_percentage`, moved out of `wrap.rs`).
**Follow-up:** no rot score wired into a pane's own header yet (`score: None` always — see [[Ctx Supervisors]]). Spec: `docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md`; plan: `docs/superpowers/plans/2026-08-13-zirv-dashboard.md`.

### 2026-08-13: Handoff harvest, richer `status`, and split mail counts (agent-coordination wave 3)
**What:** Opt-in handoff-to-memory harvesting (`[memory] harvest`, default off): right after a *distilled* (never structural-fallback) rot restart, one extra cheap-model call extracts durable repository facts (`Gotchas learned`/`Files touched` only) as strict `key: body` lines, stored via `remember` with `source = "handoff"`. `zirv ctx status` gained a registry-backed `sessions:` block (agent/verb/pid/age/live-or-stale, plus orphaned sockets labeled `(no record)`) and a `memory:` summary line. `zirv ctx optimize`'s report now includes a memory-bank size summary that never quotes an entry's key or body. The T12b bar's mail count now splits broadcast from session-addressed (`mail 2+1`).
**Key changes:** src/commands/ctx/memory.rs (`harvest_from_handoff`, `harvest_prompt`, `parse_harvest`), exec.rs/wrap.rs (harvest call sites in the rot-restart paths only, not nudge), status.rs (`sessions_lines`, `format_age`), optimize.rs (`MemorySummary`, `memory_bank_summary`, `render_memory_section`), chrome.rs/wrap.rs (`BarState::unread_mail` now `(broadcast, direct)`), tests/fixtures/fake-model.sh (`harvest` mode).
**Follow-up:** none for this wave.

### 2026-08-12: `zirv chat`/`zirv agent`, bare-zirv alias, and `status`'s chat/mail lines
**What:** Top-level routing for the "just run `zirv`" wave: bare `zirv` aliases to `zirv ctx chat` (repo has a `.zirv` dir, stdin is a tty) or `zirv help` otherwise, exiting 0 either way instead of clap's old usage-error exit 2. `zirv chat`/`zirv agent` are further top-level aliases for `zirv ctx chat`/`zirv ctx agent`, reserved so a script can never shadow them. `zirv ctx status` gained a `chat:` line (the adapter `chat` would launch and the rule that picked it, or why nothing qualifies) and a `mail: N unread` line, both degrading rather than failing the rest of the command.
**Key changes:** src/main.rs (`top_level_ctx_alias`, `rewrite_ctx_alias_args`, `bare_invocation_target`, `zirv_dir_present`), src/utils.rs (`RESERVED_COMMANDS` +chat/+agent), src/commands/help.rs, src/commands/ctx/status.rs (`describe_chat`), README, CLAUDE.md, vault pages. Landed alongside (not touched by this wave): `chat.rs`/`agent.rs`/`mail.rs` verbs, `chrome.rs`/`announce.rs` terminal chrome.
**Follow-up:** none for this wave; `announce.rs`'s event channel was still a placeholder at the time these docs were written — see its own module doc.

### 2026-08-12: Agent enable/disable gate (.zirv/.settings.toml)
**What:** New zirv-wide settings file toggling the claude/codex harnesses, enforced in `adapters::select` before `ready()`. Repo layer can only narrow; env is operator authority. Malformed repo file falls back to an operator-only/deny-all gate.
**Key changes:** src/settings.rs (new), adapters/mod.rs + 10 call sites, utils/help/input reserved-name guards, ctx status, README, vault pages. PR #18.
**Follow-up:** harness roadmap (session registry, mailbox, codex completion) awaits prioritization — see [[Decision Log]] and PR #18 description.

### 2026-08-12: Obsidian vault created
**What:** Set up the docs/obsidian vault (23 notes: Architecture, Modules, Concepts, Development) mirroring the zirv-fitness setup, plus Claude Code wiring: CLAUDE.md vault contract with doc-update trigger table, vault-keeper agent, doc-coverage push hook, staleness checker.
**Key changes:** docs/obsidian/**, CLAUDE.md, .claude/settings.json, .claude/agents/vault-keeper.md, scripts/check-doc-*.sh, .gitignore.
**Follow-up:** none.
