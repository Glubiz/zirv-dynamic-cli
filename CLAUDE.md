# Zirv Dynamic CLI

Cross-platform CLI for executing developer-defined YAML/JSON/TOML scripts.

> For comprehensive documentation, see `docs/obsidian/_system-context.md` (agent entry point) and `docs/obsidian/Home.md` (vault navigation).

## Build & Test

```bash
cargo build
cargo test --verbose -- --test-threads=1
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

## Architecture

- `src/main.rs` — CLI entry point, arg parsing, built-in command dispatch
- `src/commands/` — Built-in commands (create, init, help, version)
- `src/script_runner/` — Script execution engine
  - `script.rs` — Script data model and execution loop
  - `command.rs` — Single command execution with parameter substitution (`${var}`)
  - `command_types.rs` — Command step kinds (Command, Commands, Agent)
  - `options.rs` — Per-command options (interactive, OS filter, delay, fallback)
  - `mod.rs` — Context building from params/secrets, entry point for execution
- `src/input.rs` — Clap CLI argument definitions
- `src/utils.rs` — File parsing (YAML/JSON/TOML), shortcuts, path helpers
- `src/settings.rs` — `.zirv/.settings.toml` (per-agent enable/disable), distinct from `ctx.toml`
- `src/commands/ctx/` — Context management for AI agent sessions (`zirv ctx <verb>`)
  - `mod.rs` — Verb tree and dispatch, intercepted in `main.rs` before script lookup
  - `config.rs` / `state.rs` / `log.rs` — Layered config, platform state dir, decision log
  - `event.rs` / `rot.rs` — Normalized events and the pure deterministic rot engine
  - `adapters/` — `AgentAdapter` trait plus the claude and codex adapters
  - `score.rs` / `handoff.rs` / `resume.rs` / `hook.rs` / `status.rs` — One module per verb
  - `run_loop.rs` / `exec.rs` / `wrap.rs` — The three supervisors (`loop` is a keyword)
  - `signal.rs` / `supervise.rs` / `term.rs` — Turn-signal sockets, process primitives, raw mode
  - `pace.rs` / `usage.rs` / `window.rs` — Usage pacing gate, the `usage` verb (and statusline tee), rolling usage-window state
  - `optimize.rs` / `prompt.rs` — Configuration analysis and the injected session prompt
  - `chat.rs` — `zirv ctx chat`: an interactive orchestrator session built from the resolved adapter and driven through `wrap`
  - `agent.rs` — `zirv ctx agent <name> <prompt>`: one-shot delegation to a supervised headless worker, driven through `exec`
  - `mail.rs` — `zirv ctx send`/`zirv ctx inbox`: repo-scoped inter-session notes, read once then moved to `read/`
  - `sessions.rs` — The live session registry (`<state>/sessions/<short8>.json`) plus `zirv ctx nudge`
  - `memory.rs` — The cross-session memory bank (`zirv ctx remember`/`recall`/`forget`) and handoff-harvest opt-in
  - `chrome.rs` — Terminal chrome (launch banner, reserved status bar, colour) eligibility and pure renderers
  - `announce.rs` — The `zirv ▸` announcement channel on stderr
  - `dash/` — The `zirv chat` dashboard multiplexer: `mod.rs` (event loop, `Ctrl+A` prefix keys, overlays), `pane.rs` (supervised ConPTY child behind a vt100 screen), `ui.rs` (pure ratatui renderers), `spawnreq.rs` (spawn-request IPC for `zirv ctx agent` panes), `roster.rs` (quit capture / restore-on-relaunch)

## Conventions

- Rust edition 2024
- All command options use `#[serde(default)]` or `Option<T>`
- Parameters use `?` suffix for optional (e.g., `"branch?"`)
- Scripts live in `.zirv/` directories (local or global `~/.zirv/`)
- `zirv ctx` is a built-in resolved in `main.rs` before YAML script lookup, so a
  `.zirv/ctx.yaml` script named `ctx` is shadowed. `.zirv/ctx.toml` is the ctx
  config file and is excluded from script listing in `help.rs`.
- The rot engine is pure: no clock, no filesystem, no environment reads inside
  `rot.rs`, so the same events always produce the same verdict.
- `wrap` must never make a session worse. No `unwrap`/`expect` on its hot path,
  raw-mode restore happens in explicit arms (the release profile is
  `panic = "abort"`), and any supervision failure degrades to pure passthrough.
- Test fixtures under `tests/fixtures/` are data files only; tests stay inline in
  `#[cfg(test)] mod tests`. Re-record the claude fixture with
  `scripts/record-claude-fixture.py`.
