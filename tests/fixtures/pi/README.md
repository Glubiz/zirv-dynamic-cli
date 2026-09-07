# pi session.jsonl fixture

Hand-built, not captured from a real run: `pi-coding-agent` (npm
`pi-coding-agent`, published as `@earendil-works/pi-coding-agent`) is not
installed on the machine this fixture was written on. Every field is taken
from the upstream source at `github.com/badlogic/pi-mono` on `main`, fetched
2026-09-07:

- `packages/coding-agent/src/core/session-manager.ts` -- `SessionEntryBase`
  (`type`, `id`, `parentId`, `timestamp`), `SessionMessageEntry` (`type:
  "message"`, `message: AgentMessage`), `CompactionEntry` (`type:
  "compaction"`, `summary`, `firstKeptEntryId`, `tokensBefore`, optional
  `usage`).
- `packages/agent/src/types.ts` -- `AgentMessage` wraps the `Message` union.
- `packages/ai/src/types.ts` -- `Message = UserMessage | AssistantMessage |
  ToolResultMessage`; `AssistantMessage` (`content`, `api`, `provider`,
  `model`, `usage`, `stopReason`); `ToolResultMessage` (`toolCallId`,
  `toolName`, `content`, `isError`); `TextContent` (`type: "text"`, `text`);
  `ToolCall` (`type: "toolCall"`, `id`, `name`, `arguments`); `Usage`
  (`input`, `output`, `cacheRead`, `cacheWrite`, `totalTokens`, `cost`).

Fields `api`/`provider`/`stopReason`/`cost` are included for shape
completeness only -- `adapters::pi::PiAdapter::parse_events`/
`transcript_usage`/`structural_context`/`model_hint` do not read them.

Five lines, one small "add a health check endpoint" turn:

1. `a1` -- a `user` message (starts a turn).
2. `a2` -- an `assistant` message: text ("I'll check the router first."), a
   `toolCall` to `read`, `usage.input = 1200`.
3. `a3` -- the matching `toolResult` for `a2`'s call, `isError: false`.
4. `a4` -- the final `assistant` message: text ("Added GET /health returning
   200 OK."), `usage.input = 1500`, `usage.cacheRead = 300`.
5. `a5` -- a `compaction` entry, exercising `NormalizedEvent::Compaction`.

Summed assistant usage across `a2` + `a4` (asserted in
`adapters::pi::tests::transcript_usage_sums_every_assistant_messages_own_usage`):
`input_tokens = 2700`, `output_tokens = 300`, `cache_read_input_tokens = 300`,
`cache_creation_input_tokens = 0`.
