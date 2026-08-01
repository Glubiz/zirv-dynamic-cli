# ctx test fixtures

Data files only. Cargo compiles nothing here; the Rust tests that read these
live inline in `src/commands/ctx/`.

## claude-real-session.jsonl

A scrubbed slice of a genuine Claude Code transcript, recorded with
`scripts/record-claude-fixture.py`. It contains a `compact_boundary` system
event, assistant messages with real `usage` fields, `tool_use` blocks, and
`tool_result` blocks with and without `is_error`.

It contains no sidechain event: every row carries `"isSidechain":false`, so the
sidechain filter in `parse_events` is covered by the synthetic case in
`sidechain_meta_and_garbage_lines_are_skipped`, not by this file.

Scrub rules applied by the recorder:

- `/Users/jonathansolskov` becomes `/home/testuser`, `jonathansolskov` becomes `testuser`
- credential-shaped strings become `REDACTED`: `sk-*` (at a word boundary),
  `gh*_*`, `AKIA*`, PEM private-key headers, `ApiKey ...`, `Bearer ...`, `eyJ*`,
  `key=<hex>`, and email addresses
- every string is truncated to 200 characters
- `sessionId` and `session_id` are pinned to `00000000-0000-4000-8000-000000000001`

The committed fixture predates the word-boundary rule, so a few harmless words
(`task-notification`) are over-scrubbed to `taREDACTED`. That is cosmetic, and
re-recording would need another real transcript, so it stays as is.

To re-record: `python3 scripts/record-claude-fixture.py <path-to-transcript>`,
then re-run `cargo test ctx::adapters::claude`. Both the fixture and
`claude-real-session.expected.json` must be committed together.

## claude-real-session.expected.json

Event counts derived from the fixture by the recorder. The Rust parser test
asserts its own counts equal these, which pins parser regressions against real
data.

## fake-agent.sh, fake-model.sh, stub-tui.sh

Executable stand-ins used by the supervisor tests. See the header comment in
each script.
