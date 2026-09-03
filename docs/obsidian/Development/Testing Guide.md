---
last-verified: 2026-09-03
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

## Prompt-prefix stability harness (issue #299)

`tests/fixtures/fake-agent.sh` gains `FAKE_AGENT_PROMPT_LOG=<path>`: when set,
each invocation appends `\x1e<turn-index>\x1e` followed by the exact bytes of
the composed system prompt that run was actually handed, byte-exact, framed,
no trailing newline. Which argv shape it reads depends on the delivery
mechanism that adapter actually used for that run: `--append-system-prompt
<text>` (logged verbatim) or `--append-system-prompt-file <path>` (the
FILE's bytes are logged, never the path) for claude; `-c
developer_instructions=<json>` for codex. A run with none of the three present
still logs an empty-payload frame. See [[Context Management]]'s "Prefix
stability" section for what this is asserting and why.

The assertion half is inline `#[cfg(test)]` tests in `src/commands/ctx/
compile.rs` and `src/commands/ctx/prompt.rs`: `compile::test_support::
prefix_diff` (the shared first-differing-byte-offset-plus-context comparison
helper) and `compile::test_support::assert_change_confined_to_layer` (the
"a state change perturbs only its own declared suffix" gate, using
`compile::layers_of`'s layer-attribution). Both `test_support` items and
`compile::layers_of`/`CompiledContext::emitted_layers`/`EmittedLayer` are
`#[cfg(test)]`-only: this harness adds no runtime behavior and nothing to any
shipped code path, it only makes an existing invariant (the composed prompt's
own prefix stability) failable at test time. The fixture-driven case
(`two_turns_through_the_fake_agent_produce_matching_prefixes`) is
`#[cfg(unix)]`, like every other real-PTY/real-subprocess test in this
project — see [[Known Issues]] for why those only run in the Linux Docker
round, never on Windows.

## Quick Reference

| Task | Command |
|---|---|
| Run tests | `cargo test --verbose -- --test-threads=1` |
| Re-record claude fixture | `scripts/record-claude-fixture.py` |

**If changed:** update [[Getting Started]] if these commands change.
