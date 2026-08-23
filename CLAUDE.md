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
- `src/commands/workflow/` — Provider-neutral engineering workflows
  - `skill.rs` / `capability.rs` — versioned skills, layered registry, logical capabilities
  - `engine.rs` / `classify.rs` — durable phases and deterministic intent/complexity/risk
  - `verification.rs` / `review.rs` — targeted checks, evidence, review packages/findings
  - `artifact.rs` / `telemetry.rs` — static-first outputs and privacy-conscious statistics
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
  - `policy.rs` / `safety.rs` — Canonical per-capability permissions policy (`[policy]`) and the harness-neutral command safety classifier (`[safety]`, `zirv ctx safety check|list|explain`), each translated per adapter
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
- `skill`, `workflow`, `test`, `verify`, and `artifact` are also raw-argv
  built-ins with an independent clap tree. `.zirv/verify.toml` is reserved
  from script lookup.
- Workflow state is Zirv-owned and durable; only the current step's selected
  skill context is injected. Repository skills are untrusted requests and can
  never widen operator policy/capabilities: a repo manifest may only ADD an id,
  and one colliding with a built-in or operator-global skill is ignored with a
  warning (operator-global may still override a built-in). The `[workflow]`
  config section is `REPO_FORBIDDEN` in full: `repo_checks_enabled` gates
  whether `.zirv/verify.toml` and `package.json` script commands execute at all
  (off = listed with a skip line, never run, and never passing evidence),
  `repo_skills_enabled` gates the repo skill layer, and the three
  `telemetry_*` keys replaced plain `ZIRV_WORKFLOW_TELEMETRY*` environment
  reads any repo script could set for itself. Repo-supplied check timeouts are
  clamped to 900s and repo-supplied checks to 32, gate or no gate. The workflow
  reviewer is pinned read-only through `AgentAdapter::read_only_args`, the same
  flags `distiller_cmd` uses, because its prompt embeds a repo diff.
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
  **Orchestrator** session (`chat`/`wrap`) still never gets bodies, but now
  gets a live one-line advisory typed in at a verified-idle turn boundary
  (`wrap`'s own `MAIL_POLL`-cadence poll, or the dashboard's mail sweep for
  an attached pane) rather than only a stderr unread count. Mail filenames
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
  only once it has actually reached one of these two channels. `zirv ctx
  inbox` itself now consumes the caller-visible mail it displays by
  default; `--peek` keeps the old broad, idempotent read (including mail
  addressed to other sessions), and `--consume` is a no-op alias kept only
  for backward compatibility.
- `ctx/mod.rs`'s `CtxCli` `about` text calls `adapters::readiness_note()`,
  which calls `ready()` on every registered adapter; this is cached in a
  process-wide `OnceLock` (`ctx_about()`) since it otherwise re-runs on every
  `dispatch()` call, including hook/statusline invocations that never
  display it.
- `pace.use_credits`/`poll_enabled`/`poll_min_interval_secs`/`blind_delay_secs`
  are `REPO_FORBIDDEN`: a repo checkout must not change how often zirv polls a
  vendor usage endpoint, declare its own credits-cover-overage exemption, or
  shrink the fail-safe delay a genuinely blind gate applies instead of
  proceeding unthrottled (T8). `poll.rs`'s `HttpPoller` reads an OAuth
  token fresh from disk on every call and never caches, logs, or persists it
  anywhere but that one outbound request; it is consulted only as a fallback
  once the passive collector reading has gone stale, floored to at most one
  attempt per `poll_min_interval_secs`, and never called from a path that
  must stay network-free (`wrap`'s status-bar redraw never constructs one --
  though its pre-spawn launch path now does, see below).
- **`pace.enabled`/`max_percent`/`soft_percent` are a spend gate, not
  advisory, and are no longer a plain repo-overridable tuning knob (T9,
  2026-08-22, revisiting the reasoning above).** A repo layer may narrow
  pacing -- lower `max_percent`/`soft_percent`, or force `enabled = true`
  even against an operator's own `false` -- but may never widen it: raise
  either percentage above the operator's own value, or turn pacing off.
  Modeled on `policy::resolve`'s own narrowing fold (`Stance::max`), not
  added to `REPO_FORBIDDEN` outright, so the legitimate "this repo is
  expensive, be more careful here" case still works: `config.rs`'s
  `narrow_pace_bool`/`narrow_pace_percent` lift these three keys out of both
  layers before the ordinary deep merge (the same seam `[policy]` and
  `sandbox.extra_deny` already use) and fold them before `ZIRV_CTX_PACE*`
  env, which still wins outright over both layers. See the Decision Log for
  why the old "a repo can already disable pacing, so guarding the band alone
  is theater" reasoning no longer holds.
- **T10: `wrap`'s pre-spawn launch path now consults the pacing gate too**,
  closing the coverage gap where `wrap` standalone, `zirv ctx chat`'s
  orchestrator, and every dashboard pane (both launched through the same
  `wrap::run_with`/`dash::run_dashboard` code) never paced at all -- only the
  reactive `scan_for_limit` caught a vendor-imposed limit, after the fact.
  Interactive and headless launches get different treatment for the same
  decision (`pace::InteractiveGate`, mapped from the same `PaceDecision`
  `wait_for_window` already computes): the soft band shows the usage and a
  skippable pause (any keypress or `--force-pace` proceeds); the hard
  ceiling refuses by default and requires a deliberate `'y'` confirmation or
  `--force-pace`; blind data reuses `usage_source_hint`'s own reason/remedy
  rather than silently inheriting the T8 delay. Gated on both stdin *and*
  stdout being real terminals (mirroring `chrome::dash_eligible`'s own
  double-check) -- there is no one to prompt otherwise, and blocking on a
  `crossterm` keypress read with no terminal attached is exactly the kind of
  silent hang a spend gate must not cause; a non-interactive `wrap`
  invocation is out of scope for this fix (`exec`/`loop` are the headless
  supervisors, and already gated). A dashboard **worker** pane spawned while
  the dashboard's own event loop is live (`fulfill_spawn_request`) cannot
  reuse the blocking keypress read at all -- it would collide with the
  dashboard's own input loop reading the same `crossterm` stream -- so it
  gates non-interactively instead: the soft band spawns anyway with a
  notice, and the hard ceiling refuses the spawn outright with no
  confirmation possible from that call site. Only the dashboard's own
  *first* (orchestrator) pane spawn, which runs before `enable_raw_mode`/
  `EnterAlternateScreen`, is early enough to reuse the full interactive
  treatment safely.
- **T11: `exec::run_with`/`run_loop::run_with`'s `sleep_fn`/`now_fn` are
  injectable now** (`run_with_clock`, a `pub(crate)` sibling each thin
  `run_with` wrapper delegates to with the real clock/sleep), the same
  `FakeClock`-style seam `pace.rs`'s own unit tests already use one layer
  down. Before this, the T8 fail-safe delay was verifiable in `pace.rs` but
  not at this integration level, so both files' test suites zeroed
  `ZIRV_CTX_PACE_BLIND_DELAY_SECS` via their shared `base_env` test helper
  just to stay fast -- a real regression risk, since nothing proved the
  delay actually reached a real `sleep_fn` call. It does now, in one
  dedicated fast test per file (`ZIRV_CTX_PACE_BLIND_DELAY_SECS` overridden
  back to a small nonzero value, recorded through an injected closure rather
  than actually slept).

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
| Command safety policy change (`[safety]`, `safety.rs`, the wired `PreToolUse` hook) | `Modules/Command Safety.md`, `Modules/Ctx Adapters.md`, `Concepts/Untrusted Configuration.md` |
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
