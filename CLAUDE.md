# Zirv Dynamic CLI

Cross-platform CLI for executing developer-defined YAML/JSON/TOML scripts.

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
  - `command_types.rs` — Command type enum (Command, Script chaining)
  - `options.rs` — Per-command options (interactive, OS filter, delay, fallback)
  - `mod.rs` — Context building from params/secrets, entry point for execution
- `src/input.rs` — Clap CLI argument definitions
- `src/utils.rs` — File parsing (YAML/JSON/TOML), shortcuts, path helpers
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
  `--out` path. A test asserts the analysed tree is unchanged after a run.
- Repo-provided prompt text is untrusted input, like the repo `ctx.toml` layer:
  capped, labeled, and unable to enable itself.
