# Compaction fixtures (issue #426)

Data only -- see `tests/fixtures`' own convention; nothing here is compiled
or executed. Each `<name>.txt` is a realistic, synthetic command output (no
secrets, no real paths beyond `/home/testuser`); the sibling `<name>.cmd`
holds the exact one-line command that "produced" it, read verbatim by the
regression test in `src/commands/ctx/output.rs`
(`compaction_ratio_never_regresses_below_the_fixture_floor`).

That test drives every fixture through the SAME engine
`hook::run_posttool` drives for a real captured Bash result --
`classify_compaction` then `capture_text` -- with the default `[output]`
config, and asserts:

- every fixture's summary is smaller than its raw bytes (the never-worse
  guard already enforces this in production; this is the regression net for
  it specifically over realistic-shaped output), and
- the AGGREGATE byte reduction across every fixture is at least the floor
  recorded in `floor.txt` (a bare float, e.g. `0.60` meaning at least 60%
  aggregate reduction).

## The floor rule

`floor.txt` is a ratchet, not a target:

- If a change to the compaction engine measurably IMPROVES the aggregate
  ratio, lower the bar is wrong -- RAISE `floor.txt` to the new measured
  value, rounded down to the nearest `0.05` (never round up: the floor must
  stay a value every subsequent run can actually clear).
- Never LOWER `floor.txt` without a `zirv ctx remember --repo` memory entry
  explaining why the aggregate ratio regressed on purpose (e.g. a
  mandatory-content guarantee got strictly bigger for a good reason). A
  silent lowering defeats the entire point of this test.

## Fixture set

Eleven fixtures cover the shapes `zirv ctx`'s compaction engine actually
special-cases: `cargo test` (failures), `cargo build` (warnings only),
`cargo nextest run` (failures), `pytest` (failures), `npm install` (registry
noise), `npm test` (jest-style failures), `git diff` and `git log -p`
(unified diff, bounded per-file listing rather than head/tail), a generic
unrecognised build tool with a linker failure, and a JSON API-shaped output
(`kubectl get pods -o json`, exercised via the JSON-aware summary path
rather than the line-based scan). All are large enough to clear the
relevant `[output]` compaction threshold for their scope.
