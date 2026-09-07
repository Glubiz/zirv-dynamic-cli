# copilot events.jsonl fixture

Hand-built, not captured from a real run: GitHub Copilot CLI (npm
`@github/copilot`, packed version `1.0.83` as of 2026-09-07 via `npm pack
@github/copilot@latest`) ships as a loader around a compiled per-platform
binary with no bundled JS source to read, and is not installed on the machine
this fixture was written on. The `events.jsonl` row shapes below are taken
from two independent sources, neither of which is GitHub's own published
documentation (there is none yet -- `github/copilot-cli#3551`, "Formalize
events.jsonl as an official hook/integration API", is still an open feature
request as of 2026-09-07):

- Jon Chew's blog post <https://jonmagic.com/posts/github-copilot-session-search-and-resume-cli/>
  (a GitHub employee; the post states he "helped confirm some of the original
  findings by reviewing internal code"), for the `session.start`,
  `user.message`, `assistant.turn_start`, `tool.execution_start`,
  `tool.execution_complete` and `session.shutdown` `type` values and their
  `data` shapes (`content`, `turnId`, `toolName`/`arguments`, `success`).
- `ccusage/ccusage` (MIT-licensed open source, `rust/adapters/copilot/src/
  parser.rs`, fetched via `gh api repos/ccusage/ccusage/contents/...`), whose
  `CopilotSessionStateEvent`/`CopilotSessionStateData`/
  `CopilotSessionModelMetrics`/`CopilotSessionUsage` structs and their own
  inline test fixtures independently confirm `session.shutdown`'s own
  `data.modelMetrics.<model>.usage` field names (`inputTokens`,
  `outputTokens`, `cacheReadTokens`, `cacheWriteTokens`, `reasoningTokens`)
  and `data.modelMetrics.<model>.requests.count`.

Both sources agree the format is explicitly UNVERSIONED and may drift between
Copilot CLI releases ("the event payloads evolve, but the broad structure
still looks like this" / "treat ... the files under session-state/ as
internal implementation details").

Thirteen lines, a small two-turn "add a health check endpoint, then a test"
session that restarts once (hence two `session.shutdown` rows):

1. `session.start` -- repository/branch context, not read by this adapter.
2. `user.message` -- "Add a health check endpoint" (starts turn 1).
3. `assistant.turn_start` -- `turnId` only, no response text (see
   `adapters::copilot`'s own module doc comment, "Deliberately UNSUPPORTED",
   for why no verified event carries the model's own reply text at all).
4. `tool.execution_start` -- `read` on `src/router.ts`.
5. `tool.execution_complete` -- `success: true`.
6. `tool.execution_start` -- `write` on `src/router.ts`.
7. `tool.execution_complete` -- `success: false` (a failed write, exercising
   `NormalizedEvent::ToolResult { is_error: true }`).
8. `session.shutdown` (`shutdown-1`) -- `claude-sonnet-4.5`, `inputTokens:
   1200, outputTokens: 300, cacheReadTokens: 200, cacheWriteTokens: 100`.
9. `user.message` -- "Also add a test" (starts turn 2).
10. `assistant.turn_start` -- `turnId` only, `turn-2`.
11. `tool.execution_start` -- `write` on `src/router.test.ts`.
12. `tool.execution_complete` -- `success: true`.
13. `session.shutdown` (`shutdown-2`) -- `gpt-5.3-codex`, `inputTokens: 500,
    outputTokens: 120`, no cache tokens.

Summed usage across both shutdown rows (asserted in
`adapters::copilot::tests::transcript_usage_sums_every_shutdown_row_and_excludes_cache_from_input`):
`input_tokens = (1200 - 200 - 100) + (500 - 0 - 0) = 1400`,
`output_tokens = 300 + 120 = 420`, `cache_creation_input_tokens = 100`,
`cache_read_input_tokens = 200` -- the input-token subtraction mirrors
`ccusage`'s own verified `uncached_session_input_tokens`, since Copilot's own
`inputTokens` counts cache reads/writes inclusively while
`TranscriptUsage::input_tokens` excludes them by contract.
