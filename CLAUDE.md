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

## Conventions

- Rust edition 2024
- All command options use `#[serde(default)]` or `Option<T>`
- Parameters use `?` suffix for optional (e.g., `"branch?"`)
- Scripts live in `.zirv/` directories (local or global `~/.zirv/`)