- `zirv ctx optimize` is report-only. It may read any configuration surface and
  write only to stdout, its own report copy under the state dir, and an explicit
  `--out` path. A test asserts the analysed tree is unchanged after a run. The
  judgment/distiller model child's own tools are restricted too, for both
  adapters, not just zirv's own code path: each embeds untrusted repo prompt
  text in its own prompt, so the guarantee would otherwise rest on model
  judgment alone. `ClaudeAdapter::distiller_cmd` pins
  `--disallowedTools=Write,Edit,Bash,NotebookEdit`; `CodexAdapter::distiller_cmd`
  pins `--sandbox read-only`, verified against codex-cli 0.105.0
  (`npm install -g @openai/codex`, the version most operators actually get).
  The two restrictions are not identical guarantees: codex-cli 0.146.0 (a
  brew-only capture, not what npm publishes) additionally documents
  `--ignore-rules`/`--ignore-user-config`, which 0.105.0 does not have, so
  codex's distiller still reads the repo's `.rules` execpolicy files and the
  operator's own `~/.codex/config.toml` on top of AGENTS.md -- a known,
  recorded residual (see Known Issues), not a gap in this guarantee's own
  tests. Verified against the real CLI; see
  docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md and
  docs/superpowers/notes/2026-07-31-codex-cli-facts.md.
- Repo-provided prompt text is untrusted input, like the repo `ctx.toml` layer:
  capped, labeled, and unable to enable itself.
- Repo `.settings.toml` may only disable agents, never enable one: the same
  trust asymmetry as `ctx.toml`'s repo-forbidden keys, folded per agent rather
  than deep-merged (see `src/settings.rs`).
- Bare `zirv` (no arguments) is an alias: it starts `zirv ctx chat` in a repo
  with a **local** `.zirv` directory and both stdin and stdout attached to a
  real terminal, otherwise it shows `zirv help` — a deliberate behavior
  change from clap's own bare-invocation handling, which was a usage error
  (exit 2). A global `~/.zirv` alone does not count (only a local `.zirv`
  says "this repo is zirv-managed"), and both stdin *and* stdout have to be
  a terminal (`zirv | less` must not open a chat session into the pipe).
  `zirv chat` and `zirv agent` are further top-level aliases for `zirv ctx
  chat`/`zirv ctx agent`, checked against raw argv in `main.rs` before clap
  runs (same interception style as `ctx` itself) and reserved in
  `utils::RESERVED_COMMANDS` (compared case-insensitively, like
  `RESERVED_ZIRV_FILES`) so a script can never shadow them, in any case. An
  explicit `zirv chat` is not subject to the bare-invocation TTY rule. Any
  agent workflow that pipes `zirv`'s stdout/stdin should not rely on the bare
  form: pipe into `zirv ctx chat` (or a specific verb) explicitly instead.
- `REPO_FORBIDDEN` in `config.rs` also covers `mail.enabled`,
  `mail.max_delivered_bytes`, `chrome.events`, and `agent`, on top of the
  `agent_bin`/`handoff.model`/`optimize.model`/`prompt.*` keys: a repo
  checkout must not be able to raise its own delivered-mail cap, re-enable
  mail delivery an operator disabled, silence the `zirv ▸` announcement
  channel (including its own degradation notices), or pick which vendor
  account gets spent (`agent = "codex"` reaches `resolve_default`'s
  *configured* arm, which never consults the repo-narrowing guard the
  no-`agent`-configured fallback loop has). `~/.zirv/ctx.toml`, `ZIRV_CTX_*`
  and flags may still set every one of these; only a repo checkout may not.
- `zirv ctx send`/`zirv ctx inbox` deliver full message bodies only into a
  headless **Worker** session's prompt (`exec`/`loop`, gated by
  `cfg.mail.enabled`, consumed via `mail::consume` right after so a later
  launch/cycle does not see the same message again); an interactive
  **Orchestrator** session (`chat`/`wrap`) gets a one-line unread-count
  advisory instead, never the message bodies. Mail filenames
  (`mail::store`) get a collision-free `_NNN` suffix on a same-second
  collision, since `now_secs()` has one-second granularity and two real
  sends that close together is common, not a rare edge case. For an adapter
  with real system-prompt injection (claude) that Worker delivery is into
  the composed prompt, via `with_mail_layer`; for one without any injection
  mechanism (codex today, `capabilities().system_prompt == false`) that path
  delivers nothing at all (`injection_args_for_session` always returns an
  empty argv for it), so `task_prompt_with_mail_fallback` instead appends the
  same mail block onto the task prompt text itself — the one channel such an
  adapter has (argv, or stdin on a Windows shim launch). Mail is consumed
  only once it has actually reached one of these two channels.
