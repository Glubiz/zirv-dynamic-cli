# Qwen Code chat-session fixture

`chat-session.jsonl` is a hand-built append-only JSONL file (one JSON object
per line, no comment syntax -- provenance for each line lives here instead)
shaped after the real, verified record shapes `ChatRecordingService` writes
in `@qwen-code/qwen-code@0.23.0` (`npm pack @qwen-code/qwen-code@latest`,
captured 2026-09-07). Verified against the published npm tarball's bundled
JS (`package/chunks/chunk-4F7GQGXB.js`), not the (also fetched) GitHub source
at `QwenLM/qwen-code@main`, since the tarball's bundle is what actually keeps
the readable function/field names this fixture is shaped from.

Every record shares one base shape (`ChatRecordingService.createBaseRecord`):
`{uuid, parentUuid, sessionId, timestamp, type, provenance, cwd, version,
gitBranch}`. `type` is one of `"user"`, `"assistant"`, `"tool_result"`,
`"system"`.

- **Line 1, 2 (real user turn 1 + its assistant turn)**: the `"user"` row
  (`u1`) carries no `subtype` -- a genuine turn start. The `"assistant"` row
  (`a1`) adds `model` and `message` (`recordAssistantTurn`) plus
  `usageMetadata` (`GenerateContentResponseUsageMetadata`:
  `promptTokenCount`/`candidatesTokenCount`/`cachedContentTokenCount`/
  `totalTokenCount`, the same field names gemini-cli's own API usage carries,
  confirmed present in this codebase too). Unlike gemini-cli, qwen writes
  this ONE record per turn -- `recordAssistantTurn` builds the whole record
  and appends it once, never re-appending the same `uuid` later -- so there
  is no duplicate-id folding residual to exercise here (contrast
  `tests/fixtures/gemini/README.md`'s own fixture, whose whole point is that
  residual).
- **Line 3 (`t1`, `type: "tool_result"`)**: `recordToolResult`'s own shape --
  `message` carries a `functionResponse` part and `toolCallResult` is present.
  Included for realism only: no verified stable shape for `toolCallResult`
  was found in the time available (see `qwen.rs`'s own doc comment,
  "Deliberately UNSUPPORTED"), so nothing in
  `src/commands/ctx/adapters/qwen.rs` reads this line's `toolCallResult`, and
  `parse_events` skips `"tool_result"` rows entirely.
- **Line 4 (`n1`, `type: "user"`, `subtype: "mid_turn_user_message"`)**: a
  SYNTHETIC user-role row (`recordMidTurnUserMessage`) -- carries a
  `subtype`, so it must never register as a fresh turn. This is the fixture's
  own point: `QwenAdapter::is_user_turn` copies qwen's own internal rule
  verbatim (`chatRecordingService.ts`'s `restoreSessionState`: `record.type
  === "user" && record.subtype === void 0`) for exactly this reason, and
  `parse_events_skips_synthetic_user_rows_with_a_subtype` in `qwen.rs`
  asserts this line produces no `TurnStart`.
- **Line 5, 6 (real user turn 2 + its assistant turn)**: same shape as turn 1,
  with different token counts (`input_tokens == 420`, vs `150` for turn 1) so
  `transcript_usage`'s per-row (not folded, not cumulative) summation is
  distinguishable from a bug that always reports the first or last turn's
  reading.

Source cited above is inside the tarball at
`package/chunks/chunk-4F7GQGXB.js`, unpacked from
`@qwen-code/qwen-code@0.23.0` on 2026-09-07. That tarball is not committed to
this repository -- only this fixture and the citations above are.
