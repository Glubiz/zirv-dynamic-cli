# ctx test fixtures

Data files only. Cargo compiles nothing here; the Rust tests that read these
live inline in `src/commands/ctx/`.

## claude-real-session.jsonl

A scrubbed slice of a genuine Claude Code transcript, recorded with
`scripts/record-claude-fixture.py`. It deliberately contains a
`compact_boundary` system event, assistant messages with real `usage` fields,
`tool_use` blocks, `tool_result` blocks with and without `is_error`, and at
least one sidechain event.

Scrub rules applied by the recorder:

- `/Users/jonathansolskov` becomes `/home/testuser`, `jonathansolskov` becomes `testuser`
- credential-shaped strings (`sk-*`, `gh*_*`, `ApiKey ...`, `Bearer ...`, `eyJ*`) become `REDACTED`
- every string is truncated to 200 characters
- `sessionId` and `session_id` are pinned to `00000000-0000-4000-8000-000000000001`

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
