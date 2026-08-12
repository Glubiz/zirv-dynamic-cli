---
last-verified: 2026-08-12
---

# Testing Guide

## Running tests

```bash
cargo test --verbose -- --test-threads=1
```

Serial execution (`--test-threads=1`) is required, not optional — see
[[Known Issues]] for why parallel runs are unsafe here.

## Where tests live

Tests stay inline in `#[cfg(test)] mod tests` next to the code they cover
(e.g. `src/script_runner/command.rs`, `src/commands/ctx/rot.rs`,
`src/commands/ctx/wrap.rs`) — there is no separate top-level test tree for
unit tests.

`tests/fixtures/` holds data files only, not test code: recorded sessions
(`claude-real-session.jsonl` / `.expected.json`), fake agent/model/statusline
shell scripts used as test doubles, and sample statusline JSON. Nothing under
`tests/fixtures/` is itself executed as a test.

## Re-recording fixtures

The claude fixture (`tests/fixtures/claude-real-session.*`) is re-recorded
with:

```bash
scripts/record-claude-fixture.py
```

Don't hand-edit the recorded fixture — regenerate it instead so it stays a
faithful capture of a real session.

## Quick Reference

| Task | Command |
|---|---|
| Run tests | `cargo test --verbose -- --test-threads=1` |
| Re-record claude fixture | `scripts/record-claude-fixture.py` |

**If changed:** update [[Getting Started]] if these commands change.
