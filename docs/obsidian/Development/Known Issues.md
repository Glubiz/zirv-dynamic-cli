---
last-verified: 2026-08-12
---

# Known Issues

Gotchas that have cost debugging time. Remove an entry once it's resolved — this
file tracks live traps, not history (use [[Decision Log]] or [[Work Journal]]
for that).

Each entry gets a changelog comment at the top of the file, newest first:

```
<!-- Updated YYYY-MM-DD (branch, state): what changed -->
```

<!-- Updated 2026-08-12 (feat/obsidian-vault, seeded): initial gotchas pulled from repo CLAUDE.md -->

## `ctx` shadows `.zirv/ctx.yaml`

`zirv ctx` is a built-in resolved in `main.rs` before YAML script lookup, so a
`.zirv/ctx.yaml` script named `ctx` is silently shadowed and never runs.
`.zirv/ctx.toml` is a different file — it's the ctx config, and it's excluded
from script listing in `help.rs`.

## Tests must run with `--test-threads=1`

`cargo test --verbose -- --test-threads=1` is required, not optional — tests
share state (state dir, fixtures) and will flake or corrupt each other under
the default parallel test runner.

## `wrap`'s hot path assumes `panic = "abort"`

The release profile is `panic = "abort"`, so a panic on `wrap`'s hot path
cannot unwind to a cleanup handler — raw-mode terminal restore must happen in
explicit arms, not in a `Drop` guard relying on unwind. No `unwrap`/`expect` on
that path; any supervision failure must degrade to pure passthrough instead of
leaving the terminal in raw mode.
