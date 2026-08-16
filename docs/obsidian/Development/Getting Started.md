---
last-verified: 2026-08-12
---

# Getting Started

Cross-platform CLI for executing developer-defined YAML/JSON/TOML scripts.

## Build & Test

```bash
cargo build
cargo test --verbose -- --test-threads=1
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

## Scripts

Developer-defined scripts live in `.zirv/` directories — either project-local
or global at `~/.zirv/`. See [[_system-context]] for how the CLI resolves and
dispatches them, and [[Testing Guide]] for how the test suite covers them.

## Quick Reference

| Task | Command |
|---|---|
| Build | `cargo build` |
| Test (serial) | `cargo test --verbose -- --test-threads=1` |
| Format check | `cargo fmt -- --check` |
| Lint | `cargo clippy --all-targets -- -D warnings` |

**If changed:** update [[Testing Guide]] if these commands change.