- `ctx/mod.rs`'s `CtxCli` `about` text calls `adapters::readiness_note()`,
  which calls `ready()` on every registered adapter; this is cached in a
  process-wide `OnceLock` (`ctx_about()`) since it otherwise re-runs on every
  `dispatch()` call, including hook/statusline invocations that never
  display it.
- `pace.use_credits`/`poll_enabled`/`poll_min_interval_secs` are
  `REPO_FORBIDDEN`: a repo checkout must not change how often zirv polls a
  vendor usage endpoint or declare its own credits-cover-overage exemption.
  `pace.soft_percent` is deliberately **not** repo-forbidden (the spec rules
  it a tuning knob, and a repo can already set `pace.enabled = false`, so
  forbidding the band alone would be security theater). `poll.rs`'s `HttpPoller` reads an OAuth
  token fresh from disk on every call and never caches, logs, or persists it
  anywhere but that one outbound request; it is consulted only as a fallback
  once the passive collector reading has gone stale, floored to at most one
  attempt per `poll_min_interval_secs`, and never called from a path that
  must stay network-free (`wrap`'s status-bar redraw never constructs one).

## Using the Obsidian Vault

The `docs/obsidian/` vault is the project's knowledge base. Start with `docs/obsidian/_system-context.md` for agent-optimized context, or `docs/obsidian/Home.md` for full navigation.

### Before Starting Work

1. **Read `docs/obsidian/_system-context.md`** — mandatory first read for every session. Contains the module map, key flows, gotchas, and cross-reference index.
2. **Check Active Work** (`docs/obsidian/Development/Active Work.md`) — in-progress work and handoff context from the previous session.
3. **Check the Work Journal** (`docs/obsidian/Development/Work Journal.md`) — read the last 2–3 entries for recent context.
4. **Check Known Issues** (`docs/obsidian/Development/Known Issues.md`) — if working in an area with known gotchas.
5. **Check the Decision Log** (`docs/obsidian/Development/Decision Log.md`) — before proposing alternative approaches.

### After Completing Work

1. **Update Active Work** — move current work to "Recently Completed", add context for the next session.
2. **Log significant work** in the Work Journal. Cap each entry at ~10 lines; link out instead of inlining. When the active journal grows past ~10 entries, move the oldest to a quarterly file under `docs/obsidian/Development/journal-archive/`.
3. **Log non-obvious decisions** in the Decision Log — cap ~15 lines; if it needs more, write a spec under `docs/superpowers/specs/` and link it. Undocumented decisions get re-debated next session.
4. **Log new gotchas** in Known Issues; remove entries for resolved issues.
5. **Update affected documentation pages** per the table below — check the "If changed" line on each page you touched. Prefer deletion over layering.

### Obsidian Documentation Updates

Update vault pages when a **change in behavior, contract, or architecture** lands — not on every diff that touches a file. Each page has a `last-verified` date in its YAML frontmatter; update it when you verify or modify a page.

**Triggers that require a doc update:**

| Change type | Update |
|-------------|--------|
| CLI argument or built-in command added/changed | `Modules/Built-in Commands.md` |
| Script file format, option, or parameter semantics change | `Concepts/Script Files.md`, `Modules/Script Runner.md` |
| Shortcut resolution change | `Concepts/Shortcuts.md` |
| ctx verb added/removed/changed | `Modules/Ctx Subsystem.md` plus the specific module page |
| Adapter behavior change (claude/codex) | `Modules/Ctx Adapters.md` |
| Rot engine event or verdict change | `Modules/Rot Engine.md`, `Concepts/Context Management.md` |
| Supervisor behavior change (loop/exec/wrap, signals, raw mode) | `Modules/Ctx Supervisors.md` |
| Pacing/usage/window change | `Modules/Usage and Pacing.md` |
| Dependency added/removed or release profile change | `Architecture/Technology Stack.md` |
| Non-obvious architectural decision | `Development/Decision Log.md` (length cap above) |
| Significant sprint/session work | `Development/Work Journal.md` (length cap and archive rule above) |
| New gotcha discovered / old gotcha resolved | `Development/Known Issues.md` |

**Do NOT trigger doc updates for:**
- Refactors that don't change external behavior (internal helper extraction, formatting).
- Content that belongs in a commit message, PR body, or spec file.
- Bug fixes, unless the root cause is a gotcha worth preserving in Known Issues.
- New test files, dependency patch bumps, or CI-only changes.

When adding content, look first for an existing page that already covers the topic and extend it. When a topic fits two pages, pick one canonical owner and link from the other — no parallel copies.
