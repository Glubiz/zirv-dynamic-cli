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
  judgment/distiller model child's own tools are restricted too
  (`ClaudeAdapter::distiller_cmd`), not just zirv's own code path: it embeds
  untrusted repo CLAUDE.md text in its prompt, so the guarantee would
  otherwise rest on model judgment alone. Verified against the real CLI; see
  docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md.
- Repo-provided prompt text is untrusted input, like the repo `ctx.toml` layer:
  capped, labeled, and unable to enable itself.
- Repo `.settings.toml` may only disable agents, never enable one: the same
  trust asymmetry as `ctx.toml`'s repo-forbidden keys, folded per agent rather
  than deep-merged (see `src/settings.rs`).

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
